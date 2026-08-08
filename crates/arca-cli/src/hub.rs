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
//! # 现在如何产出 `RemoteState::Tombstoned`（M2a tombstone 计划 Task 3）
//!
//! `arca_format::items` 的版本链**只记录 upsert 形状的记录**：FORMAT.md
//! §7.2 明文规定 `tombstone`/`rename` 都不在 `items/<item_id>.jsonl` 产生新
//! 版本（`version_id` 沿用改动前最后一个存活版本的 id），tombstone 的权威记录
//! 只存在于 `journal/<epoch>.jsonl` 的 `op:"tombstone"` 事件里——所以只靠
//! `index/`+`items/` 这两个数据源，结构上分辨不出「这个路径被删除了」与
//! 「这个路径从来没有过」，M1 因此把 `RemoteState::Tombstoned` 留成一个
//! 刻意保留、当时不可达的变体（`arca_core::reconcile` 决策表依赖它的分支
//! 完整写好并被属性测试覆盖，只是没有任何调用方能喂出这个输入）。
//!
//! 本函数现在多读一处：整段 journal（`crate::journal::read_all`），算出
//! 「每个 `item_id` 最后一条事件是不是 tombstone」，据此在结果里插入
//! `RemoteState::Tombstoned`——`arca-core` 一行都不用改，这正是当初把决策
//! 与执行分开的收益。
//!
//! # 两种「这个路径被 tombstone 了」的磁盘证据，一律优先信 journal
//!
//! - **`index/` 记录已经被清理**（tombstone 执行的一部分，本切片不实现
//!   执行本身，由后续 M2a 切片补上）：这个路径压根不出现在 `index/` 里，
//!   只能靠 journal 才知道它曾经存在过、现在被删了——本函数在遍历完
//!   `index/` 之后，为每个「最后一条事件是 tombstone、且它记录的路径当前
//!   没有存活 index 记录认领」的 item 补一条 `Tombstoned`。
//! - **`index/` 记录还没被清理**（tombstone 执行落在清理 `index/` 之前的
//!   崩溃窗口，或那一步压根还没实现）：这个路径仍有一条指向该 item 的
//!   index 记录，若继续走旧逻辑（读 items 链 + 探测 `files/` 内容），会因为
//!   内容已经被移进 `.arca/trash/`（`crate::trash::move_to_trash`）而报
//!   `HubError::MissingContent`——那是把"刚执行完 tombstone"误诊断成
//!   "存储根损坏"。所以本函数现在**先查这条 index 记录的 `item_id` 是否
//!   已被 journal 判定为 tombstone，查到就直接产出 `Tombstoned`，根本不去
//!   碰 `files/`**；`index/` 记录本身是否已被清理不影响这个判断的正确性。
//!
//! 一个路径先后被多个 item 占用（删除后用同名路径新建、又再次删除）时，
//! 以 journal 里 **`seq` 更大**（更晚）的那条 tombstone 为准——旧 item 的
//! tombstone 是历史，不应该盖过路径的当前归属。
//!
//! # `RemoteState::Present` 必须以 `files/<path>` 实际存在为前提（评审 Critical #1）
//!
//! `index/` + `items/` 只是**指针**：它们说"这个路径映射到这个 item，这个
//! item 的当前版本是这个哈希"，但指针本身不携带字节。`sync.rs` 现在把写入
//! 顺序改成了 `files/` → `items/` → `index/`（内容先于指针发布），但这只
//! 处理了"新写入"这一侧——**已经存在的存储根仍可能因为 ENOSPC、拔盘、或
//! 用户直接手动动过 `files/`（I1 本就鼓励这样做）而处于"指针完整、内容
//! 缺失"的状态**。若 `read_remote` 只凭 `index/`+`items/` 就产出 `Present`，
//! 调用方（`sync`/`status`/`doctor`/`ls`/`cat`/`resolve`）会把这当成"这个
//! 文件已经在 hub"，最危险的连带后果是 `sync` 的零传输认领路径
//! （`Action::AdoptBaseline`）：本地有同名同内容的文件，`sync` 因此把基线
//! 写成"已同步"，而 hub 端根本没有字节——用户据此放心删除本地副本，
//! 内容两边都没了。所以这里必须把"内容是否存在"纳入 `Present` 的前提，
//! 读不到内容不是"这个文件不存在"（那会让下次 sync 静默重新上传，看似
//! 自愈，实则掩盖了存储根损坏），是 [`HubError::MissingContent`]——一种
//! 损坏，按 I5 如实报告，整体停下。

