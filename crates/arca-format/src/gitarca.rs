//! vault 根注册表 `/.gitarca` 的解析与序列化（spec §4.3）。
//!
//! 只做两件事：声明哪些目录归 arca（`[[dataset]]`：path + hub 名），
//! 以及登记可用的 hub 端点（`[hub.<name>]`：instance_id + url）。
//!
//! 端点无关身份：`instance_id` 是稳定身份，`url` 只是"当前怎么连过去"。
//!
//! TODO(M0)：数据结构定义、TOML 解析/序列化、schema 版本校验、
//! 一致性检查素材（同一路径登记两次 → 拒绝，§4.3.2）。

use crate::error::FormatError;
use serde::{Deserialize, Serialize};

const MAX_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubEntry {
    pub instance_id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetEntry {
    pub path: String,
    pub hub: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub schema: u32,
    #[serde(default)]
    hub: std::collections::BTreeMap<String, HubEntry>,
    #[serde(default)]
    dataset: Vec<DatasetEntry>,
}

impl Registry {
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let registry: Registry = toml::from_str(text).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("TOML 解析失败：{e}"),
        })?;
        if registry.schema > MAX_SCHEMA {
            return Err(FormatError::UnsupportedVersion { found: registry.schema, max: MAX_SCHEMA });
        }
        Ok(registry)
    }

    /// 序列化为 TOML 文本。
    ///
    /// 注意：brief 里的参考实现是 `toml::to_string_pretty(self).unwrap_or_default()`，
    /// 失败时静默退化为空字符串。这里刻意不采用——调用方大概率是
    /// `fs::write(".gitarca", registry.to_toml())`，若序列化失败被吞掉，写回磁盘的
    /// 就是一个空的根注册表，等价于把用户仓库里已登记的全部 hub 与 dataset 条目
    /// 清空，且没有任何报错提示（既违反 I5"绝不猜测"，也违反 I9 的"配置/清单
    /// 优先原始字节"精神）。改为返回 `Result`：失败时把原因带给调用方，由它决定
    /// 是中止写入还是提示用户，而不是 panic（I5 同样禁止 panic）或吞掉错误。
    pub fn to_toml(&self) -> Result<String, FormatError> {
        toml::to_string_pretty(self).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("TOML 序列化失败：{e}"),
        })
    }

    pub fn hub(&self, name: &str) -> Option<&HubEntry> {
        self.hub.get(name)
    }

    pub fn datasets(&self) -> &[DatasetEntry] {
        &self.dataset
    }

    /// spec §4.3.2 的一致性规则：引用存在、路径唯一、不得嵌套。
    /// 违反即拒绝，绝不静默激活（I5）。
    pub fn validate(&self) -> Result<(), FormatError> {
        let mut seen: Vec<String> = Vec::new();
        for entry in &self.dataset {
            if !self.hub.contains_key(&entry.hub) {
                return Err(FormatError::Malformed {
                    line: 0,
                    reason: format!("数据集 {:?} 引用了未登记的 hub {:?}", entry.path, entry.hub),
                });
            }
            let normalized = crate::path_rules::normalize(&entry.path);
            for existing in &seen {
                if existing.as_str() == normalized {
                    return Err(FormatError::Malformed {
                        line: 0,
                        reason: format!("路径 {normalized:?} 被登记了两次"),
                    });
                }
                if normalized.starts_with(&format!("{existing}/"))
                    || existing.starts_with(&format!("{normalized}/"))
                {
                    return Err(FormatError::Malformed {
                        line: 0,
                        reason: format!("数据集 {normalized:?} 与 {existing:?} 嵌套；归属必须唯一"),
                    });
                }
            }
            seen.push(normalized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const 样例: &str = r#"
schema = 1

[hub.home]
instance_id = "3f2a000000000000000000000000beef"
url = "https://nas.example.com:8443"

[[dataset]]
path = "assets"
hub  = "home"
"#;

    #[test]
    fn 解析注册表() {
        let reg = Registry::parse(样例).unwrap();
        assert_eq!(reg.hub("home").unwrap().url, "https://nas.example.com:8443");
        assert_eq!(reg.datasets().len(), 1);
        assert_eq!(reg.datasets()[0].path, "assets");
    }

    #[test]
    fn 拒绝未知_schema_版本() {
        assert!(Registry::parse("schema = 99\n").is_err());
    }

    #[test]
    fn 拒绝引用了不存在的_hub() {
        let text = "schema = 1\n[[dataset]]\npath = \"assets\"\nhub = \"ghost\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(reg.validate().is_err(), "引用不存在的 hub 必须拒绝（spec §4.3.2）");
    }

    #[test]
    fn 拒绝同一路径登记两次() {
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"assets\"\nhub = \"h\"\n\
                    [[dataset]]\npath = \"assets\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(reg.validate().is_err(), "重复路径必须拒绝（spec §4.3.2）");
    }

    #[test]
    fn 拒绝嵌套数据集() {
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"assets\"\nhub = \"h\"\n\
                    [[dataset]]\npath = \"assets/inner\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(reg.validate().is_err(), "归属必须唯一，嵌套拒绝（spec §4.3.2）");
    }

    #[test]
    fn 拒绝损坏的_toml_而不是_panic() {
        assert!(Registry::parse("[[[").is_err());
        assert!(Registry::parse("").is_err());
    }
}
