//! `Transport`：客户端看 hub 的唯一接口（M2b Task 1，见
//! `docs/superpowers/plans/2026-08-08-m2b-arcad-cas.md`「为什么先抽传输，再写
//! 服务端」一节）。
//!
//! 在这个抽象出现之前，`hub.rs`/`sync.rs`/`gates.rs`/`trash.rs` 全都直接摸
//! `arca_store::root::StorageRoot`——M2a 的切片评审点名：闸门第 4 道
//! （`gates::DeleteCheck`）拿 `&StorageRoot` 的签名会在 HTTP 传输下挡路，且
//! O(n·m) 的重复扫描在网络往返下会从"慢"变成"不可用"。本模块先把"客户端
//! 需要向 hub 问什么、提交什么"提炼成一个不关心传输方式的接口，
//! [`local::LocalTransport`] 是它的第一个实现（`file://`，本切片交付）；
//! `http::HttpTransport`（M2b 后续 Task）会是第二个，`sync.rs` 的调和闭环本身
//! 不因传输方式改变而分叉。
//!
//! # 本切片（Task 1）的落地范围：新增，不强推全部既有调用点
//!
//! **判据是"512 个测试一条不改，全部照常通过"**（brief 原文）——这条约束比
//! "让每一处直接摸 `StorageRoot` 的代码都改道"更硬。`hub::read_remote`、
//! `trash::move_to_trash`/`restore`、`journal::append`/`AppendBatch` 这些函数的
//! 签名被现有测试大量直接引用（`hub.rs`/`sync.rs`/`gates.rs`/`trash.rs` 各自的
//! `#[cfg(test)] mod tests` 里手工拼场景时会直接调用它们），改变签名意味着
//! 直接改测试代码——那正是 brief 明确要求"停下报告"而不是自行判断的情形。
//!
//! 所以本模块的落地策略是：
//!
//! 1. [`local::LocalTransport`] 是一个**完整、独立、可正确工作**的 `file://`
//!    实现，内部复用 `hub::read_remote`/`trash::*`/`journal::*` 已经交付、
//!    已被测试覆盖的原语（不重新实现落盘细节），不是占位符。
//! 2. **评审点名的那个改动确实落地**：`gates.rs` 新增 `check_delete_transport`
//!    与 `DeleteCheckTransport`（不含 `&StorageRoot`/`trash_entries`，靠
//!    [`Transport::recoverable`] 完成第 4 道闸门），`sync.rs` 的 `Action::DeleteLocal`
//!    分支**生产代码路径**已经切换到经它执行——`gates::check_delete`/`DeleteCheck`
//!    保持原样不动（`gates.rs` 自己的测试继续验证它），新旧两条路径共享同一份
//!    三方哈希核验逻辑（见 `gates.rs` 的 `check_retention_transport`）。
//! 3. `execute_upload`/`execute_download`/`execute_tombstone`（`sync.rs`）的内部
//!    写入路径本切片**不**改道：它们各自依赖 `arca_store::atomic::Batch` /
//!    `journal::AppendBatch` 做跨整轮 `sync()` 的批量收口（M1d/M2a 修过的
//!    O(n²) 性能问题），[`Transport::commit`]/[`Transport::tombstone`] 是面向
//!    单次 CAS 提交的接口，语义上匹配的是 HTTP 的一次 `PUT`/`DELETE`，不是
//!    "本地批量写入"这个不同的性能场景——这条留给 Task 3/4/5 接线 HTTP 时
//!    重新评估批处理策略，本切片只保证接口本身完整、正确、有测试。
//!
//! # 与 brief 字面签名的两处刻意偏离
//!
//! - **`read_remote` 返回整个路径 → 状态的 map，不是单个路径的状态**。
//!   brief 给的签名是 `fn read_remote(&self, path: &str) -> Result<RemoteState,
//!   TransportError>`，但实际调用方（`sync()` 的调和循环、`status.rs`、
//!   `doctor.rs`）都需要"这一刻 hub 侧全部已知路径的状态"这个整体快照去做
//!   三态对账，逐路径查询要么重复 N 次全量 journal 扫描（本地开销从 O(n) 变
//!   O(n²)），要么在 `Transport` 实现内部另行做一次等价的整体缓存——不如让
//!   接口本身就是这个自然的一致性单位。`PROTOCOL.md` §1.2 的 `GET
//!   .../state` 端点同样是一次性枚举，与这个形状一致。
//! - **没有单独的 `read_remote(path)`**：需要单路径视角时，调用方从整体 map
//!   里 `get(path).cloned().unwrap_or(RemoteState::Absent)` 即可（`sync.rs`
//!   现有代码本就是这样用 `hub::read_remote` 的返回值）。
//!
//! 与 `sync.rs`/`scan.rs`/`journal.rs` 顶部同一条先例：brief 落后于实现时
//! 落地考量的细节，本模块在这里记录偏离之处与理由，不是自行改需求。

