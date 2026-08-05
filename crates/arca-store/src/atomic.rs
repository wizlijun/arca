//! 原子写入（tmp → fsync → rename → fsync 父目录）。
//!
//! **要保证的性质**：崩溃后要么看到旧内容、要么看到新内容，绝不看到半截——
//! arca 存的是用户笔记里唯一一份的照片，一次断电写出半个文件而系统以为它
//! 完好，是本项目最不能接受的失败。
//!
//! 契约见 FORMAT.md §4：存储根下所有目录必须位于同一文件系统，`rename`
//! 才谈得上原子；本模块把临时文件建在 `<root>/.arca/tmp/` 下正是依赖这条
//! 前提——不能用 `std::env::temp_dir()`，那多半是另一个文件系统（如 tmpfs）。

use crate::root::{RootEscape, StorageRoot};
use arca_format::hub_layout::layout;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// 原子写入失败的失败态。彼此可区分（I5：如实报告失败的性质）。
#[derive(Debug)]
pub enum AtomicError {
    /// 目标相对路径本身不合法（逃出存储根）——这是调用方的参数错误，不是
    /// IO 故障。直接包住 [`RootEscape`] 而不是拍扁成字符串塞进 `Io`：
    /// `RootEscape` 已经带着结构化的 `relative` + `reason`，拍扁会让调用方
    /// 失去按类型区分「参数错误」与「磁盘/权限故障」的能力，也会让这条
    /// 错误看起来像是可以重试的 IO 抖动——其实重试多少次都不会成功，
    /// 必须改调用方传入的路径。
    InvalidPath(RootEscape),
    /// 常规 IO 失败：权限、磁盘满、目标父目录不可创建等。
    Io { path: String, reason: String },
    /// `rename` 报告 tmp 与目标不在同一文件系统——`rename` 因此不是原子的。
    /// 这是存储根违反 FORMAT.md §4「所有目录必须位于同一文件系统」前提的
    /// 配置错误，不是可重试的 IO 抖动，需要人工介入修正挂载布局
    /// （needs_human），不应自动重试。
    CrossDevice { tmp: String, target: String },
}

impl fmt::Display for AtomicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AtomicError::InvalidPath(e) => write!(f, "{e}"),
            AtomicError::Io { path, reason } => write!(f, "原子写入 {path} 失败：{reason}"),
            AtomicError::CrossDevice { tmp, target } => write!(
                f,
                "临时文件 {tmp} 与目标 {target} 不在同一文件系统，rename 不是原子的——\
                 这是存储根的配置错误（违反 FORMAT.md §4），需要人工修正挂载布局，不应重试"
            ),
        }
    }
}

impl std::error::Error for AtomicError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AtomicError::InvalidPath(e) => Some(e),
            AtomicError::Io { .. } | AtomicError::CrossDevice { .. } => None,
        }
    }
}

/// `StorageRoot::join` 现在返回 `Result<PathBuf, RootEscape>`（`Path::join`
/// 遇到绝对路径会把根整个丢掉，必须校验）。用 `From` 而不是在每个调用点
/// `.map_err(...)`：`?` 在 `write` 里一步到位地把「路径逃逸」并入本模块的
/// 错误类型，调用方看到的仍是唯一的 `AtomicError`，不必关心它内部由几种
/// 子错误拼成。
impl From<RootEscape> for AtomicError {
    fn from(e: RootEscape) -> Self {
        AtomicError::InvalidPath(e)
    }
}

