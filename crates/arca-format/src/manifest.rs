//! 行式清单 `<dataset>/.arca/manifest`——git 侧的二进制影子（spec §4.4.1）。
//!
//! 格式刻意选择行式而非 TOML（三方合并友好，同 `.gitignore` 的理由）：
//! - 首行 `#%arca-manifest v1`；
//! - 一行一条、按路径字节序排序、Tab 分隔、确定性序列化（同内容必同字节）；
//! - 字段：`路径 \t blake3:哈希 \t 字节数 \t mtime(RFC3339)`。
//!
//! Tab 与换行被路径规则禁止（见 [`crate::path_rules`]），因此分隔无歧义。
//!
//! I5（绝不猜测）的两处具体落地：
//! - **重复路径**是歧义状态（不知道哪条权威），拒绝并报错，绝不静默去重；
//! - manifest 是整体原子重写、非 append-only 格式，正文中不存在「崩溃残留的
//!   空行」这种正常情况，因此正文空行一律视为损坏并拒绝。
//!
//! TODO(M0)：与实体的一致性比对素材（§6.3 第 10 条）。

use crate::error::FormatError;
use crate::path_rules;
use arca_chunk::hash::ContentHash;
use std::collections::HashSet;

const HEADER: &str = "#%arca-manifest v1";
const MAX_VERSION: u32 = 1;

/// 清单中的一条记录：路径、内容哈希、字节数、mtime。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: String,
    pub hash: ContentHash,
    pub size: u64,
    pub mtime: String,
}

/// 行式清单：内部条目恒按路径 UTF-8 字节序排序（确定性序列化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    entries: Vec<ManifestEntry>,
}

impl Manifest {
    /// 内部按路径 UTF-8 字节序排序，保证确定性序列化。
    ///
    /// 重复路径是歧义状态（不知道哪条权威）——拒绝并返回
    /// `Malformed { line: 0, .. }`（`line: 0` 表示非行式上下文，绝不静默去重（I5）。
    pub fn from_entries(mut entries: Vec<ManifestEntry>) -> Result<Self, FormatError> {
        entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        for pair in entries.windows(2) {
            if pair[0].path == pair[1].path {
                return Err(FormatError::Malformed {
                    line: 0,
                    reason: format!("路径 {:?} 重复出现", pair[0].path),
                });
            }
        }
        Ok(Manifest { entries })
    }

    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// 解析行式清单文本。任意损坏输入返回 `Err`，绝不 panic（I5）。
    ///
    /// 正文空行、字段数错误、重复路径均视为损坏并拒绝——manifest 是整体
    /// 原子重写的格式，不存在「正常残留」，拒绝优于尽力恢复（I5）。
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let mut lines = text.lines().enumerate();
        let (_, header) = lines.next().ok_or(FormatError::Malformed {
            line: 0,
            reason: "清单为空，缺少头部".to_string(),
        })?;
        parse_header(header.trim_end_matches('\r'))?;

        let mut entries = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::new();
        for (zero_based, raw) in lines {
            let line_no = zero_based + 1;
            let line = raw.trim_end_matches('\r');
            if line.is_empty() {
                return Err(FormatError::Malformed {
                    line: line_no,
                    reason: "正文行为空；manifest 整体重写、无追加残留，空行只可能是损坏".to_string(),
                });
            }
            let entry = parse_entry(line, line_no)?;
            if !seen_paths.insert(entry.path.clone()) {
                return Err(FormatError::Malformed {
                    line: line_no,
                    reason: format!("路径 {:?} 在第 {line_no} 行重复出现", entry.path),
                });
            }
            entries.push(entry);
        }
        Manifest::from_entries(entries)
    }

    /// 确定性序列化：同内容必产生同字节。
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut out = String::from(HEADER);
        out.push('\n');
        for entry in &self.entries {
            out.push_str(&entry.path);
            out.push('\t');
            out.push_str(&entry.hash.to_text());
            out.push('\t');
            out.push_str(&entry.size.to_string());
            out.push('\t');
            out.push_str(&entry.mtime);
            out.push('\n');
        }
        out
    }
}

