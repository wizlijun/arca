//! hub 存储根磁盘布局（spec §4.2）：目录常量、`format.json` 卷身份标记。
//!
//! - `files/`：逃生舱（I1），当前版本永远完整平放；
//! - `.arca/{index,items,chunks,journal,trash,uploads,tmp,locks}/`：旁路元数据；
//! - `format.json`：格式版本 + `dataset_id`——卷身份标记（I11：挂载缺失即离线，
//!   绝不把未挂载的卷当空库）。
//!
//! vault 侧 `.arca/` 与 hub 侧 `.arca/` 结构不同，须可区分（§4.3，防误绑）。
//!
//! TODO(M0)：`format.json` 结构、布局常量、两种 `.arca/` 的判别函数。

use crate::error::FormatError;
use crate::model::ItemId;
use serde::{Deserialize, Serialize};

/// `format.json` 的记录格式版本（单条记录的 `"v"` 字段，FORMAT.md §0）。
const RECORD_VERSION: u32 = 1;
/// 存储根格式版本（`"format"` 字段）已知的最高值。二者独立演进，互不覆盖（§0）。
const MAX_FORMAT_VERSION: u32 = 1;
/// v1 唯一认可的哈希算法；其他值一律拒绝，绝不猜测（FORMAT.md §5）。
const HASH_ALGO_BLAKE3: &str = "blake3";

/// `format.json`——hub 存储根的卷身份标记（FORMAT.md §5，I11）。
///
/// `dataset_id` 必须与 hub 配置、客户端绑定请求三方一致；不一致时数据集必须离线，
/// 绝不呈现为空库、绝不触发删除对账——这是本文件存在的唯一理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatJson {
    pub format: u32,
    pub dataset_id: String,
    pub hash_algo: String,
    pub created_at: String,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u32,
    format: u32,
    dataset_id: String,
    hash_algo: String,
    created_at: String,
}

impl FormatJson {
    /// 解析。两个独立的版本校验都必须做（§0：记录版本 `v` 与存储根格式版本
    /// `format` 各自演进，不能只查其中一个）；`hash_algo` 非 `"blake3"` 拒绝，
    /// v1 只认这一种，绝不"尽力"按其他算法解读（I5）。
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let wire: Wire = serde_json::from_str(text).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("format.json 解析失败：{e}"),
        })?;
        if wire.v > RECORD_VERSION {
            return Err(FormatError::UnsupportedVersion {
                found: wire.v,
                max: RECORD_VERSION,
            });
        }
        if wire.format > MAX_FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                found: wire.format,
                max: MAX_FORMAT_VERSION,
            });
        }
        if wire.hash_algo != HASH_ALGO_BLAKE3 {
            return Err(FormatError::Malformed {
                line: 0,
                reason: format!(
                    "不支持的 hash_algo {:?}；v1 只认 {HASH_ALGO_BLAKE3:?}",
                    wire.hash_algo
                ),
            });
        }
        Ok(FormatJson {
            format: wire.format,
            dataset_id: wire.dataset_id,
            hash_algo: wire.hash_algo,
            created_at: wire.created_at,
        })
    }

    /// 序列化为 JSON。判断点同 items/journal/index 三处 `to_line`/`to_json`：返回
    /// `Result` 而非 `unwrap_or_default()`。这里的后果比其他三处更重——`format.json`
    /// 是卷身份标记本身（I11），若序列化失败被静默写成空字符串，磁盘上就会出现一个
    /// 内容为空的 `format.json`；下次挂载时它既不匹配任何 `dataset_id`、又不是
    /// "文件缺失"（缺失有明确的"未初始化"语义），会落进两种处置分支之外的第三种
    /// 未定义状态。`Wire` 全是标量/字符串字段，`Err` 分支当前不可达，保留 `Result`
    /// 签名为未来加字段留防线。
    pub fn to_json(&self) -> Result<String, FormatError> {
        let wire = Wire {
            v: RECORD_VERSION,
            format: self.format,
            dataset_id: self.dataset_id.clone(),
            hash_algo: self.hash_algo.clone(),
            created_at: self.created_at.clone(),
        };
        serde_json::to_string(&wire).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("format.json 序列化失败：{e}"),
        })
    }
}

/// hub 存储根布局：目录/文件名常量与分片路径拼接（FORMAT.md §4）。
///
/// 所有路径都是相对于 `dataset_root` 的相对路径，用 `/` 拼接（不做平台路径转换，
/// 调用方在真正落盘前自行转换为 `PathBuf`）。
pub mod layout {
    use super::ItemId;
    use arca_chunk::hash::ContentHash;

