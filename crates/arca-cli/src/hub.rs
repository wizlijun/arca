//! 从存储根读出远端状态（M1d Task 3）：三态调和的第三个输入端。
//!
//! 只读两处：`.arca/index/`（路径 → `item_id`）与 `.arca/items/`（该 `item_id`
//! 的版本链，取最后一条为当前版本）。任何一处读不到或读不懂都**立即报错，
//! 不跳过**——这与 `arca_store::fsck` 的纪律出发点相同（I5：如实报告失败的
//! 性质，不静默丢线索），但收敛方式不同：`fsck` 是诊断工具，职责是扫完整个
//! 存储根、把发现的问题**逐条累积**进报告里；`read_remote` 是操作路径，喂给
//! `arca_core::decide` 的每一条 `RemoteState` 都要被当真去执行动作（下载/
//! 上传/删除），遇到第一条读不懂的记录就必须整体停下——继续拿着不完整、
//! 已知有问题的远端视图去调和，比停下报告更危险。
//!
//! # 本函数结构上产不出 `RemoteState::Tombstoned`——这是已知的、刻意的缺口
//!
//! `arca_format::items` 的版本链**只记录 upsert 形状的记录**：FORMAT.md
//! §7.2 明文规定 `tombstone`/`rename` 都不在 `items/<item_id>.jsonl` 产生新
//! 版本（`version_id` 沿用改动前最后一个存活版本的 id），tombstone 的权威记录
//! 只存在于 `journal/<epoch>.jsonl` 的 `op:"tombstone"` 事件里。而 spec §12.3
//! 的里程碑表把 `tombstone` 与 `journal+longpoll` 明确划进 **M2**，不在 M1
//! 范围内。所以 `read_remote` 目前的两个数据源（`index/` + `items/`）**结构上
//! 就无法分辨**「这个路径被删除了」与「这个路径从来没有过」——两者在
//! `index/` 里都表现为"没有这条记录"。
//!
//! 这个缺口是刻意保留、不是遗漏：`arca_core::state::RemoteState::Tombstoned`
//! 这个变体、以及 `arca_core::reconcile` 决策表里依赖它的分支（`DeleteLocal`、
//! `Conflict{modify_vs_delete}` 等）**完整保留、不做任何删减**——M2 把 journal
//! 接上之后，`read_remote` 只需要新增"该路径最近一次 journal 事件是否是
//! tombstone"这一步判断，不需要改 `arca-core` 一个字节。
//!
//! **连带后果（读这段的人，大概率是在写 Task 6 的 `sync`，请一并处理）：**
//! 因为 `read_remote` 永远不产出 `Tombstoned`，"远端删除"不会传播到本地——
//! 本地会一直认为那个文件还在。反过来，"本地删除"这一侧不受影响，三态仍是
//! `(base=present, local=absent, remote=present)`，决策表照常给出
//! `Action::TombstoneRemote{parent}`；但 M1 的 `items/<item_id>.jsonl` 里
//! 没有任何地方可以承载一条 tombstone 记录（上面已经说明为什么），也就是说
//! **M1 没有落盘 `TombstoneRemote` 的地方**。`sync`（Task 6）对这个 `Action`
//! 绝不能悄悄当 no-op 处理——按 I5，能力缺失要如实报告：本轮应把它记为
//! 「删除传播属 M2，本轮未执行」，并让 `arca sync` 的退出码反映出有未完成的
//! 工作，而不是安静退出、让用户以为删除已经生效。

use arca_core::state::RemoteState;
use arca_format::hub_layout::layout;
use arca_format::index::IndexRecord;
use arca_format::items;
use arca_store::root::StorageRoot;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 读远端状态失败——彼此可区分（I5）。
#[derive(Debug)]
pub enum HubError {
    /// 一条 index 记录本身无法读取或解析（JSON 损坏、路径不合规、`item_id`
    /// 编码不对）。
    CorruptIndex { path: String, reason: String },
    /// index 记录指向的 `item_id` 在 `.arca/items/` 下没有对应文件——悬空
    /// 引用：index 说"这个路径映射到这个 item"，但这个 item 的版本链压根
    /// 不存在。与 [`HubError::CorruptItems`]（"文件在，读不懂"）是两种不同
    /// 性质的故障，不折叠成同一个变体。
    MissingItems { path: String, item_id: String },
    /// `.arca/items/<xx>/<item_id>.jsonl` 存在，但无法解析为合法版本链，
    /// 或解析出的版本链为空（结构上不应该出现，出现即损坏）。
    CorruptItems { item_id: String, reason: String },
    /// 读取 `.arca/index/` 或 `.arca/items/` 目录本身失败，且不是"目录不存在"
    /// （真正的 IO 故障：权限、路径某一级类型不对等）。
    Io { path: String, reason: String },
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubError::CorruptIndex { path, reason } => {
                write!(f, "index 记录 {path} 无法读取或解析：{reason}")
            }
            HubError::MissingItems { path, item_id } => write!(
                f,
                "index 记录 {path} 指向 item_id {item_id}，但 .arca/items/ 下没有对应的版本链文件"
            ),
            HubError::CorruptItems { item_id, reason } => {
                write!(f, "item {item_id} 的版本链无法解析：{reason}")
            }
            HubError::Io { path, reason } => write!(f, "读取 {path} 失败：{reason}"),
        }
    }
}

