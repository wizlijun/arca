//! hub 本机配置 `hub.toml`（不进 vault 仓库，spec §4.6）：
//! `instance_id` 稳定身份 + 每数据集的存储根路径映射（关联认 `dataset_id`）。
//!
//! 参考 lazync：`server/src/nc_server_config.pas`。
//!
//! # 格式（M2b Task 3）
//!
//! ```toml
//! instance_id = "0123456789abcdef0123456789abcdef"
//!
//! [[dataset]]
//! id = "9c41000000000000000000000000abcd"
//! path = "/srv/arca/notes"
//!
//! [[dataset]]
//! id = "a1b2000000000000000000000000c3d4"
//! path = "/srv/arca/photos"
//! ```
//!
//! `instance_id`/`dataset.id` 都是 32 位小写十六进制（与 `item_id`/`dataset_id`
//! 同一编码纪律，FORMAT.md §1，[`arca_format::model::is_hex32`]）——本模块在
//! 解析时就拒绝不合规的编码，不留到 [`crate::storage::Dataset::open`] 才发现
//! （那时已经是每请求的路径，参数错误应该在进程启动时就报出来，不是第一次
//! 请求时才诊断出「配置从一开始就是错的」）。
//!
//! `dataset.id` 重复：拒绝——两个存储根路径映射到同一个 `dataset_id` 是
//! 配置歧义（这个 `dataset_id` 该服务哪个存储根？），不静默取其一。

use arca_format::model::is_hex32;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// 单个数据集的存储根路径映射。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetConfig {
    pub id: String,
    pub path: PathBuf,
}

/// 解析后的 `hub.toml`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubConfig {
    pub instance_id: String,
    pub datasets: Vec<DatasetConfig>,
}

/// 配置读取/解析失败——彼此可区分（I5）。
#[derive(Debug)]
pub enum ConfigError {
    /// 读取 `hub.toml` 本身失败（不存在、权限等）。
    Io { path: String, reason: String },
    /// TOML 语法或字段类型不对。
    Toml { path: String, reason: String },
    /// `instance_id` 不是合法的 32 位小写十六进制。
    BadInstanceId { value: String },
    /// 某个 `dataset.id` 不是合法的 32 位小写十六进制。
    BadDatasetId { value: String },
    /// 两条 `[[dataset]]` 记录用了同一个 `id`——配置歧义，拒绝，不静默取其一。
    DuplicateDatasetId { value: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, reason } => write!(f, "读取 {path} 失败：{reason}"),
            ConfigError::Toml { path, reason } => write!(f, "{path} 不是合法的 TOML：{reason}"),
            ConfigError::BadInstanceId { value } => write!(
                f,
                "instance_id {value:?} 不是合法的 32 位小写十六进制（FORMAT.md §1）"
            ),
            ConfigError::BadDatasetId { value } => write!(
                f,
                "dataset.id {value:?} 不是合法的 32 位小写十六进制（FORMAT.md §1）"
            ),
            ConfigError::DuplicateDatasetId { value } => write!(
                f,
                "dataset.id {value:?} 在 hub.toml 中出现了不止一次——两个存储根不能映射到\
                 同一个 dataset_id，这是配置歧义，拒绝启动"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(serde::Deserialize)]
struct Wire {
    instance_id: String,
    #[serde(default)]
    dataset: Vec<DatasetWire>,
}

#[derive(serde::Deserialize)]
struct DatasetWire {
    id: String,
    path: PathBuf,
}

impl HubConfig {
    /// 从已读入内存的 TOML 文本解析——与文件系统解耦，测试直接构造字符串。
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let wire: Wire = toml::from_str(text).map_err(|e| ConfigError::Toml {
            path: "<内存中的配置文本>".to_string(),
            reason: e.to_string(),
        })?;

        if !is_hex32(&wire.instance_id) {
            return Err(ConfigError::BadInstanceId {
                value: wire.instance_id,
            });
        }

        let mut seen = std::collections::BTreeSet::new();
        let mut datasets = Vec::with_capacity(wire.dataset.len());
        for d in wire.dataset {
            if !is_hex32(&d.id) {
                return Err(ConfigError::BadDatasetId { value: d.id });
            }
            if !seen.insert(d.id.clone()) {
                return Err(ConfigError::DuplicateDatasetId { value: d.id });
            }
            datasets.push(DatasetConfig {
                id: d.id,
                path: d.path,
            });
        }

        Ok(HubConfig {
            instance_id: wire.instance_id,
            datasets,
        })
    }

    /// 从磁盘路径加载并解析。
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Self::parse(&text).map_err(|e| match e {
            ConfigError::Toml { reason, .. } => ConfigError::Toml {
                path: path.display().to_string(),
                reason,
            },
            other => other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
instance_id = "0123456789abcdef0123456789abcdef"

[[dataset]]
id = "9c41000000000000000000000000abcd"
path = "/srv/arca/notes"

[[dataset]]
id = "a1b2000000000000000000000000c3d4"
path = "/srv/arca/photos"
"#;

    #[test]
    fn 解析合法配置() {
        let cfg = HubConfig::parse(VALID).unwrap();
        assert_eq!(cfg.instance_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(cfg.datasets.len(), 2);
        assert_eq!(cfg.datasets[0].id, "9c41000000000000000000000000abcd");
        assert_eq!(cfg.datasets[0].path, PathBuf::from("/srv/arca/notes"));
    }

    #[test]
    fn 没有任何数据集也合法() {
        let cfg = HubConfig::parse(r#"instance_id = "0123456789abcdef0123456789abcdef""#).unwrap();
        assert!(cfg.datasets.is_empty());
    }

    #[test]
    fn 拒绝不合规的_instance_id() {
        let text = r#"instance_id = "not-hex""#;
        assert!(matches!(
            HubConfig::parse(text),
            Err(ConfigError::BadInstanceId { .. })
        ));
    }

    #[test]
    fn 拒绝不合规的_dataset_id() {
        let text = r#"
instance_id = "0123456789abcdef0123456789abcdef"
[[dataset]]
id = "太短"
path = "/srv/arca/notes"
"#;
        assert!(matches!(
            HubConfig::parse(text),
            Err(ConfigError::BadDatasetId { .. })
        ));
    }

    #[test]
    fn 拒绝重复的_dataset_id() {
        let text = r#"
instance_id = "0123456789abcdef0123456789abcdef"
[[dataset]]
id = "9c41000000000000000000000000abcd"
path = "/srv/arca/notes"
[[dataset]]
id = "9c41000000000000000000000000abcd"
path = "/srv/arca/notes-2"
"#;
        assert!(matches!(
            HubConfig::parse(text),
            Err(ConfigError::DuplicateDatasetId { .. })
        ));
    }

    #[test]
    fn 拒绝畸形_toml() {
        assert!(matches!(
            HubConfig::parse("这不是 toml {{{"),
            Err(ConfigError::Toml { .. })
        ));
    }

    #[test]
    fn 拒绝缺少_instance_id() {
        assert!(HubConfig::parse("").is_err());
    }

    #[test]
    fn load_读取磁盘上的配置文件() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.toml");
        fs::write(&path, VALID).unwrap();
        let cfg = HubConfig::load(&path).unwrap();
        assert_eq!(cfg.datasets.len(), 2);
    }

    #[test]
    fn load_对不存在的文件报_io_错误() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert!(matches!(
            HubConfig::load(&path),
            Err(ConfigError::Io { .. })
        ));
    }
}
