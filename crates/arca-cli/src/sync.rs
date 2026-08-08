//! `file://` 同步闭环（M1d Task 6）：扫描本地 → 读基线 → 读存储根 → 交给
//! `arca_core::decide` 出决策 → 按 [`arca_core::reconcile::Action`] 执行 →
//! 更新基线。`file://` 不是一种传输协议，它就是"dataset_root 在本地文件系统
//! 上"这一事实——没有网络、没有守护进程，`sync` 是这个闭环唯一的执行者。
//!
//! **决策全部来自 `arca_core::decide`，本模块不得有第二套判断逻辑**——见
//! CLAUDE.md「架构约束」。本模块只负责"收到一个 [`arca_core::reconcile::Action`]
//! 之后具体怎么做"，即下表右列；左列（该不该这么做）永远由 `arca-core` 决定：
//!
//! | Action | 执行 |
//! | --- | --- |
//! | `Noop` | 无 |
//! | `Upload{parent}` | 写 `files/` + 追加 `items/` + 更新 `index/` |
//! | `Download{version_id}` | 从存储根读出内容写到本地 |
//! | `AdoptBaseline{hash, version_id}` | 零传输，只更新基线 |
//! | `DeleteLocal{item_id}` | **过四道闸门（`gates::check_delete`，M2a Task 4）后**才移除本地副本；任一闸门不过则不删，计入 `delete_blocked` |
//! | `TombstoneRemote{item_id, parent}` | 提交 tombstone：`files/` → `.arca/trash/` + 追加 journal `op=tombstone` 事件（M2a Task 4 收尾，复用 Task 3 交付的 `trash`/`journal` 原语） |
//! | `Conflict{..}` | 不动数据，计入报告 |
//! | `NeedsHuman{..}` | 停下，计入报告 |
//!
//! # 与 brief 字面签名的一处刻意偏离
//!
//! Task 6 brief 给的签名是 `sync(dataset, root, sink)`，不含 `actor` 参数。
//! 但 `items::Version` 的每条记录都要求归因（I8：每个事件可归因），`actor`
//! 是调用者的上下文（账号/设备/会话），`sync` 作为 sans-io 风格的执行器
//! 不该自己伸手去读环境变量拼一个——那会把"从哪来"的判断权拿走，也让
//! 确定性测试没法注入固定值。与 `scan.rs` 顶部同一条先例（brief 签名落后
//! 于实现，文档说明偏离之处）。
//!
//! # tombstone 传播的两端：接收（Task 4 前半）与发起（Task 4 收尾）都已接通
//!
//! M2a Task 3 之后，`hub::read_remote` 已经能读 journal 产出
//! `RemoteState::Tombstoned`（决策表 `present|unchanged|tombstoned ->
//! DeleteLocal` 第一次在真实运行中可达）；Task 4 前半接通了 `DeleteLocal`
//! 的**安全执行**——过四道闸门（`gates::check_delete`）之后才真的移除本地
//! 副本，任一闸门不过则不删、计入 `SyncReport::delete_blocked`。
//!
//! `TombstoneRemote`（本地删除 → 向 hub **提交** tombstone）此前也无处落盘
//! （brief 字面只覆盖接收侧的四道闸门），但端到端验证要求这条链路必须真的
//! 走通——`execute_tombstone` 补上了"发起"这一侧：直接复用 Task 3 已经交付、
//! 已被独立测试覆盖的 [`crate::trash::move_to_trash`] + [`crate::journal::append`]，
//! **不新增任何销毁数据的代码路径**（I3：移进 `.arca/trash/` 是 tombstone，
//! 不是物理销毁，保留期内 `arca restore` 能找回）。这一步之所以安全、甚至是
//! Task 1（下载内容 fsync 纪律）想要防住的那类崩溃场景的进一步兜底：即便
//! 某台设备因为过去的 bug/崩溃把"内容还在但没同步"的文件误判成本地删除，
//! 传播到 hub 的后果也只是把内容移进回收站（可恢复），不是无法挽回的销毁。
//! `SyncReport::tombstone_submitted` 记录本轮成功提交的路径（不影响
//! `is_clean()`，与 `deleted_local` 同一性质——正常完成的动作）；
//! `tombstone_pending` 字段保留给未来可能出现的"决定要提交但暂时不安全"的
//! 情形（当前决策表到达 `TombstoneRemote` 时 `remote_state` 必然是
//! `RemoteState::Present`，`trash::move_to_trash` 因此总能找到源文件，这个
//! 桶目前恒空，但类型上保留，供未来加发起侧闸门时使用）。

use crate::{baseline, clock, gates, hub, ids, journal, scan, trash, vault};
use arca_core::reconcile::{decide_traced, Action};
use arca_core::state::{BaseState, LocalState, RemoteState};
use arca_format::hub_layout::layout;
use arca_format::index::IndexRecord;
use arca_format::items;
use arca_format::manifest::{Manifest, ManifestEntry};
use arca_format::model::{Actor, ItemId, Version};
use arca_format::path_rules;
use arca_format::trace::TraceSink;
use arca_store::atomic::{self, AtomicError, Batch};
use arca_store::root::StorageRoot;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 一次 `sync` 的执行结果：每类 [`Action`] 各自落进一个分类桶，按路径排序
/// （确定性，供 `--json` 输出与测试断言）。
#[derive(Debug, Default)]
pub struct SyncReport {
    pub uploaded: Vec<String>,
    pub downloaded: Vec<String>,
    pub adopted: Vec<String>,
    pub deleted_local: Vec<String>,
    /// `DeleteLocal` 被四道闸门（`gates::check_delete`）里的至少一道拦下——
    /// **不删**（I3、I5：状态模糊或不安全就停下，绝不"尽力删"）。逐条保留
    /// 具体是哪个 [`gates::GateFailure`]，供 `arca sync` 的诊断输出与
    /// `--json` 消费。
    pub delete_blocked: Vec<(String, gates::GateFailure)>,
    /// 本轮成功提交给 hub 的 tombstone（`TombstoneRemote` 已执行：内容进了
    /// `.arca/trash/`，journal 追加了 `op=tombstone` 事件）——正常完成的动作，
    /// 与 `deleted_local` 同一性质，不计入 `is_clean()` 的"有问题"判断。
    pub tombstone_submitted: Vec<String>,
    /// 决定要提交 tombstone 但暂时做不到的路径——**不是空操作**，是"本该做
    /// 但这一版做不了"的如实记录（见模块顶部文档）。当前决策表到达
    /// `TombstoneRemote` 时源文件必然还在 hub（`remote_state` 是
    /// `RemoteState::Present`），这个桶目前恒空；类型上保留，供未来给发起侧
    /// 加安全闸门时使用。
    pub tombstone_pending: Vec<String>,
    pub conflicts: Vec<String>,
    pub needs_human: Vec<String>,
    /// 扫描阶段被拒绝、根本没能进入调和的路径（不合规路径、符号链接等）。
    pub scan_rejected: Vec<(String, scan::RejectReason)>,
    /// 本次运行是否因为基线缺失/损坏而整体重置（触发了一次全量对账）。
    pub baseline_reset: bool,
}

