//! `.arca/trash/` 的写入（M2a tombstone 计划 Task 3，FORMAT.md §7.3）。
//!
//! tombstone **不是删除**：`files/<path>` 下的内容被**移动**（`rename`，同一
//! 文件系统天然原子）进 `.arca/trash/<trash_id>.data`，旁边写一条
//! `.arca/trash/<trash_id>.meta` 记录原逻辑路径、`item_id`、移入时间——保留期
//! 内 `arca restore`（M2a Task 5）能整体找回。绝不 copy+unlink：那会在"复制
//! 完成"与"删除源"之间留一个窗口，窗口内进程崩溃会让同一份内容同时出现在
//! 两个地方（尚可接受）或——如果实现反过来先删后复制——彻底丢失（I3 绝不
//! 接受）。`rename` 没有这个窗口：它在文件系统层面要么完全没发生、要么完全
//! 发生。
//!
//! 落盘走 [`arca_store::atomic::rename`]（移动 `.data`）+
//! [`arca_store::atomic::write`]（写 `.meta`），不在本模块重新实现 fsync
//! 事务链——与 Task 1 的教训同一条纪律：`arca-store` 已经做过的持久化原语，
//! 调用方复用，不复制粘贴。
//!
//! # 写入顺序：`.data` 先于 `.meta`（FORMAT.md §7.3）
//!
//! 与 §6 index 记录"内容先于指针发布"、`sync.rs::execute_upload` 的
//! `files/ → items/ → index/` 顺序同一条纪律：`.data` 先移动到位，`.meta`
//! 后写。崩溃可能留下一个没有 `.meta` 的孤儿 `.data`——内容仍在，只是暂时
//! 找不到它对应哪个路径，无害、可诊断；绝不会留下一个指向不存在内容的
//! `.meta`。

use arca_chunk::hash::ContentHash;
use arca_format::error::FormatError;
use arca_format::hub_layout::layout;
use arca_format::index::IndexRecord;
use arca_format::items;
use arca_format::model::{Actor, ItemId, Version};
use arca_format::path_rules;
use arca_store::atomic::{self, AtomicError};
use arca_store::root::StorageRoot;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

const RECORD_VERSION: u32 = 1;

/// 回收站条目的标识：32 位小写十六进制，创建时分配、永不复用——与 `item_id`
/// 同一编码与分配纪律（FORMAT.md §1、§7.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrashId([u8; 16]);

impl TrashId {
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 解析 32 位小写十六进制——与 [`arca_format::model::ItemId::parse`] 同一
    /// 编码纪律。供 [`list`] 从 `.arca/trash/<trash_id>.meta` 的文件名反推
    /// `trash_id`。
    pub fn parse(text: &str) -> Result<Self, TrashError> {
        let bad = || {
            TrashError::Format(FormatError::Malformed {
                line: 0,
                reason: format!("trash_id {text:?} 不是合法的 32 位小写十六进制"),
            })
        };
        if text.len() != 32 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(bad());
        }
        let mut bytes = [0u8; 16];
        for (i, slot) in bytes.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
        }
        Ok(TrashId(bytes))
    }
}

impl fmt::Display for TrashId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// 回收站写入失败——彼此可区分（I5）。
#[derive(Debug)]
pub enum TrashError {
    /// `.data`/`.meta` 的原子写入/移动失败（`arca_store::atomic` 报告）。
    Atomic(AtomicError),
    /// `.meta` 解析/序列化失败，或 `trash_id` 编码不合法。
    Format(FormatError),
    /// 列出/读取 `.arca/trash/` 时的常规 IO 故障（权限等）——与"记录本身
    /// 读不懂"是不同性质的失败。
    Io { path: String, reason: String },
}

impl fmt::Display for TrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrashError::Atomic(e) => write!(f, "回收站写入失败：{e}"),
            TrashError::Format(e) => write!(f, "回收站记录序列化失败：{e}"),
            TrashError::Io { path, reason } => write!(f, "回收站 {path} 读写失败：{reason}"),
        }
    }
}

impl std::error::Error for TrashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TrashError::Atomic(e) => Some(e),
            TrashError::Format(e) => Some(e),
            TrashError::Io { .. } => None,
        }
    }
}

