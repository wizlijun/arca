//! [`LocalTransport`]：[`super::Transport`] 的 `file://` 实现（M2b Task 1）。
//!
//! 不重新实现落盘细节——每个方法都复用 `hub::read_remote`/`trash::*`/
//! `journal::*` 已经交付、已被各自模块的测试覆盖的原语，本模块只负责"按
//! `Transport` 的形状把它们组织起来，并补上 CAS 检查"（[`Transport::read_remote`]/
//! [`Transport::list`]/[`Transport::read_content`] 之前完全没有 CAS 语义可言：
//! 现有的 `sync.rs::execute_upload` 从不校验 `parent` 是否仍是 hub 侧当前版本，
//! 因为决策表在调和时刻已经决定了要不要 `Upload`，执行侧对此来者不拒——这个
//! 缺口本切片不动它（`execute_upload` 保持原样，见 `transport/mod.rs` 顶部
//! 「本切片的落地范围」一节），但 [`Transport::commit`]/[`Transport::tombstone`]
//! 作为面向未来 HTTP 语义设计的新接口，值得从一开始就做对：一次 `PUT`/
//! `DELETE` 就应该在服务端真正校验 If-Match。

use super::{
    BatchOutcome, CommitOutcome, CommitRequest, Recoverable, RenameRequest, TombstoneRequest,
    Transport, TransportError,
};
use crate::{hub, journal, trash};
use arca_chunk::hash::ContentHash;
use arca_core::state::RemoteState;
use arca_format::hub_layout::layout;
use arca_format::index::IndexRecord;
use arca_format::items;
use arca_format::journal::{JournalEvent, Op};
use arca_format::model::{Actor, ItemId, Version, VersionId};
use arca_format::path_rules;
use arca_store::atomic;
use arca_store::root::StorageRoot;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};

/// `file://` 传输：包一个已打开、身份已确认的存储根。
///
/// `trash_cache`：`.arca/trash/` 的一次性快照，在第一次调用 [`Transport::recoverable`]
/// 时惰性读入，同一个 `LocalTransport` 实例内的后续调用复用它——与
/// `sync.rs::sync` 现有的"循环开始前只读一次 `trash::list`"是同一条纪律
/// （评审 Important #3：避免 O(n·m) 的重复目录遍历），只是把"读一次、缓存"
/// 的责任从调用方（`sync()`）搬进了 `Transport` 实现内部，调用方不再需要
/// 自己操心这件事。一个 `LocalTransport` 只应在一次调和（一次 `sync()` 调用）
/// 的生命周期内使用——与旧代码"每次 `sync()` 重新拍一次快照"的假设一致。
pub struct LocalTransport<'a> {
    root: &'a StorageRoot,
    trash_cache: RefCell<Option<Vec<trash::TrashEntry>>>,
}

impl<'a> LocalTransport<'a> {
    pub fn new(root: &'a StorageRoot) -> Self {
        Self {
            root,
            trash_cache: RefCell::new(None),
        }
    }

    /// 惰性加载并返回 `.arca/trash/` 快照的一份克隆——`RefCell` 挡在中间是
    /// 因为 `Transport::recoverable` 的签名是 `&self`（trait 要求所有方法
    /// 只读借用，不强加"调用方必须拿 `&mut`"的约束，未来 HTTP 实现里也没有
    /// "内部状态"这回事），只能靠内部可变性做惰性缓存。返回克隆而不是借用：
    /// 条目数量在真实数据集里不会大到克隆成为瓶颈，换来的是不需要处理
    /// `Ref` 的生命周期与 trait 方法签名（`Result<Option<Recoverable>, _>`，
    /// 不是一个借用）之间的冲突。
    fn trash_snapshot(&self) -> Result<Vec<trash::TrashEntry>, TransportError> {
        if let Some(cached) = self.trash_cache.borrow().as_ref() {
            return Ok(cached.clone());
        }
        let entries = trash::list(self.root).map_err(TransportError::Trash)?;
        *self.trash_cache.borrow_mut() = Some(entries.clone());
        Ok(entries)
    }

    /// C2 修复：流式提交——调用方（`arcad` 的 `PUT` 处理器）已经把请求体
    /// 边到达边写进一个 [`atomic::TmpWriter`]、边写边算好了哈希，不再把
    /// 整份内容缓冲进内存（评审实测：改造前一次 600MB 的 `PUT` 会让 RSS
    /// 从 6MB 涨到 1.86GB）。本方法接手"内容已经落在 tmp、身份/CAS 校验
    /// 通过之后把它过户成正式版本"这一半，与 [`Transport::commit`]
    /// （`bytes: Vec<u8>` 版，供 `file://` 同步使用）共享同一套身份/CAS
    /// 判断（[`validate_commit`]）——两条路径的判断标准必须是同一份代码，
    /// 不是分别维护的两份，否则迟早会悄悄分叉出不一致的安全边界。
    ///
    /// 不是 [`Transport`] trait 的一部分：`Transport::commit` 的签名要求
    /// 整份 `bytes`，这是专门为"请求体不能整份缓冲进内存"这一个调用点
    /// （HTTP `PUT`）开的口子——`Transport` trait 本身要不要长出一个通用的
    /// 流式写入方法留给 M2c/M2e（见 `transport/mod.rs` 顶部记录的接口
    /// 缺口），这里不越界改 trait 形状。
    ///
    /// 身份/CAS 校验不通过时会调用 `writer.abandon()` 清理 tmp 文件——
    /// 调用方交出 `writer` 之后就不必、也不应该再管它的生命周期，无论
    /// 结果是 `Committed`、`Conflict` 还是 `IdentityMismatch`。
    #[allow(clippy::too_many_arguments)]
    pub fn commit_streamed(
        &self,
        path: &str,
        item_id: ItemId,
        version_id: VersionId,
        parent: Option<VersionId>,
        writer: atomic::TmpWriter,
        hash: ContentHash,
        size: u64,
        mtime: String,
        actor: Actor,
    ) -> Result<CommitOutcome, TransportError> {
        // 评审 I3：跨进程排他——见 `arca_store::lock` 模块文档。持有到函数
        // 返回为止（含写入），涵盖整段"读当前状态 → CAS 校验 → 写入"临界区。
        let _lock = arca_store::lock::acquire(self.root).map_err(TransportError::Lock)?;
        let remote = self.read_remote()?;
        let item_last_version = match validate_commit(self.root, &remote, path, item_id, &parent)? {
            Ok(v) => v,
            Err(outcome) => {
                writer.abandon();
                return Ok(outcome);
            }
        };

        let target = format!("{}/{}", layout::FILES_DIR, path);
        writer
            .finish(self.root, &target)
            .map_err(TransportError::Atomic)?;

        let version = Version {
            version_id: version_id.clone(),
            item_id,
            parent: item_last_version,
            hash,
            size,
            mtime,
            actor,
            committed_at: crate::clock::now_rfc3339(),
        };
        append_item_version(self.root, &version)?;
        write_index_record(self.root, path, item_id)?;
        append_upsert_journal_event(self.root, &version, path)?;

        Ok(CommitOutcome::Committed {
            item_id,
            version_id,
        })
    }
}