impl SyncReport {
    /// 是否"干净"：没有任何冲突、没有需要人工介入的状态、没有未完成的
    /// tombstone 传播、也没有扫描阶段被拒绝的路径。供调用方（`arca sync`
    /// 命令壳）决定退出码——0 = 干净，非 0 = 有问题/有未完成（spec §3.2）。
    pub fn is_clean(&self) -> bool {
        self.delete_blocked.is_empty()
            && self.tombstone_pending.is_empty()
            && self.conflicts.is_empty()
            && self.needs_human.is_empty()
            && self.scan_rejected.is_empty()
    }

    /// 本次实际改动了什么（供 Rule of Silence：全同步时这几项皆空，命令壳
    /// 据此决定是否需要在 stdout 打印任何东西）。
    pub fn changed(&self) -> bool {
        !self.uploaded.is_empty()
            || !self.downloaded.is_empty()
            || !self.adopted.is_empty()
            || !self.deleted_local.is_empty()
            || !self.tombstone_submitted.is_empty()
    }
}

/// `sync` 失败——真正的 IO/格式故障，与"决策落在 Conflict/NeedsHuman 这类
/// 正常但需要报告的终态"是不同性质的结果（后者进 [`SyncReport`]，不是 `Err`）。
#[derive(Debug)]
pub enum SyncError {
    Scan(scan::ScanError),
    Baseline(baseline::BaselineError),
    Hub(hub::HubError),
    Atomic(AtomicError),
    Format(arca_format::error::FormatError),
    /// 提交 tombstone 时 `trash::move_to_trash` 失败。
    Trash(crate::trash::TrashError),
    /// 提交 tombstone 时 journal 追加失败。
    Journal(crate::journal::JournalError),
    Io {
        path: String,
        reason: String,
    },
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Scan(e) => write!(f, "{e}"),
            SyncError::Baseline(e) => write!(f, "{e}"),
            SyncError::Hub(e) => write!(f, "{e}"),
            SyncError::Atomic(e) => write!(f, "{e}"),
            SyncError::Format(e) => write!(f, "{e}"),
            SyncError::Trash(e) => write!(f, "{e}"),
            SyncError::Journal(e) => write!(f, "{e}"),
            SyncError::Io { path, reason } => write!(f, "{path}：{reason}"),
        }
    }
}

impl std::error::Error for SyncError {}