fn io_err(path: &Path, e: io::Error) -> TrashError {
    TrashError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// `.meta` 记录：原逻辑路径、`item_id`、移入回收站的时刻（FORMAT.md §7.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashMeta {
    pub path: String,
    pub item_id: ItemId,
    pub deleted_at: String,
}

#[derive(Serialize, Deserialize)]
struct MetaWire {
    v: u32,
    path: String,
    item_id: String,
    deleted_at: String,
}

impl TrashMeta {
    fn to_json(&self) -> Result<String, FormatError> {
        let wire = MetaWire {
            v: RECORD_VERSION,
            path: self.path.clone(),
            item_id: self.item_id.to_hex(),
            deleted_at: self.deleted_at.clone(),
        };
        serde_json::to_string(&wire).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!(".meta 序列化失败：{e}"),
        })
    }

    /// 解析 `.meta` 单行 JSON——供 `arca restore`（Task 5）与本模块自身的
    /// 测试往返使用。
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let wire: MetaWire = serde_json::from_str(text).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!(".meta 解析失败：{e}"),
        })?;
        if wire.v > RECORD_VERSION {
            return Err(FormatError::UnsupportedVersion {
                found: wire.v,
                max: RECORD_VERSION,
            });
        }
        let item_id = ItemId::parse(&wire.item_id).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("item_id {:?} 不合法：{e}", wire.item_id),
        })?;
        Ok(TrashMeta {
            path: wire.path,
            item_id,
            deleted_at: wire.deleted_at,
        })
    }
}

fn data_path(id: TrashId) -> String {
    format!("{}/{}.data", layout::TRASH_DIR, id.to_hex())
}

fn meta_path(id: TrashId) -> String {
    format!("{}/{}.meta", layout::TRASH_DIR, id.to_hex())
}

/// 把 `files/<path>` 的内容移进 `.arca/trash/`：分配一个全新 `trash_id`，
/// `rename` 内容到 `<trash_id>.data`，再写 `<trash_id>.meta`（原逻辑路径、
/// `item_id`、`deleted_at`）。返回分配到的 `trash_id`。
///
/// `deleted_at` 由调用方注入（不在本函数内读系统时钟）——与
/// `StorageRoot::create` 的 `created_at`、`items::Version` 的 `committed_at`
/// 同一条纪律：确定性测试需要能固定这个值。
///
/// 调用方须保证 `files/<path>` 存在——不存在时 [`arca_store::atomic::rename`]
/// 会报普通 `Io`（`NotFound`），本函数不做额外包装（见其文档）。
pub fn move_to_trash(
    root: &StorageRoot,
    path: &str,
    item_id: ItemId,
    deleted_at: &str,
) -> Result<TrashId, TrashError> {
    let trash_id = TrashId(crate::ids::random_bytes16());

    let source = format!("{}/{}", layout::FILES_DIR, path);
    atomic::rename(root, &source, &data_path(trash_id)).map_err(TrashError::Atomic)?;

    let meta = TrashMeta {
        path: path.to_string(),
        item_id,
        deleted_at: deleted_at.to_string(),
    };
    let text = meta.to_json().map_err(TrashError::Format)?;
    atomic::write(root, &meta_path(trash_id), text.as_bytes()).map_err(TrashError::Atomic)?;

    Ok(trash_id)
}

/// 一条回收站条目：分配到的 `trash_id` + 对应的 `.meta` 记录。供闸门第 4 道
/// （`gates::check_delete`，保留期存在性核验）与 `arca restore`（Task 5：
/// 定位要找回的内容、`--list` 列出可恢复条目）共用——两者都需要"这个
/// `item_id`/`path` 在回收站里对应哪个 `trash_id`"这同一份事实，不重复实现
/// 一遍目录遍历。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    pub trash_id: TrashId,
    pub meta: TrashMeta,
}

