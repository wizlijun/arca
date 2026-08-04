//! 数据集自描述 `<dataset>/.arca/dataset.toml`（spec §4.3）。
//!
//! 字段：`schema` · `dataset_id`（全局唯一，不可变）· `hub_instance_id`（认身份不认 URL）·
//! `public_base_url` / `url_style`（发布配置，§4.9）。
//!
//! 数据集随 `.arca/` 整体搬迁：身份、清单、目录卡、hub 归属全都跟着走。
//!
//! TODO(M0)：数据结构定义、TOML 解析/序列化。

use crate::error::FormatError;
use serde::{Deserialize, Serialize};

const MAX_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UrlStyle {
    Path,
    Hash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetConfig {
    pub schema: u32,
    pub dataset_id: String,
    pub hub_instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_style: Option<UrlStyle>,
}

impl DatasetConfig {
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let cfg: DatasetConfig = toml::from_str(text).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("TOML 解析失败：{e}"),
        })?;
        if cfg.schema > MAX_SCHEMA {
            return Err(FormatError::UnsupportedVersion { found: cfg.schema, max: MAX_SCHEMA });
        }
        Ok(cfg)
    }

    /// 序列化为 TOML 文本。
    ///
    /// 与 `gitarca::Registry::to_toml` 同理：不用 `unwrap_or_default()` 吞掉
    /// 序列化失败——那会让 `<dataset>/.arca/dataset.toml` 被静默写成空文件，
    /// 丢失 `dataset_id`/`hub_instance_id` 这类不可变身份信息，且调用方毫无察觉
    /// （违反 I5）。改为返回 `Result`，失败原因交给调用方处理。
    ///
    /// `Err` 分支目前不可达：字段全是标量（`u32`/`String`/`Option<String>`/
    /// `Option<UrlStyle>`），没有表或表数组，`toml` crate 序列化任意取值都不会失败。
    /// 保留 `Result` 是为未来加字段留防线，没有为 `Err` 分支单独写测试，用这条注释
    /// 说明判断依据，避免它"静默地无人验证"。
    pub fn to_toml(&self) -> Result<String, FormatError> {
        toml::to_string_pretty(self).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("TOML 序列化失败：{e}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析数据集配置() {
        let text = r#"
schema = 1
dataset_id = "9c41000000000000000000000000abcd"
hub_instance_id = "3f2a000000000000000000000000beef"
public_base_url = "https://cdn.example.com/assets"
url_style = "path"
"#;
        let cfg = DatasetConfig::parse(text).unwrap();
        assert_eq!(cfg.dataset_id, "9c41000000000000000000000000abcd");
        assert_eq!(cfg.url_style, Some(UrlStyle::Path));
    }

    #[test]
    fn 发布配置可缺省() {
        let text = "schema = 1\ndataset_id = \"9c41000000000000000000000000abcd\"\n\
                    hub_instance_id = \"3f2a000000000000000000000000beef\"\n";
        let cfg = DatasetConfig::parse(text).unwrap();
        assert!(cfg.public_base_url.is_none());
    }

    #[test]
    fn 拒绝未知_url_style_而不是猜测() {
        let text = "schema = 1\ndataset_id = \"a\"\nhub_instance_id = \"b\"\nurl_style = \"magic\"\n";
        assert!(DatasetConfig::parse(text).is_err());
    }

    #[test]
    fn 拒绝缺失必填字段() {
        assert!(DatasetConfig::parse("schema = 1\n").is_err());
    }

    #[test]
    fn 往返序列化保持一致() {
        let text = r#"
schema = 1
dataset_id = "9c41000000000000000000000000abcd"
hub_instance_id = "3f2a000000000000000000000000beef"
public_base_url = "https://cdn.example.com/assets"
url_style = "hash"
"#;
        let cfg = DatasetConfig::parse(text).unwrap();
        let serialized = cfg.to_toml().unwrap();
        let reparsed = DatasetConfig::parse(&serialized).unwrap();
        assert_eq!(cfg, reparsed);
    }
}
