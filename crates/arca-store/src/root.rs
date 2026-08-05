//! 存储根的打开与卷身份校验（I11）。
//!
//! **为什么这是一个显式的三态而不是 `Option`**：未挂载的卷与空库在字节上难以区分，
//! 语义上却天差地别。把前者当后者，同步引擎会认为远端删光了文件，
//! 于是触发删除对账，清掉用户本地的数据（spec §4.6、I11）。
//! 所以「根不存在」「身份不符」「身份读不出来」必须是三种彼此可区分的失败，
//! 而不是统一折叠成「没有数据」。
//!
//! 格式契约见 `FORMAT.md` §4（布局）与 §5（format.json）。

use arca_format::error::FormatError;
use arca_format::hub_layout::{layout, FormatJson};
use arca_format::model::is_hex32;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// 打开存储根时的失败。彼此可区分（I5：如实报告失败的性质）。
#[derive(Debug)]
pub enum MountError {
    /// 根目录不存在，或根目录存在但 `.arca/format.json` 这个具体文件不存在
    /// （即 `fs::read_to_string` 返回 `ErrorKind::NotFound`）——卷未挂载、
    /// 路径写错，或挂载点下面是个本地建的空壳目录。**绝不能当成「库是空的」**。
    /// 若路径上某一级类型不对（例如 `.arca` 是文件而非目录导致的 `ENOTDIR`），
    /// 那是另一种故障，落在 `Io`，不在这里。
    Absent { path: String },
    /// 身份标记存在但与期望不符——挂到了别的数据集上（spec §4.6 的防误绑）。
    IdentityMismatch { expected: String, found: String },
    /// 身份标记存在但读不出来。与 `Absent` 是不同的故障；带上路径，
    /// 因为本类型面向的正是 fsck 这类要扫多个挂载点的只读巡检，
    /// 不点名路径运维就没法行动。
    Malformed { path: String, source: FormatError },
    /// 读取失败（权限、符号链接环、路径某一级类型不对等）。与「不存在」是不同的故障。
    Io { path: String, reason: String },
    /// 调用方传入的 `expected_dataset_id` 本身不合法（不是 32 位小写十六进制）。
    /// 这是调用方的参数错误，不是卷的问题——不应与 `IdentityMismatch` 混为一谈。
    BadExpectedId { value: String },
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MountError::Absent { path } => write!(
                f,
                "存储根 {path} 缺少 {}——卷未挂载、路径错误，或这不是一个 arca 存储根（绝不视为空库，I11）",
                layout::FORMAT_JSON
            ),
            MountError::IdentityMismatch { expected, found } => write!(
                f,
                "卷身份不符：期望 dataset_id {expected}，实际是 {found}——挂到了别的数据集上"
            ),
            MountError::Malformed { path, source } => {
                write!(f, "存储根 {path} 的身份标记无法解析：{source}")
            }
            MountError::Io { path, reason } => write!(f, "读取 {path} 失败：{reason}"),
            MountError::BadExpectedId { value } => write!(
                f,
                "调用方传入的期望 dataset_id {value:?} 不是合法的 32 位小写十六进制——这是参数错误，不是卷的问题"
            ),
        }
    }
}

impl std::error::Error for MountError {}

/// `StorageRoot::join` 中相对路径试图逃出存储根时的失败。
///
/// `Path::join` 本身没有防护：`relative` 若是绝对路径会把 `self.path` 整个丢掉
/// （`root.join("/etc/passwd")` 返回 `/etc/passwd`），`..` 也是原样透传不解析。
/// `StorageRoot` 存在的意义就是「持有它就不必在每个调用点重新推导根的安全性」，
/// 所以这个校验必须在类型内部做，而不是靠调用方自觉只传 `layout::` 常量。
#[derive(Debug)]
pub struct RootEscape {
    pub relative: String,
    pub reason: &'static str,
}

impl fmt::Display for RootEscape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "路径 {:?} 会逃出存储根：{}", self.relative, self.reason)
    }
}

impl std::error::Error for RootEscape {}

/// 一个已打开、身份已确认的存储根。
///
/// 持有它即代表「这个根存在、是 arca 存储根、且身份与期望一致」——
/// 后续的读写不必再重复这些判断。
#[derive(Debug)]
pub struct StorageRoot {
    path: PathBuf,
    format: FormatJson,
}

impl StorageRoot {
    /// 打开存储根并校验身份。
    ///
    /// `expected_dataset_id` 为 `None` 时跳过身份比对——`fsck` 这类只读巡检
    /// 不一定知道期望值。为 `Some` 时必须是合法的 32 位小写十六进制
    /// （否则是调用方参数错误，见 `MountError::BadExpectedId`），且与卷内
    /// 记录的身份不符即失败（I11）。
    ///
    /// **只读**：无论成功失败都不创建任何文件或目录。
    pub fn open(root: &Path, expected_dataset_id: Option<&str>) -> Result<Self, MountError> {
        if let Some(expected) = expected_dataset_id {
            if !is_hex32(expected) {
                return Err(MountError::BadExpectedId {
                    value: expected.to_string(),
                });
            }
        }

        let format_path = root.join(layout::FORMAT_JSON);
        let text = match fs::read_to_string(&format_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MountError::Absent {
                    path: root.display().to_string(),
                })
            }
            Err(e) => {
                return Err(MountError::Io {
                    path: format_path.display().to_string(),
                    reason: e.to_string(),
                })
            }
        };

        let format = FormatJson::parse(&text).map_err(|source| MountError::Malformed {
            path: format_path.display().to_string(),
            source,
        })?;

        if let Some(expected) = expected_dataset_id {
            if expected != format.dataset_id {
                return Err(MountError::IdentityMismatch {
                    expected: expected.to_string(),
                    found: format.dataset_id.clone(),
                });
            }
        }

        Ok(StorageRoot {
            path: root.to_path_buf(),
            format,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> &FormatJson {
        &self.format
    }

    pub fn dataset_id(&self) -> &str {
        &self.format.dataset_id
    }

    /// 拼接存储根内的相对路径。传 `layout::` 里的常量，不要手写字面量。
    ///
    /// 拒绝三种会逃出存储根的输入：绝对路径、含 `..` 父目录引用的路径、
    /// 含 Windows 盘符前缀（如 `C:`）的路径。`a..b` 这样文件名里含两个点
    /// 但不构成父目录引用的路径会被放行。
    pub fn join(&self, relative: &str) -> Result<PathBuf, RootEscape> {
        let rel_path = Path::new(relative);

        if rel_path.is_absolute() {
            return Err(RootEscape {
                relative: relative.to_string(),
                reason: "绝对路径会丢弃存储根，逃出存储根之外",
            });
        }

        for component in rel_path.components() {
            match component {
                Component::ParentDir => {
                    return Err(RootEscape {
                        relative: relative.to_string(),
                        reason: "含 `..` 父目录引用，可能逃出存储根",
                    });
                }
                Component::Prefix(_) => {
                    return Err(RootEscape {
                        relative: relative.to_string(),
                        reason: "含盘符前缀，逃出存储根",
                    });
                }
                _ => {}
            }
        }

        Ok(self.path.join(rel_path))
    }
}
