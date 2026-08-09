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
//! | `DeleteLocal{item_id}` | **过四道闸门（`gates::check_delete`，M2a Task 4）后**——`client` 角色移除本地副本，`server` 角色移进工作区侧本地回收站（M2d Task 2，见 [`execute_delete_local`]）；任一闸门不过则不删，计入 `delete_blocked` |
//! | `TombstoneRemote{item_id, parent}` | 提交 tombstone：`files/` → `.arca/trash/` + 清理 index 记录 + 追加 journal `op=tombstone` 事件（M2a Task 4 收尾，复用 Task 3 交付的 `trash`/`journal` 原语；清理 index 记录见评审 Important #2） |
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
//!
//! # 角色改变 `DeleteLocal` 的执行侧，绝不改变决策本身（M2d Task 2）
//!
//! `arca-core` 的决策表**不认识角色**——`present|unchanged|tombstoned ->
//! DeleteLocal` 这一格的产出与角色无关，四道闸门（`gates::check_delete*`）
//! 同样不认识角色。角色只在闸门**之后**的执行侧分流（spec §4.7）：
//!
//! - `client` 角色（默认，见 `crate::role`）：与 M2a 起的既有行为一致，
//!   `fs::remove_file`——本地视为可再生缓存，数据的唯一副本转移到 hub。
//! - `server` 角色：**不 `unlink`**，把本地副本移进工作区侧的本地回收站
//!   （`crate::local_trash`，FORMAT.md §9.5）——这台设备承诺"本地永远有
//!   完整数据，任何云侧语义都不会缩减它"，物理销毁只经未来显式的清理命令
//!   （本切片不新增任何销毁路径，I3）。
//!
//! 这个分工必须留在这里、留在执行侧——**不要把角色塞进 `arca_core` 的决策
//! 表**：决策表回答的是"这个格子该不该产生 `DeleteLocal`"，与哪台设备用
//! 什么策略执行它是两个正交的问题；一旦决策表也认识角色，两端（client/hub）
//! 共用的 sans-io 状态机就会长出一份只有客户端才有意义的字段，破坏
//! `arca-core` "无 IO、两端共用、纯状态机"的设计（CLAUDE.md「架构约束」）。
//! `sync()`/`sync_transport()` 共用同一份执行函数（[`execute_delete_local`]）
//! ——此前两个函数各自内联了一份几乎相同的 `DeleteLocal` 处理逻辑，如果
//! 角色分流在两处各写一遍，就会有演化出两套不同角色语义的风险（`Transport`
//! 抽象当初正是为了消除这类分叉，见上文「与 `sync()` 的关系」），因此收敛
//! 成一个函数，两个调用点只负责拼出各自的 [`gates::DeleteCheckTransport`]。

// 旧 `file://` 引擎被收敛掉之后，下面几个私有 helper
// （`prepare_upload`/`execute_download`/`execute_tombstone_content`/
// `remove_index_record`）只剩测试在用。**不在这一轮删**：它们的移除会牵动
// 一批测试的构造方式，与「收敛引擎」是两件事，混在一起会让这次改动难以复核。
// 留给一次专门的清理，见 `crates/arca-conformance/tests/nightmare/README.md`。
#![allow(dead_code)]

use crate::transport::{CommitOutcome, CommitRequest, RenameRequest, TombstoneRequest, Transport};
use crate::{baseline, clock, gates, hub, ids, local_trash, role, scan, trash, vault};
use arca_chunk::hash::ContentHash;
use arca_core::reconcile::{decide_traced, Action};
use arca_core::state::{BaseState, LocalState, RemoteState};
use arca_format::hub_layout::layout;
use arca_format::manifest::{Manifest, ManifestEntry};
use arca_format::model::{Actor, ItemId};
use arca_format::path_rules;
use arca_format::trace::TraceSink;
use arca_store::atomic::{self, AtomicError};
use arca_store::root::StorageRoot;
use std::collections::{BTreeMap, BTreeSet};
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
    /// `DeleteLocal` 过闸门后，**`client` 角色**移除了本地副本（`fs::remove_file`）
    /// ——数据仍然安全，唯一副本转移到 hub。`server` 角色过闸门后走的是
    /// [`deleted_to_local_trash`]（`Self::deleted_to_local_trash`），不落在
    /// 这里——两个桶分开是故意的：调用方（`arca sync` 的命令壳）据此给出
    /// 不同的措辞（"已移除本地副本" vs "已移入本地回收站"，见 M2d Task 2
    /// brief），不能靠单一文案覆盖两种截然不同的本地终态。
    pub deleted_local: Vec<String>,
    /// `DeleteLocal` 过闸门后，**`server` 角色**把本地副本移进了工作区侧
    /// 本地回收站（`crate::local_trash`，FORMAT.md §9.5）——原文件不再在
    /// 原路径，但内容仍在 `<dataset>/.arca/client/trash/` 下可以找回，见
    /// [`execute_delete_local`] 与模块顶部「角色改变 `DeleteLocal` 的执行侧」
    /// 一节。与 `deleted_local` 同一性质（正常完成的动作），不计入
    /// `is_clean()` 的"有问题"判断。
    pub deleted_to_local_trash: Vec<String>,
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
    /// 本轮检测并提交的改名（`(旧路径, 新路径)`）——**只有 [`sync_transport`]
    /// 会填充这个字段**（M2c Task 5：`sync()`/`file://` 路径本切片不变，
    /// 见 `sync_transport` 模块文档「与 `sync()` 的关系」）；`item_id` 原样
    /// 延续（I7），不产生新版本。
    pub renamed: Vec<(String, String)>,
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
            || !self.deleted_to_local_trash.is_empty()
            || !self.tombstone_submitted.is_empty()
            || !self.renamed.is_empty()
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
    /// 读 `<dataset_root>/.arca/client/role.toml` 失败——文件缺失不会走到
    /// 这里（`role::read` 把缺失吸收成默认角色），只有内容非法/真正的 IO
    /// 故障才会（M2d Task 2，见 `role` 模块文档「与 baseline 刻意不同的
    /// 错误处理策略」）。
    Role(crate::role::RoleError),
    /// `server` 角色执行 `DeleteLocal` 时，移入工作区侧本地回收站失败
    /// （M2d Task 2，见 [`execute_delete_local`]）。
    LocalTrash(crate::local_trash::LocalTrashError),
    Io {
        path: String,
        reason: String,
    },
    /// **仅 [`sync_transport`]**（M2c Task 5）：`Transport` 操作失败——网络
    /// 故障/协议错误/数据集离线，`class()` 已经把这三者分开
    /// （`crate::transport::TransportError::class`），调用方（命令壳）据此
    /// 决定重试/停下/报告 bug。
    Transport(crate::transport::TransportError),
    /// **仅 [`sync`]**（评审 C2/I7）：本轮 `Upload` 批量提交
    /// （`LocalTransport::commit_batch`）时发现 CAS 冲突或身份不符——理论上
    /// 不该发生：`sync()` 面向单进程、一次只跑一次的场景（模块顶部「与
    /// brief 字面签名的一处刻意偏离」一节），本次调和用来做决策的 `remote`
    /// 快照与批量提交之间不应该有其它写入方介入。一旦真的发生（例如同一
    /// 存储根被另一个进程并发写入），必须整体停下如实报告，不能悄悄丢弃
    /// 部分已经在内存里更新过的 `baseline`（I5）——`commit_batch` 本身已经
    /// 保证"整批不生效"，`sync()` 因此也不会保存基线、不会让这些路径被
    /// 误判为"已经同步成功"；下次重跑会用磁盘上未受影响的旧基线重新决策。
    ///
    /// `outcome` 装箱：`CommitOutcome::Conflict` 内嵌一个 `RemoteState`，
    /// 直接内联会把 `SyncError` 整体的按值大小拖过 `clippy::result_large_err`
    /// 的阈值，累及本文件里每一个返回 `Result<_, SyncError>` 的函数——纯粒度
    /// 考量，不改变语义（`arcad/src/api.rs::open_dataset` 的 `Box<Response>`
    /// 同一处置纪律）。
    UploadRejected {
        path: String,
        outcome: Box<CommitOutcome>,
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
            SyncError::Role(e) => write!(f, "{e}"),
            SyncError::LocalTrash(e) => write!(f, "{e}"),
            SyncError::Io { path, reason } => write!(f, "{path}：{reason}"),
            SyncError::Transport(e) => write!(f, "{e}"),
            SyncError::UploadRejected { path, outcome } => write!(
                f,
                "上传 {path:?} 时批量提交被拒绝（{outcome:?}）——理论上不该发生，\
                 说明存储根在本次调和期间被其它写入方并发改动过，未保存任何基线更新，\
                 重跑一次 sync 会用磁盘上未受影响的旧基线重新决策"
            ),
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
    // **这里没有第二个引擎。** 本函数曾是一份独立实现（222 行函数体），
    // 与 `sync_transport` 并列——两条实现必然漂移，而它确实漂移了：改名
    // 检测只加进了 `sync_transport`，于是同一个改名在 `file://` 上退化成
    // 「上传 + tombstone」，新建 item_id（**违反 I7 身份跨改名稳定**）、
    // 内容全量重传、版本链分叉。而 `file://` 恰恰是 CLAUDE.md 说的
    // 「一等用户」路径。这正是 `Transport` 抽象当初要消除的那类分叉
    // （M2d 评审原话）。
    //
    // 收敛的性能代价实测为零：一万文件基准 239.3s → 240.0s（噪声级别）——
    // 那 240 秒里 238 秒花在 `adopt` 上，而 `adopt` 不走 `sync`。动手前我
    // 以为 `commit_batch` 没被 `sync_transport` 用上会造成回退，**测量
    // 推翻了这个假设**，记在这里免得下一个人据此做一轮不必要的重构。
    let transport = crate::transport::local::LocalTransport::new(root);
    sync_transport(dataset_root, &transport, actor, sink)
}

