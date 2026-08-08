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
use std::collections::BTreeMap;

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
    hub: BTreeMap<String, HubEntry>,
    #[serde(default)]
    dataset: Vec<DatasetEntry>,
}

impl Registry {
    /// 构造一个当前 schema 版本的注册表，供 M1 的 `arca init` / `arca register`
    /// 生成 `.gitarca`（评审指出：只有 `parse` 没有构造器，下游没法生成新注册表）。
    pub fn new(hub: BTreeMap<String, HubEntry>, dataset: Vec<DatasetEntry>) -> Self {
        Self {
            schema: MAX_SCHEMA,
            hub,
            dataset,
        }
    }

    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let registry: Registry = toml::from_str(text).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("TOML 解析失败：{e}"),
        })?;
        if registry.schema > MAX_SCHEMA {
            return Err(FormatError::UnsupportedVersion {
                found: registry.schema,
                max: MAX_SCHEMA,
            });
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
    ///
    /// `Err` 分支目前不可达：字段全是 `u32`/`String`/`BTreeMap<String, _>`/`Vec<_>`，
    /// 且结构体字段顺序是标量（`schema`）在前、表/表数组（`hub`/`dataset`）在后——
    /// 这正是 TOML 对"标量键必须先于表键"的要求，`toml` crate 不会因此报错。保留
    /// `Result` 签名是为未来字段变化留下防线（例如日后调整字段顺序或加入不可序列化
    /// 的类型），而不是当前就能构造出失败样例；因此没有为 `Err` 分支单独写测试，
    /// 用这条注释代替，避免它"静默地无人验证"。
    pub fn to_toml(&self) -> Result<String, FormatError> {
        toml::to_string_pretty(self).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("TOML 序列化失败：{e}"),
        })
    }

    pub fn hub(&self, name: &str) -> Option<&HubEntry> {
        self.hub.get(name)
    }

    /// 遍历全部已登记的 hub 条目（名字 + 内容），按名字字节序（`BTreeMap` 的
    /// 天然迭代顺序）。`Registry` 没有原地修改 API（不变数据 + `Registry::new`
    /// 重建），调用方（`arca-cli` 的 `register`/`init`）想"新增一个 hub、其余
    /// 保持不变"时，必须先能完整读出现有的 hub 集合，再连同新条目一起传给
    /// `Registry::new`——`hub()` 只能按名字单点查询，不够用。
    pub fn hubs(&self) -> impl Iterator<Item = (&str, &HubEntry)> {
        self.hub.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn datasets(&self) -> &[DatasetEntry] {
        &self.dataset
    }

    /// 归一化视图：路径先经 [`crate::path_rules::check`]（与 `validate()` 同一段代码）
    /// 后再返回，绝不是 `datasets()` 里原样保留的用户书写形式。
    ///
    /// **评审 Critical #1 的另一半成因**：`validate()` 算出的归一化路径只用于比较，
    /// 从不写回 `entry.path`——所以一个 `validate()` 判定合规的路径（例如 Windows
    /// 上 `path = "assets\sub"`，`path_rules::normalize` 明确把 `\` 当分隔符接受）
    /// 如果被下游直接当字符串使用（典型场景：拼进 `.gitignore` 反选块），会产出一段
    /// 实际不生效的规则。调用方（如 `arca-git::ignore_block` 生成反选块）应当优先
    /// 用这里的输出而不是 `datasets()`。
    ///
    /// 这不是唯一防线——`ignore_block::render` 自己也会独立校验（生成器只此一处，
    /// 不能指望调用方总是先调用这里）；这个方法存在的意义是让"拿到的路径已经是
    /// 归一化过的"成为默认路径，而不需要每个调用方都重新走一遍 `path_rules::check`。
    ///
    /// 路径本身不合规时返回 `Err`（理应已被 `validate()` 挡在更早的阶段；这里独立
    /// 校验是防御性的，不假设调用方已经 `validate()` 过）。
    pub fn normalized_datasets(&self) -> Result<Vec<DatasetEntry>, FormatError> {
        self.dataset
            .iter()
            .map(|entry| {
                let normalized = crate::path_rules::check(&entry.path)?;
                Ok(DatasetEntry {
                    path: normalized,
                    hub: entry.hub.clone(),
                })
            })
            .collect()
    }

    /// spec §4.3.2 的一致性规则：路径本身合规、引用存在、路径唯一、不得嵌套。
    /// 违反即拒绝，绝不静默激活（I5）。
    ///
    /// `.gitarca` 是用户手编、且直接驱动文件系统访问的文件——`path = "../outside"`
    /// 或 `path = "/etc"` 这类能逃出 vault 的路径声明必须在这里就被拒绝，不能指望
    /// 下游某一层兜底。因此用 `path_rules::check()`（而不是只做规范化的
    /// `normalize()`）：它在返回规范化路径的同时，也拒绝绝对路径、`..` 父引用等
    /// `path_rules::check` 覆盖到的所有非法形态。
    ///
    /// 相等性与嵌套前缀判断都按 [`crate::path_rules::casefold`]（ASCII-only 大小写
    /// 折叠，FORMAT.md §2）比较，而不是原始字符串——macOS / Windows 的文件系统默认
    /// 大小写不敏感，`path = "Assets"` 与 `path = "assets"` 若只比较原始字符串会双双
    /// 通过校验，随后解析到磁盘上同一个目录：两个数据集、两个 hub、一棵树
    /// （评审 Important #11）。错误信息仍然展示用户写的原始大小写（`normalized`），
    /// 只有比较这一步用折叠后的形式。
    pub fn validate(&self) -> Result<(), FormatError> {
        let mut seen: Vec<(String, String)> = Vec::new();
        for entry in &self.dataset {
            if !self.hub.contains_key(&entry.hub) {
                return Err(FormatError::Malformed {
                    line: 0,
                    reason: format!("数据集 {:?} 引用了未登记的 hub {:?}", entry.path, entry.hub),
                });
            }
            let normalized =
                crate::path_rules::check(&entry.path).map_err(|status| FormatError::Malformed {
                    line: 0,
                    reason: format!("数据集路径 {:?} 不合规：{}", entry.path, status.as_str()),
                })?;
            let folded = crate::path_rules::casefold(&normalized);
            for (existing_display, existing_folded) in &seen {
                if existing_folded == &folded {
                    return Err(FormatError::Malformed {
                        line: 0,
                        reason: format!(
                            "路径 {normalized:?} 与 {existing_display:?} 只是大小写不同，\
                             在大小写不敏感的文件系统上会碰撞，视为同一路径被登记了两次"
                        ),
                    });
                }
                if folded.starts_with(&format!("{existing_folded}/"))
                    || existing_folded.starts_with(&format!("{folded}/"))
                {
                    return Err(FormatError::Malformed {
                        line: 0,
                        reason: format!(
                            "数据集 {normalized:?} 与 {existing_display:?} 嵌套；归属必须唯一"
                        ),
                    });
                }
            }
            seen.push((normalized, folded));
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
        assert!(
            reg.validate().is_err(),
            "引用不存在的 hub 必须拒绝（spec §4.3.2）"
        );
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
        assert!(
            reg.validate().is_err(),
            "归属必须唯一，嵌套拒绝（spec §4.3.2）"
        );
    }

    #[test]
    fn 拒绝损坏的_toml_而不是_panic() {
        assert!(Registry::parse("[[[").is_err());
        assert!(Registry::parse("").is_err());
    }

    #[test]
    fn 前缀相同但非目录前缀不算嵌套() {
        // 回归测试：嵌套判断若被误写成 `starts_with(existing)`（不带分隔符），
        // "assets2" 会被误判为嵌套在 "assets" 里。必须精确比较 "assets/" 前缀。
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"assets\"\nhub = \"h\"\n\
                    [[dataset]]\npath = \"assets2\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(
            reg.validate().is_ok(),
            "assets 与 assets2 只是前缀相同，不是嵌套"
        );
    }

    #[test]
    fn 拒绝仅大小写不同的重复路径() {
        // 评审 Important #11：macOS / Windows 默认大小写不敏感文件系统上
        // "Assets" 与 "assets" 会解析到同一个目录。
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"Assets\"\nhub = \"h\"\n\
                    [[dataset]]\npath = \"assets\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(
            reg.validate().is_err(),
            "大小写不同但折叠后相同的路径必须拒绝"
        );
    }

    #[test]
    fn 拒绝大小写不同的嵌套数据集() {
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"Assets\"\nhub = \"h\"\n\
                    [[dataset]]\npath = \"assets/inner\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(
            reg.validate().is_err(),
            "大小写不同的嵌套同样必须拒绝，归属必须唯一"
        );
    }

    #[test]
    fn 拒绝父引用路径() {
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"../outside\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(reg.validate().is_err(), "逃出 vault 的父引用路径必须拒绝");
    }

    #[test]
    fn 拒绝绝对路径() {
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"/etc\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(reg.validate().is_err(), "绝对路径必须拒绝");
    }

    #[test]
    fn normalized_datasets_把反斜杠归一化而不是原样返回() {
        // 评审 Critical #1：`validate()` 接受 "assets\sub"（`\` 被当分隔符），
        // 但只把归一化结果用于比较，不写回 `entry.path`。`normalized_datasets()`
        // 就是为了让调用方不再需要自己重新走一遍 `path_rules::check`。
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"assets\\\\sub\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(
            reg.validate().is_ok(),
            "反斜杠路径本身合规，应通过 validate"
        );
        assert_eq!(reg.datasets()[0].path, "assets\\sub", "原始形式保持不变");
        let normalized = reg.normalized_datasets().unwrap();
        assert_eq!(normalized[0].path, "assets/sub", "归一化视图必须用 / 分隔");
    }

    #[test]
    fn hubs_遍历出全部已登记条目() {
        let reg = Registry::parse(样例).unwrap();
        let all: Vec<(&str, &HubEntry)> = reg.hubs().collect();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "home");
        assert_eq!(all[0].1.url, "https://nas.example.com:8443");
    }

    #[test]
    fn 往返序列化保持一致() {
        let reg = Registry::parse(样例).unwrap();
        let text = reg.to_toml().unwrap();
        let reparsed = Registry::parse(&text).unwrap();
        assert_eq!(reg, reparsed);
    }

    #[test]
    fn 构造的注册表往返序列化后内容不变() {
        let mut hub = std::collections::BTreeMap::new();
        hub.insert(
            "home".to_string(),
            HubEntry {
                instance_id: "3f2a000000000000000000000000beef".to_string(),
                url: "https://nas.example.com:8443".to_string(),
            },
        );
        let dataset = vec![DatasetEntry {
            path: "assets".to_string(),
            hub: "home".to_string(),
        }];
        let reg = Registry::new(hub, dataset);

        let text = reg.to_toml().unwrap();
        let reparsed = Registry::parse(&text).unwrap();
        assert_eq!(reg, reparsed);
        assert_eq!(
            reparsed.hub("home").unwrap().url,
            "https://nas.example.com:8443"
        );
    }
}
