//! `index/<xx>/<hash>.json`：路径 → 身份映射（FORMAT.md §6）。
//!
//! 与 items/journal 不同，index 记录不是 append-only 事件流，而是每个索引键
//! （`BLAKE3(小写规范化路径)`）单独一个文件、整体原子替换（tmp → fsync → rename），
//! 从不追加、从不原地改写。`path` 存规范化后的显示路径（保留原始大小写，索引键本身
//! 才是大小写不敏感的那一层，见 [`crate::path_rules::index_key`]）。

use crate::error::FormatError;
use crate::model::ItemId;
use serde::{Deserialize, Serialize};

const RECORD_VERSION: u32 = 1;

/// 一条 index 记录：路径 → 身份映射。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRecord {
    pub item_id: ItemId,
    pub path: String,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u32,
    item_id: String,
    path: String,
}

impl IndexRecord {
    /// 解析。`path` 走 [`crate::path_rules::check`]（而非仅 `normalize`）：index
    /// 文件由 hub 直接落盘，路径字段必须已经是合规的规范化显示路径，容忍非法值
    /// 等于把上游未校验的输入悄悄放进存储层（I5：绝不猜测/绝不尽力解析）。
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let wire: Wire = serde_json::from_str(text).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("JSON 解析失败：{e}"),
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
        let path = crate::path_rules::check(&wire.path)?;
        Ok(IndexRecord { item_id, path })
    }

    /// 序列化为单行 JSON。判断点同 `items::to_line` / `journal::JournalEvent::to_line`：
    /// 返回 `Result` 而非 `unwrap_or_default()`——index 记录是整体原子替换（tmp →
    /// fsync → rename），若序列化失败被吞成空字符串，写下去的就是一个内容为空、
    /// 但文件名仍指向某个哈希的 index 文件，读者会把它当作「该路径存在但记录损坏」，
    /// 比「压根没写」更难诊断。`Wire` 全是标量/字符串字段，`Err` 分支当前不可达，
    /// 保留 `Result` 签名为未来加字段留防线。
    pub fn to_json(&self) -> Result<String, FormatError> {
        let wire = Wire {
            v: RECORD_VERSION,
            item_id: self.item_id.to_hex(),
            path: self.path.clone(),
        };
        serde_json::to_string(&wire).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("index 记录序列化失败：{e}"),
        })
    }
}

/// 宽松提取 `item_id`，**不**要求 `path` 合规——诊断专用。
///
/// 唯一已知调用方是 `arca-store` 的 fsck：当一条 index 记录整体解析失败
/// （[`IndexRecord::parse`] 返回 `Err`）时，fsck 需要判断这条损坏记录到底是不是
/// 当前正在核对的那个 item 的记录，才能报出正确的诊断——`CorruptIndex`（"记录
/// 存在但读不出来"）而不是 `OrphanIndex`（"压根没有这条记录"，二者是不同性质
/// 的故障，绝不可折叠，评审 Important #4）。`item_id` 字段仍然经过完整的编码
/// 校验（复用 [`ItemId::parse`]），只是不再要求 `path` 也合规——因此本函数的
/// 返回值只能用于把问题归因到正确的 item，**绝不能**把它当作可信的路径映射
/// 来源去定位磁盘上的文件（那正是 `IndexRecord::parse` 拒绝的东西）。
pub fn extract_item_id_lenient(text: &str) -> Option<ItemId> {
    #[derive(Deserialize)]
    struct Lenient {
        item_id: Option<String>,
    }
    let raw: Lenient = serde_json::from_str(text).ok()?;
    ItemId::parse(&raw.item_id?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemId;

    #[test]
    fn 索引记录往返一致() {
        let record = IndexRecord {
            item_id: ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            path: "京都/鸭川.png".into(),
        };
        let text = record.to_json().unwrap();
        assert!(!text.contains('\n'), "记录内不得含裸换行");
        assert_eq!(IndexRecord::parse(&text).unwrap(), record);
    }

    #[test]
    fn 拒绝畸形_json() {
        assert!(IndexRecord::parse("").is_err());
        assert!(IndexRecord::parse("{不是json}").is_err());
        assert!(
            IndexRecord::parse(r#"{"v":1,"item_id":"3f2a000000000000000000000000beef"}"#).is_err()
        );
    }

    #[test]
    fn 拒绝不合规路径() {
        let text = r#"{"v":1,"item_id":"3f2a000000000000000000000000beef","path":"../逃逸.png"}"#;
        assert!(IndexRecord::parse(text).is_err());
    }

    #[test]
    fn 拒绝未来的记录版本() {
        let text = r#"{"v":99,"item_id":"3f2a000000000000000000000000beef","path":"a.png"}"#;
        assert!(
            IndexRecord::parse(text).is_err(),
            "高于已知版本必须拒绝（I10）"
        );
    }

    #[test]
    fn 宽松提取在path不合规时仍能拿到item_id() {
        let text = r#"{"v":1,"item_id":"3f2a000000000000000000000000beef","path":"../逃逸.png"}"#;
        assert!(IndexRecord::parse(text).is_err(), "严格解析必须拒绝");
        assert_eq!(
            extract_item_id_lenient(text).map(|id| id.to_hex()),
            Some("3f2a000000000000000000000000beef".to_string()),
            "宽松提取不应因 path 不合规而放弃 item_id"
        );
    }

    #[test]
    fn 宽松提取对完全非法_json_或非法_item_id_返回_none() {
        assert!(extract_item_id_lenient("不是json").is_none());
        assert!(extract_item_id_lenient(r#"{"v":1,"path":"a.png"}"#).is_none());
        assert!(extract_item_id_lenient(r#"{"v":1,"item_id":"zz","path":"a.png"}"#).is_none());
    }

    #[test]
    fn 换行字段被转义而不是裸换行() {
        // 只测 to_json 的转义行为，不走 parse 往返——path_rules::check 本就会拒绝含
        // 裸换行的路径（parse 侧已有『拒绝不合规路径』测试覆盖），这里要验证的是
        // serde_json 序列化本身确实做了转义，而不是原样吐出裸字节。
        let record = IndexRecord {
            item_id: ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            path: "a\nb.png".into(),
        };
        let text = record.to_json().unwrap();
        assert!(!text.contains('\n'), "应转义而不是裸换行：{text}");
        assert!(text.contains("\\n"), "应含转义后的 \\n：{text}");
    }
}