/// 准备一次 `Upload`：读本地内容、分配/延续身份，构造好一条
/// [`CommitRequest`] 与新的基线状态——**不触碰磁盘**（不写 `files/`/
/// `items/`/`index/`/journal）。真正的落盘交给调用方（[`sync`]/
/// [`sync_transport`]）收尾时的一次批量 `Transport::commit_batch`（评审
/// C2/I7，见 [`sync`] 顶部「本轮全部 `Upload`」一节）：`commit_batch` 内部
/// 已经是"内容先于指针发布"（`files/` → `items/` → `index/` → journal）
/// 的完整实现（`transport::local::LocalTransport::commit_batch` 文档），
/// 本函数不重新实现一遍写入顺序，只负责"这次提交该携带什么"。
fn prepare_upload(
    dataset_root: &Path,
    path: &str,
    base: &BaseState,
    remote_state: &RemoteState,
    parent: Option<arca_format::model::VersionId>,
    actor: &SyncActor,
) -> Result<(CommitRequest, BaseState), SyncError> {
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

    let req = CommitRequest {
        path: path.to_string(),
        item_id,
        version_id: version_id.clone(),
        parent,
        bytes,
        mtime,
        actor: actor.clone(),
    };

    Ok((
        req,
        BaseState::Present {
            item_id,
            version_id,
            hash,
            size,
        },
    ))
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

/// 执行一次 `Action::DeleteLocal`：先跑四道闸门（与角色无关），过闸门后
/// **按角色分流执行侧**——`client` 移除本地副本，`server` 移进工作区侧本地
/// 回收站，二者都不影响 hub 侧任何状态（决策与提交早在 `decide`/上游产出
/// `DeleteLocal` 时就已经完成，本函数只处理本地文件系统这一侧）。完整分工
/// 论证见模块顶部「角色改变 `DeleteLocal` 的执行侧，绝不改变决策本身」一节
/// ——**不要在这里之外的任何地方（尤其是 `arca_core`）重新判断一次角色**，
/// `sync()`/`sync_transport()` 都只应该调这一个函数。
fn execute_delete_local(
    dataset_root: &Path,
    path: &str,
    item_id: ItemId,
    device_role: role::Role,
    check: &gates::DeleteCheckTransport,
    baseline: &mut baseline::Baseline,
    report: &mut SyncReport,
) -> Result<(), SyncError> {
    match gates::check_delete_transport(check) {
        Ok(()) => {
            let local_path = dataset_root.join(to_native(path));
            match device_role {
                role::Role::Client => {
                    // 与既有行为逐字一致（M2a 起）：本地已经不存在也是
                    // 正常的幂等终态，不是错误。
                    match fs::remove_file(&local_path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(io_err(&local_path, e)),
                    }
                    report.deleted_local.push(path.to_string());
                }
                role::Role::Server => {
                    // 不 unlink：移进本地回收站，物理销毁只经未来显式的
                    // 清理命令（I3，本切片不实现、不新增任何销毁路径）。
                    //
                    // 评审 Minor #2：`move_to_trash` 在源已不存在时返回
                    // `Ok(None)`（与 client 分支对 `fs::remove_file` 的
                    // `NotFound` 同一条幂等纪律：这次调用之前源就已经不在了，
                    // 不是错误）——此前这里无条件 push 进
                    // `deleted_to_local_trash`，用户会看到一行
                    // `delete-local-trash <path>` 与"已移入本地回收站"的说明，
                    // 但那个路径根本没有对应的 `.data`/`.meta`，恢复指引指向
                    // 一个不存在的文件。按 `Option` 分流：只有真的移动了什么，
                    // 才计入这个桶。
                    let at = clock::now_rfc3339();
                    if local_trash::move_to_trash(dataset_root, &local_path, path, item_id, &at)
                        .map_err(SyncError::LocalTrash)?
                        .is_some()
                    {
                        report.deleted_to_local_trash.push(path.to_string());
                    }
                }
            }
            baseline.remove(path);
        }
        Err(failure) => {
            // 闸门拒绝：不删、不改基线、不碰角色分流——如实计入报告（I5）。
            report.delete_blocked.push((path.to_string(), failure));
        }
    }
    Ok(())
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
///
/// # 第三步：清理 index 记录（评审 Important #2）
///
/// journal 目前是"这个路径被删除了"的**唯一**证据——把 journal 清空（人为
/// 误删，或 M2b 未来的 epoch 轮转/压缩）会让每一个已 tombstone 的路径的
/// index 记录（此前从不清理，永久指向一个已经不存在于 `files/` 的 item）
/// 突然变得无法解释：`hub::read_remote` 找不到对应的 tombstone 事件，只能
/// 去读 `files/<path>`，读到 `NotFound`，report 出 `MissingContent`——一次
/// 局部的历史丢失，波及的却是**整个存储根**的读取（`read_remote` 遇到第一个
/// 错误就整体停止）。
///
/// 选择的出路：让"没有 index 记录"本身成为"这个路径已被删除"的证据——
/// `move_to_trash` 与 `journal::append` 之后，这里主动删掉 `path` 的 index
/// 记录。之后即便 journal 彻底丢失这次删除的历史，`read_remote` 的主循环
/// 压根不会再遍历到这个路径（它已经不在 `index/` 里），这个路径就干净地
/// 退化成 `RemoteState::Absent`（"看起来从未存在过"，信息降级，不是错误），
/// 不会把整个存储根的读取拖下水。相比"声明 journal 永不压缩"，这条路线
/// 不需要许下一个随着数据量增长会越来越难兑现的承诺，也顺带避免了
/// `index/` 里永久堆积每一次删除留下的死指针。
///
/// 用普通 `fs::remove_file`（不经 `arca_store::atomic`，那套机制目前只提供
/// `write`/`rename`，没有对称的原子删除）：这不是 I3 意义上的"销毁数据"——
/// index 记录只是指向已经安全移进 `.arca/trash/` 的内容的一个指针，删掉
/// 指针不影响内容本身的可恢复性。若这一步与前两步之间崩溃，最坏情况是
/// index 记录残留（退回本改动之前的行为），由 `hub.rs` 的
/// `is_pending_tombstone` 兜底诊断（评审 Important #1），不会更糟。
///
/// **只做物理搬移**（`.arca/trash/` + index 清理），**不追加 journal 事件**
/// ——journal 事件的追加推迟到 [`sync`] 收尾时统一进行（评审 C2 实机复现
/// 修复，见 [`sync`] 内「两个独立的 `AppendBatch` 不能交错提交」一节）：
/// 早先的实现在循环开始前就 `journal::AppendBatch::open` 一次（给
/// tombstone 用），循环结束后再 `commit_batch`（给 Upload 用，内部自己也
/// 会开、写、提交一个 `AppendBatch`）——`commit_batch` 的这次内部写入发生
/// 在外层 `journal_batch` 早已拍好的快照**之后**，外层 `journal_batch` 收尾
/// 时的 `commit()` 会把自己内存里那份（不含 Upload 事件的）快照整体重写回
/// 磁盘，直接冲掉 `commit_batch` 刚写入的 upsert 事件——两个 `AppendBatch`
/// 各自都是"open 时读一次、commit 时整体重写"，交错使用必然互相践踏。
/// 返回这次 tombstone 使用的 `at` 时间戳，供调用方稍后统一构造 journal 事件。
fn execute_tombstone_content(
    root: &StorageRoot,
    path: &str,
    item_id: ItemId,
) -> Result<String, SyncError> {
    let at = clock::now_rfc3339();
    trash::move_to_trash(root, path, item_id, &at).map_err(SyncError::Trash)?;
    remove_index_record(root, path)?;
    Ok(at)
}

/// 从 `.arca/index/<key>.json` 移除 `path` 的记录——`execute_tombstone` 的
/// 第三步（评审 Important #2，见其文档）。记录本就不存在（这条路径此前已经
/// 被 tombstone 过、或从来就没有 index 记录）视为无操作，不是错误——与
/// `fs::remove_file` 在其它地方的幂等处理一致。
fn remove_index_record(root: &StorageRoot, path: &str) -> Result<(), SyncError> {
    let key = path_rules::index_key(path);
    let rel = layout::index_path(&key);
    let full = root.path().join(&rel);
    match fs::remove_file(&full) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(&full, e)),
    }
}