pub mod local;

use arca_chunk::hash::ContentHash;
use arca_core::state::RemoteState;
use arca_format::model::{Actor, ItemId, VersionId};
use std::collections::BTreeMap;
use std::fmt;

/// 提交一个新版本所需的全部信息。
///
/// `item_id`/`version_id` 由调用方（`arca-cli` 的调和执行侧，即
/// `arca_core::reconcile` 决策表选中 `Action::Upload` 之后的执行者）预先分配好
/// 再传入——`VersionId` 是时间戳+随机数、**不由内容派生**（`PROTOCOL.md` §1.2
/// 的判断记录：M1b 的教训是 CAS 必须认版本号，不能认 ETag/内容哈希），这个
/// 决定权本就不该下放到传输层；`Transport::commit` 只管"把这次提交落到存储、
/// 用 `parent` 做 CAS 检查"，不替调用方决定该分配哪个身份。
#[derive(Debug, Clone)]
pub struct CommitRequest {
    pub path: String,
    pub item_id: ItemId,
    pub version_id: VersionId,
    /// CAS 的 If-Match 对象：`None` 表示"仅当这个路径此刻完全不存在时创建"
    /// （`arca_core::reconcile::Action::Upload{parent:None}` 的语义）。
    pub parent: Option<VersionId>,
    pub bytes: Vec<u8>,
    /// 内容自身的修改时间（`FORMAT.md` 定义的字段语义），不是提交时刻的墙上
    /// 时钟——`committed_at` 由 `Transport` 实现自己按提交时刻生成，不需要
    /// 调用方传入（与 `sync.rs::execute_upload` 现有做法一致）。
    pub mtime: String,
    pub actor: Actor,
}

/// 提交一个 tombstone 所需的全部信息，字段含义与 [`CommitRequest`] 对应部分相同。
#[derive(Debug, Clone)]
pub struct TombstoneRequest {
    pub path: String,
    pub item_id: ItemId,
    /// CAS 的 If-Match 对象：决策表给出的、这次要终结的远端当前版本
    /// （`Action::TombstoneRemote` 恒为 `Some`，远端已知该 item 才会走到这格）。
    pub parent: VersionId,
    pub actor: Actor,
    pub at: String,
}

/// 一次 `commit`/`tombstone` 的结果。
///
/// **`Conflict` 是协议层的正常结果，不是错误**——`PROTOCOL.md` §7 对
/// `class=protocol` 的定义：走结构化冲突流程，不作为错误处理。M1b 已经在
/// `Decision::into_outcome` 上踩过这个形状问题（把冲突塞进 `Err` 会让一个
/// 冲突文件中止整轮 sweep，见 `arca-core/src/reconcile.rs` 的教训），
/// `CommitOutcome` 因此是 `Ok` 里的一个变体，不是 `TransportError`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// 提交成功。
    Committed {
        item_id: ItemId,
        version_id: VersionId,
    },
    /// CAS 冲突：`parent` 与 hub 此刻记录的当前版本不一致。`actual` 是冲突
    /// 发生时刻 hub 侧对这个路径的真实认知（供调用方决定要不要基于它重新
    /// 走一次调和），与 hub 侧记录同源。
    Conflict {
        expected_parent: Option<VersionId>,
        actual: RemoteState,
    },
    /// 身份校验失败（评审 C1）：客户端声称的 `item_id` 与这次操作实际应
    /// 归属的 item_id 不符——**不是**"版本过期"（那是 `Conflict`，换一个
    /// 正确的 `parent` 重试就能成功）；这是"你打错了身份"，无论重试多少次、
    /// 无论 `parent` 换成什么都不该成功，必须先修正客户端对 `item_id` 的
    /// 认知，因此不能被折叠进 `Conflict`（那会让调用方以为这是可以通过
    /// 重新调和解决的普通冲突）。三种触发场景见 `local.rs::commit`/
    /// `tombstone` 的实现注释：路径已被另一个 item_id 占用、这个 item_id
    /// 已经在别的路径下有归属、这个 item_id 已被 tombstone 终结。
    IdentityMismatch {
        /// 这次操作声明要落在的路径。
        path: String,
        /// 客户端声称的 item_id。
        claimed_item_id: ItemId,
        /// 冲突对象此刻真正的归属 item_id——`None` 表示冲突源不是"另一个
        /// item_id 占着"，而是"这个 item_id 自己已经被 tombstone 终结"。
        actual_item_id: Option<ItemId>,
    },
}