/// C1 身份/CAS 校验的共用核心——[`LocalTransport::commit`]（`bytes` 版）与
/// [`LocalTransport::commit_streamed`]（C2 流式版，供 `arcad` 使用）共享
/// 同一套判断，不各自实现一遍：两条路径独立演化、判断标准悄悄分叉，是
/// 比"多传几个参数"更危险的维护负担（那正是能让 C1 这类漏洞重新长出来
/// 的土壤）。
///
/// - `Ok(Ok(item_last_version))`：全部校验通过——`item_last_version` 是
///   这个 item 自己版本链的链尾（C1 修法：新记录的 `parent` 从这里推导，
///   不是直接信调用方传入的 `parent` 声明）。
/// - `Ok(Err(outcome))`：`IdentityMismatch` 或 `Conflict`——调用方原样把
///   它当作最终结果返回，不再继续任何写入。
/// - `Err(_)`：读取过程本身失败（真损坏：journal/items 解析不了等），
///   向上传播（I5：绝不吞掉真损坏去猜一个校验结果）。
fn validate_commit(
    root: &StorageRoot,
    remote: &BTreeMap<String, RemoteState>,
    path: &str,
    item_id: ItemId,
    parent: &Option<VersionId>,
) -> Result<Result<Option<VersionId>, CommitOutcome>, TransportError> {
    let current = remote.get(path).cloned().unwrap_or(RemoteState::Absent);

    // 校验 0：这个 item_id 一旦被 tombstone 就此终结（spec §4.1「删除后
    // 重建 = 新身份」），不允许任何后续提交复用它——不论这次声称的是
    // "创建"（parent:None）还是"推进"（If-Match 带一个看起来合法的旧
    // version_id，那正是它自己被终结前的最后一个版本，攻击者从
    // GET /state 就能读到）。items/<item_id>.jsonl 的链本身不会因为
    // tombstone 而改变（FORMAT.md §7.2 tombstone 不产生新版本），单靠
    // "链尾对不对得上 parent" 分辨不出这一种，必须直接问 journal。
    if hub::item_is_tombstoned(root, item_id).map_err(TransportError::Hub)? {
        return Ok(Err(CommitOutcome::IdentityMismatch {
            path: path.to_string(),
            claimed_item_id: item_id,
            actual_item_id: None,
        }));
    }

    // 校验 1：这个路径此刻若真的**活着**（`Present`）被别的 item 占用，
    // 调用方声称的 item_id 必须与它一致——HTTP 是不可信输入的入口，
    // `Arca-Item-Id` 此前只经过语法解析，从未核对它是不是真的拥有这个
    // 路径。**只看 `Present`，不看 `Tombstoned`**（评审 C2 实机复现修复：
    // 路由 `sync()` 的 `Upload` 改走 `commit_batch` 后，「删除后原地重建
    // 为全新身份」——spec §4.1 明文预期、`sync.rs::prepare_upload` 在
    // `parent:None` 时就是这么做的——会被这里挡下来，因为旧实现把
    // `Tombstoned` 也算作"有归属"，导致一个从未被使用过的全新 item_id
    // 在一个已经被删除的路径上创建，被误判成"身份不符"）。`Tombstoned`
    // 状态下真正需要挡住的攻击是"复用那个已经终结的旧 item_id 本身"，
    // 校验 0（`hub::item_is_tombstoned`，判断的是调用方声称的 `item_id`
    // 自己，与路径无关）已经完整覆盖了这一种，不需要校验 1 再重复挡一遍
    // ——两条校验挡的是不同维度："这个身份还能不能用"（校验 0）与"这个
    // 路径此刻活着占着的是不是这个身份"（校验 1，只对活着的占用有意义）。
    if let RemoteState::Present { item_id: owner, .. } = &current {
        if *owner != item_id {
            return Ok(Err(CommitOutcome::IdentityMismatch {
                path: path.to_string(),
                claimed_item_id: item_id,
                actual_item_id: Some(*owner),
            }));
        }
    }

    // 校验 2：这个 item_id 不能在别的路径下已经有归属——否则两个不同路径
    // 各自声明同一个全新 item_id，会把两条互不相干的"首版本"追加进同一个
    // items/<item_id>.jsonl，那个文件的链从此断裂，之后任何触碰它的请求
    // 都会失败（评审 C1 利用 1，数据集级 DoS）。
    if let Some(other_path) = remote
        .iter()
        .find_map(|(p, s)| (p != path && owner_item_id(s) == Some(item_id)).then(|| p.clone()))
    {
        return Ok(Err(CommitOutcome::IdentityMismatch {
            path: other_path,
            claimed_item_id: item_id,
            actual_item_id: Some(item_id),
        }));
    }

    // 校验 3：真正的 CAS 比较对象是这个 item 自己的版本链尾，不是路径视角
    // 的"当前版本"——两者在校验 1/2 都通过之后理应相等，这里直接问 item
    // 自己的历史是纵深防御，也是"parent 从 item 自己的最后版本推导"这条
    // 修法的落点。
    let item_last_version = read_item_last_version(root, item_id)?.map(|v| v.version_id);
    if &item_last_version != parent {
        return Ok(Err(CommitOutcome::Conflict {
            expected_parent: parent.clone(),
            actual: current,
        }));
    }

    Ok(Ok(item_last_version))
}

/// 一个 `RemoteState` 此刻的归属 item_id——`Present` 与 `Tombstoned` 都算
/// "有归属"。**只用于 [`validate_commit`] 校验 2**（"这个 item_id 不能在
/// 别的路径下已经有归属"）：那条校验挡的是"同一个全新 item_id 被两个不同
/// 路径各自声明"，`Tombstoned` 状态下这个 item_id 早就在校验 0 被挡住过
/// 一次（它自己已经终结，不可能是"全新"的），这里把 `Tombstoned` 也算作
/// "有归属"不会改变结果，只是不必再假设"一定不会撞上"。**不要用于校验 1**
/// （评审 C2 实机复现修复）：校验 1 判断的是"这个路径此刻是否被别的身份
/// **活着**占用"，`Tombstoned` 不是"活着占用"——见 [`validate_commit`]
/// 校验 1 的完整论证。
fn owner_item_id(state: &RemoteState) -> Option<ItemId> {
    match state {
        RemoteState::Present { item_id, .. } => Some(*item_id),
        RemoteState::Tombstoned { item_id, .. } => Some(*item_id),
        RemoteState::Absent => None,
    }
}