fn parse_header(header: &str) -> Result<(), FormatError> {
    let version = header.strip_prefix("#%arca-manifest v").ok_or(FormatError::Malformed {
        line: 1,
        reason: format!("头部应为 {HEADER:?}，实得 {header:?}"),
    })?;
    let found: u32 = version.parse().map_err(|_| FormatError::Malformed {
        line: 1,
        reason: format!("版本号 {version:?} 不是整数"),
    })?;
    if found > MAX_VERSION {
        return Err(FormatError::UnsupportedVersion { found, max: MAX_VERSION });
    }
    Ok(())
}

fn parse_entry(line: &str, line_no: usize) -> Result<ManifestEntry, FormatError> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() != 4 {
        return Err(FormatError::Malformed {
            line: line_no,
            reason: format!("应有 4 个 Tab 分隔字段，实得 {}", fields.len()),
        });
    }
    let path = path_rules::check(fields[0]).map_err(|status| FormatError::Malformed {
        line: line_no,
        reason: format!("路径不合规：{status:?}"),
    })?;
    let hash = ContentHash::parse(fields[1]).map_err(|e| FormatError::Malformed {
        line: line_no,
        reason: format!("哈希不合规：{e}"),
    })?;
    let size: u64 = fields[2].parse().map_err(|_| FormatError::Malformed {
        line: line_no,
        reason: format!("大小 {:?} 不是无符号整数", fields[2]),
    })?;
    if fields[3].is_empty() {
        // mtime 字段虽是不透明 String（格式细节校验留给 Task 9），但空串不可能
        // 是合法的 RFC 3339 时间戳（FORMAT.md §1）——结构性缺失，拒绝而非放行。
        return Err(FormatError::Malformed {
            line: line_no,
            reason: "mtime 字段为空".to_string(),
        });
    }
    Ok(ManifestEntry { path, hash, size, mtime: fields[3].to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_chunk::hash::ContentHash;

    fn 样例哈希(seed: &[u8]) -> ContentHash {
        ContentHash::from_bytes(seed)
    }

    #[test]
    fn 解析合法清单() {
        let text = format!(
            "#%arca-manifest v1\n京都/鸭川.png\t{}\t2411008\t2026-08-04T10:22:31Z\n",
            样例哈希(b"a").to_text()
        );
        let manifest = Manifest::parse(&text).unwrap();
        assert_eq!(manifest.entries().len(), 1);
        assert_eq!(manifest.entries()[0].path, "京都/鸭川.png");
        assert_eq!(manifest.entries()[0].size, 2411008);
    }

    #[test]
    fn 序列化按路径字节序排序且往返一致() {
        let entries = vec![
            ManifestEntry { path: "z.png".into(), hash: 样例哈希(b"z"), size: 1, mtime: "2026-08-04T10:00:00Z".into() },
            ManifestEntry { path: "a.png".into(), hash: 样例哈希(b"a"), size: 2, mtime: "2026-08-04T10:00:00Z".into() },
        ];
        let manifest = Manifest::from_entries(entries).unwrap();
        let text = manifest.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "#%arca-manifest v1");
        assert!(lines[1].starts_with("a.png"));
        assert!(lines[2].starts_with("z.png"));
        assert_eq!(Manifest::parse(&text).unwrap(), manifest);
    }

    #[test]
    fn 同内容必产生同字节() {
        let mk = |order: [&str; 2]| {
            Manifest::from_entries(
                order.iter().map(|p| ManifestEntry {
                    path: (*p).into(), hash: 样例哈希(p.as_bytes()), size: 1,
                    mtime: "2026-08-04T10:00:00Z".into(),
                }).collect()
            ).unwrap().to_string()
        };
        assert_eq!(mk(["a.png", "b.png"]), mk(["b.png", "a.png"]));
    }

    #[test]
    fn 拒绝缺失或错误的头部() {
        assert!(Manifest::parse("").is_err());
        assert!(Manifest::parse("京都/鸭川.png\tblake3:00\t1\tt\n").is_err());
        assert!(Manifest::parse("#%arca-manifest v99\n").is_err());
    }

    #[test]
    fn 拒绝字段数错误的行并报出行号() {
        let text = "#%arca-manifest v1\na.png\tblake3:00\n";
        match Manifest::parse(text) {
            Err(crate::error::FormatError::Malformed { line, .. }) => assert_eq!(line, 2),
            other => panic!("应报第 2 行损坏，实得 {other:?}"),
        }
    }

    #[test]
    fn 拒绝不合规路径() {
        let text = format!("#%arca-manifest v1\n../逃逸.png\t{}\t1\t2026-08-04T10:00:00Z\n", 样例哈希(b"x").to_text());
        assert!(Manifest::parse(&text).is_err());
    }

    #[test]
    fn 空清单只有头部() {
        let manifest = Manifest::from_entries(vec![]).unwrap();
        assert_eq!(manifest.to_string(), "#%arca-manifest v1\n");
        assert_eq!(Manifest::parse("#%arca-manifest v1\n").unwrap(), manifest);
    }

    #[test]
    fn 解析含重复路径的文本应报错且行号正确() {
        let text = format!(
            "#%arca-manifest v1\na.png\t{}\t1\t2026-08-04T10:00:00Z\nb.png\t{}\t2\t2026-08-04T10:00:00Z\na.png\t{}\t3\t2026-08-04T10:00:00Z\n",
            样例哈希(b"a").to_text(),
            样例哈希(b"b").to_text(),
            样例哈希(b"a2").to_text(),
        );
        match Manifest::parse(&text) {
            Err(crate::error::FormatError::Malformed { line, reason }) => {
                assert_eq!(line, 4, "应报第二次出现（第 4 行）");
                assert!(reason.contains("a.png"), "reason 应指明具体重复的路径：{reason}");
            }
            other => panic!("应报重复路径损坏，实得 {other:?}"),
        }
    }

    #[test]
    fn from_entries_传入重复路径应返回错误() {
        let entries = vec![
            ManifestEntry { path: "a.png".into(), hash: 样例哈希(b"a"), size: 1, mtime: "2026-08-04T10:00:00Z".into() },
            ManifestEntry { path: "a.png".into(), hash: 样例哈希(b"a2"), size: 2, mtime: "2026-08-04T10:00:00Z".into() },
        ];
        match Manifest::from_entries(entries) {
            Err(crate::error::FormatError::Malformed { line, reason }) => {
                assert_eq!(line, 0, "非行式上下文应为 line: 0");
                assert!(reason.contains("a.png"));
            }
            other => panic!("应报重复路径损坏，实得 {other:?}"),
        }
    }

    #[test]
    fn 拒绝正文中间的空行并报出行号() {
        let text = format!(
            "#%arca-manifest v1\na.png\t{}\t1\t2026-08-04T10:00:00Z\n\nb.png\t{}\t2\t2026-08-04T10:00:00Z\n",
            样例哈希(b"a").to_text(),
            样例哈希(b"b").to_text(),
        );
        match Manifest::parse(&text) {
            Err(crate::error::FormatError::Malformed { line, .. }) => assert_eq!(line, 3),
            other => panic!("应报第 3 行为空行损坏，实得 {other:?}"),
        }
    }

    #[test]
    fn 文件末尾有换行仍能正常解析() {
        // str::lines() 不会因末尾换行产生额外的空行元素，正文空行拒绝不应误伤正常文件。
        let text = format!(
            "#%arca-manifest v1\na.png\t{}\t1\t2026-08-04T10:00:00Z\n",
            样例哈希(b"a").to_text()
        );
        assert!(text.ends_with('\n'));
        let manifest = Manifest::parse(&text).unwrap();
        assert_eq!(manifest.entries().len(), 1);
    }

    #[test]
    fn 拒绝字段数为五的行() {
        let text = format!(
            "#%arca-manifest v1\na.png\t{}\t1\t2026-08-04T10:00:00Z\t多余字段\n",
            样例哈希(b"a").to_text()
        );
        assert!(Manifest::parse(&text).is_err());
    }

    #[test]
    fn 拒绝空_mtime_字段() {
        // mtime 是不透明 String（不做 RFC3339 格式校验），但空串结构性缺失，明确拒绝。
        let text = format!("#%arca-manifest v1\na.png\t{}\t1\t\n", 样例哈希(b"a").to_text());
        match Manifest::parse(&text) {
            Err(crate::error::FormatError::Malformed { line, reason }) => {
                assert_eq!(line, 2);
                assert!(reason.contains("mtime"));
            }
            other => panic!("应报 mtime 为空，实得 {other:?}"),
        }
    }
}