/// 列出 `.arca/trash/` 下全部条目，按 `trash_id` 排序（确定性输出，`arca-cli`
/// 一贯纪律）。
///
/// `.arca/trash/` 目录本身不存在视为空列表——全新存储根、从未发生过删除的
/// 合法状态，不是错误。目录存在但其中某个 `.meta` 文件解析失败（编码不合法
/// 的文件名、或内容损坏）则整体报错，不静默跳过：回收站是"删除是否安全"的
/// 权威依据（闸门第 4 道要靠它判断"内容是否还能取回"），读错一条就是对"保留
/// 期内到底有什么"撒谎，比停下报告更危险（I5）。非 `.meta` 后缀的文件（即
/// `.data` 本身）直接跳过——它们不是记录，是记录所指向的内容。
pub fn list(root: &StorageRoot) -> Result<Vec<TrashEntry>, TrashError> {
    let dir = root.path().join(layout::TRASH_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(&dir, e)),
    };

    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".meta") else {
            continue;
        };
        let trash_id = TrashId::parse(stem)?;
        let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
        let meta = TrashMeta::parse(&text).map_err(TrashError::Format)?;
        out.push(TrashEntry { trash_id, meta });
    }
    out.sort_by(|a, b| a.trash_id.cmp(&b.trash_id));
    Ok(out)
}

/// `.data` 内容此刻是否确实存在——闸门第 4 道在"有一条 `.meta` 记录"之上
/// 再核验一次内容本身没有缺失（`.meta` 先于内容存在的窗口理论上不该出现，
/// 见 [`move_to_trash`] `.data` 先于 `.meta` 的写入顺序纪律；闸门存在的
/// 意义恰恰是不单信一份证据）。用 `symlink_metadata`：只探测"有没有东西"，
/// 不需要读取内容本身，与 `hub.rs::read_current_version` 探测 `files/`
/// 内容存在性的手法一致。
pub fn data_exists(root: &StorageRoot, id: TrashId) -> bool {
    root.path().join(data_path(id)).symlink_metadata().is_ok()
}

/// 读出 `.data` 的原始字节——`arca restore`（Task 5）用它把内容拿回来重新
/// 写进 `files/`。**只读，不删除、不移动**：本切片不实现任何清理回收站的
/// 代码路径，物理销毁只经显式 `arca gc`（I3，M2 后续切片）。
pub fn read_content(root: &StorageRoot, id: TrashId) -> Result<Vec<u8>, TrashError> {
    let path = root.path().join(data_path(id));
    fs::read(&path).map_err(|e| io_err(&path, e))
}

// ---------------------------------------------------------------------------
// `arca restore`（M2a tombstone 计划 Task 5，spec §7）
// ---------------------------------------------------------------------------

/// 恢复失败——彼此可区分（I5）。
#[derive(Debug)]
pub enum RestoreError {
    /// 保留期内找不到这个路径对应的回收站记录——从未删除过，或者已经被
    /// `arca gc`（M2 后续切片，本切片不实现）物理销毁。
    NotFound {
        path: String,
    },
    Trash(TrashError),
    Format(FormatError),
    Journal(crate::journal::JournalError),
    Atomic(AtomicError),
    Io {
        path: String,
        reason: String,
    },
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::NotFound { path } => write!(
                f,
                "{path}：保留期内没有可恢复的回收站记录——从未删除过，或已被 `arca gc` 物理销毁"
            ),
            RestoreError::Trash(e) => write!(f, "{e}"),
            RestoreError::Format(e) => write!(f, "{e}"),
            RestoreError::Journal(e) => write!(f, "{e}"),
            RestoreError::Atomic(e) => write!(f, "{e}"),
            RestoreError::Io { path, reason } => write!(f, "{path}：{reason}"),
        }
    }
}

impl std::error::Error for RestoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RestoreError::Trash(e) => Some(e),
            RestoreError::Format(e) => Some(e),
            RestoreError::Journal(e) => Some(e),
            RestoreError::Atomic(e) => Some(e),
            RestoreError::NotFound { .. } | RestoreError::Io { .. } => None,
        }
    }
}

