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
//! | `DeleteLocal{item_id}` | 移除本地副本（权威副本在 hub） |
//! | `TombstoneRemote{item_id, parent}` | M1 无处落盘（tombstone 属 M2）——如实报告，绝不静默当 no-op |
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
//! # tombstone 无处落盘的连带后果（读 `hub.rs` 顶部注释）
//!
//! `hub::read_remote` 结构上产不出 `RemoteState::Tombstoned`（M1 没有把
//! journal 接上），所以 `TombstoneRemote` 这一格在 M1 的真实运行中**必然
//! 是"发生过"但从未被"验证执行成功"**——它只能停在"如实报告"这一步。
//! `SyncReport::is_clean()` 把它算进"有问题"，`arca sync` 的退出码据此非零，
//! 绝不能让用户误以为删除已经生效。

use crate::{baseline, clock, hub, ids, scan, vault};
use arca_core::reconcile::{decide_traced, Action};
use arca_core::state::{BaseState, LocalState, RemoteState};
use arca_format::hub_layout::layout;
use arca_format::index::IndexRecord;
use arca_format::items;
use arca_format::manifest::{Manifest, ManifestEntry};
use arca_format::model::{Actor, ItemId, Version};
use arca_format::path_rules;
use arca_format::trace::TraceSink;
use arca_store::atomic::{AtomicError, Batch};
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
    /// M1 无处落盘的 tombstone 传播——**不是空操作**，是"本该做但这一版做不了"
    /// 的如实记录（见模块顶部文档）。
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
        self.tombstone_pending.is_empty()
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
    Io { path: String, reason: String },
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Scan(e) => write!(f, "{e}"),
            SyncError::Baseline(e) => write!(f, "{e}"),
            SyncError::Hub(e) => write!(f, "{e}"),
            SyncError::Atomic(e) => write!(f, "{e}"),
            SyncError::Format(e) => write!(f, "{e}"),
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

            Action::DeleteLocal { .. } => {
                let local_path = dataset_root.join(to_native(path));
                match fs::remove_file(&local_path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_err(&local_path, e)),
                }
                baseline.remove(path);
                report.deleted_local.push(path.clone());
            }

            Action::TombstoneRemote { .. } => {
                // M1 无处落盘（tombstone 属 M2）——不改基线、不删任何东西，
                // 如实计入报告。绝不静默当 no-op（见模块顶部文档）。
                report.tombstone_pending.push(path.clone());
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

    let local_path = dataset_root.join(to_native(path));
    write_local_atomic(&local_path, &bytes).map_err(|e| io_err(&local_path, e))?;

    Ok(BaseState::Present {
        item_id,
        version_id,
        hash,
        size,
    })
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
fn to_native(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for seg in path.split('/') {
        out.push(seg);
    }
    out
}

/// 原子写一个任意本地文件（tmp → rename，同目录）。
///
/// 不做 `arca_store::atomic` 那一整套 fsync 事务链——下载下来的本地副本是
/// hub 权威内容的一份可重建投影（与 I9 对可抛弃投影的宽容度同理）：这次写入
/// 若被打断，`baseline` 还没来得及记下这个路径，下次 `arca sync` 会重新判定
/// 需要 `Download` 并再下载一次，不构成数据风险。
fn write_local_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp_name = format!(".{file_name}.arca-tmp-{}", std::process::id());
    let tmp = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    };
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
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
    fn 本地删除传播为远端删除但m1无处落盘_如实报告() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("e.txt"), b"content").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        fs::remove_file(dataset.path().join("e.txt")).unwrap();
        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

        assert_eq!(report.tombstone_pending, vec!["e.txt".to_string()]);
        assert!(
            !report.is_clean(),
            "未完成的 tombstone 传播必须让退出码非零"
        );
        // 绝不能被静默当 no-op：不应该出现在任何其它分类桶里。
        assert!(report.uploaded.is_empty());
        assert!(report.downloaded.is_empty());
        assert!(report.deleted_local.is_empty());
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