    /// 逃生舱（I1）：当前版本永远完整平放的普通文件树。
    pub const FILES_DIR: &str = "files";
    /// 旁路元数据根目录。
    pub const ARCA_DIR: &str = ".arca";
    /// 卷身份标记（§5）。
    pub const FORMAT_JSON: &str = ".arca/format.json";
    /// 路径 → 身份映射（§6）。
    pub const INDEX_DIR: &str = ".arca/index";
    /// 版本链（§7.1）。
    pub const ITEMS_DIR: &str = ".arca/items";
    /// 块存储（§8）。
    pub const CHUNKS_DIR: &str = ".arca/chunks";
    /// 事件流 + `epoch` 指针文件（§7.2、§4）。
    pub const JOURNAL_DIR: &str = ".arca/journal";
    /// 回收站，M2 定义。
    pub const TRASH_DIR: &str = ".arca/trash";
    /// 上传暂存，M2 定义。
    pub const UPLOADS_DIR: &str = ".arca/uploads";
    /// 写入暂存；孤儿普通文件可安全清除，出现符号链接或目录则启动失败（§4）。
    pub const TMP_DIR: &str = ".arca/tmp";
    /// `arca.lock` + `<id>.txn`，M2 定义。
    pub const LOCKS_DIR: &str = ".arca/locks";

    /// `items/<xx>/<item_id>.jsonl`，`<xx>` 为 `item_id` 前 2 位十六进制。
    pub fn item_path(id: &ItemId) -> String {
        format!("{ITEMS_DIR}/{}/{}.jsonl", id.shard(), id.to_hex())
    }

    /// `index/<xx>/<hash>.json`，`<xx>` 为哈希前 2 位十六进制。
    pub fn index_path(hash: &ContentHash) -> String {
        let hex = hash.to_hex();
        format!("{INDEX_DIR}/{}/{}.json", &hex[..2], hex)
    }

    /// `chunks/<xx>/<hash>.zst`，`<xx>` 为哈希前 2 位十六进制。
    pub fn chunk_path(hash: &ContentHash) -> String {
        let hex = hash.to_hex();
        format!("{CHUNKS_DIR}/{}/{}.zst", &hex[..2], hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemId;

    #[test]
    fn format_json_往返一致() {
        let text = r#"{"v":1,"format":1,"dataset_id":"9c41000000000000000000000000abcd","hash_algo":"blake3","created_at":"2026-08-04T10:00:00Z"}"#;
        let parsed = FormatJson::parse(text).unwrap();
        assert_eq!(parsed.dataset_id, "9c41000000000000000000000000abcd");
        assert_eq!(
            FormatJson::parse(&parsed.to_json().unwrap()).unwrap(),
            parsed
        );
    }

    #[test]
    fn 拒绝未来的格式版本() {
        let text = r#"{"v":1,"format":99,"dataset_id":"a","hash_algo":"blake3","created_at":"t"}"#;
        assert!(
            FormatJson::parse(text).is_err(),
            "高于已知版本必须拒绝（I10）"
        );
    }

    #[test]
    fn 拒绝未知哈希算法() {
        let text = r#"{"v":1,"format":1,"dataset_id":"a","hash_algo":"md5","created_at":"t"}"#;
        assert!(FormatJson::parse(text).is_err());
    }

    #[test]
    fn 分片路径按前两位十六进制() {
        let id = ItemId::parse("3f2a000000000000000000000000beef").unwrap();
        assert_eq!(
            layout::item_path(&id),
            ".arca/items/3f/3f2a000000000000000000000000beef.jsonl"
        );
    }

    #[test]
    fn 拒绝未来的记录版本() {
        let text = r#"{"v":99,"format":1,"dataset_id":"a","hash_algo":"blake3","created_at":"t"}"#;
        assert!(
            FormatJson::parse(text).is_err(),
            "记录 v 字段高于已知版本也必须拒绝，与 format 字段分开校验（FORMAT.md §0）"
        );
    }

    #[test]
    fn 拒绝畸形_json() {
        assert!(FormatJson::parse("").is_err());
        assert!(FormatJson::parse("{不是json}").is_err());
    }

    #[test]
    fn index_path与chunk_path按哈希前两位十六进制分片() {
        let hash = arca_chunk::hash::ContentHash::from_bytes(b"hello");
        let hex = hash.to_hex();
        assert_eq!(
            layout::index_path(&hash),
            format!(".arca/index/{}/{}.json", &hex[..2], hex)
        );
        assert_eq!(
            layout::chunk_path(&hash),
            format!(".arca/chunks/{}/{}.zst", &hex[..2], hex)
        );
    }
}