use arca_core::state::RemoteState;
use arca_format::hub_layout::layout;
use arca_format::index::IndexRecord;
use arca_format::items;
use arca_format::journal::{JournalEvent, Op};
use arca_format::model::{ItemId, VersionId};
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
    /// index/items 两个指针都完整、互相一致，但它们指向的内容在
    /// `files/<path>` 下缺失——**这是损坏，不是"没有这个文件"**（评审
    /// Critical #1，见模块顶部 doc comment）。
    MissingContent { path: String, item_id: String },
    /// 读取 `.arca/index/`、`.arca/items/` 或 `files/` 目录本身失败，且不是
    /// "目录不存在"（真正的 IO 故障：权限、路径某一级类型不对等）。
    Io { path: String, reason: String },
    /// 读 journal（用于判断某个 item 是否已被 tombstone）失败——journal 是
    /// 真相源，读错等于伪造历史，见 `crate::journal` 的损坏处置纪律。
    Journal(crate::journal::JournalError),
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
            HubError::MissingContent { path, item_id } => write!(
                f,
                "index/items 记录 {path}（item_id {item_id}）完整一致，但 files/{path} 缺失——\
                 存储根损坏，绝不当作\"文件不存在\"静默处理"
            ),
            HubError::Io { path, reason } => write!(f, "读取 {path} 失败：{reason}"),
            HubError::Journal(e) => write!(f, "读取 journal 失败：{e}"),
        }
    }
}

impl std::error::Error for HubError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HubError::Journal(e) => Some(e),
            _ => None,
        }
    }
}

