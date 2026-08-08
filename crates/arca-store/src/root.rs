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
use arca_format::trace::{ErrorClass, EventKind, TraceRecord, TraceSink};
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

/// `StorageRoot::create` 引导一个全新存储根时的失败。彼此可区分（I5）。
///
/// **为什么这个函数住在 `arca-store` 而不是某个消费者 crate**：读写存储根的
/// 消费者不止一个——M1 的 `arca-cli`（`file://` 直连同步、`arca adopt`）与
/// M2 的 `arcad`（hub 侧初始化数据集根）都需要「引导一个全新存储根」这件事，
/// 且创建逻辑必须与 `StorageRoot::open`/`FormatJson::parse` 的校验逻辑挨在一起：
/// 将来 `format.json` 的字段变化，两者要一起改，放在消费者 crate 里会漂移
/// （建出一个自己打不开的根）。M1a 的计划没排到这个函数，属于本轮（M1d）
/// 对 `arca-store` 职责范围的合理补齐，而非越界。
#[derive(Debug)]
pub enum CreateError {
    /// 调用方传入的 `dataset_id` 不合法（不是 32 位小写十六进制）——参数错误，
    /// 不是磁盘状态的问题。`dataset_id` 由调用方生成并传入（不在本 crate 内生成），
    /// 这样测试才能构造确定性的样例，与 `open_traced` 的 `t_abs_us` 注入同一条纪律。
    BadDatasetId { value: String },
    /// 该路径下已经有 `format.json`——拒绝覆盖。绝不静默重置一个可能活着的存储根
    /// （I5：状态模糊就停下；这里状态并不模糊，是明确"已存在"，同样拒绝）。
    AlreadyExists { path: String },
    /// 骨架目录（`files/`、`.arca/{index,items,chunks,journal,tmp,trash,uploads,locks}/`）
    /// 创建失败：权限、磁盘满等。
    Io { path: String, reason: String },
    /// `format.json` 序列化失败（`FormatJson::to_json` 目前不可达的分支，见其文档）。
    Format(FormatError),
    /// `format.json` 的原子写入失败——复用 [`crate::atomic::write`]，因此这里直接
    /// 包住 [`crate::atomic::AtomicError`]，不拍扁成字符串。
    Write(crate::atomic::AtomicError),
}

impl fmt::Display for CreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateError::BadDatasetId { value } => write!(
                f,
                "调用方传入的 dataset_id {value:?} 不是合法的 32 位小写十六进制——这是参数错误"
            ),
            CreateError::AlreadyExists { path } => write!(
                f,
                "{path} 已经存在 {}，拒绝覆盖——绝不重置一个可能活着的存储根",
                layout::FORMAT_JSON
            ),
            CreateError::Io { path, reason } => write!(f, "创建骨架目录 {path} 失败：{reason}"),
            CreateError::Format(e) => write!(f, "format.json 序列化失败：{e}"),
            CreateError::Write(e) => write!(f, "写入 format.json 失败：{e}"),
        }
    }
}