/// 从最终基线重新生成 `<dataset_root>/.arca/manifest`（评审 Important #4）：
/// `sync` 收尾时的基线就是"这个数据集当前每个受管路径的哈希/大小的权威
/// 快照"，清单只是它在 git 侧的行式镜像。**每次 `sync` 都要重新生成**，
/// 不能只靠 `arca adopt` 生成一次——见本函数调用点的注释。
///
/// `mtime` 取文件自身的 mtime（FORMAT.md 定义的字段语义），不是"写清单
/// 这一刻"的墙上时钟——用 `prepare_upload` 同一段 `rfc3339_from_systemtime`
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

// =============================================================================
// sync_transport：`Transport` 泛化的同步引擎（M2c Task 5）
// =============================================================================
//
// # 与 `sync()` 的关系
//
// `sync()`（本文件顶部）直接摸 `&StorageRoot`，走 `arca_store::atomic::Batch`/
// `journal::AppendBatch` 把一整轮的存储根写入收口到一次 fsync——这是刻意的
// 性能设计（M1d 批量归档），只有本地磁盘才谈得上"批量收口"这件事本身。
// `arca adopt` 内部仍然调用 `sync()`（保持 `file://`-only，不变，见
// `adopt.rs`）；本节新增的 [`sync_transport`] 是 `arca sync` 命令壳
// （`commands/porcelain.rs::sync_cmd`）**面向未来全部 hub**的同步入口——
// `file://` 经 `transport::local::LocalTransport`，`http://` 经
// `transport::http::HttpTransport`，两者共享同一份决策/执行逻辑。
//
// 这不是简单的"把 `sync()` 泛化成 `<T: Transport>`"：`Transport::commit`
// 每次调用各自加锁、各自一次 CAS 临界区（面向 HTTP 的一次 `PUT` 语义），
// 不是 `sync()` 里"整轮收口成一次 fsync"那种批处理——`sync_transport` 因此
// 没有 `Batch`/`AppendBatch`，每个 `Action` 对应一次独立的 `Transport` 调用，
// 网络场景下这是正确的性能形状（HTTP 无法把"多次写入合并成一次系统调用"），
// 本地场景下多付的是"每个文件一次 `arca.lock` 获取/释放"的开销，不是正确性
// 代价（`LocalTransport::commit`/`tombstone`/`rename` 都各自持有跨进程锁）。
//
// # 改名检测：客户端启发式，不进 `arca-core` 决策表
//
// `arca_core::reconcile::decide` 是逐路径独立判断的三态调和表（spec 设计
// 如此，`arca-core` 本切片未改一行——见 CLAUDE.md「架构约束」）：它不产生
// "改名"这个动作，也不该产生——识别"路径 A 消失、路径 B 以相同内容出现"
// 需要跨路径比较，不是任何单个路径的三态能表达的判断。改名的检测因此完全
// 留在这里（[`detect_renames`]），检测到之后才调用
// [`Transport::rename`]（见其文档「为什么需要这第三个写入原语」）；检测
// 不到（没有消失的路径、消失的内容没有对应的新路径、匹配有歧义）时，
// 两条路径各自照常走 `decide()` 的常规决策（旧路径 `TombstoneRemote`，
// 新路径 `Upload{parent:None}`）——退化行为完全等同于"没有改名检测"这个
// 功能不存在时的表现，不会因为检测失败就卡住整轮同步。
pub fn sync_transport<T: Transport>(
    dataset_root: &Path,
    transport: &T,
    actor: &SyncActor,
    sink: &mut dyn TraceSink,
) -> Result<SyncReport, SyncError> {
    let scan_result = scan::scan_dataset(dataset_root, sink).map_err(SyncError::Scan)?;
    let mut baseline = baseline::load(dataset_root).map_err(SyncError::Baseline)?;
    let baseline_reset = baseline.was_reset();
    let remote = transport.read_remote().map_err(SyncError::Transport)?;
    // M2d Task 2：与 `sync()` 同一条纪律，见其同名注释。
    let device_role = role::read(dataset_root).map_err(SyncError::Role)?;

    let mut report = SyncReport {
        scan_rejected: scan_result.rejected.clone(),
        baseline_reset,
        ..SyncReport::default()
    };

    // 闸门第 1 道（read_roots 范围）——与 `sync()` 同一条纪律，见其文档
    // 同名注释。
    let scanned_paths: BTreeSet<String> = scan_result.files.keys().cloned().collect();

    // --- 改名：检测 + 提交（先于常规决策循环，见模块文档） -----------------
    let renames = detect_renames(&scan_result, &baseline, &remote);
    let mut handled: BTreeSet<String> = BTreeSet::new();
    for (old_path, new_path) in renames {
        let base = baseline.get(&old_path);
        let BaseState::Present {
            item_id,
            version_id: parent,
            ..
        } = base
        else {
            // `detect_renames` 只会给出 baseline 里确实是 Present 的旧路径
            // ——这个分支结构上不可达，写出来是防御性的（I5：绝不假装
            // 一个不可能状态能被安全忽略），不静默跳过。
            return Err(SyncError::Io {
                path: old_path.clone(),
                reason: "detect_renames 给出的旧路径在 baseline 里不是 Present（不可能状态）"
                    .to_string(),
            });
        };
        let at = clock::now_rfc3339();
        let outcome = transport
            .rename(&RenameRequest {
                old_path: old_path.clone(),
                new_path: new_path.clone(),
                item_id,
                parent: parent.clone(),
                actor: actor.clone(),
                at,
            })
            .map_err(SyncError::Transport)?;
        match outcome {
            CommitOutcome::Committed {
                item_id,
                version_id,
            } => {
                let hash = match baseline.get(&old_path) {
                    BaseState::Present { hash, size, .. } => Some((hash, size)),
                    BaseState::Absent => None,
                };
                let Some((hash, size)) = hash else {
                    unreachable!("上面已经匹配过 Present，这里必然仍是 Present")
                };
                baseline.remove(&old_path);
                baseline.set(
                    new_path.clone(),
                    BaseState::Present {
                        item_id,
                        version_id,
                        hash,
                        size,
                    },
                );
                report.renamed.push((old_path.clone(), new_path.clone()));
                handled.insert(old_path);
                handled.insert(new_path);
            }
            // 改名这一步的 CAS/身份校验没过——多半是另一端已经先动过这两个
            // 路径之一。不特殊处理、不当作错误：把这两个路径原样丢回常规
            // 决策循环，`decide()` 会按它们各自此刻的三态给出恰当的
            // Upload/TombstoneRemote/Conflict，安全但退化成"没有改名检测"
            // 的行为（模块文档「改名检测」一节）。
            CommitOutcome::Conflict { .. } | CommitOutcome::IdentityMismatch { .. } => {}
        }
    }

    // --- 改名：接收端识别（对称的另一半，见 `detect_remote_renames` 文档） --
    //
    // 上面那段是"这台设备发起了改名，提交给 hub"；这里是"hub 上已经发生过
    // 一次改名（另一台设备提交的），这台设备如何在本地体现它"。不做这一步
    // 的后果：接收端会把旧路径读成 `RemoteState::Absent`（`hub::read_remote`
    // 不从 `Op::Rename` 事件推导任何状态，只有 `Op::Tombstone` 才产出
    // `Tombstoned`）——`decide()` 面对"基线说存在、本地也存在、远端却凭空
    // 消失"只能给出 `reconcile.needs_human`（`remote_vanished_without_tombstone`，
    // I5：不能把"消失"猜成"删除"），新路径则被当成普通新增走一次
    // `Download`——功能上不算错（数据不会丢，item_id 仍然正确、可以从 hub
    // 侧核对），但用户体验是"多了一个需要人工介入的告警 + 一次不必要的
    // 下载"，不是「另一端 sync 后也改名」这个直觉结果。
    // `handled` 传进去过滤——上面那段"本地发起"的改名已经就地更新了
    // `baseline`（旧路径删、新路径加），但 `remote` 仍是本轮开头读到的
    // 那份快照（还没反映刚提交的改名）；不排除 `handled` 里的路径的话，
    // `detect_remote_renames` 会把"我们自己刚提交的改名"错误地在同一轮
    // 里识别成"反向的远端改名"（用陈旧的 `remote` 快照与刚更新的
    // `baseline` 互相印证，得到一个方向相反的假阳性）。
    let remote_renames = detect_remote_renames(&scan_result, &baseline, &remote, &handled);
    for (old_path, new_path) in remote_renames {
        let old_local = dataset_root.join(to_native(&old_path));
        let new_local = dataset_root.join(to_native(&new_path));
        if let Some(parent) = new_local.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err(&new_local, e))?;
        }
        match fs::rename(&old_local, &new_local) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // 本地旧文件在检测之后、改名之前的这一瞬间被外部动过——
                // 极窄的竞态窗口，不构成数据风险：跳过这一对，留给下一轮
                // `sync_transport` 重新判断（那时 `scan_dataset` 会看到
                // 新的本地状态），不是本函数需要在这里补救的情形。
                continue;
            }
            Err(e) => return Err(io_err(&new_local, e)),
        }
        let (item_id, version_id, hash, size) = match remote.get(&new_path) {
            Some(RemoteState::Present {
                item_id,
                version_id,
                hash,
                size,
            }) => (*item_id, version_id.clone(), *hash, *size),
            other => unreachable!(
                "detect_remote_renames 只会给出 remote 里确实是 Present 的新路径，实得 {other:?}"
            ),
        };
        baseline.remove(&old_path);
        baseline.set(
            new_path.clone(),
            BaseState::Present {
                item_id,
                version_id,
                hash,
                size,
            },
        );
        report.renamed.push((old_path.clone(), new_path.clone()));
        handled.insert(old_path);
        handled.insert(new_path);
    }

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(scan_result.files.keys().cloned());
    paths.extend(baseline.iter().map(|(p, _)| p.clone()));
    paths.extend(remote.keys().cloned());
    paths.retain(|p| !handled.contains(p));

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
                let local_path = dataset_root.join(to_native(path));
                let bytes = fs::read(&local_path).map_err(|e| io_err(&local_path, e))?;
                let hash = ContentHash::from_bytes(&bytes);
                let size = bytes.len() as u64;
                let item_id = match &parent {
                    None => ids::new_item_id(),
                    Some(_) => base.item_id().or_else(|| remote_state.item_id()).expect(
                        "Upload{parent:Some(_)} 意味着 base 或 remote 至少一方已知这个 item",
                    ),
                };
                let version_id = ids::new_version_id();
                let mtime = fs::metadata(&local_path)
                    .and_then(|m| m.modified())
                    .map(rfc3339_from_systemtime)
                    .unwrap_or_else(|_| clock::now_rfc3339());
                let outcome = transport
                    .commit(&CommitRequest {
                        path: path.clone(),
                        item_id,
                        version_id: version_id.clone(),
                        parent,
                        bytes,
                        mtime,
                        actor: actor.clone(),
                    })
                    .map_err(SyncError::Transport)?;
                match outcome {
                    CommitOutcome::Committed {
                        item_id,
                        version_id,
                    } => {
                        baseline.set(
                            path.clone(),
                            BaseState::Present {
                                item_id,
                                version_id,
                                hash,
                                size,
                            },
                        );
                        report.uploaded.push(path.clone());
                    }
                    // 提交时刻才发现的 CAS 冲突——`decide()` 用的是循环开始
                    // 时读到的那份 `remote` 快照，两机并发写同一路径时，
                    // 对方可能恰好在这中间提交成功。**不更新基线**：下一轮
                    // `decide()` 会用（仍然是旧的）基线 + 本地内容 + 这一次
                    // 已经变化的远端内容重新判断，落进
                    // `both_new_divergent`/`three_way_divergent` 的常规
                    // Conflict 分支——双方各自的内容都原封不动（远端已提交
                    // 的版本没被覆盖，本地文件也没被覆盖），这正是「双版本
                    // 并存、绝不静默覆盖」（spec §5.3）在 CAS 冲突这条路径
                    // 上的落地，不需要额外发明"冲突副本"机制。
                    CommitOutcome::Conflict { .. } => {
                        report.conflicts.push(path.clone());
                    }
                    // 身份校验没过（item_id 已被别的路径占用，或已被
                    // tombstone 终结）——状态模糊，按 I5 停下等人，不猜测
                    // 该怎么处理。
                    CommitOutcome::IdentityMismatch { .. } => {
                        report.needs_human.push(path.clone());
                    }
                }
            }

            Action::Download { version_id } => {
                let _ = &version_id; // 语义同 `execute_download`，见其文档。
                let local_path = dataset_root.join(to_native(path));
                let (item_id, new_version_id, hash, size) = match &remote_state {
                    RemoteState::Present {
                        item_id,
                        version_id,
                        hash,
                        size,
                    } => (*item_id, version_id.clone(), *hash, *size),
                    other => unreachable!("Download 只在 remote 是 Present 时产生，实得 {other:?}"),
                };
                // 与 `execute_download` 同一条 fsync 纪律（M2a Task 1）：
                // `atomic::write_local` 负责 tmp → fsync → rename → fsync
                // 父目录的原子提交链；内容本身经
                // `Transport::read_content`——它的流式实现
                // （`read_content_into`）已经保证 `Transport` 这一侧（读取
                // 网络响应体/本地文件）不整份缓冲（服务端 C2 的镜像，见
                // `transport::http::HttpTransport` 模块文档），`arca-cli`
                // 没有能替代 `write_local(bytes: &[u8])` 的流式落盘原语
                // （那需要一个不依赖 `tempfile`——它只是 dev-dependency——
                // 的客户端侧临时文件方案，超出本切片范围，与 `execute_download`
                // 现有实现的内存特征完全一致，不是本切片引入的新缺口）。
                let bytes = transport.read_content(path).map_err(SyncError::Transport)?;
                atomic::write_local(dataset_root, &local_path, &bytes)
                    .map_err(SyncError::Atomic)?;
                baseline.set(
                    path.clone(),
                    BaseState::Present {
                        item_id,
                        version_id: new_version_id,
                        hash,
                        size,
                    },
                );
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
                let check = gates::DeleteCheckTransport {
                    path,
                    item_id,
                    scanned_paths: &scanned_paths,
                    remote_state: &remote_state,
                    dataset_root,
                    base: &base,
                    transport,
                };
                execute_delete_local(
                    dataset_root,
                    path,
                    item_id,
                    device_role,
                    &check,
                    &mut baseline,
                    &mut report,
                )?;
            }

            Action::TombstoneRemote { item_id, parent } => {
                let at = clock::now_rfc3339();
                let outcome = transport
                    .tombstone(&TombstoneRequest {
                        path: path.clone(),
                        item_id,
                        parent,
                        actor: actor.clone(),
                        at,
                    })
                    .map_err(SyncError::Transport)?;
                match outcome {
                    CommitOutcome::Committed { .. } => {
                        baseline.remove(path);
                        report.tombstone_submitted.push(path.clone());
                    }
                    CommitOutcome::Conflict { .. } => {
                        report.conflicts.push(path.clone());
                    }
                    CommitOutcome::IdentityMismatch { .. } => {
                        report.needs_human.push(path.clone());
                    }
                }
            }

            Action::Conflict { .. } => {
                report.conflicts.push(path.clone());
            }

            Action::NeedsHuman { .. } => {
                report.needs_human.push(path.clone());
            }
        }
    }

    baseline.save(dataset_root).map_err(SyncError::Baseline)?;
    write_manifest(dataset_root, &baseline)?;

    Ok(report)
}

