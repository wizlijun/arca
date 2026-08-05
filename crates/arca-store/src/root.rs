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
use arca_format::trace::{EventKind, TraceRecord, TraceSink};
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
    ///
    /// 不发任何 trace 事件——是 [`Self::open_traced`] 注入 `t_abs_us = 0` 与
    /// `NullSink` 的薄壳（Rule of Silence：不接 sink 就没有诊断开销）。
    pub fn open(root: &Path, expected_dataset_id: Option<&str>) -> Result<Self, MountError> {
        let mut sink = arca_format::trace::NullSink;
        Self::open_traced(root, expected_dataset_id, 0, &mut sink)
    }

    /// `open` 的可观测版本：每一条返回路径（成功与失败）都发一条 `mount.check`
    /// trace 事件（spec §3.3、FORMAT.md §10.3 的事件表）。
    ///
    /// **失败路径的 trace 比成功路径更重要**——挂载检查是全项目最危险的判断
    /// （把未挂载的卷当成空库会触发删除对账，清掉用户本地数据，I11）。事后要能
    /// 回答「当时期望的 dataset_id 是什么、实际读到的是什么、判定结果是什么」，
    /// 靠的就是失败路径同样留下的这条记录，而不是去正则捞日志字符串。
    ///
    /// `t_abs_us` 由调用方注入，函数内部绝不读系统时钟：`arca-store` 虽然做
    /// IO，时钟仍需注入，spec §11.2 的确定性模拟测试才能逐字节重放挂载检查——
    /// 这正是崩溃注入测试要覆盖的场景之一。
    ///
    /// trace 字段语义：
    /// - `ok`：本次检查的判定结果。
    /// - `found`：卷内实际读到的身份。根缺失、读取失败、解析失败，或调用方
    ///   参数本身不合法而从未发起读取时，均为**空字符串而非省略该字段**——
    ///   agent 对字段做精确匹配，缺字段与空值是不同的信号。
    /// - `expect`：调用方传入的期望值。`expected_dataset_id` 为 `None` 时
    ///   省略本字段——这时确实没有期望，不是「期望是空字符串」。
    /// - `dataset_id`：本次检查的主体标识，取 `expect`（若有）否则 `found`
    ///   （若已知），供按数据集过滤 trace，不参与判定本身。
    pub fn open_traced(
        root: &Path,
        expected_dataset_id: Option<&str>,
        t_abs_us: u64,
        sink: &mut dyn TraceSink,
    ) -> Result<Self, MountError> {
        if let Some(expected) = expected_dataset_id {
            if !is_hex32(expected) {
                emit_mount_check(sink, t_abs_us, false, Some(expected), "");
                return Err(MountError::BadExpectedId {
                    value: expected.to_string(),
                });
            }
        }

        let format_path = root.join(layout::FORMAT_JSON);
        let text = match fs::read_to_string(&format_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                emit_mount_check(sink, t_abs_us, false, expected_dataset_id, "");
                return Err(MountError::Absent {
                    path: root.display().to_string(),
                });
            }
            Err(e) => {
                emit_mount_check(sink, t_abs_us, false, expected_dataset_id, "");
                return Err(MountError::Io {
                    path: format_path.display().to_string(),
                    reason: e.to_string(),
                });
            }
        };

        let format = match FormatJson::parse(&text) {
            Ok(format) => format,
            Err(source) => {
                emit_mount_check(sink, t_abs_us, false, expected_dataset_id, "");
                return Err(MountError::Malformed {
                    path: format_path.display().to_string(),
                    source,
                });
            }
        };

        if let Some(expected) = expected_dataset_id {
            if expected != format.dataset_id {
                emit_mount_check(sink, t_abs_us, false, Some(expected), &format.dataset_id);
                return Err(MountError::IdentityMismatch {
                    expected: expected.to_string(),
                    found: format.dataset_id.clone(),
                });
            }
        }

        emit_mount_check(
            sink,
            t_abs_us,
            true,
            expected_dataset_id,
            &format.dataset_id,
        );
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

/// 发一条 `mount.check` trace 事件（`open_traced` 的每条返回路径都调它）。
///
/// `found` 由调用方以 `""` 显式传入表示「未知/未读到」，本函数不做默认值替换——
/// 空字符串与省略字段是两种不同的信号，省略与否必须由调用点按语义决定。
fn emit_mount_check(
    sink: &mut dyn TraceSink,
    t_abs_us: u64,
    ok: bool,
    expect: Option<&str>,
    found: &str,
) {
    let dataset_id = expect.unwrap_or(found).to_string();
    let mut record = TraceRecord::new(EventKind::MountCheck, t_abs_us)
        .with("ok", ok)
        .with("found", found.to_string())
        .with("dataset_id", dataset_id);
    if let Some(expect) = expect {
        record = record.with("expect", expect.to_string());
    }
    sink.record(record);
}