fn io_err(path: &Path, e: io::Error) -> HubError {
    HubError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// 一个 item 最后一条 journal 事件若是 tombstone，这里记下判断
/// `RemoteState::Tombstoned` 与解决"同一路径被多个 item 先后占用"所需的全部
/// 信息。`seq` 只用于跨 item 比较新旧，不出现在最终的 `RemoteState` 里。
#[derive(Debug, Clone)]
struct TombstoneInfo {
    item_id: ItemId,
    version_id: VersionId,
    path: String,
    seq: u64,
}

/// 读整段 journal，算出「每个 `item_id` 最后一条事件是不是 tombstone」。
///
/// journal 事件按 `seq` 单调递增追加（`crate::journal::read_all` 已经保证
/// 这一点），所以只需按顺序遍历、对同一个 `item_id` 反复覆盖，最后留下的
/// 自然就是最后一条事件——不需要额外排序。一个 `item_id` 一旦 tombstone，
/// 它的血脉就此终结（spec §4.1：删除后重建 = 新身份，走全新 `item_id`），
/// 不存在"后来又被 upsert 复活"的情形，所以这里"最后一条是不是 tombstone"
/// 足以唯一确定这个 item 的终态。
fn tombstoned_by_item(events: &[JournalEvent]) -> BTreeMap<ItemId, TombstoneInfo> {
    let mut last_by_item: BTreeMap<ItemId, &JournalEvent> = BTreeMap::new();
    for event in events {
        last_by_item.insert(event.item_id, event);
    }
    last_by_item
        .into_iter()
        .filter_map(|(item_id, event)| {
            if event.op == Op::Tombstone {
                Some((
                    item_id,
                    TombstoneInfo {
                        item_id,
                        version_id: event.version_id.clone(),
                        path: event.path.clone(),
                        seq: event.seq,
                    },
                ))
            } else {
                None
            }
        })
        .collect()
}

/// 从一个已打开、身份已确认的存储根读出「每个当前受管路径的远端状态」。
///
/// 只读：不修改、不创建任何文件。路径不在返回的 map 里，调用方按
/// `RemoteState::Absent` 处理——`arca_core::decide` 本就以此为默认（见
/// `arca_core::state::RemoteState`）。产出按路径排序的 `BTreeMap`：同一存储根
/// 状态两次调用必须得到同一份结果。
///
/// **现在会产出 `RemoteState::Tombstoned`**——做法与两种磁盘证据的优先级见
/// 模块顶部 doc comment。
pub fn read_remote(root: &StorageRoot) -> Result<BTreeMap<String, RemoteState>, HubError> {
    let (_cursor, journal_events) = crate::journal::read_all(root).map_err(HubError::Journal)?;
    let tombstoned = tombstoned_by_item(&journal_events);

    let mut result = BTreeMap::new();
    let mut claimed_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let index_dir = root.path().join(layout::INDEX_DIR);

    for shard in read_dir_sorted(&index_dir)? {
        for record_path in read_dir_sorted(&shard)? {
            let text = fs::read_to_string(&record_path).map_err(|e| io_err(&record_path, e))?;
            let record = IndexRecord::parse(&text).map_err(|e| HubError::CorruptIndex {
                path: record_path.display().to_string(),
                reason: e.to_string(),
            })?;
            claimed_paths.insert(record.path.clone());

            let state = if let Some(info) = tombstoned.get(&record.item_id) {
                // index/ 记录还没被清理（tombstone 执行落在清理之前的崩溃
                // 窗口，或那一步还没实现）——journal 说了算，绝不去碰
                // files/（内容已经被移进 .arca/trash/，探测会误报
                // MissingContent，见模块顶部 doc comment）。
                RemoteState::Tombstoned {
                    item_id: info.item_id,
                    version_id: info.version_id.clone(),
                }
            } else {
                read_current_version(root, &record)?
            };
            result.insert(record.path, state);
        }
    }

    // index/ 记录已经被清理、只剩 journal 能证明"这个路径曾经存在过、现在
    // 被删了"的情形：为每个最后一条事件是 tombstone、且没有存活 index 记录
    // 认领同一路径的 item 补一条 Tombstoned。多个 item 先后占用同一路径时，
    // 以 seq 更大（更晚）的那条为准（模块顶部 doc comment）。
    let mut by_path: BTreeMap<String, TombstoneInfo> = BTreeMap::new();
    for info in tombstoned.into_values() {
        if claimed_paths.contains(&info.path) {
            continue; // 已经在上面的循环里按 index 记录处理过。
        }
        match by_path.get(&info.path) {
            Some(existing) if existing.seq >= info.seq => {}
            _ => {
                by_path.insert(info.path.clone(), info);
            }
        }
    }
    for (path, info) in by_path {
        result.insert(
            path,
            RemoteState::Tombstoned {
                item_id: info.item_id,
                version_id: info.version_id,
            },
        );
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

    // 指针完整不等于内容存在（评审 Critical #1，见模块顶部 doc comment）：
    // `files/<path>` 是权威内容的唯一落点（I1），index/items 只是指向它的
    // 指针。用 `symlink_metadata`（不跟随链接）只探测"这个位置有没有东西"，
    // 不读取内容本身——存在性检查不需要打开文件。
    let files_path = root
        .path()
        .join(format!("{}/{}", layout::FILES_DIR, record.path));
    match fs::symlink_metadata(&files_path) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(HubError::MissingContent {
                path: record.path.clone(),
                item_id: item_id.to_hex(),
            });
        }
        Err(e) => return Err(io_err(&files_path, e)),
    }

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

    /// 写一条 index 记录 + 对应的 items 版本链（单个 upsert 版本）+ `files/`
    /// 下的实际内容——三者合起来才是"这个路径在 hub 侧完整存在"（评审
    /// Critical #1：`read_remote` 现在要求内容真的在场，测试 fixture 也必须
    /// 反映这一点，否则会掩盖 `MissingContent` 这类回归）。
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

        let files_full = dir.join(layout::FILES_DIR).join(path);
        fs::create_dir_all(files_full.parent().unwrap()).unwrap();
        fs::write(&files_full, content).unwrap();

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

    /// 评审 Critical #1 的核心复现测试：手工造出「index/items 完整、
    /// `files/<path>` 内容缺失」的存储根——用户直接删了 `files/` 下的字节
    /// （ENOSPC、拔盘、或 I1 鼓励的手动操作都可能造出这个状态），指针仍然
    /// 完整。`read_remote` 绝不能把这当成"这个路径没有记录"（那会让 sync
    /// 的零传输认领路径把谎言写进基线），必须报 `MissingContent`。
    #[test]
    fn index与items完整但files内容缺失时报错而不是当成不存在() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());
        let id = ItemId::from_bytes([0x77; 16]);
        write_indexed_item(dir.path(), "c.bin", id, b"precious content");

        // 模拟"内容缺失，指针完好"：删掉 files/ 下的字节，index/items 原封不动。
        let files_path = dir.path().join("files/c.bin");
        assert!(files_path.is_file(), "测试前置条件：内容应先被写出");
        fs::remove_file(&files_path).unwrap();

        let root = open(dir.path());
        let err = read_remote(&root).unwrap_err();
        match err {
            HubError::MissingContent { path, item_id } => {
                assert_eq!(path, "c.bin");
                assert_eq!(item_id, id.to_hex());
            }
            other => panic!("应为 MissingContent，实得 {other:?}"),
        }
    }

    /// 健康存储根、journal 里没有任何 tombstone 事件时，`read_remote` 不应
    /// 凭空产出 `Tombstoned`——回归测试防止未来有人从 items 链的某个巧合
    /// 状态"猜"出一个 tombstone 来（journal 才是唯一的证据来源）。
    #[test]
    fn journal没有tombstone事件时不产出tombstoned() {
        let dir = tempfile::tempdir().unwrap();
        write_format_json(dir.path());
        write_indexed_item(dir.path(), "a.txt", ItemId::from_bytes([0x33; 16]), b"z");

        let root = open(dir.path());
        let remote = read_remote(&root).unwrap();
        assert!(remote
            .values()
            .all(|state| !matches!(state, RemoteState::Tombstoned { .. })));
    }

    // -----------------------------------------------------------------
    // M2a tombstone 计划 Task 3：journal 接上之后 `RemoteState::Tombstoned`
    // 第一次可达。
    // -----------------------------------------------------------------

    fn 造存储根(dir: &Path) {
        write_format_json(dir);
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        fs::create_dir_all(dir.join(".arca/trash")).unwrap();
        fs::create_dir_all(dir.join(".arca/journal")).unwrap();
    }

    fn actor() -> Actor {
        Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        }
    }

    /// 手工移除某个路径在 `.arca/index/` 下的记录文件——模拟"tombstone 执行
    /// 已经把 index 记录清理掉"这一步（真正的执行流程属 Task 4，这里只手工
    /// 拼出执行完成后的存储根状态）。与 `sync.rs` 里
    /// `崩溃在index写入之前遗留的孤儿字节...` 测试用的是同一个手法。
    fn remove_index_record(dir: &Path, path: &str) {
        let key = path_rules::index_key(path);
        let shard = dir.join(".arca/index").join(&key.to_hex()[..2]);
        let record_path = shard.join(format!("{}.json", key.to_hex()));
        fs::remove_file(record_path).unwrap();
    }

    /// brief 明确要求的场景：写 tombstone 后 `read_remote` 产出 `Tombstoned`，
    /// `files/` 下内容已不在但 `.arca/trash/` 里在。这里模拟一次"完整执行"
    /// （内容移入 trash + 追加 tombstone 事件 + 清理 index 记录），验证
    /// `read_remote` 的输出与磁盘上的痕迹都符合预期。
    #[test]
    fn 写tombstone后read_remote产出tombstoned且files内容已移入trash() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let id = ItemId::from_bytes([0x55; 16]);
        let version = write_indexed_item(dir.path(), "a.png", id, b"content");
        let root = open(dir.path());

        crate::trash::move_to_trash(&root, "a.png", id, "2026-08-08T09:10:00Z").unwrap();
        let event = JournalEvent {
            seq: 1,
            op: Op::Tombstone,
            item_id: id,
            version_id: version.version_id.clone(),
            path: "a.png".to_string(),
            from: None,
            actor: actor(),
            at: "2026-08-08T09:10:00Z".to_string(),
        };
        crate::journal::append(&root, &event).unwrap();
        remove_index_record(dir.path(), "a.png");

        let remote = read_remote(&root).unwrap();
        match remote.get("a.png") {
            Some(RemoteState::Tombstoned {
                item_id,
                version_id,
            }) => {
                assert_eq!(*item_id, id);
                assert_eq!(*version_id, version.version_id);
            }
            other => panic!("应为 Tombstoned，实得 {other:?}"),
        }

        assert!(
            !dir.path().join("files/a.png").exists(),
            "files/ 下的内容应已移走"
        );
        let has_trash_data = fs::read_dir(dir.path().join(".arca/trash"))
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".data"));
        assert!(has_trash_data, "内容应能在 .arca/trash/ 下找到");
    }

    /// 崩溃窗口场景：tombstone 已经执行（内容进了 trash、journal 也写了），
    /// 但清理 `index/` 记录那一步还没发生（或者压根没实现）——`read_remote`
    /// 必须仍然优先信 journal，产出 `Tombstoned`，而不是去读 `files/` 内容
    /// 报出 `MissingContent`（那会把"刚执行完 tombstone"误诊断成"存储根
    /// 损坏"）。
    #[test]
    fn index记录未清理时journal的tombstone仍然优先不报missing_content() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let id = ItemId::from_bytes([0x66; 16]);
        let version = write_indexed_item(dir.path(), "b.png", id, b"content");
        let root = open(dir.path());

        crate::trash::move_to_trash(&root, "b.png", id, "2026-08-08T09:10:00Z").unwrap();
        crate::journal::append(
            &root,
            &JournalEvent {
                seq: 1,
                op: Op::Tombstone,
                item_id: id,
                version_id: version.version_id.clone(),
                path: "b.png".to_string(),
                from: None,
                actor: actor(),
                at: "2026-08-08T09:10:00Z".to_string(),
            },
        )
        .unwrap();
        // 刻意不调用 remove_index_record：index/ 记录仍然在。

        let remote = read_remote(&root).unwrap();
        assert!(
            matches!(remote.get("b.png"), Some(RemoteState::Tombstoned { .. })),
            "实得 {:?}",
            remote.get("b.png")
        );
    }

    /// 同一路径被两个 item 先后占用、且都已经 tombstone、都没有存活的
    /// index 记录时，`read_remote` 必须以 `seq` 更大（更晚）的那条为准——
    /// 旧 item 的 tombstone 是历史，不应该盖过路径的当前归属。
    #[test]
    fn 同一路径被多个item先后占用时以更晚的tombstone为准() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        let item1 = ItemId::from_bytes([0x01; 16]);
        let v1 = VersionId::new("20260808T090000Z", &"1".repeat(32)).unwrap();
        crate::journal::append(
            &root,
            &JournalEvent {
                seq: 1,
                op: Op::Upsert,
                item_id: item1,
                version_id: v1.clone(),
                path: "a.png".into(),
                from: None,
                actor: actor(),
                at: "t1".into(),
            },
        )
        .unwrap();
        crate::journal::append(
            &root,
            &JournalEvent {
                seq: 2,
                op: Op::Tombstone,
                item_id: item1,
                version_id: v1,
                path: "a.png".into(),
                from: None,
                actor: actor(),
                at: "t2".into(),
            },
        )
        .unwrap();

        let item2 = ItemId::from_bytes([0x02; 16]);
        let v2 = VersionId::new("20260808T091000Z", &"2".repeat(32)).unwrap();
        crate::journal::append(
            &root,
            &JournalEvent {
                seq: 3,
                op: Op::Upsert,
                item_id: item2,
                version_id: v2.clone(),
                path: "a.png".into(),
                from: None,
                actor: actor(),
                at: "t3".into(),
            },
        )
        .unwrap();
        crate::journal::append(
            &root,
            &JournalEvent {
                seq: 4,
                op: Op::Tombstone,
                item_id: item2,
                version_id: v2.clone(),
                path: "a.png".into(),
                from: None,
                actor: actor(),
                at: "t4".into(),
            },
        )
        .unwrap();

        let remote = read_remote(&root).unwrap();
        match remote.get("a.png") {
            Some(RemoteState::Tombstoned {
                item_id,
                version_id,
            }) => {
                assert_eq!(*item_id, item2, "应以更晚的 tombstone（item2）为准");
                assert_eq!(*version_id, v2);
            }
            other => panic!("应为 item2 的 Tombstoned，实得 {other:?}"),
        }
    }

    /// brief 明确要求的决策表验证：`decide` 对 `(present, unchanged,
    /// tombstoned)` 给出 `DeleteLocal`——用 `read_remote` 真实产出的
    /// `RemoteState`，不是手写一个假的，确保这条链路真的接通了。
    #[test]
    fn decide对present_unchanged_tombstoned给出deletelocal() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let id = ItemId::from_bytes([0x77; 16]);
        let version = write_indexed_item(dir.path(), "c.png", id, b"content");
        let root = open(dir.path());

        crate::trash::move_to_trash(&root, "c.png", id, "t").unwrap();
        crate::journal::append(
            &root,
            &JournalEvent {
                seq: 1,
                op: Op::Tombstone,
                item_id: id,
                version_id: version.version_id.clone(),
                path: "c.png".to_string(),
                from: None,
                actor: actor(),
                at: "t".to_string(),
            },
        )
        .unwrap();
        remove_index_record(dir.path(), "c.png");

        let remote = read_remote(&root).unwrap();
        let remote_state = remote.get("c.png").unwrap();

        let base = arca_core::state::BaseState::Present {
            item_id: id,
            version_id: version.version_id.clone(),
            hash: version.hash,
            size: version.size,
        };
        let local = arca_core::state::LocalState::Present {
            hash: version.hash,
            size: version.size,
        };
        let decision = arca_core::reconcile::decide(&base, &local, remote_state);
        match decision.action {
            arca_core::reconcile::Action::DeleteLocal { item_id } => assert_eq!(item_id, id),
            other => panic!("应为 DeleteLocal，实得 {other:?}"),
        }
        assert_eq!(decision.reason, "remote_tombstoned");
    }
}
