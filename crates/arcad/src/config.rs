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

/// TLS 配置（M2e Task 4，spec §9）——**可选**：不配置就是明文 `http://`
/// （本机/内网场景合法，M2b/M2c 一路就是这么跑的）。
///
/// 两项必须同时给出：只给证书不给私钥（或反过来）是配置错误，拒绝启动而
/// 不是"忽略 TLS 继续用明文起"——后者会让运维以为自己已经在 TLS 后面，
/// 而实际上所有流量都是明文（I5：绝不静默降级到一个更弱的保证）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    /// PEM 证书链文件路径（自签名时就是那一张叶子证书）。
    pub cert: PathBuf,
    /// PEM 私钥文件路径（PKCS#8 / PKCS#1 / SEC1 皆可）。
    pub key: PathBuf,
}

/// 解析后的 `hub.toml`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubConfig {
    pub instance_id: String,
    pub datasets: Vec<DatasetConfig>,
    /// `None` = 明文 http://（合法且是默认）；`Some` = 启用 TLS。
    pub tls: Option<TlsConfig>,
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
    /// `[tls]` 只给了 `cert` 或只给了 `key`——见 [`TlsConfig`] 的文档。
    IncompleteTls { missing: &'static str },
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
            ConfigError::IncompleteTls { missing } => write!(
                f,
                "[tls] 缺少 {missing}——证书与私钥必须同时给出。拒绝启动，绝不「忽略 TLS \
                 继续用明文起」：那会让你以为流量已经加密，而实际上全是明文（I5）。"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

// **评审 Minor 项**：`deny_unknown_fields`——修复前两个结构体都没有这道
// 校验，`hub.toml` 里任何拼错的键（典型例子：把 `[[dataset]]` 误写成
// `[[datasets]]`，多了一个 `s`）都会被 `toml`/`serde` 静默忽略：`dataset`
// 字段的 `#[serde(default)]` 让它退化成空 `Vec`，`arcad --check` 因此零个
// 数据集、零输出、exit 0——运维会以为"配置没问题、就是没数据集"，而不是
// "配置写错了一个字母"。M2d 计划引入多卷映射后，这类静默面只会更危险
// （更多字段、更多拼错的机会），这里先把它收紧：任何未识别的键在解析阶段
// 就报错，不留到运行期才发现"数据集怎么没起来"。
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    instance_id: String,
    #[serde(default)]
    dataset: Vec<DatasetWire>,
    #[serde(default)]
    tls: Option<TlsWire>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TlsWire {
    #[serde(default)]
    cert: Option<PathBuf>,
    #[serde(default)]
    key: Option<PathBuf>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
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

        let tls = match wire.tls {
            None => None,
            Some(t) => match (t.cert, t.key) {
                (Some(cert), Some(key)) => Some(TlsConfig { cert, key }),
                (None, Some(_)) => return Err(ConfigError::IncompleteTls { missing: "cert" }),
                (Some(_), None) => return Err(ConfigError::IncompleteTls { missing: "key" }),
                // `[tls]` 空表：等价于没写，按明文处理（不是错误——一个空
                // 的表段是运维在注释掉配置时的常见中间态）。
                (None, None) => None,
            },
        };

        Ok(HubConfig {
            instance_id: wire.instance_id,
            datasets,
            tls,
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

    /// **评审 Minor 攻击重跑**：`[[dataset]]` 误写成 `[[datasets]]`（多了
    /// 一个 `s`）此前会被 `#[serde(default)]` 的空 `Vec` 静默吞掉——
    /// `arcad --check` 零输出、exit 0，服务启动后零个数据集，运维毫无
    /// 察觉。修复后（`deny_unknown_fields`）必须报错，不能悄悄当作"没有
    /// 数据集"放行。
    #[test]
    fn 拒绝拼错的顶层键_datasets多了一个s() {
        let text = r#"
instance_id = "0123456789abcdef0123456789abcdef"
[[datasets]]
id = "9c41000000000000000000000000abcd"
path = "/srv/arca/notes"
"#;
        let err = HubConfig::parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::Toml { .. }), "实得 {err:?}");
    }

    /// 同一纪律延伸到数据集记录内部的字段——`id` 拼成 `ids` 之类同样不该
    /// 被静默忽略（那会让这条记录在实际语义上"路径没有 id"，但因为
    /// `id: String` 是必填字段，这种情形本来就会因缺字段而报错；这里额外
    /// 覆盖的是"多了一个不认识的字段"这种此前会被吞掉的输入）。
    #[test]
    fn 拒绝数据集记录里多出的未知字段() {
        let text = r#"
instance_id = "0123456789abcdef0123456789abcdef"
[[dataset]]
id = "9c41000000000000000000000000abcd"
path = "/srv/arca/notes"
extra_unknown_field = "surprise"
"#;
        let err = HubConfig::parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::Toml { .. }), "实得 {err:?}");
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