impl std::error::Error for HubError {}

fn io_err(path: &Path, e: io::Error) -> HubError {
    HubError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// 从一个已打开、身份已确认的存储根读出「每个当前受管路径的远端状态」。
///
/// 只读：不修改、不创建任何文件。路径不在返回的 map 里，调用方按
/// `RemoteState::Absent` 处理——`arca_core::decide` 本就以此为默认（见
/// `arca_core::state::RemoteState`）。产出按路径排序的 `BTreeMap`：同一存储根
/// 状态两次调用必须得到同一份结果。
///
/// **不产出 `RemoteState::Tombstoned`**——原因与连带后果见模块顶部 doc comment。
pub fn read_remote(root: &StorageRoot) -> Result<BTreeMap<String, RemoteState>, HubError> {
    let mut result = BTreeMap::new();
    let index_dir = root.path().join(layout::INDEX_DIR);

    for shard in read_dir_sorted(&index_dir)? {
        for record_path in read_dir_sorted(&shard)? {
            let text = fs::read_to_string(&record_path).map_err(|e| io_err(&record_path, e))?;
            let record = IndexRecord::parse(&text).map_err(|e| HubError::CorruptIndex {
                path: record_path.display().to_string(),
                reason: e.to_string(),
            })?;
            let state = read_current_version(root, &record)?;
            result.insert(record.path, state);
        }
    }
    Ok(result)
}

/// 读一个 item 的版本链，取最后一条作为当前版本。
fn read_current_version(root: &StorageRoot, record: &IndexRecord) -> Result<RemoteState, HubError> {
    let item_id = record.item_id;
    let item_path_rel = layout::item_path(&item_id);
    let full_path = root.path().join(&item_path_rel);

    let text = match fs::read_to_string(&full_path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(HubError::MissingItems {
                path: record.path.clone(),
                item_id: item_id.to_hex(),
            });
        }
        Err(e) => return Err(io_err(&full_path, e)),
    };

    let chain = items::parse_chain(&text).map_err(|e| HubError::CorruptItems {
        item_id: item_id.to_hex(),
        reason: e.to_string(),
    })?;
    let current = chain.last().ok_or_else(|| HubError::CorruptItems {
        item_id: item_id.to_hex(),
        reason: "版本链为空（结构上不应该出现，出现即损坏）".to_string(),
    })?;

    Ok(RemoteState::Present {
        item_id: current.item_id,
        version_id: current.version_id.clone(),
        hash: current.hash,
        size: current.size,
    })
}

