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

use arca_format::error::FormatError;
use arca_format::hub_layout::layout;
use arca_format::model::ItemId;
use arca_store::atomic::{self, AtomicError};
use arca_store::root::StorageRoot;
use serde::{Deserialize, Serialize};
use std::fmt;

const RECORD_VERSION: u32 = 1;

/// 回收站条目的标识：32 位小写十六进制，创建时分配、永不复用——与 `item_id`
/// 同一编码与分配纪律（FORMAT.md §1、§7.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrashId([u8; 16]);

impl TrashId {
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
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
    /// `.meta` 序列化失败。
    Format(FormatError),
}

impl fmt::Display for TrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrashError::Atomic(e) => write!(f, "回收站写入失败：{e}"),
            TrashError::Format(e) => write!(f, "回收站记录序列化失败：{e}"),
        }
    }
}

impl std::error::Error for TrashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TrashError::Atomic(e) => Some(e),
            TrashError::Format(e) => Some(e),
        }
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
}