/// 改名检测：内容哈希匹配的"消失路径 ↔ 新增路径"配对，**唯一匹配才算数**
/// （I5：绝不猜测）——同一份内容在这一轮里有多处消失、或多处新增，任何一种
/// 歧义都直接放弃把它们当作改名处理，退回常规的 tombstone+upload 路径
/// （模块文档「改名检测」一节）。
///
/// "消失"的判定同时要求 **hub 侧仍然认得这个 item 的这个版本**
/// （`remote.get(old_path)` 与 baseline 记录的 item_id/version_id 一致）：
/// 如果远端已经不是我们认识的状态（被别的设备删除/修改过），说明这不是
/// 单纯的本地改名，是需要走常规调和（很可能落进 Conflict/TombstoneRemote）
/// 的场景，不应该被 rename 检测抢先接管。
fn detect_renames(
    scan_result: &scan::ScanResult,
    baseline: &baseline::Baseline,
    remote: &BTreeMap<String, RemoteState>,
) -> Vec<(String, String)> {
    use std::collections::HashMap;

    let mut vanished: HashMap<ContentHash, Vec<String>> = HashMap::new();
    for (path, state) in baseline.iter() {
        let BaseState::Present {
            item_id,
            version_id,
            hash,
            ..
        } = state
        else {
            continue;
        };
        if scan_result.files.contains_key(path) {
            continue; // 本地还在，不是"消失"。
        }
        match remote.get(path) {
            Some(RemoteState::Present {
                item_id: r_item,
                version_id: r_version,
                ..
            }) if *r_item == *item_id && r_version == version_id => {
                vanished.entry(*hash).or_default().push(path.clone());
            }
            _ => {}
        }
    }

    let mut appeared: HashMap<ContentHash, Vec<String>> = HashMap::new();
    for (path, local) in &scan_result.files {
        let LocalState::Present { hash, .. } = local else {
            continue;
        };
        if baseline.get(path) != BaseState::Absent {
            continue; // baseline 里已经有记录——不是"全新出现"的路径。
        }
        appeared.entry(*hash).or_default().push(path.clone());
    }

    let mut renames: Vec<(String, String)> = Vec::new();
    for (hash, olds) in vanished {
        let [old_path] = olds.as_slice() else {
            continue; // 同一内容多处消失——歧义，不猜。
        };
        let Some(news) = appeared.get(&hash) else {
            continue;
        };
        let [new_path] = news.as_slice() else {
            continue; // 同一内容多处新增——歧义，不猜。
        };
        renames.push((old_path.clone(), new_path.clone()));
    }
    renames.sort();
    renames
}