/// 排序读目录：使 `read_remote` 的遍历顺序确定。**只把"目录不存在"当作合法的
/// 空状态**（全新存储根还没有任何 `index/` 分片目录时属于正常情况，brief
/// Task 3 的"空存储根"测试点）——除此之外的任何 IO 错误（权限等）都必须
/// 向上传播，不能像 `arca_store::fsck::read_dir_sorted` 那样统一吞成空
/// `Vec`：fsck 是尽量扫、能扫多少算多少的诊断工具，本函数是操作路径，
/// 吞掉一个真实的读取失败等于对调用方谎报"这个分片下没有任何记录"。
fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, HubError> {
    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut paths = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| io_err(dir, e))?;
                paths.push(entry.path());
            }
            paths.sort();
            Ok(paths)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io_err(dir, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_chunk::hash::ContentHash;
    use arca_format::hub_layout::FormatJson;
    use arca_format::model::{Actor, ItemId, Version, VersionId};
    use arca_format::path_rules;

    /// 在 `dir` 下写一个最小合法的 `format.json`，使 `StorageRoot::open` 能打开它。
    fn write_format_json(dir: &Path) {
        let format = FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-05T09:00:00Z".to_string(),
        };
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    /// 写一条 index 记录 + 对应的 items 版本链（单个 upsert 版本）。
    fn write_indexed_item(dir: &Path, path: &str, item_id: ItemId, content: &[u8]) -> Version {
        let hash = ContentHash::from_bytes(content);
        let version = Version {
            version_id: VersionId::new("20260805T093012Z", &"0".repeat(32)).unwrap(),
            item_id,
            parent: None,
            hash,
            size: content.len() as u64,
            mtime: "2026-08-05T09:00:00Z".to_string(),
            actor: Actor {
                account: "bruce".into(),
                device: "mac".into(),
                session: "s1".into(),
            },
            committed_at: "2026-08-05T09:00:05Z".to_string(),
        };

        let item_rel = layout::item_path(&item_id);
        let item_full = dir.join(&item_rel);
        fs::create_dir_all(item_full.parent().unwrap()).unwrap();
        fs::write(
            &item_full,
            format!("{}\n", items::to_line(&version).unwrap()),
        )
        .unwrap();

        let key = path_rules::index_key(path);
        let index_shard = dir.join(".arca/index").join(&key.to_hex()[..2]);
        fs::create_dir_all(&index_shard).unwrap();
        let record = IndexRecord {
            item_id,
            path: path.to_string(),
        };
        fs::write(
            index_shard.join(format!("{}.json", key.to_hex())),
            record.to_json().unwrap(),
        )
        .unwrap();

        version
    }

    fn open(dir: &Path) -> StorageRoot {
        StorageRoot::open(dir, None).unwrap()
    }

    #[test]
    fn 健康存储根读出所有当前版本() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());
        let id_a = ItemId::from_bytes([0x3f; 16]);
        let id_b = ItemId::from_bytes([0x8b; 16]);
        write_indexed_item(dir.path(), "京都/鸭川.png", id_a, b"content a");
        write_indexed_item(dir.path(), "notes/a.md", id_b, b"content b");

        let root = open(dir.path());
        let remote = read_remote(&root).unwrap();

        assert_eq!(remote.len(), 2);
        match remote.get("京都/鸭川.png").unwrap() {
            RemoteState::Present {
                item_id,
                hash,
                size,
                ..
            } => {
                assert_eq!(*item_id, id_a);
                assert_eq!(*hash, ContentHash::from_bytes(b"content a"));
                assert_eq!(*size, "content a".len() as u64);
            }
            other => panic!("应为 Present，实得 {other:?}"),
        }
        assert!(matches!(
            remote.get("notes/a.md").unwrap(),
            RemoteState::Present { .. }
        ));
    }

    #[test]
    fn 空存储根返回空map() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());
        let root = open(dir.path());
        let remote = read_remote(&root).unwrap();
        assert!(remote.is_empty());
    }

    #[test]
    fn 损坏的items版本链报错而不是跳过() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());
        let id = ItemId::from_bytes([0x3f; 16]);
        write_indexed_item(dir.path(), "a.txt", id, b"content");

        // 破坏 items 文件内容。
        let item_full = dir.path().join(layout::item_path(&id));
        fs::write(&item_full, "不是合法的版本记录\n").unwrap();

        let root = open(dir.path());
        let err = read_remote(&root).unwrap_err();
        assert!(matches!(err, HubError::CorruptItems { .. }), "实得 {err:?}");
    }

    #[test]
    fn index指向的item没有版本链文件时报错() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());
        let id = ItemId::from_bytes([0x3f; 16]);

        // 只写 index 记录，不写对应的 items 文件——index 与 items 不一致。
        let key = path_rules::index_key("a.txt");
        let index_shard = dir.path().join(".arca/index").join(&key.to_hex()[..2]);
        fs::create_dir_all(&index_shard).unwrap();
        let record = IndexRecord {
            item_id: id,
            path: "a.txt".to_string(),
        };
        fs::write(
            index_shard.join(format!("{}.json", key.to_hex())),
            record.to_json().unwrap(),
        )
        .unwrap();

        let root = open(dir.path());
        let err = read_remote(&root).unwrap_err();
        match err {
            HubError::MissingItems { path, item_id } => {
                assert_eq!(path, "a.txt");
                assert_eq!(item_id, id.to_hex());
            }
            other => panic!("应为 MissingItems，实得 {other:?}"),
        }
    }

    #[test]
    fn 损坏的index记录报错而不是跳过() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());

        let index_shard = dir.path().join(".arca/index/ff");
        fs::create_dir_all(&index_shard).unwrap();
        fs::write(index_shard.join("ffff.json"), "不是合法json").unwrap();

        let root = open(dir.path());
        let err = read_remote(&root).unwrap_err();
        assert!(matches!(err, HubError::CorruptIndex { .. }), "实得 {err:?}");
    }

    #[test]
    fn 两次读取同一存储根产生完全相同的结果() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());
        write_indexed_item(dir.path(), "a.txt", ItemId::from_bytes([0x11; 16]), b"x");
        write_indexed_item(dir.path(), "b.txt", ItemId::from_bytes([0x22; 16]), b"y");

        let root = open(dir.path());
        let first = read_remote(&root).unwrap();
        let second = read_remote(&root).unwrap();
        assert_eq!(first, second);
    }

    /// 不产出 `Tombstoned`：文档化的已知缺口本身也要有回归测试守着，防止
    /// 未来有人在没有先接上 journal 的情况下，悄悄从 items 链的某个巧合
    /// 状态"猜"出一个 tombstone 来。
    #[test]
    fn 当前实现结构上不会产出tombstoned() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());
        write_indexed_item(dir.path(), "a.txt", ItemId::from_bytes([0x33; 16]), b"z");

        let root = open(dir.path());
        let remote = read_remote(&root).unwrap();
        assert!(remote
            .values()
            .all(|state| !matches!(state, RemoteState::Tombstoned { .. })));
    }
}
