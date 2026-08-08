//! `Transport`：客户端看 hub 的唯一接口（M2b Task 1，见
//! `docs/superpowers/plans/2026-08-08-m2b-arcad-cas.md`「为什么先抽传输，再写
//! 服务端」一节）。
//!
//! # M2c Task 1：补齐四条缺口
//!
//! M2b 切片评审在「Readiness for M2c/M2e」里点名了四条这个 trait 当时够不着
//! 的能力（`docs/superpowers/plans/2026-08-08-m2c-journal-longpoll.md`「为什么
//! 第一个任务是补 trait 而不是写 HTTP」一节）：
//!
//! 1. **流式读**：[`Transport::read_content_into`]——`read_content` 强制调用方
//!    整份内容驻留内存，这是服务端 C2（600MB PUT 让 RSS 涨到 1.86GB）的镜像，
//!    只是发生在客户端一侧；两端要一起修，否则 M2e 的 HTTP 客户端会继承同样
//!    的内存曲线。
//! 2. **Range/续传**：[`Transport::read_range`]——服务端的 206 已经能用且经
//!    评审验证（`arcad/src/api.rs::get_file`），只是这个 trait 够不着，
//!    `http::HttpTransport`（Task 5）没有对应方法可调。
//! 3. **按哈希寻址的读**：[`Transport::read_by_hash`]——`arca cat <hash>`
//!    （`PROTOCOL.md` §5.0b）没有 HTTP 对应，服务端补
//!    `GET /v1/datasets/{id}/blobs/{hash}`（`PROTOCOL.md` §1.2，先写协议
//!    再实现，I10）。
//! 4. **批量提交**：[`Transport::commit_batch`]——`sync.rs` 本地已经用
//!    `arca_store::atomic::Batch` 把内容写入的目录 fsync 收口到一次，但每个
//!    文件仍是一次独立的 `Transport::commit` 调用；HTTP 场景下这意味着 1 万
//!    文件的 sweep 是 1 万次网络往返。批量提交**要么整批成功要么整批不
//!    生效**——不做"部分成功"，那会让调用方无法判断该从哪里重试（I5）；
//!    CAS 仍逐条校验，任一条 `parent` 过期即整批失败，明确指出是哪一条
//!    （[`BatchOutcome::Rejected`]）。
//!
//! 这四条都是接口扩展，不是行为变更：现有依赖 [`Transport::commit`]/
//! [`Transport::read_content`] 等既有方法的调用点与测试一行不改（判据见
//! Task 1 brief），新方法是纯粹的加法。
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

pub mod http;
pub mod local;

use arca_chunk::hash::ContentHash;
use arca_core::state::RemoteState;
use arca_format::model::{Actor, ItemId, VersionId};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;

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