/// 改名检测的接收端一半——对称于 [`detect_renames`]（那是"本地发起"），
/// 这里识别"hub 上已经发生过一次改名，这台设备该怎么在本地体现它"：
/// 旧路径本地内容原封未动（与基线一致，没有本地独立修改）、但 hub 侧那个
/// item_id/version_id 现在挂在另一个路径下——这就是同一个改名事件从
/// 接收端看到的样子，不需要下载任何字节（内容本来就在本地），只需要一次
/// 本地 `fs::rename`。**唯一匹配才算数**（I5：同一条规则，见
/// [`detect_renames`] 文档）：一个 item_id/version_id 在 `remote` 里只可能
/// 出现在一个路径下（`local.rs::commit`/`rename` 的身份校验保证这一点），
/// 所以这里不会有"同一 item 多个候选新路径"的歧义，但新路径本地若已经被
/// 别的内容占用，仍然放弃，交给常规决策处理。
fn detect_remote_renames(
    scan_result: &scan::ScanResult,
    baseline: &baseline::Baseline,
    remote: &BTreeMap<String, RemoteState>,
    handled: &BTreeSet<String>,
) -> Vec<(String, String)> {
    let mut renames: Vec<(String, String)> = Vec::new();
    for (old_path, base) in baseline.iter() {
        if handled.contains(old_path) {
            continue; // 本轮已经被"本地发起改名"处理过，见调用点注释。
        }
        let BaseState::Present {
            item_id,
            version_id,
            hash,
            ..
        } = base
        else {
            continue;
        };
        // 本地内容必须与基线一致（没有独立的本地修改）——否则本地这一侧
        // 也在变化，不能让"远端改名"这个判断悄悄吞掉本地的修改，交给常规
        // 冲突/上传路径处理。
        match scan_result.files.get(old_path) {
            Some(LocalState::Present { hash: h, .. }) if h == hash => {}
            _ => continue,
        }
        // 远端此刻这个路径必须已经不在（否则不是"消失"，是别的场景）。
        if matches!(remote.get(old_path), Some(RemoteState::Present { .. })) {
            continue;
        }
        let Some(new_path) = remote.iter().find_map(|(p, s)| match s {
            RemoteState::Present {
                item_id: i,
                version_id: v,
                ..
            } if i == item_id && v == version_id && p != old_path && !handled.contains(p) => {
                Some(p)
            }
            _ => None,
        }) else {
            continue;
        };
        if scan_result.files.contains_key(new_path) {
            continue; // 本地已经有内容占着这个新路径——不覆盖，交给常规决策。
        }
        renames.push((old_path.clone(), new_path.clone()));
    }
    renames.sort();
    renames
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::reconcile::decide;
    use arca_format::hub_layout::FormatJson;
    use arca_format::index::IndexRecord;
    use arca_format::items;
    use arca_format::model::Version;
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
        // **评审 C2 修复后的必要更新**：第一次 `sync()`（上传 "j.txt"）现在
        // 会经 `commit_batch` 追加一条 `seq=1` 的 `op=upsert` 事件（修复前
        // journal 全程 0 字节，这里能硬编码 `seq: 1`；那正是 C2 要修的洞）——
        // 这条手工拼的 tombstone 事件必须取真正的下一个 `seq`，不能再假设
        // journal 是空的。
        let next_seq = crate::journal::next_seq(&root).unwrap();
        crate::journal::append(
            &root,
            &arca_format::journal::JournalEvent {
                seq: next_seq,
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

    /// M2d Task 2 核心验收：**同一个远端 tombstone**，两台设备的本地终态因
    /// 角色不同而不同——`client` 角色移除本地副本（既有行为不变），
    /// `server` 角色不 unlink，把内容挪进工作区侧本地回收站、原文件仍可
    /// 找回；无论哪种角色，hub 侧的状态都完全一样（角色只影响执行侧，
    /// 不影响提交给 hub 的任何东西，见 `execute_delete_local` 文档）。
    #[test]
    fn 同一远端tombstone下client移除本地副本_server移入本地回收站可找回() {
        fn 造一次tombstone传播场景(
            given_role: role::Role,
        ) -> (tempfile::TempDir, tempfile::TempDir) {
            let dataset = tempfile::tempdir().unwrap();
            let store = tempfile::tempdir().unwrap();
            造存储根(store.path());
            fs::create_dir_all(store.path().join(".arca/trash")).unwrap();
            fs::create_dir_all(store.path().join(".arca/journal")).unwrap();
            fs::write(dataset.path().join("j.txt"), b"content").unwrap();

            if matches!(given_role, role::Role::Server) {
                role::write(dataset.path(), given_role).unwrap();
            }

            let root = open_root(store.path());
            let mut sink = NullSink;
            sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

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
            let next_seq = crate::journal::next_seq(&root).unwrap();
            crate::journal::append(
                &root,
                &arca_format::journal::JournalEvent {
                    seq: next_seq,
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
            assert!(
                report.is_clean(),
                "{given_role:?} 角色下 DeleteLocal 应是正常终态：{report:?}"
            );

            match given_role {
                role::Role::Client => {
                    assert_eq!(report.deleted_local, vec!["j.txt".to_string()]);
                    assert!(report.deleted_to_local_trash.is_empty());
                }
                role::Role::Server => {
                    assert_eq!(report.deleted_to_local_trash, vec!["j.txt".to_string()]);
                    assert!(report.deleted_local.is_empty());
                }
            }

            (dataset, store)
        }

        // --- client 角色：本地副本被移除，工作区侧不留任何回收站条目 ---
        let (client_dataset, client_store) = 造一次tombstone传播场景(role::Role::Client);
        assert!(
            !client_dataset.path().join("j.txt").exists(),
            "client 角色：本地副本应已被移除"
        );
        assert!(
            !client_dataset.path().join(".arca/client/trash").exists()
                || fs::read_dir(client_dataset.path().join(".arca/client/trash"))
                    .map(|mut it| it.next().is_none())
                    .unwrap_or(true),
            "client 角色不应在本地回收站留下任何条目"
        );

        // --- server 角色：原路径消失，但内容仍可从本地回收站找回 ---
        let (server_dataset, server_store) = 造一次tombstone传播场景(role::Role::Server);
        assert!(
            !server_dataset.path().join("j.txt").exists(),
            "server 角色：原路径不应再持有内容（已挪进本地回收站）"
        );
        let local_trash_dir = server_dataset.path().join(".arca/client/trash");
        let data_entry = fs::read_dir(&local_trash_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().ends_with(".data"))
            .expect("server 角色：本地回收站必须留有一条 .data 记录");
        assert_eq!(
            fs::read(data_entry.path()).unwrap(),
            b"content",
            "server 角色：本地回收站里的内容必须与被删除前完全一致——原文件仍可找回"
        );

        // --- 两种角色下 hub 侧的状态完全一样：都只是把权威副本移进
        //     `.arca/trash/`，角色只影响客户端执行侧，不影响提交给 hub 的
        //     任何东西 ---
        for store in [&client_store, &server_store] {
            let trash_has_data = fs::read_dir(store.path().join(".arca/trash"))
                .unwrap()
                .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".data"));
            assert!(trash_has_data, "hub 的权威副本必须仍在 .arca/trash/ 里");
        }
    }

    /// 评审 Minor #2 的核心复现测试：`server` 角色下，如果本地文件在
    /// `DeleteLocal` 真正执行之前就已经不在了（用户手动删了/这次调用是重跑），
    /// `local_trash::move_to_trash` 如实返回 `None`——这个路径不该出现在
    /// `report.deleted_to_local_trash` 里。此前无条件 push，用户会看到一行
    /// `delete-local-trash j.txt` 与"已移入本地回收站"的说明，但
    /// `.arca/client/trash/` 下根本没有对应的 `.data`/`.meta`，恢复指引指向
    /// 一个不存在的文件。
    #[test]
    fn server角色下源已不存在时deleted_to_local_trash不空报() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::create_dir_all(store.path().join(".arca/trash")).unwrap();
        fs::create_dir_all(store.path().join(".arca/journal")).unwrap();
        fs::write(dataset.path().join("j.txt"), b"content").unwrap();
        role::write(dataset.path(), role::Role::Server).unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

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
        let next_seq = crate::journal::next_seq(&root).unwrap();
        crate::journal::append(
            &root,
            &arca_format::journal::JournalEvent {
                seq: next_seq,
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

        // 关键：在 DeleteLocal 真正执行之前，本地文件已经不在了（模拟用户
        // 手动删除，或这次调用是重跑）——`decide` 仍会因为基线记录了这个
        // path 而产出 `DeleteLocal`，但 `local_path` 此刻已经没有内容可挪。
        fs::remove_file(dataset.path().join("j.txt")).unwrap();

        let report = sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
        assert!(
            report.deleted_to_local_trash.is_empty(),
            "源已不存在，不该假装挪了一份可恢复的回收站记录：{report:?}"
        );
        let local_trash_dir = dataset.path().join(".arca/client/trash");
        assert!(
            !local_trash_dir.exists()
                || fs::read_dir(&local_trash_dir)
                    .map(|mut it| it.next().is_none())
                    .unwrap_or(true),
            "本地回收站不该留下任何条目（没有内容被真的移进来）"
        );
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
            chunks: None,
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
        // 判据是**语义**而不是包装层：`MissingContent` 必须原样传到调用方、
        // 整轮同步必须失败。走 `Transport` 抽象之后它包在 `Transport(Hub(..))`
        // 里而不是直接的 `Hub(..)`——那是收敛到单一引擎的结果，不是行为变化。
        let text = format!("{err:?}");
        assert!(
            text.contains("MissingContent") && text.contains("c.bin"),
            "应整体报错为 MissingContent 并点名路径，实得 {err:?}"
        );

        // 绝不能有任何一个字节被误当作"已同步"：基线必须仍是空的。
        let baseline = crate::baseline::load(dataset.path()).unwrap();
        assert!(
            baseline.get("c.bin") == arca_core::state::BaseState::Absent,
            "报错路径上绝不能把基线写成已同步"
        );
    }

    // -----------------------------------------------------------------
    // M2c Task 5：sync_transport（`Transport` 泛化引擎，用 `LocalTransport`
    // 验证——`HttpTransport` 走的是同一份代码路径，两机端到端演示另外用
    // 真实 `arcad` 进程验证网络这一层，这里只验证决策/执行逻辑本身）。
    // -----------------------------------------------------------------

    #[test]
    fn sync_transport_本地新增文件被上传() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("a.txt"), b"hello").unwrap();

        let root = open_root(store.path());
        let transport = crate::transport::local::LocalTransport::new(&root);
        let mut sink = NullSink;
        let report = sync_transport(dataset.path(), &transport, &actor(), &mut sink).unwrap();

        assert_eq!(report.uploaded, vec!["a.txt".to_string()]);
        assert!(report.is_clean());
        assert!(store.path().join("files/a.txt").is_file());
    }

    #[test]
    fn sync_transport_远端新增被下载() {
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        let root = open_root(store.path());
        let transport = crate::transport::local::LocalTransport::new(&root);
        let mut sink = NullSink;

        let seed = tempfile::tempdir().unwrap();
        fs::write(seed.path().join("b.txt"), b"remote content").unwrap();
        sync_transport(seed.path(), &transport, &actor(), &mut sink).unwrap();

        let dataset = tempfile::tempdir().unwrap();
        let report = sync_transport(dataset.path(), &transport, &actor(), &mut sink).unwrap();
        assert_eq!(report.downloaded, vec!["b.txt".to_string()]);
        assert!(report.is_clean());
        assert_eq!(
            fs::read(dataset.path().join("b.txt")).unwrap(),
            b"remote content"
        );
    }

    /// I7 核心验证：本地 `mv old new`（对 arca 而言就是"旧路径消失、新路径
    /// 以相同内容出现"）之后跑 `sync_transport`，`item_id` 必须原样延续，
    /// 不能因为改名而分配了一个新身份。
    #[test]
    fn sync_transport_本地改名被检测并提交_item_id不变() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("old.txt"), b"same bytes").unwrap();

        let root = open_root(store.path());
        let transport = crate::transport::local::LocalTransport::new(&root);
        let mut sink = NullSink;
        sync_transport(dataset.path(), &transport, &actor(), &mut sink).unwrap();

        let item_id_before = match crate::hub::read_remote(&root).unwrap().get("old.txt") {
            Some(RemoteState::Present { item_id, .. }) => *item_id,
            other => panic!("应为 Present，实得 {other:?}"),
        };

        // 真实改名：`mv old.txt new.txt`（内容字节不变）。
        fs::rename(
            dataset.path().join("old.txt"),
            dataset.path().join("new.txt"),
        )
        .unwrap();

        let report = sync_transport(dataset.path(), &transport, &actor(), &mut sink).unwrap();
        assert_eq!(
            report.renamed,
            vec![("old.txt".to_string(), "new.txt".to_string())]
        );
        assert!(report.is_clean());
        // 不应该被误判成"删除旧的 + 新增一个新身份"。
        assert!(report.tombstone_submitted.is_empty());
        assert!(report.uploaded.is_empty());

        let remote = crate::hub::read_remote(&root).unwrap();
        assert!(!remote.contains_key("old.txt"));
        match remote.get("new.txt") {
            Some(RemoteState::Present { item_id, .. }) => {
                assert_eq!(*item_id, item_id_before, "I7：item_id 必须跨改名稳定");
            }
            other => panic!("应为 Present，实得 {other:?}"),
        }

        // 基线也要跟着改名——不是"旧路径删了、新路径当新文件认领"。
        let baseline = crate::baseline::load(dataset.path()).unwrap();
        assert_eq!(baseline.get("old.txt"), BaseState::Absent);
        assert!(matches!(baseline.get("new.txt"), BaseState::Present { .. }));
    }

    #[test]
    fn sync_transport_本地删除传播为远端tombstone() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("e.txt"), b"content").unwrap();

        let root = open_root(store.path());
        let transport = crate::transport::local::LocalTransport::new(&root);
        let mut sink = NullSink;
        sync_transport(dataset.path(), &transport, &actor(), &mut sink).unwrap();

        fs::remove_file(dataset.path().join("e.txt")).unwrap();
        let report = sync_transport(dataset.path(), &transport, &actor(), &mut sink).unwrap();

        assert_eq!(report.tombstone_submitted, vec!["e.txt".to_string()]);
        assert!(report.is_clean());
        let remote = crate::hub::read_remote(&root).unwrap();
        assert!(matches!(
            remote.get("e.txt"),
            Some(RemoteState::Tombstoned { .. })
        ));
    }

    /// 两台设备各自通过 `sync_transport` 同步到同一个存储根，验证
    /// upload/download/rename/tombstone 全部经由同一份 `Transport` 泛化
    /// 代码路径串联起来，与 `LocalTransport` 已有的 file:// 行为一致。
    #[test]
    fn sync_transport_两台设备端到端_上传_下载_改名_删除() {
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        let root = open_root(store.path());
        let transport = crate::transport::local::LocalTransport::new(&root);
        let mut sink = NullSink;

        let device_a = tempfile::tempdir().unwrap();
        fs::write(device_a.path().join("photo.png"), b"bytes").unwrap();
        sync_transport(device_a.path(), &transport, &actor(), &mut sink).unwrap();

        let device_b = tempfile::tempdir().unwrap();
        let report_b1 = sync_transport(device_b.path(), &transport, &actor(), &mut sink).unwrap();
        assert_eq!(report_b1.downloaded, vec!["photo.png".to_string()]);

        // 设备甲改名。
        fs::rename(
            device_a.path().join("photo.png"),
            device_a.path().join("renamed.png"),
        )
        .unwrap();
        let report_a2 = sync_transport(device_a.path(), &transport, &actor(), &mut sink).unwrap();
        assert_eq!(
            report_a2.renamed,
            vec![("photo.png".to_string(), "renamed.png".to_string())]
        );

        // 设备乙同步：`detect_remote_renames`（接收端一半）识别出"旧路径
        // 本地内容原封未动、item_id/version_id 现在挂在新路径下"，本地做
        // 一次 `fs::rename`，零传输——不下载、不走 tombstone+DeleteLocal
        // 这条更笨拙但结果相同的路径。
        let report_b2 = sync_transport(device_b.path(), &transport, &actor(), &mut sink).unwrap();
        assert_eq!(
            report_b2.renamed,
            vec![("photo.png".to_string(), "renamed.png".to_string())]
        );
        assert!(report_b2.downloaded.is_empty());
        assert!(report_b2.deleted_local.is_empty());
        assert!(!device_b.path().join("photo.png").exists());
        assert_eq!(
            fs::read(device_b.path().join("renamed.png")).unwrap(),
            b"bytes"
        );
    }
}