fn io_error(path: &Path, e: &io::Error) -> AtomicError {
    AtomicError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// 同进程内保证不撞的单调计数器，配合 pid + 目标路径哈希组成 tmp 文件名。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 生成 tmp 文件名：`<pid>-<单调计数器>-<目标路径哈希>.tmp`。
///
/// 三段各防一种碰撞（约束 1：**不用随机数**）：
/// - `pid`：区分同一存储根上不同进程各自的临时文件；
/// - 单调递增计数器：区分同一进程内先后或并发发起的多次写入——计数器
///   天然不重复，且完全可复现（同一次运行、同样的调用顺序会得到同样的
///   序号），随机数在崩溃复盘时没法回放这一点；
/// - 目标路径哈希（`DefaultHasher`，键固定、确定性输出，不涉及随机种子）：
///   即便计数器意外重合（理论上不会，这里只是双保险），文件名也会因目标
///   不同而不同；顺带的好处是崩溃后人工翻 `.arca/tmp/` 残留时，从文件名
///   就能大致猜出它对应哪次写入。
fn tmp_file_name(relative_target: &str) -> String {
    let pid = std::process::id();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut hasher = DefaultHasher::new();
    relative_target.hash(&mut hasher);
    let digest = hasher.finish();
    format!("{pid}-{seq}-{digest:016x}.tmp")
}

/// 把 `bytes` 写进 `tmp_path` 并 fsync 文件本身（约束 2 的前两步）。
fn write_and_sync(tmp_path: &Path, bytes: &[u8]) -> Result<(), AtomicError> {
    let mut file = File::create(tmp_path).map_err(|e| io_error(tmp_path, &e))?;
    file.write_all(bytes).map_err(|e| io_error(tmp_path, &e))?;
    // fsync：把内容从页缓存刷到磁盘介质。没有这一步，接下来的 rename 只是
    // 让目录项指向这个 inode，inode 里的数据仍可能只停留在缓存中——崩溃后
    // 目标文件存在、大小可能都对，内容却不完整（半截），正是本模块要杜绝
    // 的失败形态。
    file.sync_all().map_err(|e| io_error(tmp_path, &e))?;
    // 显式关闭文件句柄再返回：rename 前不留着打开的句柄，是跨平台的稳妥
    // 做法（Windows 下尤其要求源文件已关闭；虽然目前只在 Unix 上跑，提前
    // 关闭没有坏处，见 sync_dir 处关于 Windows 的说明）。
    drop(file);
    Ok(())
}

/// 尽力删除本次调用自己创建的临时文件（约束 4：失败时清理，但清理失败
/// 绝不能掩盖触发清理的原始错误——所以这里吞掉 `remove_file` 的结果）。
///
/// 这是本 crate 中唯一允许出现的删除代码路径（I3：不得删除用户数据）：
/// 删的是这次调用刚刚自己创建、从未被任何索引或记录引用过的临时文件，
/// 不是用户数据，也不是任何其他调用创建的文件。
fn cleanup_tmp(tmp_path: &Path) {
    let _ = fs::remove_file(tmp_path);
}

/// 原子写入 `bytes` 到 `root` 内的 `relative_target`。
///
/// 崩溃后要么看到调用前的旧内容、要么看到这次写入的完整新内容，绝不会看到
/// 半截——靠下面严格的四步顺序（约束 2）：
///
/// 1. 内容全部写进 `.arca/tmp/` 下的临时文件；
/// 2. `File::sync_all()` 把内容从页缓存刷到磁盘——没有这一步，`rename`
///    之后目录项虽然指向新文件，文件内容却可能仍卡在缓存里，崩溃后目标
///    存在但内容不全；
/// 3. `fs::rename` 把临时文件换到目标路径——同一文件系统内单个目录项的
///    替换是原子的，这一步之前无论进行到哪里崩溃，目标路径看到的都还是
///    旧内容（或压根不存在）；
/// 4. fsync 目标所在的父目录——`rename` 只保证目录项在页缓存里立刻可见，
///    目录项本身何时落盘是另一回事：崩溃可能让这次目录项变化「消失」，
///    回退到 rename 之前的状态（这是 Unix 文件系统的已知行为，也是本函数
///    要覆盖的最后一段崩溃窗口）。不做这一步，写入在「rename 已完成、
///    宿主机没崩」的世界里看起来完全正确，只有真的断电才会暴露漏洞——
///    这正是本模块存在的意义。
///
/// 临时文件必须与目标同一文件系统（因此建在 `<root>/.arca/tmp/` 而不是
/// `std::env::temp_dir()`），否则第 3 步的 `rename` 会退化成「跨设备复制 +
/// 删除源」，不再是原子操作，也不再具备本函数承诺的性质——这正是
/// FORMAT.md §4 要求「存储根下所有目录必须位于同一文件系统」的原因。若
/// `rename` 报告跨设备失败，返回 [`AtomicError::CrossDevice`]（约束 5）：
/// 这是配置错误，不是可重试的 IO 抖动。
///
/// `.arca/tmp/` 本身的存在性由存储根的初始化/挂载流程保证（layout 契约的
/// 一部分，属于挂载阶段的职责，不在本函数的职责范围）——本函数不会创建
/// 它；若它缺失，创建临时文件会得到 `NotFound`，如实作为 `AtomicError::Io`
/// 报告（I5），不做静默补救。目标的父目录则不同：`relative_target` 可能是
/// 首次出现的深层路径（如 `files/京都/鸭川.png`），要求调用方提前手工
/// `create_dir_all` 不现实，所以第 3 步之前会自动创建（约束 3）。
pub fn write(root: &StorageRoot, relative_target: &str, bytes: &[u8]) -> Result<(), AtomicError> {
    let target = root.join(relative_target)?;

    let target_parent = target.parent().unwrap_or_else(|| root.path());
    fs::create_dir_all(target_parent).map_err(|e| io_error(target_parent, &e))?;

    // `.arca/tmp` 是布局里的固定常量，不是调用方传入的相对路径，不经过
    // `StorageRoot::join` 的逃逸校验——那道校验是为不可信输入准备的。
    let tmp_dir = root.path().join(layout::TMP_DIR);
    let tmp_path = tmp_dir.join(tmp_file_name(relative_target));

    if let Err(e) = write_and_sync(&tmp_path, bytes) {
        cleanup_tmp(&tmp_path);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp_path, &target) {
        let mapped = if e.kind() == io::ErrorKind::CrossesDevices {
            AtomicError::CrossDevice {
                tmp: tmp_path.display().to_string(),
                target: target.display().to_string(),
            }
        } else {
            io_error(&tmp_path, &e)
        };
        // rename 在 POSIX 上要么完全没发生、要么完全发生，不存在半途而废
        // 的状态——失败意味着 tmp_path 这个名字仍然是我们自己的临时文件，
        // 清理它是安全的。
        cleanup_tmp(&tmp_path);
        return Err(mapped);
    }

    // rename 成功：tmp_path 这个名字已经不存在了（它被换成了目标名），
    // 没有自己的临时文件需要清理。剩下要做的是让「这次 rename 发生过」
    // 这件事本身落盘——即约束 2 的第四步。
    sync_dir(target_parent)
}

/// fsync 一个目录，让目录项的变化（如本模块里的 `rename`）本身落盘。
///
/// **Unix**：目录可以像文件一样 `File::open` 再 `sync_all`，这是 fsync
/// 目录的标准写法，不需要 `unsafe`。
///
/// **非 Unix（Windows 等）**：`File::open` 对目录直接失败，标准库没有
/// 等价调用；这里选择跳过而不是报错，是诚实的平台局限而不是遗漏——
/// Windows 上等价的持久化保证需要平台特定 API（如 `FlushFileBuffers`
/// 配合 `FILE_FLAG_BACKUP_SEMANTICS` 打开目录句柄），属于 M3 的范围，
/// 本任务（M1）不覆盖（约束 6）。
#[cfg(unix)]
pub fn sync_dir(dir: &Path) -> Result<(), AtomicError> {
    File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_error(dir, &e))
}