/// 读某个 item_id 自己的版本链，取最后一条——C1 修法「parent 从 item 自己
/// 的最后版本推导，而不是从路径的当前版本」的读取端：`commit`/`tombstone`
/// 用它做真正的 CAS 比较对象，不再只信路径视角的"当前版本"（那正是 C1
/// 的漏洞根源：路径视角与 item 视角在正常情况下一致，但对不可信输入
/// 而言，两者可以被构造成不一致）。
///
/// 链文件不存在 → `Ok(None)`（item 从未被创建过，是"待创建"的合法状态，
/// 不是错误）；链存在但解析失败 → 与 `hub::HubError::CorruptItems` 同一
/// 严重性，包成 `TransportError::Format` 向上传播（I5：绝不吞掉真损坏）。
fn read_item_last_version(
    root: &StorageRoot,
    item_id: ItemId,
) -> Result<Option<Version>, TransportError> {
    let rel = layout::item_path(&item_id);
    let full = root.path().join(&rel);
    let text = match fs::read_to_string(&full) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(TransportError::Io {
                path: full.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    let chain = items::parse_chain(&text).map_err(TransportError::Format)?;
    Ok(chain.into_iter().next_back())
}

impl Transport for LocalTransport<'_> {
    fn read_remote(&self) -> Result<BTreeMap<String, RemoteState>, TransportError> {
        hub::read_remote(self.root).map_err(TransportError::Hub)
    }

    fn list(&self) -> Result<Vec<String>, TransportError> {
        Ok(self.read_remote()?.into_keys().collect())
    }

    fn read_content(&self, path: &str) -> Result<Vec<u8>, TransportError> {
        let full = self
            .root
            .join(&format!("{}/{}", layout::FILES_DIR, path))
            .map_err(|e| TransportError::Io {
                path: path.to_string(),
                reason: e.to_string(),
            })?;
        fs::read(&full).map_err(|e| TransportError::Io {
            path: full.display().to_string(),
            reason: e.to_string(),
        })
    }

    fn read_content_into(&self, path: &str, out: &mut dyn Write) -> Result<u64, TransportError> {
        let full = self
            .root
            .join(&format!("{}/{}", layout::FILES_DIR, path))
            .map_err(|e| TransportError::Io {
                path: path.to_string(),
                reason: e.to_string(),
            })?;
        let mut file = fs::File::open(&full).map_err(|e| TransportError::Io {
            path: full.display().to_string(),
            reason: e.to_string(),
        })?;
        // `io::copy` 用固定大小（8 KiB）的栈上缓冲往返搬运，不整份读入内存
        // ——与 `read_content` 的关键区别，见 `Transport::read_content_into`
        // 的文档（服务端 C2 的镜像修复）。
        io::copy(&mut file, out).map_err(|e| TransportError::Io {
            path: full.display().to_string(),
            reason: e.to_string(),
        })
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>, TransportError> {
        let full = self
            .root
            .join(&format!("{}/{}", layout::FILES_DIR, path))
            .map_err(|e| TransportError::Io {
                path: path.to_string(),
                reason: e.to_string(),
            })?;
        let mut file = fs::File::open(&full).map_err(|e| TransportError::Io {
            path: full.display().to_string(),
            reason: e.to_string(),
        })?;
        file.seek(SeekFrom::Start(start))
            .map_err(|e| TransportError::Io {
                path: full.display().to_string(),
                reason: e.to_string(),
            })?;
        // 只分配这一段区间大小的内存——与服务端 `bounded_read`
        // （`arcad/src/api.rs`）同一手法，不管文件本身多大。
        let mut buf = vec![0u8; len as usize];
        file.read_exact(&mut buf).map_err(|e| TransportError::Io {
            path: full.display().to_string(),
            reason: format!("读取区间 [{start}, {}) 失败：{e}", start + len),
        })?;
        Ok(buf)
    }

    fn read_by_hash(&self, hash: ContentHash) -> Result<Option<Vec<u8>>, TransportError> {
        let remote = self.read_remote()?;
        // `BTreeMap` 按路径 UTF-8 字节序迭代——多个路径共享同一份内容时，
        // 取第一个命中即结果确定（与 `cat_cmd` 现有算法同一条纪律）。
        let Some(hit_path) = remote.iter().find_map(|(p, s)| match s {
            RemoteState::Present { hash: h, .. } if *h == hash => Some(p.clone()),
            _ => None,
        }) else {
            return Ok(None);
        };
        self.read_content(&hit_path).map(Some)
    }

    fn commit(&self, req: &CommitRequest) -> Result<CommitOutcome, TransportError> {
        // 评审 I3：跨进程排他——见 `arca_store::lock` 模块文档、
        // `commit_streamed` 同一处注释。
        let _lock = arca_store::lock::acquire(self.root).map_err(TransportError::Lock)?;
        let remote = self.read_remote()?;
        let item_last_version =
            match validate_commit(self.root, &remote, &req.path, req.item_id, &req.parent)? {
                Ok(v) => v,
                Err(outcome) => return Ok(outcome),
            };

        let version = Version {
            version_id: req.version_id.clone(),
            item_id: req.item_id,
            parent: item_last_version,
            hash: ContentHash::from_bytes(&req.bytes),
            size: req.bytes.len() as u64,
            mtime: req.mtime.clone(),
            actor: req.actor.clone(),
            committed_at: crate::clock::now_rfc3339(),
        };

        // 写入顺序：files/ → items/ → index/（内容先于指针发布——
        // `sync.rs::execute_upload` 与本函数同一条纪律，见其文档）。
        let target = format!("{}/{}", layout::FILES_DIR, req.path);
        atomic::write(self.root, &target, &req.bytes).map_err(TransportError::Atomic)?;
        append_item_version(self.root, &version)?;
        write_index_record(self.root, &req.path, req.item_id)?;
        append_upsert_journal_event(self.root, &version, &req.path)?;

        Ok(CommitOutcome::Committed {
            item_id: req.item_id,
            version_id: req.version_id.clone(),
        })
    }

    fn commit_batch(&self, reqs: &[CommitRequest]) -> Result<BatchOutcome, TransportError> {
        if reqs.is_empty() {
            return Ok(BatchOutcome::Committed(Vec::new()));
        }
        // 评审 I3 同一条纪律：整批只在一次临界区内完成"读当前状态 → 校验
        // 全部 → 写入全部"，不是逐条各自加锁——这正是"整批成功要么整批
        // 不生效"在并发层面的对应：批次执行期间不会有另一个 commit/tombstone
        // 插进来篡改校验依据的快照。
        let _lock = arca_store::lock::acquire(self.root).map_err(TransportError::Lock)?;
        let remote = self.read_remote()?;

        // 预先算好"哪些 item_id 已被 tombstone 终结"，只读一次 journal——
        // 不对每条请求各自调用 `hub::item_is_tombstoned`（那会重复扫描整段
        // journal，是 `journal::append`/`gates.rs` 已经修过的 O(n·m) 同一
        // 形状在批量提交上的重演，评审 I3 先例）。tombstone 是终局状态
        // （一旦发生，`validate_commit`/本函数都拒绝任何后续提交复用同一
        // item_id），"这个 item_id 在 journal 里出现过 Tombstone 事件"与
        // "这个 item_id 的最后一条事件是 Tombstone"因此等价，不需要
        // `hub::tombstoned_by_item` 那样额外保留"最后一条事件"的开销。
        let (_cursor, journal_events) =
            journal::read_all(self.root).map_err(TransportError::Journal)?;
        let tombstoned: BTreeSet<ItemId> = journal_events
            .iter()
            .filter(|e| e.op == Op::Tombstone)
            .map(|e| e.item_id)
            .collect();

        // working 状态：批次内先前条目一旦通过校验，就模拟"已经生效"更新
        // 这两份状态，让批次内的连续版本（同一 item_id 在同一路径上先创建
        // 后推进）也能正确校验，不只是互不相干的并列路径——`working_remote`
        // 同时也是校验失败时 `Conflict.actual` 的数据来源，反映批次内先前
        // 条目真实会造成的状态，不是过时的批前快照。
        let mut working_remote: BTreeMap<String, RemoteState> = remote.clone();
        let mut working_last_version: BTreeMap<ItemId, Option<VersionId>> = BTreeMap::new();

        let mut prepared: Vec<(&CommitRequest, Option<VersionId>)> = Vec::with_capacity(reqs.len());
        for (index, req) in reqs.iter().enumerate() {
            if tombstoned.contains(&req.item_id) {
                return Ok(BatchOutcome::Rejected {
                    index,
                    outcome: CommitOutcome::IdentityMismatch {
                        path: req.path.clone(),
                        claimed_item_id: req.item_id,
                        actual_item_id: None,
                    },
                });
            }

            let current = working_remote
                .get(&req.path)
                .cloned()
                .unwrap_or(RemoteState::Absent);
            // 评审 C2 实机复现修复：与 `validate_commit` 校验 1 同一条纪律——
            // 只有路径此刻**活着**（`Present`）被别的身份占用才算冲突，
            // `Tombstoned` 不挡「删除后重建为全新身份」（spec §4.1），那个
            // 攻击面已经由上面的 `tombstoned.contains(&req.item_id)` 挡住
            // （挡的是复用那个已经终结的旧 item_id 本身，与路径无关）。
            if let RemoteState::Present { item_id: owner, .. } = &current {
                if *owner != req.item_id {
                    return Ok(BatchOutcome::Rejected {
                        index,
                        outcome: CommitOutcome::IdentityMismatch {
                            path: req.path.clone(),
                            claimed_item_id: req.item_id,
                            actual_item_id: Some(*owner),
                        },
                    });
                }
            }
            if let Some(other_path) = working_remote.iter().find_map(|(p, s)| {
                (p != &req.path && owner_item_id(s) == Some(req.item_id)).then(|| p.clone())
            }) {
                return Ok(BatchOutcome::Rejected {
                    index,
                    outcome: CommitOutcome::IdentityMismatch {
                        path: other_path,
                        claimed_item_id: req.item_id,
                        actual_item_id: Some(req.item_id),
                    },
                });
            }

            // CAS 比较对象：item 自己的版本链尾——批次内已经校验通过的条目从
            // `working_last_version` 取（尚未落盘，磁盘读不到）；第一次在本批次
            // 出现的 item_id 才去读磁盘（与单条 `commit` 的 `validate_commit`
            // 同一处置）。
            let item_last_version = match working_last_version.get(&req.item_id) {
                Some(v) => v.clone(),
                None => read_item_last_version(self.root, req.item_id)?.map(|v| v.version_id),
            };
            if item_last_version != req.parent {
                return Ok(BatchOutcome::Rejected {
                    index,
                    outcome: CommitOutcome::Conflict {
                        expected_parent: req.parent.clone(),
                        actual: current,
                    },
                });
            }

            working_remote.insert(
                req.path.clone(),
                RemoteState::Present {
                    item_id: req.item_id,
                    version_id: req.version_id.clone(),
                    hash: ContentHash::from_bytes(&req.bytes),
                    size: req.bytes.len() as u64,
                },
            );
            working_last_version.insert(req.item_id, Some(req.version_id.clone()));
            prepared.push((req, item_last_version));
        }

        // 全部校验通过——落盘：内容先经 `atomic::Batch`（目录 fsync 去重收口，
        // 与 M1d 批量归档同一手法）整批写入，再统一追加 items 链 + index
        // 记录 + journal 事件（`AppendBatch` 同样把目录 fsync 收口到一次）。
        // "内容先于指针发布"的顺序不变：全部内容先落盘，指针（items/index/
        // journal）才跟进。
        let mut file_batch = atomic::Batch::new(self.root);
        for (req, _) in &prepared {
            let target = format!("{}/{}", layout::FILES_DIR, req.path);
            file_batch
                .write(&target, &req.bytes)
                .map_err(TransportError::Atomic)?;
        }
        file_batch.commit().map_err(TransportError::Atomic)?;

        let mut journal_batch =
            journal::AppendBatch::open(self.root).map_err(TransportError::Journal)?;
        let mut outcomes = Vec::with_capacity(prepared.len());
        for (req, item_last_version) in prepared {
            let version = Version {
                version_id: req.version_id.clone(),
                item_id: req.item_id,
                parent: item_last_version,
                hash: ContentHash::from_bytes(&req.bytes),
                size: req.bytes.len() as u64,
                mtime: req.mtime.clone(),
                actor: req.actor.clone(),
                committed_at: crate::clock::now_rfc3339(),
            };
            append_item_version(self.root, &version)?;
            write_index_record(self.root, &req.path, req.item_id)?;

            let seq = journal_batch.next_seq();
            journal_batch
                .push(JournalEvent {
                    seq,
                    op: Op::Upsert,
                    item_id: version.item_id,
                    version_id: version.version_id.clone(),
                    path: req.path.clone(),
                    from: None,
                    actor: version.actor.clone(),
                    at: version.committed_at.clone(),
                })
                .map_err(TransportError::Journal)?;

            outcomes.push((req.item_id, req.version_id.clone()));
        }
        journal_batch.commit().map_err(TransportError::Journal)?;

        Ok(BatchOutcome::Committed(outcomes))
    }

    fn tombstone(&self, req: &TombstoneRequest) -> Result<CommitOutcome, TransportError> {
        // 评审 I3：跨进程排他——见 `arca_store::lock` 模块文档、
        // `commit_streamed` 同一处注释。tombstone 同样是"读当前状态 → CAS
        // 校验 → 写入（journal + trash + index 清理）"的临界区，需要与
        // `commit` 互斥同一把锁。
        let _lock = arca_store::lock::acquire(self.root).map_err(TransportError::Lock)?;
        let remote = self.read_remote()?;
        let current = remote
            .get(&req.path)
            .cloned()
            .unwrap_or(RemoteState::Absent);

        // C1 身份校验（评审利用 3：DELETE 带伪造 item_id）：这个路径此刻
        // 真正的归属必须与客户端声称的 item_id 一致，否则 tombstone 会被
        // 记到错误的身份名下——I8 的审计闭环要求 journal 里 tombstone 的
        // item_id 是真的，不是客户端说了算。
        if let Some(owner) = owner_item_id(&current) {
            if owner != req.item_id {
                return Ok(CommitOutcome::IdentityMismatch {
                    path: req.path.clone(),
                    claimed_item_id: req.item_id,
                    actual_item_id: Some(owner),
                });
            }
        }

        let item_last_version =
            read_item_last_version(self.root, req.item_id)?.map(|v| v.version_id);
        if item_last_version.as_ref() != Some(&req.parent) {
            return Ok(CommitOutcome::Conflict {
                expected_parent: Some(req.parent.clone()),
                actual: current,
            });
        }

        trash::move_to_trash(self.root, &req.path, req.item_id, &req.at)
            .map_err(TransportError::Trash)?;
        remove_index_record(self.root, &req.path)?;

        let seq = journal::next_seq(self.root).map_err(TransportError::Journal)?;
        journal::append(
            self.root,
            &arca_format::journal::JournalEvent {
                seq,
                op: arca_format::journal::Op::Tombstone,
                item_id: req.item_id,
                version_id: req.parent.clone(),
                path: req.path.clone(),
                from: None,
                actor: req.actor.clone(),
                at: req.at.clone(),
            },
        )
        .map_err(TransportError::Journal)?;

        Ok(CommitOutcome::Committed {
            item_id: req.item_id,
            version_id: req.parent.clone(),
        })
    }

    fn rename(&self, req: &RenameRequest) -> Result<CommitOutcome, TransportError> {
        // 与 `commit`/`tombstone` 同一把跨进程排他锁——见 `RenameRequest`
        // 文档「为什么需要这第三个写入原语」，同样是"读当前状态 → CAS 校验 →
        // 写入"的临界区。
        let _lock = arca_store::lock::acquire(self.root).map_err(TransportError::Lock)?;
        let remote = self.read_remote()?;

        let old_current = remote
            .get(&req.old_path)
            .cloned()
            .unwrap_or(RemoteState::Absent);

        // 校验 1（与 `validate_commit`/`tombstone` 同一纪律）：`old_path` 此刻
        // 真正的归属必须与调用方声称的 item_id 一致。
        if let Some(owner) = owner_item_id(&old_current) {
            if owner != req.item_id {
                return Ok(CommitOutcome::IdentityMismatch {
                    path: req.old_path.clone(),
                    claimed_item_id: req.item_id,
                    actual_item_id: Some(owner),
                });
            }
        } else {
            // `old_path` 此刻根本不存在这个 item——不是"版本过期"（对不上
            // 任何一个已知版本），是"打错了身份"，与 `validate_commit` 对
            // "路径此刻完全不存在但调用方声称已知某个 item"的处置一致。
            return Ok(CommitOutcome::IdentityMismatch {
                path: req.old_path.clone(),
                claimed_item_id: req.item_id,
                actual_item_id: None,
            });
        }

        // 校验 2：`new_path` 此刻不能已经被别的 item_id 占用——改名不能
        // 悄悄践踏另一个身份已经落在那个路径上的记录（与 `validate_commit`
        // 「校验 2」同一条纪律，只是这里已知要检查的具体路径，不需要
        // 遍历整个 `remote`）。
        let new_current = remote
            .get(&req.new_path)
            .cloned()
            .unwrap_or(RemoteState::Absent);
        if let Some(owner) = owner_item_id(&new_current) {
            return Ok(CommitOutcome::IdentityMismatch {
                path: req.new_path.clone(),
                claimed_item_id: req.item_id,
                actual_item_id: Some(owner),
            });
        }

        // 校验 3：真正的 CAS 比较对象是这个 item 自己的版本链尾（与
        // `validate_commit`「校验 3」同一条纪律）——内容没变，链本身不会
        // 因为这次改名而推进，只是用来确认调用方声明的 `parent` 仍然是
        // 这个 item 此刻唯一存活的版本。
        let item_last_version =
            read_item_last_version(self.root, req.item_id)?.map(|v| v.version_id);
        if item_last_version.as_ref() != Some(&req.parent) {
            return Ok(CommitOutcome::Conflict {
                expected_parent: Some(req.parent.clone()),
                actual: old_current,
            });
        }

        // 写入顺序：`files/` 物理内容先搬，index 指针后搬——I1「逃生舱」
        // 要求 `files/` 永远是路径原样平放的普通文件树（不是 item_id/哈希
        // 寻址），所以改名必须真的移动这个文件，不能只搬 index 指针。
        //
        // 顺序与 `execute_tombstone`（`sync.rs`：先 `move_to_trash` 再
        // `remove_index_record`）同一条纪律、同一个已被接受的崩溃窗口：
        // `atomic::rename` 本身对两侧目录链各自 fsync，一旦返回 `Ok` 就已经
        // 落盘确认；若进程恰好在这一步之后、index 更新完成之前崩溃，
        // `old_path` 的 index 记录会短暂指向一个物理上已经搬走的文件
        // （下次读取报 `HubError::MissingContent`），但这与
        // `execute_tombstone` 现有的崩溃窗口是同一类风险，不是本次改动
        // 新引入的缺口；`remove_index_record` 先于 `write_index_record`
        // 执行，是为了让"物理已在新址、index 还没跟上"这个窗口期的下一个
        // 可能中间态（先删旧后建新）落在"两边都读成 Absent"（安全的保守值）
        // 而不是"新路径 Present 但内容还没搬到"（同样会触发 MissingContent，
        // 只是换了一个路径报错）。
        let old_target = format!("{}/{}", layout::FILES_DIR, req.old_path);
        let new_target = format!("{}/{}", layout::FILES_DIR, req.new_path);
        atomic::rename(self.root, &old_target, &new_target).map_err(TransportError::Atomic)?;
        remove_index_record(self.root, &req.old_path)?;
        write_index_record(self.root, &req.new_path, req.item_id)?;

        let seq = journal::next_seq(self.root).map_err(TransportError::Journal)?;
        journal::append(
            self.root,
            &JournalEvent {
                seq,
                op: Op::Rename,
                item_id: req.item_id,
                version_id: req.parent.clone(),
                path: req.new_path.clone(),
                from: Some(req.old_path.clone()),
                actor: req.actor.clone(),
                at: req.at.clone(),
            },
        )
        .map_err(TransportError::Journal)?;

        Ok(CommitOutcome::Committed {
            item_id: req.item_id,
            version_id: req.parent.clone(),
        })
    }

    fn recoverable(
        &self,
        item_id: ItemId,
        expected_hash: ContentHash,
    ) -> Result<Option<Recoverable>, TransportError> {
        let entries = self.trash_snapshot()?;
        for entry in entries
            .iter()
            .filter(|e| e.meta.item_id == item_id && e.meta.hash == expected_hash)
        {
            // 三方核验的最后一环：现场重新打开 `.data`、重算哈希，与
            // `.meta.hash`/调用方期望的哈希三方一致才当作"确实可取回"
            // （评审 Critical #2，`gates.rs::check_retention` 的同一条纪律，
            // 见其文档）——不止信 `.meta` 记录的哈希，那只是"移入时刻"的
            // 内容，`.data` 此刻可能已经被外部工具截断/替换。
            let bytes =
                trash::read_content(self.root, entry.trash_id).map_err(TransportError::Trash)?;
            let hash = ContentHash::from_bytes(&bytes);
            if hash == expected_hash {
                return Ok(Some(Recoverable {
                    hash,
                    size: bytes.len() as u64,
                }));
            }
        }
        Ok(None)
    }
}

/// 追加一条版本记录到 `items/<xx>/<item_id>.jsonl`——与
/// `sync.rs::append_item_version` 同一手法（`arca_store::atomic` 只提供整文件
/// 原子替换，没有原子追加），这里不经 `Batch`：`commit` 是单条记录的一次性
/// CAS 提交，不是批量归档，没有必要引入批次收口的复杂度（模块顶部「本切片
/// 的落地范围」一节）。
fn append_item_version(root: &StorageRoot, version: &Version) -> Result<(), TransportError> {
    let rel = layout::item_path(&version.item_id);
    let full = root.path().join(&rel);
    let mut content = match fs::read_to_string(&full) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(TransportError::Io {
                path: full.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    content.push_str(&items::to_line(version).map_err(TransportError::Format)?);
    content.push('\n');
    atomic::write(root, &rel, content.as_bytes()).map_err(TransportError::Atomic)
}

/// M2c Task 1：把这次落地的新版本写进 journal（`Op::Upsert`）——补齐
/// `journal.rs`/`PROTOCOL.md` §3 记录的落地前提：M2a 只让 `tombstone` 写
/// journal（删除传播闸门当时唯一的消费者），`commit`/`commit_streamed` 从未
/// 写过 `Op::Upsert` 事件。这在 M2a/M2b 语境下无害——`hub::read_remote` 从
/// `items/`/`index/` 直接推导 `Present` 状态，不依赖 journal——但 M2c 的
/// 变更流端点（`GET .../changes`）如果只回放 tombstone/rename，客户端能看到
/// 删除却看不到新增/修改，长轮询就失去了存在的意义。`Op::Upsert` 早已在
/// `FORMAT.md` §7.2 定义，这里只是补上触发写入的一处调用点，不新增磁盘格式。
fn append_upsert_journal_event(
    root: &StorageRoot,
    version: &Version,
    path: &str,
) -> Result<(), TransportError> {
    let seq = journal::next_seq(root).map_err(TransportError::Journal)?;
    journal::append(
        root,
        &JournalEvent {
            seq,
            op: Op::Upsert,
            item_id: version.item_id,
            version_id: version.version_id.clone(),
            path: path.to_string(),
            from: None,
            actor: version.actor.clone(),
            at: version.committed_at.clone(),
        },
    )
    .map_err(TransportError::Journal)
}

/// 整体原子替换 `index/<xx>/<key>.json`——与 `sync.rs::write_index_record` 同一手法。
fn write_index_record(
    root: &StorageRoot,
    path: &str,
    item_id: ItemId,
) -> Result<(), TransportError> {
    let key = path_rules::index_key(path);
    let rel = layout::index_path(&key);
    let record = IndexRecord {
        item_id,
        path: path.to_string(),
    };
    let text = record.to_json().map_err(TransportError::Format)?;
    atomic::write(root, &rel, text.as_bytes()).map_err(TransportError::Atomic)
}

/// 从 `.arca/index/<key>.json` 移除 `path` 的记录——与
/// `sync.rs::remove_index_record` 同一手法（评审 Important #2：让"没有 index
/// 记录"本身成为"这个路径已被删除"的证据，见 `sync.rs::execute_tombstone`
/// 的文档）。记录本就不存在视为无操作，不是错误。
fn remove_index_record(root: &StorageRoot, path: &str) -> Result<(), TransportError> {
    let key = path_rules::index_key(path);
    let rel = layout::index_path(&key);
    let full = root.path().join(&rel);
    match fs::remove_file(&full) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(TransportError::Io {
            path: full.display().to_string(),
            reason: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::hub_layout::FormatJson;
    use arca_format::model::Actor;
    use std::path::Path;

    fn 造存储根(dir: &Path) {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        fs::create_dir_all(dir.join(".arca/trash")).unwrap();
        fs::create_dir_all(dir.join(".arca/journal")).unwrap();
        let format = FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    fn open(dir: &Path) -> StorageRoot {
        StorageRoot::open(dir, None).unwrap()
    }

    fn actor() -> Actor {
        Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        }
    }

    #[test]
    fn commit在parent为none且远端absent时成功创建() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        let item_id = crate::ids::new_item_id();
        let version_id = crate::ids::new_version_id();
        let outcome = transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: version_id.clone(),
                parent: None,
                bytes: b"hello".to_vec(),
                mtime: "2026-08-08T09:00:00Z".to_string(),
                actor: actor(),
            })
            .unwrap();

        match outcome {
            CommitOutcome::Committed {
                item_id: got_item,
                version_id: got_version,
            } => {
                assert_eq!(got_item, item_id);
                assert_eq!(got_version, version_id);
            }
            other => panic!("应为 Committed，实得 {other:?}"),
        }
        assert_eq!(fs::read(dir.path().join("files/a.txt")).unwrap(), b"hello");

        let remote = transport.read_remote().unwrap();
        assert!(matches!(
            remote.get("a.txt"),
            Some(RemoteState::Present { .. })
        ));
    }

    #[test]
    fn commit在parent不是none但远端absent时报冲突() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        let stale_parent =
            arca_format::model::VersionId::new("20260808T090000Z", &"1".repeat(32)).unwrap();
        let outcome = transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: Some(stale_parent.clone()),
                bytes: b"hello".to_vec(),
                mtime: "2026-08-08T09:00:00Z".to_string(),
                actor: actor(),
            })
            .unwrap();

        match outcome {
            CommitOutcome::Conflict {
                expected_parent,
                actual,
            } => {
                assert_eq!(expected_parent, Some(stale_parent));
                assert_eq!(actual, RemoteState::Absent);
            }
            other => panic!("应为 Conflict，实得 {other:?}"),
        }
        assert!(
            !dir.path().join("files/a.txt").exists(),
            "冲突时不应写入任何内容"
        );
    }

    #[test]
    fn commit两次同一路径第二次必须带正确parent才成功() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();

        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: v1.clone(),
                parent: None,
                bytes: b"v1".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        // 用过期的 parent（None，仿佛不知道 v1 已经存在）再提交一次——必须冲突。
        let stale_outcome = transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"v2-stale".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        assert!(matches!(stale_outcome, CommitOutcome::Conflict { .. }));
        assert_eq!(fs::read(dir.path().join("files/a.txt")).unwrap(), b"v1");

        // 带上正确的 parent（v1）——应当成功推进。
        let v2 = crate::ids::new_version_id();
        let outcome = transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: v2.clone(),
                parent: Some(v1),
                bytes: b"v2".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        assert!(matches!(outcome, CommitOutcome::Committed { .. }));
        assert_eq!(fs::read(dir.path().join("files/a.txt")).unwrap(), b"v2");
    }

    #[test]
    fn tombstone成功后remote变为tombstoned且内容在trash里可取回() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();

        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: v1.clone(),
                parent: None,
                bytes: b"content".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        let outcome = transport
            .tombstone(&TombstoneRequest {
                path: "a.txt".to_string(),
                item_id,
                parent: v1,
                actor: actor(),
                at: "2026-08-08T09:10:00Z".to_string(),
            })
            .unwrap();
        assert!(matches!(outcome, CommitOutcome::Committed { .. }));

        let remote = transport.read_remote().unwrap();
        assert!(matches!(
            remote.get("a.txt"),
            Some(RemoteState::Tombstoned { .. })
        ));

        let recoverable = transport
            .recoverable(item_id, ContentHash::from_bytes(b"content"))
            .unwrap();
        assert_eq!(
            recoverable,
            Some(Recoverable {
                hash: ContentHash::from_bytes(b"content"),
                size: 7,
            })
        );
    }

    /// **评审 C2 实机复现**：`sync()` 改走 `commit_batch` 之后暴露的一个真实
    /// bug——一条路径被 tombstone 之后，用**全新**（从未出现过）的 item_id
    /// 在同一路径原地重建，必须成功（spec §4.1「删除后重建 = 新身份」，
    /// `sync.rs::prepare_upload` 在 `parent:None` 时就是这么分配身份的）。
    /// 修复前 `validate_commit` 校验 1 把 `Tombstoned` 也算作"有归属"，
    /// 导致这个全新 item_id 被误判成"与路径此刻的归属不符"而报
    /// `IdentityMismatch`——`arca_cli::sync::tests::restore覆盖当前占用者时先移入trash_评审critical1实机复现`
    /// 曾经因此挂掉（该测试的第 3 步正是这个场景）。这里单独在 `Transport`
    /// 这一层直接覆盖，不依赖 `sync()` 的整条调和链路。
    #[test]
    fn commit在路径被tombstone后用全新item_id原地重建必须成功() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        let old_item = crate::ids::new_item_id();
        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: old_item,
                version_id: v1.clone(),
                parent: None,
                bytes: b"OLD".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .tombstone(&TombstoneRequest {
                path: "a.txt".to_string(),
                item_id: old_item,
                parent: v1,
                actor: actor(),
                at: "2026-08-08T09:10:00Z".to_string(),
            })
            .unwrap();

        // 全新 item_id（从未出现过），parent:None（仅创建语义）——必须成功，
        // 不能因为路径此刻的（tombstone）历史而被拒绝。
        let new_item = crate::ids::new_item_id();
        assert_ne!(new_item, old_item);
        let outcome = transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: new_item,
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"NEW".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        assert!(
            matches!(outcome, CommitOutcome::Committed { .. }),
            "实得 {outcome:?}"
        );
        let remote = transport.read_remote().unwrap();
        assert!(matches!(
            remote.get("a.txt"),
            Some(RemoteState::Present { item_id, .. }) if *item_id == new_item
        ));

        // 复用旧的（已终结的）item_id 仍然必须被拒绝——校验 0 不受这次修复
        // 影响，攻击面没有被打开。
        let resurrect = transport
            .commit(&CommitRequest {
                path: "b.txt".to_string(),
                item_id: old_item,
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"attacker".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        assert!(matches!(
            resurrect,
            CommitOutcome::IdentityMismatch {
                actual_item_id: None,
                ..
            }
        ));
    }

    /// 同一场景的 `commit_batch` 版本——`commit_batch` 有一份独立的内联校验
    /// （不是复用 `validate_commit`），必须单独覆盖，否则两条路径会悄悄
    /// 分叉（评审 C2 实机复现修复，见上一条测试与 `commit_batch` 内联校验
    /// 处的注释）。
    #[test]
    fn commit_batch在路径被tombstone后用全新item_id原地重建必须成功() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        let old_item = crate::ids::new_item_id();
        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: old_item,
                version_id: v1.clone(),
                parent: None,
                bytes: b"OLD".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .tombstone(&TombstoneRequest {
                path: "a.txt".to_string(),
                item_id: old_item,
                parent: v1,
                actor: actor(),
                at: "2026-08-08T09:10:00Z".to_string(),
            })
            .unwrap();

        let new_item = crate::ids::new_item_id();
        let outcome = transport
            .commit_batch(&[CommitRequest {
                path: "a.txt".to_string(),
                item_id: new_item,
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"NEW".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            }])
            .unwrap();
        assert!(
            matches!(outcome, BatchOutcome::Committed(ref v) if v.len() == 1),
            "实得 {outcome:?}"
        );
    }

    // -----------------------------------------------------------------
    // M2c Task 5：rename——身份不动、路径映射搬家（I7）
    // -----------------------------------------------------------------

    #[test]
    fn rename成功后item_id与内容不变_旧路径消失新路径出现() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();

        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "old.txt".to_string(),
                item_id,
                version_id: v1.clone(),
                parent: None,
                bytes: b"content".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        let outcome = transport
            .rename(&RenameRequest {
                old_path: "old.txt".to_string(),
                new_path: "new.txt".to_string(),
                item_id,
                parent: v1.clone(),
                actor: actor(),
                at: "2026-08-08T09:10:00Z".to_string(),
            })
            .unwrap();
        assert_eq!(
            outcome,
            CommitOutcome::Committed {
                item_id,
                version_id: v1.clone(),
            },
            "改名不产生新版本——version_id 原样延续"
        );

        let remote = transport.read_remote().unwrap();
        assert!(
            !remote.contains_key("old.txt")
                || matches!(remote.get("old.txt"), Some(RemoteState::Absent)),
            "旧路径不应再出现在 remote 里"
        );
        match remote.get("new.txt") {
            Some(RemoteState::Present {
                item_id: got_item,
                hash,
                size,
                ..
            }) => {
                assert_eq!(*got_item, item_id, "I7：item_id 必须原样延续");
                assert_eq!(*hash, ContentHash::from_bytes(b"content"));
                assert_eq!(*size, 7);
            }
            other => panic!("新路径应为 Present，实得 {other:?}"),
        }

        // 内容本身原地不动（只搬了 index 指针，`files/` 下的物理内容仍是
        // 同一份，`hub::read_remote` 只是让新路径的 index 指向它）。
        assert_eq!(
            transport.read_content("new.txt").unwrap(),
            b"content".to_vec()
        );
    }

    #[test]
    fn rename带过期parent时报冲突而不动数据() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();

        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "old.txt".to_string(),
                item_id,
                version_id: v1,
                parent: None,
                bytes: b"content".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        let stale =
            arca_format::model::VersionId::new("20260808T090000Z", &"1".repeat(32)).unwrap();
        let outcome = transport
            .rename(&RenameRequest {
                old_path: "old.txt".to_string(),
                new_path: "new.txt".to_string(),
                item_id,
                parent: stale,
                actor: actor(),
                at: "2026-08-08T09:10:00Z".to_string(),
            })
            .unwrap();
        assert!(matches!(outcome, CommitOutcome::Conflict { .. }));

        // 拒绝之后旧路径必须原封不动，新路径不应该出现。
        let remote = transport.read_remote().unwrap();
        assert!(matches!(
            remote.get("old.txt"),
            Some(RemoteState::Present { .. })
        ));
        assert!(!remote.contains_key("new.txt"));
    }

    #[test]
    fn rename目标路径已被别的item占用时报identity_mismatch且不动数据() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_a = crate::ids::new_item_id();
        let item_b = crate::ids::new_item_id();

        let va = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: item_a,
                version_id: va.clone(),
                parent: None,
                bytes: b"a".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .commit(&CommitRequest {
                path: "b.txt".to_string(),
                item_id: item_b,
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"b".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        // 试图把 a.txt 改名成 b.txt——b.txt 已经被另一个 item 占着。
        let outcome = transport
            .rename(&RenameRequest {
                old_path: "a.txt".to_string(),
                new_path: "b.txt".to_string(),
                item_id: item_a,
                parent: va,
                actor: actor(),
                at: "2026-08-08T09:10:00Z".to_string(),
            })
            .unwrap();
        match outcome {
            CommitOutcome::IdentityMismatch {
                path,
                actual_item_id,
                ..
            } => {
                assert_eq!(path, "b.txt");
                assert_eq!(actual_item_id, Some(item_b));
            }
            other => panic!("应为 IdentityMismatch，实得 {other:?}"),
        }

        let remote = transport.read_remote().unwrap();
        assert!(matches!(
            remote.get("a.txt"),
            Some(RemoteState::Present { item_id, .. }) if *item_id == item_a
        ));
        assert!(matches!(
            remote.get("b.txt"),
            Some(RemoteState::Present { item_id, .. }) if *item_id == item_b
        ));
    }

    #[test]
    fn tombstone带过期parent时报冲突而不动数据() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();

        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: v1,
                parent: None,
                bytes: b"content".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        let stale =
            arca_format::model::VersionId::new("20260101T000000Z", &"9".repeat(32)).unwrap();
        let outcome = transport
            .tombstone(&TombstoneRequest {
                path: "a.txt".to_string(),
                item_id,
                parent: stale,
                actor: actor(),
                at: "t".to_string(),
            })
            .unwrap();
        assert!(matches!(outcome, CommitOutcome::Conflict { .. }));
        assert!(
            dir.path().join("files/a.txt").exists(),
            "冲突时不应移动任何内容"
        );
    }

    #[test]
    fn recoverable对没有记录的item返回none() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        let result = transport
            .recoverable(crate::ids::new_item_id(), ContentHash::from_bytes(b"x"))
            .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn recoverable对哈希不匹配的记录返回none() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();

        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: v1.clone(),
                parent: None,
                bytes: b"content".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .tombstone(&TombstoneRequest {
                path: "a.txt".to_string(),
                item_id,
                parent: v1,
                actor: actor(),
                at: "t".to_string(),
            })
            .unwrap();

        // 期望一个与 trash 里实际内容不同的哈希——不应被当作可取回。
        let result = transport
            .recoverable(item_id, ContentHash::from_bytes(b"totally different"))
            .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn list枚举全部已知路径() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"a".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .commit(&CommitRequest {
                path: "b.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"b".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        let mut paths = transport.list().unwrap();
        paths.sort();
        assert_eq!(paths, vec!["a.txt".to_string(), "b.txt".to_string()]);
    }

    #[test]
    fn read_content读出commit写入的字节() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"hello world".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        assert_eq!(transport.read_content("a.txt").unwrap(), b"hello world");
    }

    /// 惰性缓存：`recoverable` 第一次调用之后，即便调用方之后又执行了新的
    /// `tombstone`（往 `.arca/trash/` 里新增了一条记录），同一个 `LocalTransport`
    /// 实例的后续 `recoverable` 查询仍然只看第一次拍下的快照——与
    /// `sync.rs::sync` 现有的"循环开始前只读一次"是同一条纪律（模块顶部
    /// 文档），这里直接对 `Transport` 实现本身钉住这个行为，不依赖 `sync()`
    /// 间接验证。
    #[test]
    fn recoverable的trash快照在同一实例内只拍一次() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_a = crate::ids::new_item_id();
        let item_b = crate::ids::new_item_id();

        let va = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: item_a,
                version_id: va.clone(),
                parent: None,
                bytes: b"a-content".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .tombstone(&TombstoneRequest {
                path: "a.txt".to_string(),
                item_id: item_a,
                parent: va,
                actor: actor(),
                at: "t1".to_string(),
            })
            .unwrap();

        // 第一次查询——触发快照拍摄，此刻只有 item_a 在回收站里。
        assert!(transport
            .recoverable(item_a, ContentHash::from_bytes(b"a-content"))
            .unwrap()
            .is_some());

        // 快照拍摄之后，另一个 item 才被删除。
        let vb = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "b.txt".to_string(),
                item_id: item_b,
                version_id: vb.clone(),
                parent: None,
                bytes: b"b-content".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .tombstone(&TombstoneRequest {
                path: "b.txt".to_string(),
                item_id: item_b,
                parent: vb,
                actor: actor(),
                at: "t2".to_string(),
            })
            .unwrap();

        // 同一个 transport 实例：仍然应该看不到 item_b（快照已经拍过了）。
        assert_eq!(
            transport
                .recoverable(item_b, ContentHash::from_bytes(b"b-content"))
                .unwrap(),
            None,
            "同一实例内 trash 快照只应拍摄一次，不应看到快照之后才发生的删除"
        );

        // 一个全新的 transport 实例（模拟下一次调和）应该能看到两者。
        let fresh = LocalTransport::new(&root);
        assert!(fresh
            .recoverable(item_b, ContentHash::from_bytes(b"b-content"))
            .unwrap()
            .is_some());
    }

    /// 评审 I3 的核心复现：`commit` 完全不依赖调用方额外加锁（`arcad` 的
    /// `Dataset::write_lock` 是它自己的进程内保护，这里刻意不用——直接模拟
    /// 两个各自独立打开同一存储根的进程/线程，只靠 `commit` 内部新获取的
    /// `arca_store::lock` 互斥）。两个线程都相信当前 parent 是 v1、都尝试
    /// 推进到各自的新版本；`commit` 内部的锁把"读当前状态 → CAS 校验 →
    /// 写入"整段临界区收窄成互斥的，后进入的线程必然在锁释放后才开始读取，
    /// 读到的已经是前一个线程写完的新状态——因此结果在调度上是**确定的**：
    /// 精确一个 `Committed`、一个 `Conflict`，不是"大概率不冲突"。修复前
    /// （`commit` 内部没有任何锁）这段读-比较-写之间存在真实的竞态窗口，
    /// 两个线程都可能读到同一个旧 parent、都通过 CAS 比较、都各自写入，
    /// 后写入的静默覆盖先写入的——这正是 `storage.rs`「`write_lock`」一节
    /// 描述、但此前只在 `arcad` 单进程内被挡住的那类竞态，本测试证明
    /// `LocalTransport` 自己（不借助任何调用方的额外锁）现在也挡得住。
    #[test]
    fn commit内部的跨进程锁使并发cas竞态结果确定而不静默覆盖() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        let item_id = crate::ids::new_item_id();
        let v1 = crate::ids::new_version_id();
        transport
            .commit(&CommitRequest {
                path: "race.txt".to_string(),
                item_id,
                version_id: v1.clone(),
                parent: None,
                bytes: b"v1".to_vec(),
                mtime: "2026-08-08T09:00:00Z".to_string(),
                actor: actor(),
            })
            .unwrap();

        let dir_path = dir.path().to_path_buf();
        let v2 = crate::ids::new_version_id();
        let v3 = crate::ids::new_version_id();

        let spawn_racer = |version_id: arca_format::model::VersionId, content: &'static [u8]| {
            let dir_path = dir_path.clone();
            let parent = v1.clone();
            std::thread::spawn(move || {
                // 各自独立打开存储根——不共享任何进程内对象，模拟两个真正
                // 独立的调用方（两个 arcad 实例，或 arcad 与并发的
                // `arca sync`）。
                let root = StorageRoot::open(&dir_path, None).unwrap();
                let transport = LocalTransport::new(&root);
                transport.commit(&CommitRequest {
                    path: "race.txt".to_string(),
                    item_id,
                    version_id,
                    parent: Some(parent),
                    bytes: content.to_vec(),
                    mtime: "2026-08-08T09:00:01Z".to_string(),
                    actor: actor(),
                })
            })
        };

        let h1 = spawn_racer(v2.clone(), b"from thread 1");
        let h2 = spawn_racer(v3.clone(), b"from thread 2");
        let r1 = h1.join().unwrap().unwrap();
        let r2 = h2.join().unwrap().unwrap();

        let committed_count = [&r1, &r2]
            .iter()
            .filter(|o| matches!(o, CommitOutcome::Committed { .. }))
            .count();
        let conflict_count = [&r1, &r2]
            .iter()
            .filter(|o| matches!(o, CommitOutcome::Conflict { .. }))
            .count();
        assert_eq!(
            committed_count, 1,
            "并发 CAS 竞态必须恰好一个成功，实得 r1={r1:?} r2={r2:?}"
        );
        assert_eq!(
            conflict_count, 1,
            "另一个必须被识别为 CAS 冲突（读到了对方已经写完的新状态），\
             不能两个都成功——那就是静默覆盖：r1={r1:?} r2={r2:?}"
        );

        // 内容与版本链本身也必须自洽：不管哪个线程赢，最终落地的内容与
        // items 链尾必须是同一个版本，不能出现"链断裂"或"内容与记录不符"。
        let final_remote = transport.read_remote().unwrap();
        match final_remote.get("race.txt") {
            Some(RemoteState::Present { version_id, .. }) => {
                assert!(*version_id == v2 || *version_id == v3);
            }
            other => panic!("应为 Present，实得 {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // M2c Task 1：四条缺口
    // -----------------------------------------------------------------

    #[test]
    fn commit现在把upsert事件写进journal() {
        // 补齐前提（模块顶部 `append_upsert_journal_event` 文档）：
        // M2c 变更流端点要能看到新增/修改，不能只看到删除。
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();
        let v1 = crate::ids::new_version_id();

        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: v1.clone(),
                parent: None,
                bytes: b"hello".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        let (_cursor, events) = journal::read_all(&root).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].op, arca_format::journal::Op::Upsert);
        assert_eq!(events[0].item_id, item_id);
        assert_eq!(events[0].version_id, v1);
        assert_eq!(events[0].path, "a.txt");
    }

    #[test]
    fn read_content_into流式读出与read_content相同的字节() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"hello streamed world".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        let mut out = Vec::new();
        let written = transport.read_content_into("a.txt", &mut out).unwrap();
        assert_eq!(written, "hello streamed world".len() as u64);
        assert_eq!(out, b"hello streamed world");
    }

    #[test]
    fn read_content_into对不存在路径报io错误() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let mut out = Vec::new();
        assert!(matches!(
            transport.read_content_into("不存在.txt", &mut out),
            Err(TransportError::Io { .. })
        ));
    }

    #[test]
    fn read_range取出正确的字节区间() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"0123456789".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        assert_eq!(transport.read_range("a.txt", 2, 3).unwrap(), b"234");
        assert_eq!(transport.read_range("a.txt", 0, 10).unwrap(), b"0123456789");
    }

    #[test]
    fn read_range越界时报io错误() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"short".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        assert!(matches!(
            transport.read_range("a.txt", 0, 100),
            Err(TransportError::Io { .. })
        ));
    }

    #[test]
    fn read_by_hash按内容哈希取回字节_多路径去重取路径序第一个() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        transport
            .commit(&CommitRequest {
                path: "z.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"shared".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: crate::ids::new_item_id(),
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"shared".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();

        let hash = ContentHash::from_bytes(b"shared");
        // 两个路径都持有相同内容——按路径排序应取 "a.txt"（排在 "z.txt" 前）。
        assert_eq!(
            transport.read_by_hash(hash).unwrap(),
            Some(b"shared".to_vec())
        );
    }

    #[test]
    fn read_by_hash查无匹配返回none而不是错误() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let result = transport
            .read_by_hash(ContentHash::from_bytes("从未出现过的内容".as_bytes()))
            .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn commit_batch空切片直接返回空的committed() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        assert_eq!(
            transport.commit_batch(&[]).unwrap(),
            BatchOutcome::Committed(vec![])
        );
    }

    #[test]
    fn commit_batch全部成功时内容与journal事件按顺序全部落地() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        let item_a = crate::ids::new_item_id();
        let item_b = crate::ids::new_item_id();
        let va = crate::ids::new_version_id();
        let vb = crate::ids::new_version_id();

        let outcome = transport
            .commit_batch(&[
                CommitRequest {
                    path: "a.txt".to_string(),
                    item_id: item_a,
                    version_id: va.clone(),
                    parent: None,
                    bytes: b"content-a".to_vec(),
                    mtime: "t".to_string(),
                    actor: actor(),
                },
                CommitRequest {
                    path: "b.txt".to_string(),
                    item_id: item_b,
                    version_id: vb.clone(),
                    parent: None,
                    bytes: b"content-b".to_vec(),
                    mtime: "t".to_string(),
                    actor: actor(),
                },
            ])
            .unwrap();

        assert_eq!(
            outcome,
            BatchOutcome::Committed(vec![(item_a, va.clone()), (item_b, vb.clone())])
        );
        assert_eq!(
            fs::read(dir.path().join("files/a.txt")).unwrap(),
            b"content-a"
        );
        assert_eq!(
            fs::read(dir.path().join("files/b.txt")).unwrap(),
            b"content-b"
        );

        let (_cursor, events) = journal::read_all(&root).unwrap();
        assert_eq!(events.len(), 2, "两条 upsert 事件都应落盘");
        assert_eq!(events[0].path, "a.txt");
        assert_eq!(events[1].path, "b.txt");
    }

    /// 批次内同一 item_id 在同一路径上连续两个版本——依赖 working 状态
    /// 而不是磁盘读取才能正确校验（模块内 `commit_batch` 文档）。
    #[test]
    fn commit_batch支持同一批次内同item的连续版本() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();
        let v1 = crate::ids::new_version_id();
        let v2 = crate::ids::new_version_id();

        let outcome = transport
            .commit_batch(&[
                CommitRequest {
                    path: "a.txt".to_string(),
                    item_id,
                    version_id: v1.clone(),
                    parent: None,
                    bytes: b"v1".to_vec(),
                    mtime: "t".to_string(),
                    actor: actor(),
                },
                CommitRequest {
                    path: "a.txt".to_string(),
                    item_id,
                    version_id: v2.clone(),
                    parent: Some(v1.clone()),
                    bytes: b"v2".to_vec(),
                    mtime: "t".to_string(),
                    actor: actor(),
                },
            ])
            .unwrap();

        assert_eq!(
            outcome,
            BatchOutcome::Committed(vec![(item_id, v1), (item_id, v2)])
        );
        assert_eq!(fs::read(dir.path().join("files/a.txt")).unwrap(), b"v2");
    }

    /// 整批要么全部成功要么全部不生效——第二条 CAS 冲突时，第一条也不应该
    /// 落盘（M2c Task 1 brief：不做"部分成功"）。
    #[test]
    fn commit_batch任一条冲突时整批不生效() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);

        let item_a = crate::ids::new_item_id();
        let item_b = crate::ids::new_item_id();
        let stale_parent =
            arca_format::model::VersionId::new("20260101T000000Z", &"9".repeat(32)).unwrap();

        let outcome = transport
            .commit_batch(&[
                CommitRequest {
                    path: "a.txt".to_string(),
                    item_id: item_a,
                    version_id: crate::ids::new_version_id(),
                    parent: None,
                    bytes: b"content-a".to_vec(),
                    mtime: "t".to_string(),
                    actor: actor(),
                },
                CommitRequest {
                    path: "b.txt".to_string(),
                    item_id: item_b,
                    // 声称已经存在一个旧版本——远端其实是 Absent，必然冲突。
                    parent: Some(stale_parent.clone()),
                    version_id: crate::ids::new_version_id(),
                    bytes: b"content-b".to_vec(),
                    mtime: "t".to_string(),
                    actor: actor(),
                },
            ])
            .unwrap();

        match outcome {
            BatchOutcome::Rejected { index, outcome } => {
                assert_eq!(index, 1, "应指明是第二条（0-based）失败");
                assert!(matches!(outcome, CommitOutcome::Conflict { .. }));
            }
            other => panic!("应为 Rejected，实得 {other:?}"),
        }

        // 整批不生效：第一条即便自己校验通过，也不应该落盘。
        assert!(
            !dir.path().join("files/a.txt").exists(),
            "第一条不应因为它自己校验通过就被单独落盘"
        );
        assert!(!dir.path().join("files/b.txt").exists());
        let (cursor, events) = journal::read_all(&root).unwrap();
        assert_eq!(cursor, None, "整批失败时不应有任何 journal 事件落盘");
        assert!(events.is_empty());
    }

    #[test]
    fn commit_batch遇到已被tombstone终结的item时报identity_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let transport = LocalTransport::new(&root);
        let item_id = crate::ids::new_item_id();
        let v1 = crate::ids::new_version_id();

        transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: v1.clone(),
                parent: None,
                bytes: b"content".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        transport
            .tombstone(&TombstoneRequest {
                path: "a.txt".to_string(),
                item_id,
                parent: v1,
                actor: actor(),
                at: "t".to_string(),
            })
            .unwrap();

        let outcome = transport
            .commit_batch(&[CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: crate::ids::new_version_id(),
                parent: None,
                bytes: b"resurrected".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            }])
            .unwrap();

        match outcome {
            BatchOutcome::Rejected { index, outcome } => {
                assert_eq!(index, 0);
                assert!(matches!(
                    outcome,
                    CommitOutcome::IdentityMismatch {
                        actual_item_id: None,
                        ..
                    }
                ));
            }
            other => panic!("应为 Rejected，实得 {other:?}"),
        }
    }
}
