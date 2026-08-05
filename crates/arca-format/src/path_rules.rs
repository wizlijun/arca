//! 相对路径规则：规范化、禁用字符、跨平台等价性。
//!
//! 客户端与 hub 必须对路径规则跑同一段代码（两端共用纪律，spec §3）。
//! Tab 与换行禁止（清单分隔依赖此约束，§4.4.1）；大小写与 Unicode 规范化
//! 需明确定义（macOS NFD / Windows 大小写不敏感）。
//!
//! 参考 lazync：`shared/src/nc_path_rules.pas`（继承其规则集与边界处理）。
//!
//! TODO(M0)：golden vectors（属 Task 5/7 范围）。

use arca_chunk::hash::ContentHash;

/// 相对路径最大字节数（继承 lazync `nc_max_relative_path_bytes`）。
pub const MAX_RELATIVE_PATH_BYTES: usize = 2048;
/// 目录最大深度，单位为段（继承 lazync `nc_max_path_depth`）。
pub const MAX_PATH_DEPTH: usize = 64;
/// 单段最大字节数（继承 lazync `nc_max_path_segment_bytes`）。
pub const MAX_SEGMENT_BYTES: usize = 240;
/// 解析后物理路径最大字节数（继承 lazync `nc_max_physical_path_bytes`）。
pub const MAX_PHYSICAL_PATH_BYTES: usize = 3800;

/// 路径校验结果。拒绝理由必须可诊断——绝不猜测、绝不截断修复（I5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStatus {
    Empty,
    Absolute,
    ParentRef,
    TooLong,
    TooDeep,
    SegmentTooLong,
    InvalidChar,
    ReservedName,
}

impl PathStatus {
    /// 稳定的短标识，用于 trace 的 `path.reject` 事件与 `--json` 输出
    /// （FORMAT.md §10.3、PROTOCOL.md §7）。**受兼容性承诺约束：只增不改**——
    /// agent 按它分支，改一个字符就是破坏性变更。`Debug` 输出不承担这个角色。
    pub fn as_str(&self) -> &'static str {
        match self {
            PathStatus::Empty => "empty",
            PathStatus::Absolute => "absolute",
            PathStatus::ParentRef => "parent_ref",
            PathStatus::TooLong => "too_long",
            PathStatus::TooDeep => "too_deep",
            PathStatus::SegmentTooLong => "segment_too_long",
            PathStatus::InvalidChar => "invalid_char",
            PathStatus::ReservedName => "reserved_name",
        }
    }
}