fn io_err(path: &Path, e: io::Error) -> SyncError {
    SyncError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// 归因上下文（I8）：谁、在哪台设备、哪次会话做了这次提交——由调用方注入，
/// 见模块顶部「刻意偏离」一节。
pub type SyncActor = Actor;

/// 跑一次完整的三态调和闭环：`dataset_root`（本地数据集目录）↔ `root`
/// （已打开、身份已确认的存储根）。
///
/// 只读扫描 + 按需写入（本地文件的新增/覆盖只发生在 `Download`；存储根内容
/// 只发生在 `Upload`）；`Noop`/`AdoptBaseline`/`Conflict`/`NeedsHuman` 都不碰
/// 任何文件字节。结束前把基线整体保存一次（brief：「执行完更新基线并保存」）——
/// 若中途失败提前返回，本次运行的基线不落盘，下次重跑会对已经成功上传/下载
/// 的路径重新决策，但那时远端已经是新内容，`decide` 会给出 `Noop`/
/// `AdoptBaseline` 而不是重复执行，不构成数据风险，只是多一轮判断。
///
/// # 存储根写入走批量提交（M1d 批量归档性能修复）
///
/// 一次 `sync` 可能对存储根做成千上万次写入（每个 `Upload` 各自要写
/// `files/<path>` + 追加 `items/<item_id>.jsonl` + 更新 `index/<key>.json`
/// 三个文件）。这些写入全部经由同一个 [`arca_store::atomic::Batch`]：文件级
/// fsync 逐次立即执行（内容持久性不打折扣），目录 fsync 延后到本函数末尾
/// 的一次 `commit()`——`Batch` 自己按目录去重，去掉的是"同一个目录被
/// fsync 一万次"这种冗余，不是任何一次真正需要的 fsync（论证见
/// `arca_store::atomic::Batch` 文档）。
///
/// `commit()` 必须在 `baseline.save()` 之前完成并检查结果：`commit` 失败
/// 意味着本次批次至少有一个目录的落盘确认失败，此时不能保存基线继续声称
/// "这些路径已经同步成功"（I3）——整个 `sync` 调用按失败上报，下次重跑会
/// 重新判定这些路径（此时内容已经在存储根，`decide` 会给出 `Noop`/
/// `AdoptBaseline` 而不是重复上传，不构成数据风险）。
pub fn sync(
    dataset_root: &Path,
    root: &StorageRoot,
    actor: &SyncActor,
    sink: &mut dyn TraceSink,
) -> Result<SyncReport, SyncError> {
    let scan_result = scan::scan_dataset(dataset_root, sink).map_err(SyncError::Scan)?;
    let mut baseline = baseline::load(dataset_root).map_err(SyncError::Baseline)?;
    let baseline_reset = baseline.was_reset();
    let remote = hub::read_remote(root).map_err(SyncError::Hub)?;

    let mut report = SyncReport {
        scan_rejected: scan_result.rejected.clone(),
        baseline_reset,
        ..SyncReport::default()
    };

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(scan_result.files.keys().cloned());
    paths.extend(baseline.iter().map(|(p, _)| p.clone()));
    paths.extend(remote.keys().cloned());

    // 闸门第 1 道（read_roots 范围）要问的正是"这个路径本次调和真的扫描到了
    // 吗"——`scan_result.files` 的键正是这个问题的答案（只有真的在磁盘上
    // 找到、判定为 `Present` 的路径才会进这个集合），与 `paths`（额外并入了
    // 基线与远端已知的路径，用于驱动整个调和循环）是不同的集合，不能共用。
    let scanned_paths: BTreeSet<String> = scan_result.files.keys().cloned().collect();

    let mut batch = Batch::new(root);

    for (idx, path) in paths.iter().enumerate() {
        let base = baseline.get(path);
        let local = scan_result
            .files
            .get(path)
            .cloned()
            .unwrap_or(LocalState::Absent);
        let remote_state = remote.get(path).cloned().unwrap_or(RemoteState::Absent);

        let decision = decide_traced(&base, &local, &remote_state, path, idx as u64, sink);

        match decision.action {
            Action::Noop => {}

            Action::Upload { parent } => {
                let new_state = execute_upload(
                    dataset_root,
                    root,
                    &mut batch,
                    path,
                    &base,
                    &remote_state,
                    parent,
                    actor,
                )?;
                baseline.set(path.clone(), new_state);
                report.uploaded.push(path.clone());
            }

            Action::Download { version_id } => {
                let new_state = execute_download(dataset_root, root, path, &remote_state)?;
                debug_assert_eq!(new_state.item_id(), remote_state.item_id());
                let _ = &version_id; // 已经等同 remote_state 的当前版本，见 execute_download
                baseline.set(path.clone(), new_state);
                report.downloaded.push(path.clone());
            }

            Action::AdoptBaseline { hash, version_id } => {
                let item_id = remote_state
                    .item_id()
                    .expect("AdoptBaseline 只在 remote 已知该 item 时产生");
                let size = match &local {
                    LocalState::Present { size, .. } => *size,
                    LocalState::Absent => unreachable!(
                        "AdoptBaseline 的全部决策表分支都要求 local 是 Present（见 arca_core::reconcile）"
                    ),
                };
                baseline.set(
                    path.clone(),
                    BaseState::Present {
                        item_id,
                        version_id,
                        hash,
                        size,
                    },
                );
                report.adopted.push(path.clone());
            }

            Action::DeleteLocal { item_id } => {
                let check = gates::DeleteCheck {
                    path,
                    item_id,
                    scanned_paths: &scanned_paths,
                    remote_state: &remote_state,
                    dataset_root,
                    base: &base,
                    root,
                };
                match gates::check_delete(&check) {
                    Ok(()) => {
                        let local_path = dataset_root.join(to_native(path));
                        match fs::remove_file(&local_path) {
                            Ok(()) => {}
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                            Err(e) => return Err(io_err(&local_path, e)),
                        }
                        baseline.remove(path);
                        report.deleted_local.push(path.clone());
                    }
                    Err(failure) => {
                        // 闸门拒绝：不删、不改基线，如实计入报告（I5）——下次
                        // 重跑会重新走一遍这四道检查，一旦竞态窗口关闭（例如
                        // 用户的修改已经通过正常上传流程同步），自然会通过。
                        report.delete_blocked.push((path.clone(), failure));
                    }
                }
            }

            Action::TombstoneRemote { item_id, parent } => {
                execute_tombstone(root, path, item_id, &parent, actor)?;
                baseline.remove(path);
                report.tombstone_submitted.push(path.clone());
            }

            Action::Conflict { .. } => {
                report.conflicts.push(path.clone());
            }

            Action::NeedsHuman { .. } => {
                report.needs_human.push(path.clone());
            }
        }
    }

    // 批次收口：本次 sync 触碰过的每个目录 fsync 一次，确认落盘。必须在
    // `baseline.save` 之前完成——commit 失败就不能保存基线继续声称这些路径
    // 已经同步成功（见本函数顶部「存储根写入走批量提交」一节，I3）。
    batch.commit().map_err(SyncError::Atomic)?;

    baseline.save(dataset_root).map_err(SyncError::Baseline)?;

    // 清单是基线在 git 侧的行式镜像（评审 Important #4）：每次 `sync` 收尾
    // 都要重新生成，不能只靠 `adopt` 生成一次就不再更新——否则协作者从 git
    // 拿到的清单会在日常 `sync` 里静默漏掉后续新增的路径（`git status` 却是
    // 干净的，因为清单本身没被标记为脏）。
    write_manifest(dataset_root, &baseline)?;

    Ok(report)
}

/// 执行一次 `Upload`：写 `files/` + 追加 `items/` + 更新 `index/`，返回新基线状态。
#[allow(clippy::too_many_arguments)]
fn execute_upload(
    dataset_root: &Path,
    root: &StorageRoot,
    batch: &mut Batch<'_>,
    path: &str,
    base: &BaseState,
    remote_state: &RemoteState,
    parent: Option<arca_format::model::VersionId>,
    actor: &SyncActor,
) -> Result<BaseState, SyncError> {
    let local_path = dataset_root.join(to_native(path));
    let bytes = fs::read(&local_path).map_err(|e| io_err(&local_path, e))?;
    let hash = arca_chunk::hash::ContentHash::from_bytes(&bytes);
    let size = bytes.len() as u64;

    // 删除后重建 = 新身份（spec §4.1）：parent 为 None 意味着 hub 完全不认识
    // 这个 item（无论是真的从未见过，还是曾经的身份已被 tombstone），必须
    // 分配一个全新的 item_id，绝不复用 remote 端可能残留的旧身份。
    let item_id = match &parent {
        None => ids::new_item_id(),
        Some(_) => base
            .item_id()
            .or_else(|| remote_state.item_id())
            .expect("Upload{parent:Some(_)} 意味着 base 或 remote 至少一方已知这个 item"),
    };
    let version_id = ids::new_version_id();

    let mtime = fs::metadata(&local_path)
        .and_then(|m| m.modified())
        .map(rfc3339_from_systemtime)
        .unwrap_or_else(|_| clock::now_rfc3339());

    let version = Version {
        version_id: version_id.clone(),
        item_id,
        parent,
        hash,
        size,
        mtime,
        actor: actor.clone(),
        committed_at: clock::now_rfc3339(),
    };

    // 写入顺序：files/ → items/ → index/（评审 Critical #1）。`index/` 是
    // "这个路径存在"的唯一指针——`hub::read_remote` 只遍历 `index/` 分片，
    // 一个路径若还没有 index 记录，它对整个系统而言就是"没有这回事"，不会
    // 被任何调用方观测到。所以指针必须最后发布：中途失败（ENOSPC、拔盘）
    // 留下的是"字节已经在 files/，但没有指针指向它"——下次重跑会把它当成
    // 全新上传重新写一遍（`ids::new_item_id()`），无害，只是多占一点存储；
    // 旧顺序（items/ → index/ → files/）失败时留下的是"指针完整，字节缺失"，
    // `hub::read_remote` 现在会把这识别为 `HubError::MissingContent`（见
    // `hub.rs`），但绝不能靠"读的时候报错"兜底——能从源头避免制造这种状态
    // 才是治本：写的时候就不产生指针先于字节的窗口。
    let target = format!("{}/{}", layout::FILES_DIR, path);
    batch.write(&target, &bytes).map_err(SyncError::Atomic)?;

    append_item_version(root, batch, &version)?;
    write_index_record(batch, path, item_id)?;

    Ok(BaseState::Present {
        item_id,
        version_id,
        hash,
        size,
    })
}

/// 执行一次 `Download`：从存储根的 `files/`（I1：当前版本永远完整平放）读出
/// 内容，原子写到本地。M1 没有历史版本重建（那需要 `.arca/chunks/`），
/// `Download` 在 M1 只可能针对"远端当前版本"——`hub::read_remote` 产出的
/// `RemoteState::Present` 本身就是当前版本，`Action::Download` 携带的
/// `version_id` 与它相等（决策表全部 `Download` 分支的 `version_id` 都取自
/// `remote.version_id`），所以直接读 `files/<path>` 即可，不需要按版本号
/// 另外定位内容。
fn execute_download(
    dataset_root: &Path,
    root: &StorageRoot,
    path: &str,
    remote_state: &RemoteState,
) -> Result<BaseState, SyncError> {
    let (item_id, version_id, hash, size) = match remote_state {
        RemoteState::Present {
            item_id,
            version_id,
            hash,
            size,
        } => (*item_id, version_id.clone(), *hash, *size),
        other => unreachable!("Download 只在 remote 是 Present 时产生，实得 {other:?}"),
    };

    let source = root
        .join(&format!("{}/{}", layout::FILES_DIR, path))
        .expect("path 已经过 path_rules::check（来自 index 记录），不会逃出存储根");
    let bytes = fs::read(&source).map_err(|e| io_err(&source, e))?;

    // 下载的内容必须 fsync 之后才允许保存基线（M2a tombstone 计划「为什么这是
    // M2 的第一块」一节；`arca_store::atomic::write_local` 的文档展开了完整
    // 论证）：这里若只是写完就 rename、不确认落盘，崩溃窗口里会留下「基线
    // 已保存、内容却丢失」的状态——下次调和把它读成「本地删除」，进而向 hub
    // 提交一次并非用户本意的 tombstone。这个函数把这个窗口关上，是
    // `execute_tombstone`（M2a Task 4 收尾）安全性的前提之一：即便这里的
    // 防线失守，`execute_tombstone` 也只会把内容移进可恢复的 `.arca/trash/`
    // （I3），不是无法挽回的销毁，但防线本身仍然值得关——避免用户为一次
    // 从未发生过的删除去手动 `arca restore`。`execute_download` 返回之前，
    // `?` 已经保证这一步失败时函数整体报错、调用方不会继续把 `new_state`
    // 写进基线。
    let local_path = dataset_root.join(to_native(path));
    atomic::write_local(dataset_root, &local_path, &bytes).map_err(SyncError::Atomic)?;

    Ok(BaseState::Present {
        item_id,
        version_id,
        hash,
        size,
    })
}

/// 执行一次 `TombstoneRemote`：向 hub 提交本地删除意图——把 `files/<path>`
/// 移进 `.arca/trash/`，追加一条 journal `op=tombstone` 事件。**这不是销毁**，
/// 内容留在回收站，保留期内 `arca restore`（M2a Task 5）能找回（I3）；
/// 复用 Task 3 已交付、已被独立测试覆盖的
/// [`crate::trash::move_to_trash`] + [`crate::journal::append`]，不在本函数
/// 重新实现落盘细节。
///
/// `version_id` 取 `parent`（决策表给出的、这次要终结的远端当前版本）：
/// journal 事件的 `version_id` 字段语义本就是"改动前最后一个存活版本的
/// id"（FORMAT.md §7.2），恰好就是决策表算好、随 `Action::TombstoneRemote`
/// 一起带出来的 CAS If-Match 对象，不需要另外去读一次版本链。
///
/// 决策表到达 `TombstoneRemote` 的两条路径（`local_deleted`）都要求
/// `remote_state` 是 `RemoteState::Present`（`arca_core::reconcile::decide_base_present`
/// 的 `(LocalState::Absent, RemoteState::Present)` 分支），而 `hub::read_remote`
/// 产出 `Present` 的前提是 `files/<path>` 确实存在（评审 Critical #1），所以
/// 正常运行下 `trash::move_to_trash` 总能找到源文件；万一磁盘在这两步之间
/// 被外部改动导致源缺失，`move_to_trash` 会如实报出 `TrashError::Atomic`
/// （包住底层的 `NotFound`），本函数整体报错、不吞掉。
fn execute_tombstone(
    root: &StorageRoot,
    path: &str,
    item_id: ItemId,
    parent: &arca_format::model::VersionId,
    actor: &SyncActor,
) -> Result<(), SyncError> {
    let at = clock::now_rfc3339();
    trash::move_to_trash(root, path, item_id, &at).map_err(SyncError::Trash)?;

    let seq = journal::next_seq(root).map_err(SyncError::Journal)?;
    journal::append(
        root,
        &arca_format::journal::JournalEvent {
            seq,
            op: arca_format::journal::Op::Tombstone,
            item_id,
            version_id: parent.clone(),
            path: path.to_string(),
            from: None,
            actor: actor.clone(),
            at,
        },
    )
    .map_err(SyncError::Journal)
}

/// 追加一条版本记录到 `items/<xx>/<item_id>.jsonl`。`items/` 是 append-only
/// 语义（FORMAT.md §7.1），但 `arca_store::atomic` 只提供整文件原子替换，
/// 没有原子追加——因此这里是"读现有内容 + 拼接新行 + 整体原子重写"，读到的
/// 现有内容本身已经是上一次原子写入的产物，不存在半截读到的风险。
fn append_item_version(
    root: &StorageRoot,
    batch: &mut Batch<'_>,
    version: &Version,
) -> Result<(), SyncError> {
    let rel = layout::item_path(&version.item_id);
    let full = root.path().join(&rel);
    let mut content = match fs::read_to_string(&full) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_err(&full, e)),
    };
    content.push_str(&items::to_line(version).map_err(SyncError::Format)?);
    content.push('\n');
    batch
        .write(&rel, content.as_bytes())
        .map_err(SyncError::Atomic)
}