/// 从 `.arca/trash/` 找回保留期内被删除的内容，写回 `files/<path>`，在
/// journal 追加一条 `op=upsert`（spec §7：保留期内一条命令找回）。
///
/// # `item_id` 决定：延续，不新铸（判断记录，见任务简报要求先停下报告的
/// 那一条——这里给出的是"两者不冲突"的结论，理由如下）
///
/// spec §4.1「删除后重建 = 新身份」针对的是 `arca_core::reconcile` 决策表里
/// `absent|added|tombstoned -> Upload{parent:None}` 这一格：**sync 在调和
/// 时刻**发现本地路径重新有了内容，但系统无法确认这是不是"同一份东西"——
/// 可能是用户把回收站里的照片手工复制回来，也可能只是凑巧用了同一个文件名
/// 存了张完全不相关的新照片。两种意图从磁盘状态上无法区分，只能保守地当作
/// 新身份，绝不能猜测用户想延续哪个历史（I5）。
///
/// `arca restore` 是完全不同的场景：用户**显式敲下这条命令**，而且命令本身
/// 明确指向"回收站里的这一份"——意图已经被消歧义，不存在"猜"的问题。延续
/// `item_id`、只产生新版本，让版本链在这次删除/恢复前后保持连续，这正是
/// "恢复"该有的语义（撤销删除，不是重新创建）；若改为新铸身份，`arca history`
/// 这类工具会把恢复前后的版本链断成两个互不相关的 item，用户凭直觉完全想不
/// 到要去哪找"删除前的历史"。**两条规则不冲突**：它们管的是两个不同的输入
/// 通道（隐式的 sync 观察 vs. 显式的用户命令），§4.1 的"新身份"规则本就是
/// 针对"系统自己拿不准"的场景设计的，`arca restore` 恰恰是"拿得准"的那一种。
///
/// # 命中同一路径的多条历史记录
///
/// 回收站可能残留同一路径的多条历史 tombstone（该路径曾被删除又恢复/重建
/// 又再次删除，且从未 `arca gc` 清理过）。取 `deleted_at` 最晚的一条——用户
/// 敲下 `arca restore <path>` 时，通常想找回的是"最近一次删除"，不是某次
/// 更早的历史（更早的版本仍然留在这个 item 的版本链里，`arca history` 能看）。
///
/// # 只读 trash，不删除 trash 记录
///
/// 本函数只读 `.arca/trash/` 的内容，不删除、不移动它——本切片不做任何过期
/// 清理，物理销毁只经显式 `arca gc`（M2 后续切片，I3）。同一份回收站记录
/// 因此可以被 `restore` 多次（每次都产生一条新版本，指向同一份原始字节）。
pub fn restore(
    root: &StorageRoot,
    path: &str,
    actor: &Actor,
    at: &str,
) -> Result<Version, RestoreError> {
    let entries = list(root).map_err(RestoreError::Trash)?;
    let chosen = entries
        .iter()
        .filter(|e| e.meta.path == path)
        .max_by(|a, b| a.meta.deleted_at.cmp(&b.meta.deleted_at))
        .ok_or_else(|| RestoreError::NotFound {
            path: path.to_string(),
        })?;

    let bytes = read_content(root, chosen.trash_id).map_err(RestoreError::Trash)?;
    let hash = ContentHash::from_bytes(&bytes);
    let size = bytes.len() as u64;
    let item_id = chosen.meta.item_id;

    let parent = last_version_id(root, item_id)?;
    let version_id = crate::ids::new_version_id();
    let version = Version {
        version_id: version_id.clone(),
        item_id,
        parent,
        hash,
        size,
        mtime: at.to_string(),
        actor: actor.clone(),
        committed_at: at.to_string(),
    };

    // 写入顺序：files/ → items/ → index/ → journal（内容先于指针发布，
    // 与 `sync.rs::execute_upload` 同一条纪律——见其文档）。
    let target = format!("{}/{}", layout::FILES_DIR, path);
    atomic::write(root, &target, &bytes).map_err(RestoreError::Atomic)?;
    append_item_line(root, &version)?;
    write_index_record(root, path, item_id)?;

    let seq = crate::journal::next_seq(root).map_err(RestoreError::Journal)?;
    crate::journal::append(
        root,
        &arca_format::journal::JournalEvent {
            seq,
            op: arca_format::journal::Op::Upsert,
            item_id,
            version_id,
            path: path.to_string(),
            from: None,
            actor: actor.clone(),
            at: at.to_string(),
        },
    )
    .map_err(RestoreError::Journal)?;

    Ok(version)
}