impl std::error::Error for CreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CreateError::Format(e) => Some(e),
            CreateError::Write(e) => Some(e),
            _ => None,
        }
    }
}

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
    /// `NullSink` 的薄壳（Rule of Silence：`TraceRecord` 仍会照常构造，
    /// 只是 `NullSink` 落地即丢，不落盘、不外泄）。
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
                emit_error(
                    sink,
                    t_abs_us,
                    "mount.bad_expected_id",
                    ErrorClass::Bug,
                    "",
                    format!(
                        "调用方传入的期望 dataset_id {expected:?} 不是合法的 32 位小写十六进制"
                    ),
                );
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
                emit_error(
                    sink,
                    t_abs_us,
                    "mount.absent",
                    ErrorClass::NeedsHuman,
                    &root.display().to_string(),
                    format!("存储根缺少 {}——卷未挂载或路径错误", layout::FORMAT_JSON),
                );
                return Err(MountError::Absent {
                    path: root.display().to_string(),
                });
            }
            Err(e) => {
                emit_mount_check(sink, t_abs_us, false, expected_dataset_id, "");
                // 权限、挂载点损坏等——同属「停下报告给人」，不归 retryable：
                // NFS 抖动那类值得重试的场景由上层策略判断，不是这里的职责
                // （§此文件顶部关于三态区分的说明）。
                emit_error(
                    sink,
                    t_abs_us,
                    "mount.io_error",
                    ErrorClass::NeedsHuman,
                    &format_path.display().to_string(),
                    e.to_string(),
                );
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
                emit_error(
                    sink,
                    t_abs_us,
                    "format.malformed",
                    ErrorClass::NeedsHuman,
                    &format_path.display().to_string(),
                    source.to_string(),
                );
                return Err(MountError::Malformed {
                    path: format_path.display().to_string(),
                    source,
                });
            }
        };

        if let Some(expected) = expected_dataset_id {
            if expected != format.dataset_id {
                emit_mount_check(sink, t_abs_us, false, Some(expected), &format.dataset_id);
                emit_error(
                    sink,
                    t_abs_us,
                    "mount.identity_mismatch",
                    ErrorClass::NeedsHuman,
                    &root.display().to_string(),
                    format!("期望 dataset_id {expected}，实际是 {}", format.dataset_id),
                );
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

    /// 引导一个全新存储根：建骨架目录（`files/`、`.arca/{index,items,chunks,
    /// journal,tmp,trash,uploads,locks}/`），原子写入 `format.json`。
    ///
    /// `root` 目录本身不必预先存在——`arca adopt` 在一个全新的挂载点上建首个
    /// 数据集时，这个目录很可能还没被创建过。`dataset_id` 由调用方生成并传入
    /// （不在本函数内生成随机数，保持本 crate 内确定性可测；`created_at` 同理，
    /// 由调用方注入当前时间字符串，本 crate 不读系统时钟）。
    ///
    /// **拒绝在已有 `format.json` 的目录上重复创建**（[`CreateError::AlreadyExists`]）：
    /// 用 `symlink_metadata` 探测存在性（不跟随链接——即便那是个指向别处的符号
    /// 链接，也已经是"这个位置有东西"，同样拒绝，不去纠结它到底指向真的
    /// `format.json` 还是别的什么）。
    ///
    /// `format.json` 本身经 [`crate::atomic::write`] 原子写入（tmp → fsync →
    /// rename → fsync 父目录链），不是裸 `fs::write`——它承载卷身份（I11），
    /// 半截写入或写入后未落盘都不可接受，与存储根内其余内容同一持久化标准。
    pub fn create(root: &Path, dataset_id: &str, created_at: &str) -> Result<Self, CreateError> {
        if !is_hex32(dataset_id) {
            return Err(CreateError::BadDatasetId {
                value: dataset_id.to_string(),
            });
        }

        let format_path = root.join(layout::FORMAT_JSON);
        match fs::symlink_metadata(&format_path) {
            Ok(_) => {
                return Err(CreateError::AlreadyExists {
                    path: format_path.display().to_string(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(CreateError::Io {
                    path: format_path.display().to_string(),
                    reason: e.to_string(),
                })
            }
        }

        // 骨架目录：files/ 是逃生舱（I1），.arca/ 下各旁路目录见 FORMAT.md §4。
        // EPOCH_FILE/index/items 等具体文件不在这里创建——那些随首次真正写入
        // 才出现，本函数只保证"目录存在"这一层前提（`atomic::write` 需要
        // `.arca/tmp/` 已存在，其余目录会在各自首次写入时按需 `create_dir_all`）。
        for dir in [
            layout::FILES_DIR,
            layout::INDEX_DIR,
            layout::ITEMS_DIR,
            layout::CHUNKS_DIR,
            layout::JOURNAL_DIR,
            layout::TMP_DIR,
            layout::TRASH_DIR,
            layout::UPLOADS_DIR,
            layout::LOCKS_DIR,
        ] {
            let full = root.join(dir);
            fs::create_dir_all(&full).map_err(|e| CreateError::Io {
                path: full.display().to_string(),
                reason: e.to_string(),
            })?;
        }

        // format.json 的记录/存储根格式版本目前都固定为 1（FORMAT.md §0、§5）。
        // 这两个值本可从 `arca_format::hub_layout` 复用，但那两个常量是私有的
        // （`FormatJson::parse`/`to_json` 内部自持版本号，调用方不需要关心）；
        // 这里只需要"v1 是当前唯一认可的版本"这一件事，直接字面量加注释，
        // 不为此扩大 `arca-format` 的公开面。
        let format = FormatJson {
            format: 1,
            dataset_id: dataset_id.to_string(),
            hash_algo: "blake3".to_string(),
            created_at: created_at.to_string(),
        };
        let bytes = format.to_json().map_err(CreateError::Format)?;

        // 此时 format.json 尚未落盘，`StorageRoot::open` 校验不过；但
        // `atomic::write` 只需要 `path()`/`join()`，不要求文件已存在——
        // 构造一个"暂存"实例专供这一次写入使用是安全的（不对外暴露）。
        let staged = StorageRoot {
            path: root.to_path_buf(),
            format,
        };
        crate::atomic::write(&staged, layout::FORMAT_JSON, bytes.as_bytes())
            .map_err(CreateError::Write)?;

        Ok(staged)
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
    /// 拒绝四种会逃出存储根、或指向存储根本身/其上级的输入：绝对路径、
    /// 含 `..` 父目录引用的路径、含 Windows 盘符前缀（如 `C:`）的路径、
    /// 以及不含任何正常分量的路径（空串 `""` 或仅 `.`）。`a..b` 这样文件名
    /// 里含两个点但不构成父目录引用的路径会被放行。
    ///
    /// 最后一种拒绝理由不那么显眼，但同样是逃逸：`""` 与 `"."` 都不含
    /// `ParentDir`、不含 `Prefix`、也不是绝对路径，字面上会放行，拼接后
    /// `target` 等于存储根本身，`target.parent()` 就成了存储根的**上一级**——
    /// 调用方（如 `atomic::write`）据此对父目录 `create_dir_all` /
    /// `sync_dir`，作用范围就悄悄溢出到根之外了。写入本身会因
    /// `rename(文件, 目录)` 失败而落不了地，不构成数据损坏，但这道校验的
    /// 职责就是「不必在每个调用点重新推导根的安全性」，放过这种输入等于
    /// 没做到。
    pub fn join(&self, relative: &str) -> Result<PathBuf, RootEscape> {
        let rel_path = Path::new(relative);

        if rel_path.is_absolute() {
            return Err(RootEscape {
                relative: relative.to_string(),
                reason: "绝对路径会丢弃存储根，逃出存储根之外",
            });
        }

        let mut has_normal_component = false;
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
                Component::Normal(_) => has_normal_component = true,
                Component::CurDir | Component::RootDir => {}
            }
        }

        if !has_normal_component {
            return Err(RootEscape {
                relative: relative.to_string(),
                reason: "不含任何正常路径分量（空串或仅 `.`），拼接后等于存储根本身或其上级",
            });
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

/// 配合失败路径的 `mount.check` 额外发一条 `error` 事件
/// （FORMAT.md §10.4、PROTOCOL.md §7）。
///
/// **为什么需要这一条，`mount.check` 不够**：`mount.check` 的载荷被
/// FORMAT.md §10.3 钉死为 `dataset_id`/`expect`/`found`/`ok` 四个字段——
/// `Absent`、`Io`、`Malformed` 三种失败 `found` 都是空字符串、`ok` 都是
/// `false`、`dataset_id` 都取同一个 `expect`，产出的载荷逐字节相同，agent
/// 看 `mount.check` 本身无法回答「是没挂载、挂着但读不了、还是身份读不出
/// 来」。改 `mount.check` 的字段集需要改 FORMAT.md 规范；`error` 事件的
/// schema 本就是为承载 `code`/`class` 设计的，不需要碰规范就能补上这条
/// 区分度。
fn emit_error(
    sink: &mut dyn TraceSink,
    t_abs_us: u64,
    code: &'static str,
    class: ErrorClass,
    path: &str,
    detail: String,
) {
    let record = TraceRecord::new(EventKind::Error, t_abs_us)
        .with("code", code)
        .with("class", class)
        .with("retryable", class.is_retryable())
        .with("path", path.to_string())
        .with("detail", detail);
    sink.record(record);
}