const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 规范化：`\` → `/`、折叠重复分隔符、丢弃空段与 `.` 段。
/// 不做 Unicode NFC/NFD 转换（FORMAT.md §2 已知限制）。
pub fn normalize(raw: &str) -> String {
    raw.split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// 校验并返回规范化路径。
pub fn check(raw: &str) -> Result<String, PathStatus> {
    if raw.is_empty() {
        return Err(PathStatus::Empty);
    }
    if is_absolute(raw) {
        return Err(PathStatus::Absolute);
    }

    let normalized = normalize(raw);
    if normalized.is_empty() {
        return Err(PathStatus::Empty);
    }
    if normalized.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(PathStatus::TooLong);
    }

    let segments: Vec<&str> = normalized.split('/').collect();
    if segments.len() > MAX_PATH_DEPTH {
        return Err(PathStatus::TooDeep);
    }

    for segment in &segments {
        if *segment == ".." {
            return Err(PathStatus::ParentRef);
        }
        if segment.len() > MAX_SEGMENT_BYTES {
            return Err(PathStatus::SegmentTooLong);
        }
        if has_invalid_char(segment) {
            return Err(PathStatus::InvalidChar);
        }
        if is_reserved(segment) {
            return Err(PathStatus::ReservedName);
        }
    }

    Ok(normalized)
}

fn is_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    match bytes.first() {
        Some(b'/') | Some(b'\\') => true,
        // 盘符形式 C:\ 或 C:/
        _ => bytes.len() >= 2 && bytes[1] == b':',
    }
}

fn has_invalid_char(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    for c in segment.chars() {
        // 控制字符含 Tab(0x09) 与换行(0x0A/0x0D)——manifest 分隔依赖此排除
        if (c as u32) < 0x20 || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            return true;
        }
    }
    matches!(segment.chars().next_back(), Some(' ') | Some('.'))
}

fn is_reserved(segment: &str) -> bool {
    let base = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .to_ascii_uppercase();
    WINDOWS_RESERVED.contains(&base.as_str())
}

/// index key 的大小写折叠规则：**仅 ASCII** 小写化（FORMAT.md §2）。
///
/// 刻意不用 `str::to_lowercase()`（Unicode 默认大小写转换）：其映射表随 Unicode
/// 版本演进，两个不同时期构建的 arca 二进制会对同一个含罕见字符的路径算出不同的
/// index key，产生同一文件的两条索引记录——而"大小写冲突拒绝、绝不静默合并"这条
/// 规则恰恰因为两者根本不碰撞而静默失效（评审 Important #10）。ASCII-only 与
/// FORMAT.md §2 已声明的"不做 NFC/NFD 转换"是同一立场：v1 路径按字节原样比较，
/// 只在最省事、最不可能随工具链漂移的 ASCII 范围内做大小写折叠。
///
/// 供需要做前缀/嵌套比较（而不只是相等性判断）的调用方复用——BLAKE3 摘要本身
/// 不可逆，无法从 [`index_key`] 的输出反推出可比较前缀的字符串。
pub fn casefold(raw: &str) -> String {
    normalize(raw).to_ascii_lowercase()
}

/// 索引键：ASCII 小写折叠后的规范化路径的 BLAKE3（见 [`casefold`]）。
/// 大小写不同但折叠后相同的路径会得到同一个键——调用方据此检出冲突并拒绝，
/// 绝不静默合并（继承 lazync STORAGE.md §File Identity Index）。
pub fn index_key(raw: &str) -> ContentHash {
    ContentHash::from_bytes(casefold(raw).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 规范化折叠分隔符与点段() {
        assert_eq!(normalize("a\\b//c/./d"), "a/b/c/d");
        assert_eq!(normalize("./x"), "x");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn 接受合法路径并返回规范化形式() {
        assert_eq!(check("京都/鸭川.png").unwrap(), "京都/鸭川.png");
        assert_eq!(check("a\\b.txt").unwrap(), "a/b.txt");
    }

    #[test]
    fn 拒绝绝对路径与父引用() {
        assert_eq!(check("/etc/passwd"), Err(PathStatus::Absolute));
        assert_eq!(check("C:/x"), Err(PathStatus::Absolute));
        assert_eq!(check("\\\\server\\share"), Err(PathStatus::Absolute));
        assert_eq!(check("a/../b"), Err(PathStatus::ParentRef));
    }

    #[test]
    fn 拒绝空路径() {
        assert_eq!(check(""), Err(PathStatus::Empty));
        assert_eq!(check("./."), Err(PathStatus::Empty));
    }

    #[test]
    fn 拒绝控制字符包括_tab_与换行() {
        // manifest 的 Tab 分隔依赖这一条（spec §4.4.1）
        assert_eq!(check("a\tb"), Err(PathStatus::InvalidChar));
        assert_eq!(check("a\nb"), Err(PathStatus::InvalidChar));
        assert_eq!(check("a<b"), Err(PathStatus::InvalidChar));
        assert_eq!(check("a?b"), Err(PathStatus::InvalidChar));
    }

    #[test]
    fn 拒绝段以空格或句点结尾() {
        assert_eq!(check("a /b"), Err(PathStatus::InvalidChar));
        assert_eq!(check("a./b"), Err(PathStatus::InvalidChar));
    }

    #[test]
    fn 拒绝_windows_保留名() {
        assert_eq!(check("CON"), Err(PathStatus::ReservedName));
        assert_eq!(check("dir/nul.txt"), Err(PathStatus::ReservedName));
        assert_eq!(check("com9.dat"), Err(PathStatus::ReservedName));
        // 但 "console.txt" 不是保留名
        assert!(check("console.txt").is_ok());
    }

    #[test]
    fn 拒绝超限路径() {
        let long_segment = "a".repeat(MAX_SEGMENT_BYTES + 1);
        assert_eq!(check(&long_segment), Err(PathStatus::SegmentTooLong));

        let deep = vec!["d"; MAX_PATH_DEPTH + 1].join("/");
        assert_eq!(check(&deep), Err(PathStatus::TooDeep));

        let long =
            format!("{}/x", "a".repeat(MAX_SEGMENT_BYTES)).repeat(MAX_RELATIVE_PATH_BYTES / 8);
        assert_eq!(check(&long).unwrap_err(), PathStatus::TooLong);
    }

    #[test]
    fn 索引键对大小写不敏感但路径本身保留大小写() {
        assert_eq!(index_key("A/B.png"), index_key("a/b.png"));
        assert_ne!(index_key("a/b.png"), index_key("a/c.png"));
        assert_eq!(check("A/B.png").unwrap(), "A/B.png");
    }

    #[test]
    fn 索引键仅做_ascii_小写折叠不随_unicode_大小写表变化() {
        // 评审 Important #10：西里尔字母 "А"（U+0410，大写）与 "а"（U+0430，小写）
        // 在 Unicode 默认大小写折叠下相等，但都不是 ASCII 字符——ASCII-only 折叠下
        // 二者必须是不同的 index key，否则就是在用会随 Unicode 版本演进的映射表
        // 决定磁盘文件名。
        assert_ne!(index_key("А.png"), index_key("а.png"));
        // ASCII 范围内的大小写折叠仍然正常工作。
        assert_eq!(casefold("A/B.png"), casefold("a/b.png"));
    }

    use proptest::prelude::*;

    proptest! {
        /// I5：任意输入都不得 panic，只能返回明确结果。
        #[test]
        fn 任意输入都不_panic(raw in ".*") {
            let _ = normalize(&raw);
            let _ = check(&raw);
            let _ = index_key(&raw);
        }

        /// 规范化是幂等的——同内容必产生同字节。
        #[test]
        fn 规范化幂等(raw in ".*") {
            let once = normalize(&raw);
            prop_assert_eq!(normalize(&once), once.clone());
        }
    }
}
