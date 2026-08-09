//! `arca publish-map` 的输出：publish-map.json 生成（M5a，spec §4.9）。
//!
//! 结构：`{schema, datasets: {前缀 → base_url/style}, items: {路径 → hash/size}}`。
//! 格式属于对外契约（站点生成器过滤器消费）。
//!
//! # 这个模块最重要的性质：**它一个 blob 都不读**
//!
//! 清单（§4.4.1）里已经有路径、哈希、大小，而链接重写只需要这三样。
//! 所以 `publish-map` 完全由清单构造——**100 GB 的图库，CI 一个字节都不用
//! 下载**。spec §4.9 把这条叫做「一个意外红利」，它也是 M5 的头条验收：
//! CI 在不下载任何 blob 的前提下构建出图片可访问的静态站。
//!
//! 这条性质不是「顺便实现成这样」——本模块的签名里**没有任何能读内容的
//! 东西**（输入是 `Manifest` 与 `DatasetConfig`，不是目录路径、不是
//! `Transport`），所以它在类型上就做不到去下载 blob。
//!
//! # 绝不改写用户的 md（§4.9 约束 ①，I6）
//!
//! arca 只产出**映射**，重写由站点生成器完成。本模块因此也没有写文件的
//! 能力——它返回一个字符串，由命令壳决定往哪儿放。

use std::collections::{BTreeMap, BTreeSet};

use arca_format::dataset::{DatasetConfig, UrlStyle};
use arca_format::manifest::Manifest;

pub const SCHEMA: u32 = 1;

/// 一个数据集在发布映射里的条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetMap {
    /// vault 内的路径前缀，**带尾随 `/`**——站点生成器拿它做前缀匹配。
    pub prefix: String,
    pub base_url: String,
    pub style: UrlStyle,
}

/// 一个受管文件在发布映射里的条目。**只有这两样**——链接重写需要的全部。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemMap {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishMap {
    pub datasets: BTreeMap<String, DatasetMap>,
    /// 键是 **vault 内的完整相对路径**（`<数据集前缀><数据集内路径>`），
    /// 与用户 md 里写的相对路径一致——站点生成器直接按它查表。
    pub items: BTreeMap<String, ItemMap>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MapError {
    /// 数据集没配 `public_base_url`——**拒绝而不是猜一个**。
    ///
    /// 「猜一个」在这里的具体形态是「用空串当 base」，那会产出一堆
    /// `/assets/x.png` 这样的相对 URL，看起来像成功了，发布出去全是死链。
    NoBaseUrl { dataset: String },
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MapError::NoBaseUrl { dataset } => write!(
                f,
                "数据集 {dataset} 没有配置 public_base_url（<dataset>/.arca/dataset.toml）——\
                 没有它就没法生成公开 URL。这里拒绝而不是用一个空前缀凑合：\
                 空前缀会产出一堆看似成功的相对链接，发布出去全是死链"
            ),
        }
    }
}

impl std::error::Error for MapError {}

/// 把一个数据集的清单折进发布映射。
///
/// `dataset_path` 是它在 vault 里的相对路径（如 `assets`）。
/// `only`（`Some`）时只收录集合内的路径——`--referenced-only` 的落点。
pub fn add_dataset(
    map: &mut PublishMap,
    dataset_path: &str,
    cfg: &DatasetConfig,
    manifest: &Manifest,
    only: Option<&BTreeSet<String>>,
) -> Result<(), MapError> {
    let base_url = cfg
        .public_base_url
        .clone()
        .ok_or_else(|| MapError::NoBaseUrl {
            dataset: dataset_path.to_string(),
        })?;

    let prefix = format!("{}/", dataset_path.trim_end_matches('/'));
    map.datasets.insert(
        dataset_path.to_string(),
        DatasetMap {
            prefix: prefix.clone(),
            // base_url 的尾随 `/` 要去掉：站点生成器拼的是 `{base}/{rest}`，
            // 两边都带斜杠会产出 `//`，某些 CDN 会 404。
            base_url: base_url.trim_end_matches('/').to_string(),
            style: cfg.url_style.unwrap_or(UrlStyle::Path),
        },
    );

    for e in manifest.entries() {
        if let Some(set) = only {
            if !set.contains(&e.path) {
                continue;
            }
        }
        map.items.insert(
            format!("{prefix}{}", e.path),
            ItemMap {
                hash: e.hash.to_text(),
                size: e.size,
            },
        );
    }
    Ok(())
}

