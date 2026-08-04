//! 统一错误类型：损坏输入 → 明确错误，绝不 panic、绝不猜测（I5）。

use crate::path_rules::PathStatus;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// 格式版本高于本实现已知的最高版本 → 拒绝，不尽力解析（I10）。
    UnsupportedVersion { found: u32, max: u32 },
    /// 结构损坏。`line` 为 1 起的行号，0 表示非行式格式。
    Malformed { line: usize, reason: String },
    BadPath(PathStatus),
    BadHash(String),
    Io(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::UnsupportedVersion { found, max } => {
                write!(f, "格式版本 {found} 高于本实现支持的 {max}；请升级 arca")
            }
            FormatError::Malformed { line, reason } => {
                if *line == 0 {
                    write!(f, "格式损坏：{reason}")
                } else {
                    write!(f, "第 {line} 行格式损坏：{reason}")
                }
            }
            FormatError::BadPath(status) => write!(f, "路径不合规：{}", status.as_str()),
            FormatError::BadHash(text) => write!(f, "哈希不合规：{text}"),
            FormatError::Io(msg) => write!(f, "IO 错误：{msg}"),
        }
    }
}

impl std::error::Error for FormatError {}

impl From<PathStatus> for FormatError {
    fn from(status: PathStatus) -> Self {
        FormatError::BadPath(status)
    }
}