/// 提交一次改名所需的全部信息——**身份不动、路径映射搬家**（I7，spec §3/§5.3：
/// 「路径是索引键，不是身份」）。
///
/// # 为什么需要这第三个写入原语，不能靠 `commit`/`tombstone` 拼出来
///
/// M2c Task 5 落地两机端到端时发现：`commit`/`tombstone` 现有的 C1 身份校验
/// （评审 C1，`local.rs::validate_commit`）刻意让"一个 item_id 只能同时归属
/// 一个路径"与"被 tombstone 的 item_id 永不可复用"这两条规则**不可绕过**——
/// 这是防住"伪造身份接管"攻击的正确设计，但也意味着 `tombstone(旧路径)` +
/// `commit(新路径, 同一 item_id)` 这种"拼出改名"的组合会被第二步的身份校验
/// 直接拒绝（旧路径的 tombstone 已经永久终结这个 item_id）。改名因此必须是
/// 自己的原语：**不产生新版本**（内容没变，`items/<item_id>.jsonl` 链不动），
/// 只搬 `index/` 的路径→item_id 映射，同时在 journal 追加一条 `op=rename`
/// 事件（`FORMAT.md` §7.2 早已定义这个操作码与 `from` 字段，只是此前从未被
/// 写入端触发——与 M2c Task 1 「`commit` 从未写 `Op::Upsert`」是同一类型的
/// 落地缺口）。
///
/// `arca-core` 的决策表（`reconcile::decide`）本身不产生"改名"这个动作——
/// 三态调和是逐路径独立判断的（spec 设计如此，`arca-core` 这次没有改一行）；
/// 改名的**检测**（同一次 `sync` 里，一个路径消失、另一个路径以相同内容
/// 出现）在 `arca-cli::sync` 里用内容哈希匹配完成，检测到之后才调用这个
/// 原语，不经过 `decide()` 的逐路径决策表。
#[derive(Debug, Clone)]
pub struct RenameRequest {
    pub old_path: String,
    pub new_path: String,
    pub item_id: ItemId,
    /// CAS 的 If-Match 对象：`old_path` 此刻的版本（内容不变，这个版本号
    /// 在改名成功后原样延续到 `new_path`）。
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

/// 一次 [`Transport::commit_batch`] 的结果——**要么整批成功要么整批不生效**
/// （M2c Task 1 brief：不做"部分成功"，那会让调用方无法判断该从哪里重试，
/// 与 I5 相悖）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    /// 全部提交成功，与 `reqs` 输入顺序一一对应，每项都是
    /// `CommitOutcome::Committed`（不是 `Committed` 变体本身以省一层
    /// 匹配——调用方通常直接要 `(item_id, version_id)`）。
    Committed(Vec<(ItemId, VersionId)>),
    /// 批次中第 `index` 项（0-based）校验未通过——**整批未写入任何内容**，
    /// 不只是这一条。`outcome` 是 [`CommitOutcome::Conflict`] 或
    /// [`CommitOutcome::IdentityMismatch`]（批量场景下不会是 `Committed`）。
    Rejected {
        index: usize,
        outcome: CommitOutcome,
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
    /// **HTTP 传输特有**（M2c Task 5）：连不上/DNS 解析失败/连接被重置/
    /// 单次请求超时——网络层面的瞬时故障，与下面的 `Offline`/`Protocol`
    /// 有本质区别：这类失败**与数据集本身的状态无关**，纯粹是"这次没连上"，
    /// 退避重试往往就好了（`class()` 为 `Retryable`）。`file://` 传输没有
    /// 这个变体的对应物——本地文件系统调用不会"连不上"。
    Network {
        reason: String,
    },
    /// **HTTP 传输特有**：服务端返回 `503`（`mount.absent`/
    /// `mount.identity_mismatch`，`PROTOCOL.md` §1.2「503：数据集离线」）——
    /// 数据集离线，不是"这个请求恰好失败了"。**I11**：客户端要如实把它
    /// 翻译成"离线"，不能当成可重试的网络抖动，也不能当成"这个数据集是
    /// 空的"（`class()` 为 `NeedsHuman`：需要人去检查存储根挂载状态，
    /// 重试不会让它自己恢复）。
    Offline {
        message: String,
    },
    /// **HTTP 传输特有**：服务端返回了一个协议表未覆盖、或响应体形状解析
    /// 不出来的结果——不是网络故障（连上了、也收到了响应），也不是已知的
    /// 结构化冲突/身份错误，是"这次交互不符合协议契约"。`class()` 为
    /// `Bug`：要么是客户端拼错了请求，要么是客户端/服务端协议版本不一致，
    /// 都不是"退避重试"或"等人工检查存储根"能解决的，需要有人看代码
    /// （I5：绝不猜测该怎么继续）。
    Protocol {
        message: String,
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
            TransportError::Network { reason } => write!(f, "网络故障：{reason}"),
            TransportError::Offline { message } => write!(f, "数据集离线：{message}"),
            TransportError::Protocol { message } => write!(f, "协议错误：{message}"),
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
            TransportError::Network { .. } => None,
            TransportError::Offline { .. } => None,
            TransportError::Protocol { .. } => None,
        }
    }
}