/// 按 §4.9 的样例形状序列化。
///
/// 手写 JSON 而不是引 serde：本 crate 目前只依赖 `arca-format`，
/// 而这份输出的形状被 spec §4.9 的样例钉死、字段极少、且**排序必须确定**
/// （`BTreeMap` 保证）——为它多引一层依赖不划算。
pub fn to_json(map: &PublishMap) -> String {
    let mut s = String::from("{\n");
    s.push_str(&format!("  \"schema\": {SCHEMA},\n"));

    s.push_str("  \"datasets\": {");
    for (i, (name, d)) in map.datasets.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "\n    {}: {{ \"prefix\": {}, \"base_url\": {}, \"style\": {} }}",
            json_str(name),
            json_str(&d.prefix),
            json_str(&d.base_url),
            json_str(match d.style {
                UrlStyle::Path => "path",
                UrlStyle::Hash => "hash",
            })
        ));
    }
    s.push_str(if map.datasets.is_empty() {
        "},\n"
    } else {
        "\n  },\n"
    });

    s.push_str("  \"items\": {");
    for (i, (path, it)) in map.items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "\n    {}: {{ \"hash\": {}, \"size\": {} }}",
            json_str(path),
            json_str(&it.hash),
            it.size
        ));
    }
    s.push_str(if map.items.is_empty() {
        "}\n"
    } else {
        "\n  }\n"
    });

    s.push_str("}\n");
    s
}