/// 第 4 道闸门要问的：这个 item 的内容此刻是否可取回（附哈希与大小）。
///
/// 返回值带哈希与大小，不只是一个布尔——这样三方核验（基线期望的哈希 = hub
/// 侧记录的哈希 = 此刻现场重算的哈希）在 `file://` 与未来 HTTP 两种传输下
/// 形状一致：`recoverable` 已经完成了"现场重算"这一步，调用方（`gates.rs`）
/// 只需要把返回的哈希与自己持有的期望哈希比对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recoverable {
    pub hash: ContentHash,
    pub size: u64,
}

/// `Transport` 操作失败——彼此可区分（I5），逐个变体对应一类下游原语已经
/// 定义好的失败形状，不重新发明一套错误分类。
#[derive(Debug)]
pub enum TransportError {
    Hub(crate::hub::HubError),
    Trash(crate::trash::TrashError),
    Journal(crate::journal::JournalError),
    Atomic(arca_store::atomic::AtomicError),
    Format(arca_format::error::FormatError),
    /// **评审 I3**：获取 `.arca/locks/arca.lock`（跨进程排他锁，见
    /// `arca_store::lock` 模块文档）本身失败——创建/打开锁文件失败，权限、
    /// 磁盘满等。与"锁被占用"不是同一件事：本实现选的是阻塞式获取
    /// （`arca_store::lock::acquire` 内部调用 `FileExt::lock`），拿不到锁会
    /// 一直等，不会以"忙"为由提前失败；这个变体只覆盖锁本身的 IO 故障。
    Lock(arca_store::lock::LockError),
    /// 常规 IO 故障，不属于以上任何一类已知形状（例如路径逃出存储根、读取
    /// `files/<path>` 本身失败）。
    Io {
        path: String,
        reason: String,
    },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Hub(e) => write!(f, "{e}"),
            TransportError::Trash(e) => write!(f, "{e}"),
            TransportError::Journal(e) => write!(f, "{e}"),
            TransportError::Atomic(e) => write!(f, "{e}"),
            TransportError::Format(e) => write!(f, "{e}"),
            TransportError::Lock(e) => write!(f, "{e}"),
            TransportError::Io { path, reason } => write!(f, "{path}：{reason}"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransportError::Hub(e) => Some(e),
            TransportError::Trash(e) => Some(e),
            TransportError::Journal(e) => Some(e),
            TransportError::Atomic(e) => Some(e),
            TransportError::Format(e) => Some(e),
            TransportError::Lock(e) => Some(e),
            TransportError::Io { .. } => None,
        }
    }
}

/// 客户端看 hub 的唯一接口——`file://` 与 `http://` 两种传输方式的共同抽象。
///
/// 刻意保持**同步**（不是 `async fn`）：`arca-cli` 是一次性进程（spec §3.1
/// 「客户端零常驻」），CLI 不得引入 tokio 之类的异步运行时（M2b Global
/// Constraints）；未来的 HTTP 实现用阻塞客户端（`reqwest` 的 blocking
/// feature 或 `ureq`），这条约束现在就要在 trait 形状里钉死，否则后续任务
/// 会被迫回头改整条调用链的签名。
pub trait Transport {
    /// 读出 hub 侧此刻已知的全部路径状态（含 tombstone 判定）——一次性快照，
    /// 未出现在返回 map 里的路径按 `RemoteState::Absent` 处理（与
    /// `hub::read_remote` 现有语义一致，见模块顶部「与 brief 字面签名的
    /// 两处刻意偏离」一节）。
    fn read_remote(&self) -> Result<BTreeMap<String, RemoteState>, TransportError>;

    /// 枚举 hub 侧全部已知路径（`status`/`verify` 用）——比 [`Transport::read_remote`]
    /// 更轻量的视角，调用方不需要每个路径的完整状态时用它。
    fn list(&self) -> Result<Vec<String>, TransportError>;

    /// 取一个路径当前版本的内容字节。
    fn read_content(&self, path: &str) -> Result<Vec<u8>, TransportError>;

    /// 提交新版本（CAS：`req.parent` 与 hub 侧当前版本不一致即 [`CommitOutcome::Conflict`]，
    /// 不是 `Err`）。
    fn commit(&self, req: &CommitRequest) -> Result<CommitOutcome, TransportError>;

    /// 提交 tombstone（同样是 CAS，冲突形状与 [`Transport::commit`] 一致）。
    fn tombstone(&self, req: &TombstoneRequest) -> Result<CommitOutcome, TransportError>;

    /// 第 4 道闸门要问的：`item_id` 对应、内容哈希等于 `expected_hash` 的版本
    /// 此刻是否可从 hub 的回收站取回。`None` 表示不可取回（要么没有这个
    /// item 的记录，要么记录都在，但没有一条候选的现场哈希与 `expected_hash`
    /// 一致——见 [`local::LocalTransport`] 的三方核验实现）。
    fn recoverable(
        &self,
        item_id: ItemId,
        expected_hash: ContentHash,
    ) -> Result<Option<Recoverable>, TransportError>;
}