/// 读出 `item_id` 版本链的最后一条 `version_id`，作为这次恢复产生的新版本
/// 的 `parent`——延续版本历史，而不是让恢复后的版本变成一条孤立的首版。
/// 链缺失（结构上不该发生：能进回收站的 item 必然曾经有过至少一条 upsert
/// 版本）时返回 `None`，不是错误——防御性地允许恢复继续进行，而不是因为
/// 一处不该出现的缺失就拒绝用户找回内容本身（内容才是用户真正要的东西）。
fn last_version_id(
    root: &StorageRoot,
    item_id: ItemId,
) -> Result<Option<arca_format::model::VersionId>, RestoreError> {
    let rel = layout::item_path(&item_id);
    let full = root.path().join(&rel);
    let text = match fs::read_to_string(&full) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(RestoreError::Io {
                path: full.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    let chain = items::parse_chain(&text).map_err(RestoreError::Format)?;
    Ok(chain.last().map(|v| v.version_id.clone()))
}

/// 追加一条版本记录到 `items/<xx>/<item_id>.jsonl`——与
/// `sync.rs::append_item_version` 同一手法（读现有内容 + 拼接新行 + 整体
/// 原子重写，`arca_store::atomic` 没有原子追加），本函数不用 `Batch`：
/// `restore` 是单条记录的一次性命令，不是批量归档，没有必要引入批次收口
/// 的复杂度。
fn append_item_line(root: &StorageRoot, version: &Version) -> Result<(), RestoreError> {
    let rel = layout::item_path(&version.item_id);
    let full = root.path().join(&rel);
    let mut content = match fs::read_to_string(&full) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(RestoreError::Io {
                path: full.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    content.push_str(&items::to_line(version).map_err(RestoreError::Format)?);
    content.push('\n');
    atomic::write(root, &rel, content.as_bytes()).map_err(RestoreError::Atomic)
}

/// 整体原子替换 `index/<xx>/<key>.json`——与 `sync.rs::write_index_record`
/// 同一手法。恢复后这个路径重新可见（`hub::read_remote` 通过 index 找到它），
/// 且不会被 journal 里更早的 tombstone 事件误判：`hub.rs` 的规则是"以 `seq`
/// 更晚的事件为准"（见其模块文档），这里追加的 upsert 事件 `seq` 必然大于
/// 此前的 tombstone。
fn write_index_record(root: &StorageRoot, path: &str, item_id: ItemId) -> Result<(), RestoreError> {
    let key = path_rules::index_key(path);
    let rel = layout::index_path(&key);
    let record = IndexRecord {
        item_id,
        path: path.to_string(),
    };
    let text = record.to_json().map_err(RestoreError::Format)?;
    atomic::write(root, &rel, text.as_bytes()).map_err(RestoreError::Atomic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::hub_layout::FormatJson;
    use std::fs;
    use std::path::Path;

    fn 造存储根(dir: &Path) {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        fs::create_dir_all(dir.join(".arca/trash")).unwrap();
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

    #[test]
    fn move_to_trash把内容从files移到trash且写出meta() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        fs::write(dir.path().join("files/a.png"), b"photo bytes").unwrap();
        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x3f; 16]);

        let trash_id = move_to_trash(&root, "a.png", item_id, "2026-08-08T09:00:05Z").unwrap();

        assert!(
            !dir.path().join("files/a.png").exists(),
            "files/ 下的内容应已移走"
        );
        let data = fs::read(dir.path().join(format!(".arca/trash/{trash_id}.data"))).unwrap();
        assert_eq!(data, b"photo bytes");

        let meta_text =
            fs::read_to_string(dir.path().join(format!(".arca/trash/{trash_id}.meta"))).unwrap();
        let meta = TrashMeta::parse(&meta_text).unwrap();
        assert_eq!(meta.path, "a.png");
        assert_eq!(meta.item_id, item_id);
        assert_eq!(meta.deleted_at, "2026-08-08T09:00:05Z");
    }

    #[test]
    fn 两次move_to_trash分配不同的trash_id() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        fs::write(dir.path().join("files/a.png"), b"a").unwrap();
        fs::write(dir.path().join("files/b.png"), b"b").unwrap();
        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x11; 16]);

        let id_a = move_to_trash(&root, "a.png", item_id, "t").unwrap();
        let id_b = move_to_trash(&root, "b.png", item_id, "t").unwrap();
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn 源不存在时报错且不产生任何trash文件() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x22; 16]);

        let err = move_to_trash(&root, "不存在.png", item_id, "t").unwrap_err();
        assert!(matches!(err, TrashError::Atomic(_)), "实得 {err:?}");

        let entries: Vec<_> = fs::read_dir(dir.path().join(".arca/trash"))
            .unwrap()
            .collect();
        assert!(entries.is_empty(), "失败时不应留下任何 trash 文件");
    }

    #[test]
    fn meta往返一致() {
        let meta = TrashMeta {
            path: "京都/鸭川.png".to_string(),
            item_id: ItemId::from_bytes([0x77; 16]),
            deleted_at: "2026-08-08T09:00:05Z".to_string(),
        };
        let text = meta.to_json().unwrap();
        assert_eq!(TrashMeta::parse(&text).unwrap(), meta);
    }

    #[test]
    fn trash_id往返一致() {
        let id = TrashId(crate::ids::random_bytes16());
        assert_eq!(TrashId::parse(&id.to_hex()).unwrap(), id);
    }

    #[test]
    fn trash_id拒绝非法编码() {
        assert!(TrashId::parse("").is_err());
        assert!(TrashId::parse("zz").is_err());
        assert!(TrashId::parse(&"A".repeat(32)).is_err(), "大写不接受");
    }

    #[test]
    fn list对空trash目录返回空列表() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        assert!(list(&root).unwrap().is_empty());
    }

    #[test]
    fn list对不存在的trash目录也返回空列表而不是报错() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        fs::remove_dir(dir.path().join(".arca/trash")).unwrap();
        let root = open(dir.path());
        assert!(list(&root).unwrap().is_empty());
    }

    #[test]
    fn list列出全部条目且data_exists与read_content一致() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        fs::write(dir.path().join("files/a.png"), b"a-content").unwrap();
        fs::write(dir.path().join("files/b.png"), b"b-content").unwrap();
        let root = open(dir.path());
        let item_a = ItemId::from_bytes([0x11; 16]);
        let item_b = ItemId::from_bytes([0x22; 16]);

        let id_a = move_to_trash(&root, "a.png", item_a, "t1").unwrap();
        let id_b = move_to_trash(&root, "b.png", item_b, "t2").unwrap();

        let entries = list(&root).unwrap();
        assert_eq!(entries.len(), 2);
        let ids: Vec<TrashId> = entries.iter().map(|e| e.trash_id).collect();
        assert!(ids.contains(&id_a));
        assert!(ids.contains(&id_b));

        for entry in &entries {
            assert!(data_exists(&root, entry.trash_id));
        }
        let content_a = entries
            .iter()
            .find(|e| e.trash_id == id_a)
            .map(|e| read_content(&root, e.trash_id).unwrap())
            .unwrap();
        assert_eq!(content_a, b"a-content");
    }

    #[test]
    fn data_exists对不存在的trash_id返回false() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let phantom = TrashId(crate::ids::random_bytes16());
        assert!(!data_exists(&root, phantom));
    }

    #[test]
    fn list遇到损坏的meta文件时整体报错而不是跳过() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        fs::write(dir.path().join("files/a.png"), b"a").unwrap();
        let root = open(dir.path());
        move_to_trash(&root, "a.png", ItemId::from_bytes([0x11; 16]), "t").unwrap();

        // 手工放一个文件名合法但内容损坏的 .meta。
        let phantom = TrashId(crate::ids::random_bytes16());
        fs::write(
            dir.path().join(format!(".arca/trash/{phantom}.meta")),
            "不是合法json",
        )
        .unwrap();

        let err = list(&root).unwrap_err();
        assert!(matches!(err, TrashError::Format(_)), "实得 {err:?}");
    }

    // -----------------------------------------------------------------
    // `arca restore`（M2a tombstone 计划 Task 5）
    // -----------------------------------------------------------------

    fn actor() -> Actor {
        Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        }
    }

    /// 完整地拼出一次"这个路径已经被 tombstone 执行过"的存储根状态：
    /// 写一条 upsert 版本 + index 记录，再执行一次真正的 tombstone
    /// （move_to_trash + journal tombstone 事件），与 `sync.rs`/`hub.rs`
    /// 测试里手工拼场景用的是同一个手法。返回原始版本，供断言 parent 链接。
    fn 造已被删除的item(
        root: &StorageRoot,
        path: &str,
        item_id: ItemId,
        content: &[u8],
    ) -> Version {
        let hash = ContentHash::from_bytes(content);
        let version = Version {
            version_id: arca_format::model::VersionId::new("20260808T090000Z", &"1".repeat(32))
                .unwrap(),
            item_id,
            parent: None,
            hash,
            size: content.len() as u64,
            mtime: "2026-08-08T09:00:00Z".to_string(),
            actor: actor(),
            committed_at: "2026-08-08T09:00:05Z".to_string(),
        };
        let item_rel = layout::item_path(&item_id);
        fs::create_dir_all(root.path().join(&item_rel).parent().unwrap()).unwrap();
        fs::write(
            root.path().join(&item_rel),
            format!("{}\n", items::to_line(&version).unwrap()),
        )
        .unwrap();
        let key = path_rules::index_key(path);
        let index_shard = root.path().join(".arca/index").join(&key.to_hex()[..2]);
        fs::create_dir_all(&index_shard).unwrap();
        fs::write(
            index_shard.join(format!("{}.json", key.to_hex())),
            IndexRecord {
                item_id,
                path: path.to_string(),
            }
            .to_json()
            .unwrap(),
        )
        .unwrap();
        fs::write(root.path().join(format!("files/{path}")), content).unwrap();

        move_to_trash(root, path, item_id, "2026-08-08T09:10:00Z").unwrap();
        crate::journal::append(
            root,
            &arca_format::journal::JournalEvent {
                seq: 1,
                op: arca_format::journal::Op::Tombstone,
                item_id,
                version_id: version.version_id.clone(),
                path: path.to_string(),
                from: None,
                actor: actor(),
                at: "2026-08-08T09:10:00Z".to_string(),
            },
        )
        .unwrap();
        // 与 hub.rs 真实的 tombstone 执行一致：index 记录本该被清理掉——不清理
        // 也不影响 read_remote 的判断（journal 优先），但 restore 之后要重新
        // 建一条，这里先移除以便测试能验证 restore 确实把它建回来了。
        fs::remove_file(index_shard.join(format!("{}.json", key.to_hex()))).unwrap();

        version
    }

    #[test]
    fn restore把内容写回files并追加新版本_保留item_id延续parent() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x3f; 16]);
        let original = 造已被删除的item(&root, "a.png", item_id, b"photo bytes");

        let restored = restore(&root, "a.png", &actor(), "2026-08-08T09:20:00Z").unwrap();

        assert_eq!(restored.item_id, item_id, "item_id 应当延续，不铸新身份");
        assert_eq!(
            restored.parent,
            Some(original.version_id.clone()),
            "parent 应指向删除前的最后一个版本，版本历史保持连续"
        );
        assert_ne!(
            restored.version_id, original.version_id,
            "应产生一条新版本，不是复活旧版本号"
        );
        assert_eq!(
            fs::read(dir.path().join("files/a.png")).unwrap(),
            b"photo bytes",
            "内容应原样写回 files/"
        );

        // trash 记录本身没有被删除——只读，不清理（本切片不做过期清理）。
        assert!(list(&root)
            .unwrap()
            .iter()
            .any(|e| e.meta.item_id == item_id));
    }

    #[test]
    fn restore后hub侧重新可见且decide不再判定为tombstoned() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x3f; 16]);
        造已被删除的item(&root, "a.png", item_id, b"photo bytes");

        let restored = restore(&root, "a.png", &actor(), "2026-08-08T09:20:00Z").unwrap();

        let remote = crate::hub::read_remote(&root).unwrap();
        match remote.get("a.png") {
            Some(arca_core::state::RemoteState::Present {
                item_id: seen_item,
                version_id,
                ..
            }) => {
                assert_eq!(*seen_item, item_id);
                assert_eq!(*version_id, restored.version_id);
            }
            other => panic!("恢复后应为 Present，实得 {other:?}"),
        }
    }

    #[test]
    fn restore时trash里没有对应路径则报not_found() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        let err = restore(&root, "从未删除过.png", &actor(), "t").unwrap_err();
        assert!(matches!(err, RestoreError::NotFound { .. }), "实得 {err:?}");
    }

    #[test]
    fn restore命中同路径多条历史记录时取最晚删除的一条() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        // 第一次删除：更早的 deleted_at。
        let item_old = ItemId::from_bytes([0x11; 16]);
        let hash_old = ContentHash::from_bytes(b"old content");
        let version_old = Version {
            version_id: arca_format::model::VersionId::new("20260808T080000Z", &"1".repeat(32))
                .unwrap(),
            item_id: item_old,
            parent: None,
            hash: hash_old,
            size: 11,
            mtime: "t".to_string(),
            actor: actor(),
            committed_at: "t".to_string(),
        };
        let item_rel = layout::item_path(&item_old);
        fs::create_dir_all(root.path().join(&item_rel).parent().unwrap()).unwrap();
        fs::write(
            root.path().join(&item_rel),
            format!("{}\n", items::to_line(&version_old).unwrap()),
        )
        .unwrap();
        fs::write(root.path().join("files/a.png"), b"old content").unwrap();
        move_to_trash(&root, "a.png", item_old, "2026-08-08T08:00:00Z").unwrap();

        // 第二次删除（同一路径、不同 item——期间发生过一次重建）：更晚的 deleted_at。
        let item_new = ItemId::from_bytes([0x22; 16]);
        let hash_new = ContentHash::from_bytes(b"new content");
        let version_new = Version {
            version_id: arca_format::model::VersionId::new("20260808T090000Z", &"2".repeat(32))
                .unwrap(),
            item_id: item_new,
            parent: None,
            hash: hash_new,
            size: 11,
            mtime: "t".to_string(),
            actor: actor(),
            committed_at: "t".to_string(),
        };
        let item_rel = layout::item_path(&item_new);
        fs::create_dir_all(root.path().join(&item_rel).parent().unwrap()).unwrap();
        fs::write(
            root.path().join(&item_rel),
            format!("{}\n", items::to_line(&version_new).unwrap()),
        )
        .unwrap();
        fs::write(root.path().join("files/a.png"), b"new content").unwrap();
        move_to_trash(&root, "a.png", item_new, "2026-08-08T09:00:00Z").unwrap();

        let restored = restore(&root, "a.png", &actor(), "2026-08-08T09:20:00Z").unwrap();
        assert_eq!(
            restored.item_id, item_new,
            "应恢复更晚删除的那条历史记录，而不是更早的那条"
        );
        assert_eq!(
            fs::read(dir.path().join("files/a.png")).unwrap(),
            b"new content"
        );
    }

    #[test]
    fn restore两次同一条trash记录各自产生独立新版本() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x3f; 16]);
        造已被删除的item(&root, "a.png", item_id, b"photo bytes");

        let first = restore(&root, "a.png", &actor(), "2026-08-08T09:20:00Z").unwrap();
        // 同一条 trash 记录被恢复两次：本切片不清理 trash（只读），所以第二次
        // `restore` 依然能命中同一条记录；`atomic::write` 对 `files/a.png`
        // 是整体覆盖，不是追加，第二次写入不会与第一次的结果冲突。
        let second = restore(&root, "a.png", &actor(), "2026-08-08T09:22:00Z").unwrap();

        assert_ne!(first.version_id, second.version_id);
        assert_eq!(first.item_id, second.item_id, "两次恢复的都是同一个 item");
    }
}