/// 整体原子替换 `index/<xx>/<key>.json`（index 记录不是 append-only，见
/// `arca_format::index` 模块文档）。
fn write_index_record(batch: &mut Batch<'_>, path: &str, item_id: ItemId) -> Result<(), SyncError> {
    let key = path_rules::index_key(path);
    let rel = layout::index_path(&key);
    let record = IndexRecord {
        item_id,
        path: path.to_string(),
    };
    let text = record.to_json().map_err(SyncError::Format)?;
    batch
        .write(&rel, text.as_bytes())
        .map_err(SyncError::Atomic)
}

/// 从最终基线重新生成 `<dataset_root>/.arca/manifest`（评审 Important #4）：
/// `sync` 收尾时的基线就是"这个数据集当前每个受管路径的哈希/大小的权威
/// 快照"，清单只是它在 git 侧的行式镜像。**每次 `sync` 都要重新生成**，
/// 不能只靠 `arca adopt` 生成一次——见本函数调用点的注释。
///
/// `mtime` 取文件自身的 mtime（FORMAT.md 定义的字段语义），不是"写清单
/// 这一刻"的墙上时钟——用 `execute_upload` 同一段 `rfc3339_from_systemtime`
/// 转换逻辑（评审 Minor：此前 `adopt.rs` 独立实现的 `write_manifest` 用的
/// 是墙上时钟，导致一次空操作的重跑也会弄脏清单，侵蚀"adopt 后 git status
/// 干净"这条 M1 验收性质——第二次跑就不成立了）。
///
/// **基线里有记录、但本地文件当前不存在的路径会被跳过，不报错**——这在
/// M1 只有一条产生途径：`Action::TombstoneRemote`（本地已删除，但 M1 没有
/// 落盘 tombstone 的地方，见 `hub.rs` 模块文档），基线因此保留了这条陈旧
/// 记录。清单是"本地当前应该有这些文件"的 git 侧影子，本地已经没有的文件
/// 继续列在清单里没有意义（也没有 mtime 可取，不能编造）；这个状态本身
/// 已经通过 `SyncReport::tombstone_pending` 如实报告、让 `is_clean()` 为假，
/// 不会被静默吞掉。其它原因导致的 `fs::metadata` 失败（权限等）仍然原样
/// 报错（I5：不能把"读不出来"也当成"本地没有"）。
fn write_manifest(dataset_root: &Path, baseline: &baseline::Baseline) -> Result<(), SyncError> {
    let mut entries = Vec::new();
    for (path, state) in baseline.iter() {
        let BaseState::Present { hash, size, .. } = state else {
            continue;
        };
        let local_path = dataset_root.join(to_native(path));
        let mtime = match fs::metadata(&local_path).and_then(|m| m.modified()) {
            Ok(t) => rfc3339_from_systemtime(t),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&local_path, e)),
        };
        entries.push(ManifestEntry {
            path: path.clone(),
            hash: *hash,
            size: *size,
            mtime,
        });
    }
    let manifest = Manifest::from_entries(entries).map_err(SyncError::Format)?;
    let manifest_path = dataset_root.join(".arca").join("manifest");
    vault::write_text_atomic(&manifest_path, &manifest.to_string())
        .map_err(|e| io_err(&manifest_path, e))
}

