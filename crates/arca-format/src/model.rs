//! 三层数据模型：身份 → 版本 → 内容（spec §4.1）。
//!
//! - `ItemId`：随机 128-bit，创建时分配，永不复用；路径是索引键，身份跨改名稳定（I7）；
//! - `Version`：`{version_id, item_id, parent_version, content_hash, size, mtime, actor, committed_at}`，
//!   hub 上线性历史；
//! - `ContentHash`：BLAKE3 原生地址（I2：blob 不可变）；SHA-256 懒计算缓存（互操作，§8）；
//! - `Actor`：账号 + 设备/agent + 会话（I8：每个事件可归因）。
//!
//! 参考 lazync：`shared/src/nc_version.pas` 的版本模型，此处升级为身份/版本/内容三层。
//!
//! TODO(M0)：`ItemId`/`Version` 的 serde 支持、golden vectors（属 Task 5/6/7 范围）。

use crate::error::FormatError;
use arca_chunk::hash::ContentHash;
use serde::{Deserialize, Serialize};

/// 身份：128-bit 随机，创建时分配，永不复用；跨改名稳定（I7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId([u8; 16]);

impl ItemId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        ItemId(bytes)
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn parse(text: &str) -> Result<Self, FormatError> {
        if text.len() != 32 {
            return Err(FormatError::Malformed {
                line: 0,
                reason: format!("item_id 长度为 {}，应为 32", text.len()),
            });
        }
        let mut bytes = [0u8; 16];
        let raw = text.as_bytes();
        for (i, slot) in bytes.iter_mut().enumerate() {
            let hi = lower_hex(raw[i * 2])?;
            let lo = lower_hex(raw[i * 2 + 1])?;
            *slot = (hi << 4) | lo;
        }
        Ok(ItemId(bytes))
    }

    /// 前 2 位十六进制——存储分片目录名（FORMAT.md §4）。
    pub fn shard(&self) -> String {
        format!("{:02x}", self.0[0])
    }
}

fn lower_hex(byte: u8) -> Result<u8, FormatError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(FormatError::Malformed {
            line: 0,
            reason: format!("非小写十六进制字节：{byte:#04x}"),
        }),
    }
}

/// 版本标识：`<紧凑时间戳>-<32 位十六进制随机>`，字典序即时间序。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VersionId(String);

impl VersionId {
    /// `timestamp` 形如 `20260804T102302Z`；`random_hex` 为 32 位小写十六进制。
    ///
    /// 注意（I5）：`timestamp` 长度按字节校验后即按字节切片，若在此处误用 `str` 的
    /// 字符边界切片，对非 ASCII 输入（含多字节 UTF-8 字符）可能因切在字符中间而
    /// panic；因此这里全程使用 `as_bytes()` 得到的字节切片做比较，字节切片按
    /// 索引范围切片不受字符边界约束，只要范围在已校验的长度内就绝不 panic。
    pub fn new(timestamp: &str, random_hex: &str) -> Result<Self, FormatError> {
        let ts_bytes = timestamp.as_bytes();
        let valid_ts = ts_bytes.len() == 16
            && ts_bytes[8] == b'T'
            && ts_bytes[15] == b'Z'
            && ts_bytes[..8].iter().all(|b| b.is_ascii_digit())
            && ts_bytes[9..15].iter().all(|b| b.is_ascii_digit());
        if !valid_ts {
            return Err(FormatError::Malformed {
                line: 0,
                reason: format!("时间戳 {timestamp:?} 不是 YYYYMMDDTHHMMSSZ 形式"),
            });
        }
        if random_hex.len() != 32
            || !random_hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(FormatError::Malformed {
                line: 0,
                reason: "随机段应为 32 位小写十六进制".to_string(),
            });
        }
        Ok(VersionId(format!("{timestamp}-{random_hex}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 事件归因（I8）：账号 + 设备/agent + 会话。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub session: String,
}

/// 一个版本。hub 上的版本链是线性的（spec §4.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub version_id: VersionId,
    pub item_id: ItemId,
    pub parent: Option<VersionId>,
    pub hash: ContentHash,
    pub size: u64,
    pub mtime: String,
    pub actor: Actor,
    pub committed_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_id_往返一致() {
        let id = ItemId::from_bytes([0x3f, 0x2a, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xbe, 0xef]);
        assert_eq!(id.to_hex(), "3f2a000000000000000000000000beef");
        assert_eq!(ItemId::parse(&id.to_hex()).unwrap(), id);
    }

    #[test]
    fn item_id_拒绝非法输入而不是_panic() {
        assert!(ItemId::parse("").is_err());
        assert!(ItemId::parse("3f2a").is_err());                      // 太短
        assert!(ItemId::parse(&"a".repeat(33)).is_err());             // 太长
        assert!(ItemId::parse(&"3F2A".repeat(8)).is_err());           // 大写不接受
        assert!(ItemId::parse("zz2a000000000000000000000000beef").is_err());
    }

    #[test]
    fn version_id_的字典序即时间序() {
        let early = VersionId::new("20260804T102302Z", &"0".repeat(32)).unwrap();
        let late = VersionId::new("20260804T102303Z", &"0".repeat(32)).unwrap();
        assert!(early.as_str() < late.as_str());
    }

    #[test]
    fn version_id_拒绝错误形状() {
        assert!(VersionId::new("2026-08-04T10:23:02Z", &"0".repeat(32)).is_err()); // 非紧凑形式
        assert!(VersionId::new("20260804T102302Z", "abc").is_err());               // 随机段长度不对
    }

    /// 回归测试（I5）：`timestamp` 恰好 16 字节但由多字节 UTF-8 字符组成，
    /// 若实现退化回按字符边界切片 `str`（如 `timestamp[..8]`），当某个多字节
    /// 字符横跨切点时会 panic；本测试确保只返回 `Err`，不 panic。
    #[test]
    fn version_id_对多字节输入不_panic() {
        // "京都鸭" 3 个汉字（各 3 字节）+ "ABCDEFG" 7 个 ASCII 字符 = 16 字节，
        // 第 8 字节落在「鸭」这个字符内部，不是字符边界。
        let timestamp = "京都鸭ABCDEFG";
        assert_eq!(timestamp.len(), 16);
        assert!(VersionId::new(timestamp, &"0".repeat(32)).is_err());
    }

    /// 回归测试（I5）：`item_id` 恰好 32 字节但由多字节 UTF-8 字符组成，确保
    /// `ItemId::parse` 只返回 `Err`，不 panic。
    #[test]
    fn item_id_对多字节输入不_panic() {
        // 10 个汉字（各 3 字节）+ "AB" 2 个 ASCII 字符 = 32 字节。
        let text = "京都鸭川书法兰亭序扫AB";
        assert_eq!(text.len(), 32);
        assert!(ItemId::parse(text).is_err());
    }
}