#[cfg(not(unix))]
pub fn sync_dir(_dir: &Path) -> Result<(), AtomicError> {
    Ok(())
}

/// 清理 `.arca/tmp/` 下的崩溃残留报告。
///
/// `removed` 是被删掉的孤儿普通文件数；`refused` 逐条记录本次拒绝处理的
/// 条目（相对路径），说明「为什么不删」——不是静默跳过。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub removed: usize,
    pub refused: Vec<String>,
}

/// 清理 `root` 的 `.arca/tmp/` 目录：删掉里面的孤儿**普通文件**，
/// 符号链接与目录一律拒绝并记入 `refused`，绝不递归删除（I3 与 I5 的交叉，
/// 见本模块顶部注释与 `STORAGE.md` §Move And Delete Recovery）。
///
/// 判断条目类型必须用 [`fs::symlink_metadata`] 而不是 [`fs::metadata`]：
/// 后者会跟随符号链接，于是一个指向目录的链接会被误判成目录（本该拒绝却
/// 走了别的分支），一个指向普通文件的链接会被误判成普通文件从而被
/// `remove_file` 删掉——那正是「顺着链接删」，可能删掉的是用户的真实数据，
/// 是本函数存在的意义要防止的事。
///
/// `tmp/` 目录本身不存在时视为无操作（不是错误）：还没写过任何东西的
/// 全新存储根，或调用方尚未完成挂载流程创建它，都不构成需要清理的状态。
pub fn sweep_tmp(root: &StorageRoot) -> Result<SweepReport, AtomicError> {
    let tmp_dir = root.path().join(layout::TMP_DIR);
    let mut report = SweepReport::default();

    let entries = match fs::read_dir(&tmp_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(io_error(&tmp_dir, &e)),
    };

    for entry in entries {
        let entry = entry.map_err(|e| io_error(&tmp_dir, &e))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| io_error(&path, &e))?;
        let name = entry.file_name().to_string_lossy().to_string();

        if meta.is_file() {
            fs::remove_file(&path).map_err(|e| io_error(&path, &e))?;
            report.removed += 1;
        } else {
            // 目录或符号链接：拒绝处理，绝不递归删除、绝不顺着链接删除。
            report.refused.push(name);
        }
    }

    Ok(report)
}