/// 把索引/清单使用的 `/` 分隔路径转成当前平台的 [`PathBuf`]。
///
/// `pub(crate)`：`gates.rs` 的第 3 道闸门（基线一致性）需要用同一套转换
/// 规则重新定位本地文件，不能自己另写一份——两处对"逻辑路径怎么落到本地
/// 文件系统路径"的理解必须逐字节一致，否则闸门检查的就不是同一个文件。
pub(crate) fn to_native(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for seg in path.split('/') {
        out.push(seg);
    }
    out
}

fn rfc3339_from_systemtime(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    clock::rfc3339_from_unix_secs(secs as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::reconcile::decide;
    use arca_format::hub_layout::FormatJson;
    use arca_format::trace::NullSink;
    use std::fs;

    fn actor() -> SyncActor {
        Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        }
    }

    fn open_root(dir: &Path) -> StorageRoot {
        StorageRoot::open(dir, None).unwrap()
    }

    fn 造存储根(dir: &Path) {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        let format = FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    #[test]
    fn 本地新增文件被上传() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("a.txt"), b"hello").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        assert_eq!(report.uploaded, vec!["a.txt".to_string()]);
        assert!(report.is_clean());
        assert!(store.path().join("files/a.txt").is_file());
        assert_eq!(
            fs::read(store.path().join("files/a.txt")).unwrap(),
            b"hello"
        );

        // 重新读远端，应该能看到这个新 item。
        let remote = hub::read_remote(&root).unwrap();
        assert!(matches!(
            remote.get("a.txt"),
            Some(RemoteState::Present { .. })
        ));
    }

    #[test]
    fn 远端新增被下载到本地() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());

        // 先用一次 sync 把内容"种"到远端（模拟另一台设备已经上传过）。
        let seed_dataset = tempfile::tempdir().unwrap();
        fs::write(seed_dataset.path().join("b.txt"), b"remote content").unwrap();
        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(seed_dataset.path(), &root, &actor(), &mut sink).unwrap();

        // 本地数据集是全新的、从未同步过——应当把远端内容下载下来。
        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(report.downloaded, vec!["b.txt".to_string()]);
        assert!(report.is_clean());
        assert_eq!(
            fs::read(dataset.path().join("b.txt")).unwrap(),
            b"remote content"
        );
    }

    #[test]
    fn 两端各自新增相同内容走零传输认领() {
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        let root = open_root(store.path());
        let mut sink = NullSink;

        // 设备甲先上传。
        let device_a = tempfile::tempdir().unwrap();
        fs::write(device_a.path().join("c.txt"), b"same content").unwrap();
        sync(device_a.path(), &root, &actor(), &mut sink).unwrap();

        // 设备乙独立创建了同一份内容，但从未与这次上传对账过（基线为空）。
        let device_b = tempfile::tempdir().unwrap();
        fs::write(device_b.path().join("c.txt"), b"same content").unwrap();
        let report = sync(device_b.path(), &root, &actor(), &mut sink).unwrap();

        assert_eq!(report.adopted, vec!["c.txt".to_string()]);
        assert!(report.uploaded.is_empty(), "零传输：不应该再次上传");
        assert!(report.downloaded.is_empty());
        assert!(report.is_clean());
        // 本地文件内容必须原样保留（I6）——AdoptBaseline 不改动本地文件。
        assert_eq!(
            fs::read(device_b.path().join("c.txt")).unwrap(),
            b"same content"
        );
    }

    #[test]
    fn 收敛后再跑一次是全静默的noop() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("d.txt"), b"content").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let first = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        assert!(first.changed(), "首次同步应该有实际动作（上传）");

        let second = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        assert!(!second.changed(), "第二次同步应该什么都不用做");
        assert!(second.is_clean());
    }

    #[test]
    fn 本地删除传播为远端tombstone且hub权威副本仍可恢复() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("e.txt"), b"content").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        fs::remove_file(dataset.path().join("e.txt")).unwrap();
        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        // M2a Task 4 收尾：TombstoneRemote 现在真的执行——本地删除成功提交
        // 为 hub 侧的 tombstone，是正常完成的动作，不再是"无处落盘的报告"。
        assert_eq!(report.tombstone_submitted, vec!["e.txt".to_string()]);
        assert!(report.tombstone_pending.is_empty());
        assert!(
            report.is_clean(),
            "成功提交的 tombstone 是正常终态，不应让退出码非零：{report:?}"
        );
        assert!(report.changed(), "提交 tombstone 是一次真实的状态变化");
        // 绝不能被静默当 no-op：不应该出现在任何其它分类桶里。
        assert!(report.uploaded.is_empty());
        assert!(report.downloaded.is_empty());
        assert!(report.deleted_local.is_empty());

        // hub 侧现在应该看到 Tombstoned，且内容确实在 .arca/trash/ 里——
        // 不是被销毁（I3）。
        let remote = hub::read_remote(&root).unwrap();
        assert!(matches!(
            remote.get("e.txt"),
            Some(RemoteState::Tombstoned { .. })
        ));
        assert!(!store.path().join("files/e.txt").exists());
        let trash_has_data = fs::read_dir(store.path().join(".arca/trash"))
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".data"));
        assert!(trash_has_data, "hub 的权威副本必须仍在 .arca/trash/ 里");

        // 基线应已清空这个路径——本地、远端都不再"存在"这个 item。
        let baseline = crate::baseline::load(dataset.path()).unwrap();
        assert_eq!(baseline.get("e.txt"), BaseState::Absent);
    }

    // -----------------------------------------------------------------
    // Task 1（M2a tombstone 计划）：下载内容的 fsync 纪律。
    //
    // 完整背景见 `docs/superpowers/plans/2026-08-08-m2a-tombstone.md`
    // 「为什么这是 M2 的第一块」一节，以及 `execute_download`/
    // `arca_store::atomic::write_local` 顶部的论证。这里只放两条测试：
    // 先证明隐患真实存在（下面第一条），再证明修复后这个窗口关上了
    // （第二条）。
    // -----------------------------------------------------------------

    /// 证明隐患真实存在：不是凭空构造一个 `BaseState`/`RemoteState`，而是用
    /// 真实的 `sync`/`baseline`/`hub::read_remote` 拼出「基线已保存、内容却
    /// 缺失」这个崩溃窗口状态（正常下载一次后，手工删掉本地内容，模拟
    /// fsync 时机不对时崩溃会留下的后果——不是用户主动删除）。断言下一轮
    /// `decide` 给出 `TombstoneRemote`：`execute_tombstone`（M2a Task 4 收尾）
    /// 现在真的会执行这个决策——但执行的后果是把 hub 的内容移进
    /// `.arca/trash/`（I3：tombstone 不是销毁，`arca restore` 能找回），
    /// 不是无法挽回的数据丢失。这正是 Task 1 必须先于 tombstone 执行落地的
    /// 理由：关上这个窗口，用户就不会遇到一次自己从未做过的删除、需要去
    /// 手动 restore 才能找回文件的意外。
    #[test]
    fn 崩溃窗口状态_基线已保存内容却缺失_会让下一轮决策给出tombstone_remote() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());

        // 先在另一台设备上把内容上传到远端。
        let seed = tempfile::tempdir().unwrap();
        fs::write(seed.path().join("h.txt"), b"downloaded content").unwrap();
        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(seed.path(), &root, &actor(), &mut sink).unwrap();

        // 本地正常下载一次：内容落地、基线记下这个路径。
        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(report.downloaded, vec!["h.txt".to_string()]);

        // 模拟崩溃窗口的后果：内容在崩溃后消失，但基线已经落盘。
        fs::remove_file(dataset.path().join("h.txt")).unwrap();

        let base = baseline::load(dataset.path()).unwrap().get("h.txt");
        let remote = hub::read_remote(&root).unwrap();
        let remote_state = remote.get("h.txt").cloned().unwrap();
        let decision = decide(&base, &LocalState::Absent, &remote_state);

        match decision.action {
            Action::TombstoneRemote { .. } => {}
            other => panic!(
                "应为 TombstoneRemote，实得 {other:?}——隐患描述有误或已经不成立，\
                 这条测试本身需要重新核对"
            ),
        }
    }

    /// 证明修复后窗口关上了：`execute_download` 现在经
    /// `arca_store::atomic::write_local` 写入本地内容——目录落盘确认失败时
    /// 必须整体报错（`SyncError::Atomic(AtomicError::CommittedUnsynced)`），
    /// 而不是像旧的手写 tmp→rename 实现那样对这一步毫无感知、一路"成功"到
    /// 保存基线。这里直接调用 `execute_download`（跳过 `sync()` 的扫描阶段，
    /// 否则 chmod 掉的目录会先在扫描阶段报错，测不到目标的落盘确认步骤）。
    ///
    /// 用 chmod 模拟目录 fsync 失败：`rename` 只需要目录的写+执行权限，
    /// `File::open` 读该目录用于 fsync 则需要读权限，二者可以分别控制
    /// （与 `arca_store::atomic::write_local` 同款测试用的是同一个手法）。
    /// chmod 对 root 用户无效、部分文件系统也不支持权限位，先自证一次假设，
    /// 不成立就跳过而不是假装测过了。
    #[test]
    #[cfg(unix)]
    fn 修复后_目录落盘确认失败时execute_download整体报错而不静默成功() {
        use std::os::unix::fs::PermissionsExt;

        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());

        let seed = tempfile::tempdir().unwrap();
        fs::write(seed.path().join("i.txt"), b"content").unwrap();
        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(seed.path(), &root, &actor(), &mut sink).unwrap();
        let remote = hub::read_remote(&root).unwrap();
        let remote_state = remote.get("i.txt").cloned().unwrap();

        // "i.txt" 没有子目录，target_parent 恰好就是 dataset_root 本身——
        // 直接 chmod 它。
        fs::set_permissions(dataset.path(), fs::Permissions::from_mode(0o300)).unwrap();
        if fs::File::open(dataset.path()).is_ok() {
            fs::set_permissions(dataset.path(), fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("跳过：当前用户不受 chmod 限制（root 或文件系统不支持权限位）");
            return;
        }

        let result = execute_download(dataset.path(), &root, "i.txt", &remote_state);

        // 恢复权限，否则 tempdir 在 Drop 时清理不掉这个目录。
        fs::set_permissions(dataset.path(), fs::Permissions::from_mode(0o755)).unwrap();

        match result {
            Err(SyncError::Atomic(AtomicError::CommittedUnsynced { .. })) => {}
            other => panic!("应报 SyncError::Atomic(CommittedUnsynced)，实得 {other:?}"),
        }
        // 内容确实已经落地（rename 已完成）——但调用方（`sync`）看到 `Err`
        // 就不会把这次"下载"记进基线：下次重跑会重新判定需要 Download/
        // AdoptBaseline，不会把一个未确认落盘的状态当成"已同步"写死。
        assert_eq!(fs::read(dataset.path().join("i.txt")).unwrap(), b"content");
    }

    /// 端到端验证 M2a tombstone 计划 Task 3 的价值：`hub::read_remote` 现在
    /// 能产出 `RemoteState::Tombstoned`，`present|unchanged|tombstoned ->
    /// DeleteLocal` 这一格决策表**第一次在真实的 `sync()` 调用里可达**——
    /// 而 `DeleteLocal` 的执行早在 M1d Task 6 就已经写好（见本文件 `sync`
    /// 函数里 `Action::DeleteLocal` 分支），只是从未被真正触发过。这里手工
    /// 模拟"另一台设备/未来的 tombstone 执行"已经把这个路径在 hub 侧删除
    /// （内容移进 trash + 追加 tombstone 事件 + 清理 index 记录），验证本地
    /// 设备下一次 `sync()` 会把本地副本也移除——且这只是移除本地副本，
    /// hub 的权威副本仍然安全地留在 `.arca/trash/` 里（I3）。
    #[test]
    fn hub侧tombstone传播到本地时sync会执行deletelocal() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::create_dir_all(store.path().join(".arca/trash")).unwrap();
        fs::create_dir_all(store.path().join(".arca/journal")).unwrap();
        fs::write(dataset.path().join("j.txt"), b"content").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        assert!(dataset.path().join("j.txt").is_file());

        // 找出刚上传的 item_id/version，手工执行一次 tombstone（真正的执行
        // 流程属 Task 4，这里拼出执行完成后的存储根状态）。
        let remote_before = hub::read_remote(&root).unwrap();
        let (item_id, version_id) = match remote_before.get("j.txt").unwrap() {
            RemoteState::Present {
                item_id,
                version_id,
                ..
            } => (*item_id, version_id.clone()),
            other => panic!("应为 Present，实得 {other:?}"),
        };
        crate::trash::move_to_trash(&root, "j.txt", item_id, "2026-08-08T09:20:00Z").unwrap();
        crate::journal::append(
            &root,
            &arca_format::journal::JournalEvent {
                seq: 1,
                op: arca_format::journal::Op::Tombstone,
                item_id,
                version_id,
                path: "j.txt".to_string(),
                from: None,
                actor: actor(),
                at: "2026-08-08T09:20:00Z".to_string(),
            },
        )
        .unwrap();

        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        assert_eq!(report.deleted_local, vec!["j.txt".to_string()]);
        assert!(
            report.is_clean(),
            "DeleteLocal 是决策表的正常终态，不应让 is_clean 为假：{report:?}"
        );
        assert!(!dataset.path().join("j.txt").exists(), "本地副本应已被移除");
        // hub 的权威副本仍然安全——本地副本被移除后，内容必须仍能在
        // .arca/trash/ 里找到，绝不是被销毁了（I3）。
        let trash_has_data = fs::read_dir(store.path().join(".arca/trash"))
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".data"));
        assert!(trash_has_data, "hub 的权威副本必须仍在 .arca/trash/ 里");
    }

    /// 端到端闭环（M2a tombstone 计划收尾）：两台设备**只通过 `sync()` 调用**
    /// 完成"上传 → 另一设备下载 → 一台设备本地删除并同步（提交 tombstone）
    /// → 另一设备同步（过闸门后移除本地副本）→ `arca restore` 找回"整条链路，
    /// 不手工拼任何中间状态——这是本切片最终交付的性质，其它测试各自验证
    /// 链路上的一个环节，这条测试验证环节真的能串起来。
    #[test]
    fn 端到端_两台设备通过sync完成删除传播_闸门放行_restore找回() {
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        let root = open_root(store.path());
        let mut sink = NullSink;

        // 设备甲：上传。
        let device_a = tempfile::tempdir().unwrap();
        fs::write(device_a.path().join("photo.png"), b"precious bytes").unwrap();
        let report_a1 = sync(device_a.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(report_a1.uploaded, vec!["photo.png".to_string()]);

        // 设备乙：下载，两端此刻都持有这份内容。
        let device_b = tempfile::tempdir().unwrap();
        let report_b1 = sync(device_b.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(report_b1.downloaded, vec!["photo.png".to_string()]);
        assert!(device_b.path().join("photo.png").is_file());

        // 设备甲：本地删除，sync 应把删除意图提交为 hub 侧 tombstone
        // （内容进 .arca/trash/，不是销毁）。
        fs::remove_file(device_a.path().join("photo.png")).unwrap();
        let report_a2 = sync(device_a.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(report_a2.tombstone_submitted, vec!["photo.png".to_string()]);
        assert!(report_a2.is_clean());

        // 设备乙：再同步一次，四道闸门全过（本地内容与基线一致、远端确实是
        // 对同一 item 的 tombstone、路径在扫描范围内、hub 的回收站里确实
        // 有这份内容），应当移除本地副本。
        let report_b2 = sync(device_b.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(report_b2.deleted_local, vec!["photo.png".to_string()]);
        assert!(
            report_b2.delete_blocked.is_empty(),
            "四道闸门应当全过，不应被拦下：{:?}",
            report_b2.delete_blocked
        );
        assert!(report_b2.is_clean());
        assert!(!device_b.path().join("photo.png").exists());

        // arca restore：保留期内一条命令找回——直接操作共享的存储根（模拟
        // `arca restore` 命令壳的核心调用）。
        let restored_version =
            crate::trash::restore(&root, "photo.png", &actor(), "2026-08-08T10:00:00Z").unwrap();
        assert_eq!(
            fs::read(store.path().join("files/photo.png")).unwrap(),
            b"precious bytes",
            "找回的内容必须与删除前完全一致"
        );

        // 恢复后，设备丙（一台此前从未见过这个文件的新设备）同步应当能重新
        // 下载到这份找回的内容——证明 restore 不只是写了字节，index/items/
        // journal 全套指针都被正确地重新建立起来了。
        let device_c = tempfile::tempdir().unwrap();
        let report_c = sync(device_c.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(report_c.downloaded, vec!["photo.png".to_string()]);
        assert_eq!(
            fs::read(device_c.path().join("photo.png")).unwrap(),
            b"precious bytes"
        );
        let remote = hub::read_remote(&root).unwrap();
        match remote.get("photo.png") {
            Some(RemoteState::Present { version_id, .. }) => {
                assert_eq!(*version_id, restored_version.version_id)
            }
            other => panic!("恢复后应为 Present，实得 {other:?}"),
        }
    }

    #[test]
    fn 三方分叉产生结构化冲突不动数据() {
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        let root = open_root(store.path());
        let mut sink = NullSink;

        let dataset = tempfile::tempdir().unwrap();
        fs::write(dataset.path().join("f.txt"), b"v1").unwrap();
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        // 远端被另一台设备改成 v2。
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join("f.txt"), b"v1").unwrap();
        sync(other.path(), &root, &actor(), &mut sink).unwrap();
        fs::write(other.path().join("f.txt"), b"v2-from-other").unwrap();
        sync(other.path(), &root, &actor(), &mut sink).unwrap();

        // 本地也独立改成 v3（与基线不同，与远端也不同）。
        fs::write(dataset.path().join("f.txt"), b"v3-local").unwrap();
        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        assert_eq!(report.conflicts, vec!["f.txt".to_string()]);
        assert!(!report.is_clean());
        // 冲突不动数据：本地文件内容必须原样保留。
        assert_eq!(fs::read(dataset.path().join("f.txt")).unwrap(), b"v3-local");
        // 远端也不应该被改动。
        assert_eq!(
            fs::read(store.path().join("files/f.txt")).unwrap(),
            b"v2-from-other"
        );
    }

    #[test]
    fn 扫描阶段被拒绝的路径计入报告且使is_clean为假() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("CON.txt"), b"bad name").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        assert_eq!(report.scan_rejected.len(), 1);
        assert!(!report.is_clean());
    }

    #[test]
    fn 上传的版本记录归因到调用方传入的actor() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("g.txt"), b"x").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let who = Actor {
            account: "萍萍".into(),
            device: "笔记本".into(),
            session: "s9".into(),
        };
        sync(dataset.path(), &root, &who, &mut sink).unwrap();

        let remote = hub::read_remote(&root).unwrap();
        let item_id = match remote.get("g.txt").unwrap() {
            RemoteState::Present { item_id, .. } => *item_id,
            _ => panic!("应为 Present"),
        };
        let rel = layout::item_path(&item_id);
        let text = fs::read_to_string(store.path().join(rel)).unwrap();
        assert!(text.contains("萍萍"));
        assert!(text.contains("笔记本"));
    }

    /// 评审 Important #4 的复现测试：`sync` 上传新文件后，`.arca/manifest`
    /// 必须把新文件也列进去——此前只有 `adopt` 生成清单一次，后续 `sync`
    /// 从不更新，协作者据此拿到一份漏掉受管文件的清单。
    #[test]
    fn sync成功后清单被重新生成并包含新上传的文件() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("a.txt"), b"hello").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        let manifest_path = dataset.path().join(".arca/manifest");
        assert!(manifest_path.is_file(), "首次 sync 就应该生成清单");
        let manifest =
            arca_format::manifest::Manifest::parse(&fs::read_to_string(&manifest_path).unwrap())
                .unwrap();
        assert_eq!(manifest.entries().len(), 1);
        assert_eq!(manifest.entries()[0].path, "a.txt");

        // 第二次 sync 新增一个文件——清单必须把它也纳入，而不是停留在第一次
        // 生成时的快照。
        fs::write(dataset.path().join("c.bin"), b"second file").unwrap();
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        let manifest =
            arca_format::manifest::Manifest::parse(&fs::read_to_string(&manifest_path).unwrap())
                .unwrap();
        let paths: Vec<&str> = manifest.entries().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["a.txt", "c.bin"],
            "第二次 sync 后清单必须同时包含旧文件与新上传的 c.bin"
        );
    }

    /// 评审 Minor 项复现测试：清单的 mtime 必须取文件自身的 mtime，不是
    /// "写清单这一刻"的墙上时钟——否则每次重跑 sync（哪怕是空操作）都会
    /// 因为 mtime 变化而弄脏清单，侵蚀"同步收敛后 git status 干净"这条性质。
    #[test]
    fn 清单的mtime取文件自身mtime而不是写清单时的墙上时钟() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        let file_path = dataset.path().join("a.txt");
        fs::write(&file_path, b"hello").unwrap();
        let file_mtime =
            rfc3339_from_systemtime(fs::metadata(&file_path).unwrap().modified().unwrap());

        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        let manifest_path = dataset.path().join(".arca/manifest");
        let manifest =
            arca_format::manifest::Manifest::parse(&fs::read_to_string(&manifest_path).unwrap())
                .unwrap();
        assert_eq!(manifest.entries()[0].mtime, file_mtime);

        // 空操作重跑：清单文本必须逐字节相同（不能因为重新生成而漂移出一个
        // 新的 mtime，那样第二次跑就会把 git 工作树弄脏）。
        let before = fs::read_to_string(&manifest_path).unwrap();
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        let after = fs::read_to_string(&manifest_path).unwrap();
        assert_eq!(before, after, "空操作重跑不应该弄脏清单");
    }

    /// 评审 Critical #1 的写入顺序安全性复现：`execute_upload` 现在按
    /// `files/` → `items/` → `index/` 的顺序写入（内容先于指针发布）。用手工
    /// 模拟"崩溃发生在 index/ 写入之前"的中间态——直接删掉 index 记录，
    /// 只留下 files/ 与 items/——验证这个中间态是**无害**的：`index` 是
    /// `hub::read_remote` 唯一的入口，这个路径从 hub 视角"从未发布过"，
    /// 不构成任何谎言；随后 `sync` 正确地把它识别为需要人工介入
    /// （`remote_vanished_without_tombstone`——远端记录消失但本地并未改动，
    /// `arca_core::reconcile` 决策表拒绝在这种模糊状态下自作主张地重新
    /// 上传，见其文档），而不是静默声称"已同步"（旧顺序 items/→index/→files/
    /// 失败会留下相反的中间态：指针完整、字节缺失，`hub::read_remote` 会把
    /// 那种状态误读成 `Present`，进而可能被零传输 `AdoptBaseline` 认领）。
    #[test]
    fn 崩溃在index写入之前遗留的孤儿字节不会被误读为已同步() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("c.bin"), b"orphan content").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let first = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(first.uploaded, vec!["c.bin".to_string()]);

        // 模拟"写到 files/+items/ 但还没写 index/ 就崩溃"：手工删掉 index
        // 分片下唯一的记录文件。
        let index_dir = store.path().join(".arca/index");
        let mut removed = false;
        for shard in fs::read_dir(&index_dir).unwrap() {
            let shard = shard.unwrap().path();
            for entry in fs::read_dir(&shard).unwrap() {
                fs::remove_file(entry.unwrap().path()).unwrap();
                removed = true;
            }
        }
        assert!(removed, "测试前置条件：应该能找到刚写入的 index 记录并删除");

        // 内容与版本链仍在，但 hub 侧已经"看不见"这个路径了——不是谎言，
        // 是从未发布过（index 才是唯一入口）。
        let remote_after_removal = hub::read_remote(&root).unwrap();
        assert!(
            !remote_after_removal.contains_key("c.bin"),
            "index 记录被移除后，这个路径不应再被 read_remote 观测到"
        );

        // 重新跑一次 sync：本地内容与基线一致（未改动），但远端记录"凭空
        // 消失"——决策表判定为需要人工介入，绝不静默重新上传冒充"什么都
        // 没发生过"，也绝不谎称"已同步"。
        let second = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        assert_eq!(second.needs_human, vec!["c.bin".to_string()]);
        assert!(second.uploaded.is_empty());
        assert!(second.adopted.is_empty());
        assert!(!second.is_clean(), "远端记录异常消失不应被判定为干净");
        // 本地文件内容必须原样保留（I6：不动数据）。
        assert_eq!(
            fs::read(dataset.path().join("c.bin")).unwrap(),
            b"orphan content"
        );
    }

    /// 评审 Critical #1 的端到端复现：手工造出「index/items 完整、
    /// `files/c.bin` 内容缺失」的存储根，本地又恰好有一份同名同内容的文件
    /// （最危险的组合——不加防护时会走零传输 `AdoptBaseline` 认领路径，把
    /// "已同步"的谎言写进基线）。修复后 `sync` 必须整体报错，绝不能安静
    /// 认领、也绝不能把这个路径静默当成"从未存在过"。
    #[test]
    fn 索引完整但内容缺失时sync报错而不是零传输认领() {
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());

        // 手工在存储根写一条"index/items 完整但 files/ 缺内容"的记录。
        let content = b"same content";
        let hash = arca_chunk::hash::ContentHash::from_bytes(content);
        let item_id = ids::new_item_id();
        let version = Version {
            version_id: ids::new_version_id(),
            item_id,
            parent: None,
            hash,
            size: content.len() as u64,
            mtime: "2026-08-08T09:00:00Z".to_string(),
            actor: actor(),
            committed_at: "2026-08-08T09:00:05Z".to_string(),
        };
        let item_rel = layout::item_path(&item_id);
        let item_full = store.path().join(&item_rel);
        fs::create_dir_all(item_full.parent().unwrap()).unwrap();
        fs::write(
            &item_full,
            format!("{}\n", items::to_line(&version).unwrap()),
        )
        .unwrap();
        let key = path_rules::index_key("c.bin");
        let index_shard = store.path().join(".arca/index").join(&key.to_hex()[..2]);
        fs::create_dir_all(&index_shard).unwrap();
        let record = IndexRecord {
            item_id,
            path: "c.bin".to_string(),
        };
        fs::write(
            index_shard.join(format!("{}.json", key.to_hex())),
            record.to_json().unwrap(),
        )
        .unwrap();
        // 刻意不写 files/c.bin。

        let dataset = tempfile::tempdir().unwrap();
        fs::write(dataset.path().join("c.bin"), content).unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let err = sync(dataset.path(), &root, &actor(), &mut sink).unwrap_err();
        assert!(
            matches!(err, SyncError::Hub(hub::HubError::MissingContent { .. })),
            "应整体报错为 MissingContent，实得 {err:?}"
        );

        // 绝不能有任何一个字节被误当作"已同步"：基线必须仍是空的。
        let baseline = crate::baseline::load(dataset.path()).unwrap();
        assert!(
            baseline.get("c.bin") == arca_core::state::BaseState::Absent,
            "报错路径上绝不能把基线写成已同步"
        );
    }
}
