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
    CommitOutcome, CommitRequest, Recoverable, TombstoneRequest, Transport, TransportError,
};
use crate::{hub, journal, trash};
use arca_chunk::hash::ContentHash;
use arca_core::state::RemoteState;
use arca_format::hub_layout::layout;
use arca_format::index::IndexRecord;
use arca_format::items;
use arca_format::model::{ItemId, Version};
use arca_format::path_rules;
use arca_store::atomic;
use arca_store::root::StorageRoot;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io;

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

    fn current_version_of(
        &self,
        path: &str,
    ) -> Result<(RemoteState, Option<arca_format::model::VersionId>), TransportError> {
        let remote = self.read_remote()?;
        let current = remote.get(path).cloned().unwrap_or(RemoteState::Absent);
        let version = match &current {
            RemoteState::Present { version_id, .. } => Some(version_id.clone()),
            RemoteState::Tombstoned { .. } | RemoteState::Absent => None,
        };
        Ok((current, version))
    }
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

    fn commit(&self, req: &CommitRequest) -> Result<CommitOutcome, TransportError> {
        let (current, current_version) = self.current_version_of(&req.path)?;
        if current_version != req.parent {
            return Ok(CommitOutcome::Conflict {
                expected_parent: req.parent.clone(),
                actual: current,
            });
        }

        let version = Version {
            version_id: req.version_id.clone(),
            item_id: req.item_id,
            parent: req.parent.clone(),
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

        Ok(CommitOutcome::Committed {
            item_id: req.item_id,
            version_id: req.version_id.clone(),
        })
    }

    fn tombstone(&self, req: &TombstoneRequest) -> Result<CommitOutcome, TransportError> {
        let (current, current_version) = self.current_version_of(&req.path)?;
        if current_version.as_ref() != Some(&req.parent) {
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
}