impl TransportError {
    /// 该按哪种策略处置——`PROTOCOL.md` §7 定义的四类之一
    /// （[`arca_format::trace::ErrorClass`]）。M2c Task 5 brief 原话：
    /// "网络故障按 ErrorClass 分类：连不上/超时是 retryable，协议错误
    /// （412/409）走结构化流程不是错误，4xx 的参数错误是 bug"——412/409
    /// 已经在 `CommitOutcome::Conflict`/`IdentityMismatch` 里表达（这两者
    /// 是 `Ok` 的变体，不经过这个方法，见 `CommitOutcome` 文档），这里只
    /// 分类真正落进 `Err` 的几类。
    ///
    /// 本地存储层的失败（`Hub`/`Trash`/`Journal`/`Atomic`/`Format`/`Lock`/
    /// `Io`）统一归 `NeedsHuman`：这些都表示存储根本身或其可访问性出了
    /// 问题（损坏、权限、磁盘满等），与 `arcad::api::store_corrupt`
    /// （`class=needs_human`，`PROTOCOL.md` §7）同一处置纪律——退避重试
    /// 不会让损坏的文件自己变好。
    pub fn class(&self) -> arca_format::trace::ErrorClass {
        use arca_format::trace::ErrorClass;
        match self {
            TransportError::Network { .. } => ErrorClass::Retryable,
            TransportError::Offline { .. } => ErrorClass::NeedsHuman,
            TransportError::Protocol { .. } => ErrorClass::Bug,
            TransportError::Hub(_)
            | TransportError::Trash(_)
            | TransportError::Journal(_)
            | TransportError::Atomic(_)
            | TransportError::Format(_)
            | TransportError::Lock(_)
            | TransportError::Io { .. } => ErrorClass::NeedsHuman,
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

    /// 把一个路径当前版本的内容流式写进 `out`——缺口第 1 条（模块顶部
    /// 「M2c Task 1」一节）：调用方不需要先把整份内容攒成 `Vec<u8>` 再处理，
    /// `local.rs` 用有界缓冲拷贝实现（`std::io::copy`，固定大小的栈上缓冲，
    /// 不随文件大小增长）；`http.rs`（Task 5）会用流式响应体实现，两者的
    /// 内存占用都不与文件体积成正比。返回写出的字节数，供调用方（如
    /// `arca cat`）在不预先知道内容长度时也能报告"读了多少"。
    fn read_content_into(&self, path: &str, out: &mut dyn Write) -> Result<u64, TransportError>;

    /// 取一个路径当前版本内容的字节区间 `[start, start+len)`——缺口第 2 条：
    /// 服务端的 206 续传（`arcad/src/api.rs::get_file` 的 Range 处理）已经
    /// 实现并经评审验证，只是这个 trait 此前够不着，`http::HttpTransport`
    /// 没有方法可以表达"我只要这一段"。`local.rs` 用 `seek` + 有界读实现，
    /// 与服务端 `bounded_read` 同一手法：只分配这一段区间大小的内存，不管
    /// 文件本身多大。`start`/`len` 越界（区间超出内容实际大小）是调用方的
    /// 参数错误，映射为 [`TransportError::Io`]（与 `read_content` 对"文件
    /// 不存在"的处置同一严重性，不是这个 trait 的新错误分类）。
    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>, TransportError>;

    /// 按内容哈希取字节——缺口第 3 条：`arca cat <hash>`（`PROTOCOL.md`
    /// §5.0b）的传输层原语。多个路径共享同一份内容时（去重命中）按路径
    /// UTF-8 字节序取第一个命中，结果确定——与
    /// `commands/plumbing.rs::cat_cmd` 现有算法同一条纪律，这里只是把它从
    /// "直接摸 `StorageRoot`"的命令实现里提炼成传输层方法，供
    /// `arcad::api::get_blob`（`GET .../blobs/{hash}`）与未来的 HTTP
    /// `cat` 实现共用。查无匹配内容时返回 `None`，不是 `Err`——"没有这个
    /// 哈希"是完全正常的查询结果，不是传输层故障。
    fn read_by_hash(&self, hash: ContentHash) -> Result<Option<Vec<u8>>, TransportError>;

    /// 提交新版本（CAS：`req.parent` 与 hub 侧当前版本不一致即 [`CommitOutcome::Conflict`]，
    /// 不是 `Err`）。
    fn commit(&self, req: &CommitRequest) -> Result<CommitOutcome, TransportError>;

    /// 批量提交多个版本——缺口第 4 条，见模块顶部「M2c Task 1」一节与
    /// [`BatchOutcome`] 的文档：一次调用只有一次 CAS 临界区（与
    /// [`Transport::commit`] 逐条各自加锁不同），要么全部生效要么全部不生效。
    /// 空切片是合法输入，返回 `Ok(BatchOutcome::Committed(vec![]))`，不是错误。
    fn commit_batch(&self, reqs: &[CommitRequest]) -> Result<BatchOutcome, TransportError>;

    /// 提交 tombstone（同样是 CAS，冲突形状与 [`Transport::commit`] 一致）。
    fn tombstone(&self, req: &TombstoneRequest) -> Result<CommitOutcome, TransportError>;

    /// 提交一次改名（同样是 CAS；见 [`RenameRequest`] 文档「为什么需要这第三个
    /// 写入原语」）：成功时 `CommitOutcome::Committed` 携带的 `version_id` 与
    /// `req.parent` 相同（不产生新版本）。`new_path` 此刻已被别的 item_id 占用，
    /// 或 `old_path` 的归属/版本与声明不符，都走 `CommitOutcome::Conflict`/
    /// `IdentityMismatch`，与 `commit`/`tombstone` 同一套冲突形状，不新增变体。
    fn rename(&self, req: &RenameRequest) -> Result<CommitOutcome, TransportError>;

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
