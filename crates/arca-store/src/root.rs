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
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// 打开存储根时的失败。四种原因必须彼此可区分（I5：如实报告失败的性质）。
#[derive(Debug)]
pub enum MountError {
    /// 根目录不存在，或存在但没有 `.arca/format.json`——卷未挂载、路径写错、
    /// 或挂载点下面是个本地建的空壳目录。**绝不能当成「库是空的」**。
    Absent { path: String },
    /// 身份标记存在但与期望不符——挂到了别的数据集上（spec §4.6 的防误绑）。
    IdentityMismatch { expected: String, found: String },
    /// 身份标记存在但读不出来。与 `Absent` 是不同的故障。
    Malformed(FormatError),
    /// 读取失败（权限、IO 错误）。与「不存在」是不同的故障。
    Io { path: String, reason: String },
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
            MountError::Malformed(e) => write!(f, "存储根身份标记无法解析：{e}"),
            MountError::Io { path, reason } => write!(f, "读取 {path} 失败：{reason}"),
        }
    }
}

impl std::error::Error for MountError {}

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
    /// 不一定知道期望值。为 `Some` 时不符即失败（I11）。
    ///
    /// **只读**：无论成功失败都不创建任何文件或目录。
    pub fn open(root: &Path, expected_dataset_id: Option<&str>) -> Result<Self, MountError> {
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

        let format = FormatJson::parse(&text).map_err(MountError::Malformed)?;

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
    pub fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}