/// JSON 字符串转义。**必须处理控制字符**——路径来自用户的文件名，
/// 里面什么都可能有。
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            // 非 ASCII **不转义**：JSON 是 UTF-8，`鸭川.png` 原样写出来
            // 可读性更好，也与 spec §4.9 的样例一致。
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_chunk::hash::ContentHash;
    use arca_format::manifest::ManifestEntry;

    fn 清单(paths: &[(&str, u64)]) -> Manifest {
        Manifest::from_entries(
            paths
                .iter()
                .map(|(p, size)| ManifestEntry {
                    path: p.to_string(),
                    hash: ContentHash::from_bytes(p.as_bytes()),
                    size: *size,
                    mtime: "2026-08-09T00:00:00Z".into(),
                })
                .collect(),
        )
        .unwrap()
    }

    fn 配置(base: Option<&str>, style: Option<UrlStyle>) -> DatasetConfig {
        DatasetConfig {
            schema: 1,
            dataset_id: "9c41000000000000000000000000abcd".into(),
            hub_instance_id: "0123456789abcdef0123456789abcdef".into(),
            public_base_url: base.map(|s| s.to_string()),
            url_style: style,
        }
    }

    #[test]
    fn 条目键是vault内的完整相对路径() {
        let mut m = PublishMap::default();
        add_dataset(
            &mut m,
            "assets",
            &配置(Some("https://cdn.example.com/assets"), None),
            &清单(&[("京都/鸭川.png", 2411008)]),
            None,
        )
        .unwrap();

        // 用户 md 里写的就是 `assets/京都/鸭川.png`，站点生成器直接按它查表。
        let it = m.items.get("assets/京都/鸭川.png").expect("应有该条目");
        assert_eq!(it.size, 2411008);
        assert!(it.hash.starts_with("blake3:"));
    }

    #[test]
    fn base_url的尾随斜杠被去掉() {
        let mut m = PublishMap::default();
        add_dataset(
            &mut m,
            "assets",
            &配置(Some("https://cdn.example.com/assets/"), None),
            &清单(&[("a.png", 1)]),
            None,
        )
        .unwrap();
        assert_eq!(
            m.datasets["assets"].base_url, "https://cdn.example.com/assets",
            "两边都带斜杠会拼出 //，某些 CDN 会 404"
        );
        assert_eq!(m.datasets["assets"].prefix, "assets/");
    }

    /// 没配 `public_base_url` → **拒绝**。用空前缀凑合会产出一堆看似成功的
    /// 相对链接，发布出去全是死链（I5：绝不猜测）。
    #[test]
    fn 没配base_url时拒绝而不是用空前缀凑合() {
        let mut m = PublishMap::default();
        let err = add_dataset(
            &mut m,
            "assets",
            &配置(None, None),
            &清单(&[("a.png", 1)]),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            MapError::NoBaseUrl {
                dataset: "assets".into()
            }
        );
        assert!(err.to_string().contains("死链"), "{err}");
        assert!(m.items.is_empty(), "失败时不该留下半份映射");
    }

    #[test]
    fn 默认风格是path() {
        let mut m = PublishMap::default();
        add_dataset(
            &mut m,
            "assets",
            &配置(Some("https://x/"), None),
            &清单(&[("a.png", 1)]),
            None,
        )
        .unwrap();
        assert_eq!(m.datasets["assets"].style, UrlStyle::Path);
    }

    /// `--referenced-only` 的落点：只收录集合内的路径。
    /// **默认只发布被引用的资源**（§4.9 约束 ③）——直接公开整个数据集会
    /// 暴露没被任何已发布笔记引用的文件，那是隐私事故的常见来源。
    #[test]
    fn only过滤只收录被引用的路径() {
        let mut m = PublishMap::default();
        let only: BTreeSet<String> = ["用到的.png".to_string()].into_iter().collect();
        add_dataset(
            &mut m,
            "assets",
            &配置(Some("https://x"), None),
            &清单(&[("用到的.png", 1), ("没用到的私密照.png", 2)]),
            Some(&only),
        )
        .unwrap();

        assert!(m.items.contains_key("assets/用到的.png"));
        assert!(
            !m.items.contains_key("assets/没用到的私密照.png"),
            "未被引用的文件绝不能出现在发布映射里——扩大暴露面必须是显式动作"
        );
    }

    #[test]
    fn 多数据集各自独立的base_url与风格() {
        let mut m = PublishMap::default();
        add_dataset(
            &mut m,
            "assets",
            &配置(Some("https://cdn.example.com/assets"), Some(UrlStyle::Path)),
            &清单(&[("a.png", 1)]),
            None,
        )
        .unwrap();
        add_dataset(
            &mut m,
            "photo",
            &配置(Some("https://r2.example.com/photo"), Some(UrlStyle::Hash)),
            &清单(&[("b.raw", 2)]),
            None,
        )
        .unwrap();
        assert_eq!(m.datasets["assets"].style, UrlStyle::Path);
        assert_eq!(m.datasets["photo"].style, UrlStyle::Hash);
        assert_eq!(m.items.len(), 2);
    }

    #[test]
    fn 输出是合法json且键有序() {
        let mut m = PublishMap::default();
        add_dataset(
            &mut m,
            "assets",
            &配置(Some("https://cdn.example.com/assets"), Some(UrlStyle::Hash)),
            &清单(&[("z.png", 3), ("a.png", 1), ("京都/鸭川.png", 2)]),
            None,
        )
        .unwrap();
        let json = to_json(&m);
        assert!(json.contains("\"schema\": 1"));
        assert!(json.contains("\"style\": \"hash\""));
        assert!(json.contains("鸭川.png"), "非 ASCII 应原样写出：{json}");
        // 确定性：`a.png` 必须排在 `z.png` 前面。
        assert!(
            json.find("assets/a.png").unwrap() < json.find("assets/z.png").unwrap(),
            "输出必须确定性有序：{json}"
        );
    }

    #[test]
    fn 空映射也是合法json() {
        let json = to_json(&PublishMap::default());
        assert!(json.contains("\"datasets\": {}"), "{json}");
        assert!(json.contains("\"items\": {}"), "{json}");
    }

    /// 路径里的引号/反斜杠/控制字符必须被转义——它们来自用户的文件名。
    #[test]
    fn 路径里的特殊字符被正确转义() {
        assert_eq!(json_str(r#"a"b"#), r#""a\"b""#);
        assert_eq!(json_str(r"a\b"), r#""a\\b""#);
        assert_eq!(json_str("a\nb"), r#""a\nb""#);
        assert_eq!(json_str("a\u{1}b"), "\"a\\u0001b\"");
        assert_eq!(json_str("鸭川.png"), "\"鸭川.png\"");
    }
}
