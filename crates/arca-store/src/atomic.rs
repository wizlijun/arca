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
use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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
    /// `rename` **已经成功**——目标路径已经是这次写入的新内容——但随后确认
    /// 落盘的 fsync（目标所在目录及其祖先，见 `sync_dir_chain_to_root`）失败。
    ///
    /// 与 [`AtomicError::Io`] 是完全不同性质的状态，绝不能塞进同一个变体：
    /// `Io` 意味着目标完全没动（例如 `write_all` 阶段磁盘满，tmp 文件都没写完）；
    /// 这个变体意味着目标**已经**是新内容，只是「这次替换已经发生」这件事本身
    /// 是否已经落盘还不确定——崩溃后最坏情况是目录项回退到 rename 之前的状态，
    /// 新内容变得暂时不可达，但绝不会出现半截内容（本模块的头号承诺仍成立）。
    ///
    /// **调用方不应该重试整个 `atomic::write`**：重试会把内容再写一遍、再
    /// `rename` 一遍，重复了已经成功的工作，而新内容其实已经在目标位置；
    /// 真正需要重试的只是「确认这次提交已经落盘」这一步（重新 fsync 目标
    /// 所在的目录链），不是从头开始的写入。M1b 的调和状态机与 M1d 的提交
    /// 路径靠这个变体区分「该重试 fsync／该回滚／该写 journal」——嗅探
    /// `path` 字段猜测状态是本代码库明确反对的做法。
    CommittedUnsynced { target: String, reason: String },
    /// `.arca/tmp` 本身不是一个真实目录（是符号链接，或路径类型不对，如
    /// 被换成了普通文件）——`sweep_tmp` 的整套安全性都建立在「`tmp_dir`
    /// 本身是真实目录」这个前提上：`fs::read_dir` 会跟随符号链接，若
    /// `tmp_dir` 是指向别处的链接，条目级别再严格的 `symlink_metadata`
    /// 判类型也没用，删的其实是链接目标目录里的文件——那可能是任何数据。
    /// 前提不成立时必须停下诊断（I5），绝不做任何删除（I3）。
    UnexpectedTmpState { path: String, reason: &'static str },
    /// [`Batch::commit`] 收口失败：批次内每一次 `write` 的「tmp → fsync 文件 →
    /// rename」都已经完成（内容已落盘、目标路径已经是新内容），只是这些写入
    /// 各自触碰过的目录，其「目录项变化本身已经落盘」这件事尚未被全部确认。
    ///
    /// 与 [`AtomicError::CommittedUnsynced`] 是同一件事在批量粒度上的聚合：
    /// 调用方不应该重试批次里任何一次已经成功的 `write`（重复写入没有意义，
    /// 内容早已落地），只需要整体把这次批量操作视为失败上报给用户——下一次
    /// 重跑时，已经成功落地的内容会被判定为 `Noop`/`AdoptBaseline`，不会
    /// 重复传输。`entries` 逐条记录失败的目录路径与原因，供诊断（I5）。
    BatchCommitFailed { entries: Vec<(String, String)> },
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
            AtomicError::CommittedUnsynced { target, reason } => write!(
                f,
                "{target} 已经写入并 rename 成功，但确认落盘的 fsync 失败：{reason}——\
                 目标路径已经是新内容，不应重试整个写入，只需重试落盘确认这一步"
            ),
            AtomicError::UnexpectedTmpState { path, reason } => write!(
                f,
                "{path} 不是真实目录（{reason}），拒绝清理——\
                 read_dir 会跟随符号链接，继续下去可能删掉的是链接目标目录里的数据"
            ),
            AtomicError::BatchCommitFailed { entries } => {
                write!(
                    f,
                    "批量提交收口失败：{} 个目录的 fsync 未成功（批次内的写入本身\
                     已经全部落盘并 rename 成功，不应重试整个批次）：",
                    entries.len()
                )?;
                for (path, reason) in entries {
                    write!(f, " [{path}: {reason}]")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AtomicError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AtomicError::InvalidPath(e) => Some(e),
            AtomicError::Io { .. }
            | AtomicError::CrossDevice { .. }
            | AtomicError::CommittedUnsynced { .. }
            | AtomicError::UnexpectedTmpState { .. }
            | AtomicError::BatchCommitFailed { .. } => None,
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
///
/// 用 `OpenOptions::create_new(true)` 而不是 `File::create`：后者会跟随
/// 符号链接、并静默截断任何已存在的同名文件——`tmp_file_name` 里的 pid
/// 段在操作系统回收 pid 后可能与更早一次崩溃残留的文件重名，`File::create`
/// 会悄悄覆盖掉那次残留（可能还没被 `sweep_tmp` 处理），`create_new` 遇到
/// 同名文件则诚实地报 `EEXIST`，绝不跟随链接、绝不静默覆盖。
///
/// 命中 `EEXIST`（或任何 `open` 失败）时**不清理** `tmp_path`：这种情况下
/// 我们从未创建过它，它要么是别的调用正在使用的文件、要么是尚待人工/
/// `sweep_tmp` 处理的残留，删掉它不属于「本次调用清理自己创建的临时文件」
/// （见 [`cleanup_tmp`] 的注释），只有在我们确实创建成功、随后写入/同步
/// 失败时才需要清理。
fn write_and_sync(tmp_path: &Path, bytes: &[u8]) -> Result<(), AtomicError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp_path)
        .map_err(|e| io_error(tmp_path, &e))?;

    if let Err(e) = file.write_all(bytes) {
        drop(file);
        cleanup_tmp(tmp_path);
        return Err(io_error(tmp_path, &e));
    }
    // fsync：把内容从页缓存刷到磁盘介质。没有这一步，接下来的 rename 只是
    // 让目录项指向这个 inode，inode 里的数据仍可能只停留在缓存中——崩溃后
    // 目标文件存在、大小可能都对，内容却不完整（半截），正是本模块要杜绝
    // 的失败形态。
    if let Err(e) = file.sync_all() {
        drop(file);
        cleanup_tmp(tmp_path);
        return Err(io_error(tmp_path, &e));
    }
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
/// 4. fsync 目标所在的父目录、以及 `create_dir_all` 可能新建的每一层祖先
///    目录，一路向上到存储根——`rename` 只保证目录项在页缓存里立刻可见，
///    目录项本身何时落盘是另一回事：崩溃可能让这次目录项变化「消失」，
///    回退到 rename 之前的状态（这是 Unix 文件系统的已知行为，也是本函数
///    要覆盖的最后一段崩溃窗口）。同一条论证对 `create_dir_all` 新建的
///    上层目录同样成立——`files/京都/鸭川.png` 若是 `京都` 这一层第一次
///    出现，`files` 目录里指向 `京都` 的那条目录项也可能只停留在缓存里；
///    只 fsync 最深一层（`京都`）不够，`files` 不 fsync，`京都` 存在这件事
///    本身在崩溃后可能消失，`write()` 报告已提交的文件反而不可达。不做
///    这一步，写入在「rename/mkdir 已完成、宿主机没崩」的世界里看起来
///    完全正确，只有真的断电才会暴露漏洞——这正是本模块存在的意义。
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
    let (target, target_parent) = write_and_rename(root, relative_target, bytes)?;

    // rename 成功：tmp_path 这个名字已经不存在了（它被换成了目标名），
    // 没有自己的临时文件需要清理。剩下要做的是让「这次 rename 发生过」
    // 以及「create_dir_all 新建的每一层目录都存在」这两件事本身落盘——
    // 即约束 2 的第四步。从这里往后任何失败都意味着目标已经是新内容、
    // 只是持久性未确认，必须报 `CommittedUnsynced` 而不是普通 `Io`——
    // 调用方绝不能因此重试整个写入（见该变体的文档）。
    sync_dir_chain_to_root(root.path(), &target_parent).map_err(|e| {
        AtomicError::CommittedUnsynced {
            target: target.display().to_string(),
            reason: e.to_string(),
        }
    })
}

/// 流式写入句柄：[`write`] 要求调用方先把整份内容攒成 `&[u8]` 再一次性
/// 交出——HTTP 服务端在把一个请求体的全部字节吃进内存之前，内存占用就已经
/// 与请求体体积成正比，这与"一次请求不该占用与其体积成正比的内存"
/// （M2b 切片评审 C2：600MB 的 PUT 让 RSS 从 6MB 涨到 1.86GB）直接矛盾。
///
/// `TmpWriter` 把 [`write`] 拆成两段：内容这一半改成调用方自己驱动的写入
/// 循环（每次一个网络分片，边到达边写、边写边可以在调用方那侧增量算
/// 哈希），落盘这一半（fsync 文件 → rename → fsync 目标目录链，约束 2 的
/// 第 2–4 步）仍然由本类型负责，持久化保证与 [`write`] 完全一致——两者
/// 共用同一份 `tmp_file_name` 生成规则与 `create_new` 语义（约束 1：不用
/// 随机数；`EEXIST` 诚实报错，不跟随链接、不静默覆盖，见 [`write_and_sync`]
/// 的文档）。
pub struct TmpWriter {
    tmp_path: PathBuf,
    /// `finish`/`abandon` 会 `take()` 走它——之后 `Drop` 据此判断"这次写入
    /// 有没有被明确收口过"，还留着就说明调用方提前 return 或 panic 了，
    /// 兜底清理，绝不留孤儿临时文件（约束 4）。
    file: Option<File>,
}

impl TmpWriter {
    /// 在 `.arca/tmp/` 下新建一个临时文件，准备接收流式写入。
    /// `relative_target` 只用于文件名里那段哈希（供崩溃后人工排查猜测
    /// 归属），真正的落点由 [`TmpWriter::finish`] 的参数决定。
    ///
    /// **不持有 `&StorageRoot`**（评审 C2 的一处实现教训，值得记下来）：
    /// 最初的版本把 `root: &'a StorageRoot` 存成字段，`create`/`finish`
    /// 之间横跨若干次 `.await`（HTTP 服务端边接收网络分片边写）——这让
    /// `TmpWriter<'a>` 成为一个自借用其宿主 `StorageRoot` 的类型，在
    /// `arcad` 的 `PUT` handler 里跨 `.await` 持有它，会让 axum
    /// `Handler` trait 要求的 `Future: Send + 'static` 推导失败（表现成
    /// 一条不知所云的 `Handler<_, _> 未实现` 报错，而不是直接指向真正
    /// 原因的借用检查错误）。`root` 因此改成只在需要的两个方法
    /// （[`create`](Self::create)/[`finish`](Self::finish)）里按参数传入，
    /// 不跨越任何 `await` 点持有。
    pub fn create(root: &StorageRoot, relative_target: &str) -> Result<Self, AtomicError> {
        let tmp_dir = root.path().join(layout::TMP_DIR);
        let tmp_path = tmp_dir.join(tmp_file_name(relative_target));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| io_error(&tmp_path, &e))?;
        Ok(Self {
            tmp_path,
            file: Some(file),
        })
    }

    /// 追加一段字节——调用方按到达顺序多次调用即可，不需要预先知道总长度。
    pub fn write_all(&mut self, chunk: &[u8]) -> Result<(), AtomicError> {
        let file = self
            .file
            .as_mut()
            .expect("TmpWriter::write_all 不应在 finish/abandon 之后调用");
        file.write_all(chunk)
            .map_err(|e| io_error(&self.tmp_path, &e))
    }

    /// 收口：fsync 文件本身、rename 到 `relative_target`、fsync 目标所在的
    /// 目录链（约束 2 的第 2–4 步，与 [`write`] 逐字对应）。`root` 见
    /// [`TmpWriter::create`] 文档「不持有 `&StorageRoot`」一节。
    pub fn finish(mut self, root: &StorageRoot, relative_target: &str) -> Result<(), AtomicError> {
        let file = self.file.take().expect("TmpWriter::finish 只应调用一次");
        if let Err(e) = file.sync_all() {
            drop(file);
            cleanup_tmp(&self.tmp_path);
            return Err(io_error(&self.tmp_path, &e));
        }
        // 显式关闭再 rename——与 write_and_sync 同一条纪律（rename 前不留
        // 打开的句柄）。
        drop(file);

        let target = root.join(relative_target)?;
        let target_parent = target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.path().to_path_buf());
        if let Err(e) = fs::create_dir_all(&target_parent) {
            cleanup_tmp(&self.tmp_path);
            return Err(io_error(&target_parent, &e));
        }

        if let Err(e) = fs::rename(&self.tmp_path, &target) {
            let mapped = if e.kind() == io::ErrorKind::CrossesDevices {
                AtomicError::CrossDevice {
                    tmp: self.tmp_path.display().to_string(),
                    target: target.display().to_string(),
                }
            } else {
                io_error(&self.tmp_path, &e)
            };
            cleanup_tmp(&self.tmp_path);
            return Err(mapped);
        }

        sync_dir_chain_to_root(root.path(), &target_parent).map_err(|e| {
            AtomicError::CommittedUnsynced {
                target: target.display().to_string(),
                reason: e.to_string(),
            }
        })
    }

    /// 主动放弃这次写入并清理临时文件——调用方在写入循环中途发现错误
    /// （例如请求体超出体积上限、客户端提前断开）时用它：与其依赖 `Drop`
    /// 悄悄兜底，不如显式表达"这是一次已知的、主动的放弃"，调用点的意图
    /// 更清楚。
    pub fn abandon(mut self) {
        self.file.take();
        cleanup_tmp(&self.tmp_path);
    }
}

impl Drop for TmpWriter {
    fn drop(&mut self) {
        // `finish`/`abandon` 都已经 `take()` 走 `file` 并各自处理过临时
        // 文件；这里只兜底"调用方因为提前 return（`?`）或 panic 而两者都
        // 没调用"的情形——本 crate 不允许留下不属于任何记录的孤儿文件
        // （约束 4：失败时清理）。
        if self.file.take().is_some() {
            cleanup_tmp(&self.tmp_path);
        }
    }
}

/// `write` 与 [`Batch::write`] 共用的核心：tmp → fsync 文件 → rename（约束 2
/// 的前三步）。返回 `(目标路径, 目标的父目录路径)`，供调用方决定「接下来是
/// 立刻 fsync 目录链（单次 `write`）还是先记下来、延后到批量收尾一次性
/// fsync（[`Batch::write`]）」——**内容这一半的持久化保证（文件级 fsync）
/// 在这里已经完成，两条路径完全一致，绝不因为走批量就打折扣**；两条路径
/// 唯一的差别只在「目录项变化几时确认落盘」。
fn write_and_rename(
    root: &StorageRoot,
    relative_target: &str,
    bytes: &[u8],
) -> Result<(PathBuf, PathBuf), AtomicError> {
    let target = root.join(relative_target)?;

    let target_parent = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.path().to_path_buf());
    fs::create_dir_all(&target_parent).map_err(|e| io_error(&target_parent, &e))?;

    // `.arca/tmp` 是布局里的固定常量，不是调用方传入的相对路径，不经过
    // `StorageRoot::join` 的逃逸校验——那道校验是为不可信输入准备的。
    let tmp_dir = root.path().join(layout::TMP_DIR);
    let tmp_path = tmp_dir.join(tmp_file_name(relative_target));

    // `write_and_sync` 自己负责清理它自己创建成功的临时文件；命中
    // `create_new` 的 `EEXIST` 时它不会创建任何东西，也不会清理——那种
    // 情况下 tmp_path 不是我们的文件，删它就不再是「只删自己创建的孤儿
    // 文件」（I3）。
    write_and_sync(&tmp_path, bytes)?;

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

    Ok((target, target_parent))
}

/// 原子把存储根内部一个**已存在**的文件从 `from_relative` 移动到 `to_relative`
/// （`rename`，同一文件系统天然原子）+ fsync 两侧目录链确认落盘。
///
/// 与 [`write`] 是同一持久化纪律在"移动既有内容"这个场景下的对应实现：
/// `rename` 本身在 POSIX 上要么完全没发生、要么完全发生，但"这次目录项变化
/// 已经落盘"是另一回事——这次移动改了**两个**目录的目录项（源的父目录少了
/// 一条、目标的父目录多了一条），必须两侧都 fsync 到 `root`，否则崩溃可能
/// 只让其中一侧的变化持久化，留下「两边都看得到」或「两边都看不到」这份
/// 内容的状态。
///
/// 供 tombstone 执行把 `files/<path>` 移进 `.arca/trash/`（M2a tombstone
/// 计划 Task 3，FORMAT.md §7.3）：**这是移动，不是复制+删除**——过程中不
/// 存在"源与目标同时存在两份"的窗口，也不存在自己删除源文件的代码路径
/// （I3：同步路径无销毁权；`rename` 让源"消失"是操作系统对同一个 inode
/// 改名的效果，不是本模块新增的一次删除）。
///
/// 调用方须保证 `from_relative` 指向的文件确实存在——不存在时 `rename`
/// 报 `NotFound`，按普通 `Io` 错误向上传播，不做特殊包装：调用方通常在
/// 调用前已经用更贴近业务语义的方式确认过源存在（例如 tombstone 执行前的
/// 闸门检查），这里不重复发明一种"源缺失"的专属错误变体。
pub fn rename(
    root: &StorageRoot,
    from_relative: &str,
    to_relative: &str,
) -> Result<(), AtomicError> {
    let from = root.join(from_relative)?;
    let to = root.join(to_relative)?;

    let to_parent = to
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.path().to_path_buf());
    fs::create_dir_all(&to_parent).map_err(|e| io_error(&to_parent, &e))?;

    let from_parent = from
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.path().to_path_buf());

    if let Err(e) = fs::rename(&from, &to) {
        let mapped = if e.kind() == io::ErrorKind::CrossesDevices {
            AtomicError::CrossDevice {
                tmp: from.display().to_string(),
                target: to.display().to_string(),
            }
        } else {
            io_error(&from, &e)
        };
        return Err(mapped);
    }

    // rename 已经成功：从这里往后任何失败都意味着"移动本身已经发生"，只是
    // 两侧目录项变化的落盘确认还没做完，同一条 `CommittedUnsynced` 语义——
    // 调用方绝不应该因为这里失败就重试整个移动（源已经不在原处了，重试
    // `rename` 只会因为源不存在而报错）。
    sync_dir_chain_to_root(root.path(), &from_parent).map_err(|e| {
        AtomicError::CommittedUnsynced {
            target: to.display().to_string(),
            reason: format!("源目录 {} 落盘确认失败：{e}", from_parent.display()),
        }
    })?;
    sync_dir_chain_to_root(root.path(), &to_parent).map_err(|e| {
        AtomicError::CommittedUnsynced {
            target: to.display().to_string(),
            reason: format!("目标目录 {} 落盘确认失败：{e}", to_parent.display()),
        }
    })?;

    Ok(())
}

/// 批量原子写入：文件自身的「tmp → fsync → rename」逐次立即执行，持久性
/// 不打折扣；父目录的 fsync **推迟并去重**到 [`Batch::commit`]——批量归档
/// 场景下（`arca adopt`/`arca sync` 一次处理成千上万个文件），逐文件都做一次
/// 完整的目录链 fsync 是实测瓶颈：1 万文件基准里，归档占了 308.4 秒，
/// 全量校验只要 0.49 秒（`crates/arca-cli/tests/bench_10k.rs`）。
///
/// # 持久性论证：为什么这不比逐文件 `write` 弱
///
/// [`write`] 对单次调用承诺「返回时这次写入已经完全落盘，包括让它可达的
/// 目录项变化」。`Batch` 把这条承诺的粒度从「每次 `write`」改成「每次
/// `commit`」，但**没有削弱它**：
///
/// - 文件内容的 fsync（约束 2 第 1–2 步）在 [`Batch::write`] 内逐次立即执行，
///   与单次 `write` 完全一致——崩溃后绝不会看到半截内容，这条底线不变。
/// - `rename`（约束 2 第 3 步）同样在 `write` 内立即执行——目标路径在
///   `write` 返回时已经是新内容，只是这一事实尚未确认落盘。
/// - 目录 fsync（约束 2 第 4 步）延后到 `commit`，但**按目录去重后一个不漏
///   地补齐**：`Batch` 记录本批次每次写入触碰过的目标父目录及其到 `root`
///   的祖先链，`commit` 对这个去重后的集合逐一 fsync。一个目录若被十次
///   写入共享，单次 `write` 会为它重复 fsync 十次；`Batch` 只 fsync 一次——
///   这正是省下来的开销，而不是被省略的保证：fsync 一个目录的效果与
///   fsync 它十次相同（都是把该目录当前的目录项状态刷到介质），语义不因
///   去重而改变。
/// - **收口是显式的**：`commit()` 之前，本批次已经 rename 成功的写入与
///   单次 `write` 提前失败在 `sync_dir_chain_to_root` 那一步时的状态完全
///   同构——都是「内容已落盘、目录项落盘未确认」（[`AtomicError::CommittedUnsynced`]
///   与 [`AtomicError::BatchCommitFailed`] 是同一件事在两种粒度上的表达）。
///   调用方（`sync::sync`）必须在报告成功之前调用 `commit()` 并检查其结果，
///   `commit` 失败则整个操作按失败上报，绝不静默声称成功（I3）。
///   `Batch` 故意不实现「析构时兜底 fsync」——那会把「提交是否发生」这件
///   事从调用方的控制流里偷走，变成一个隐式、不可观察、失败了也无法上报
///   的副作用；显式 `commit()` 才能让「收口失败」如实地成为一个 `Result`。
///
/// 因此：单次写入路径不变（[`write`] 本身未改动一行行为）；批量路径整体
/// 提交完成时，持久性保证与「把这批写入逐个单独调用 `write`」完全等价，
/// 只是目录 fsync 的**次数**变少，**时机**从"每次"改成"批次收尾"。
pub struct Batch<'a> {
    root: &'a StorageRoot,
    touched_dirs: BTreeSet<PathBuf>,
}

impl<'a> Batch<'a> {
    /// 开启一个新批次，绑定到 `root`——批次内所有写入都落在同一个存储根，
    /// 与 [`write`] 的调用约定一致。
    pub fn new(root: &'a StorageRoot) -> Self {
        Self {
            root,
            touched_dirs: BTreeSet::new(),
        }
    }

    /// 批次内的一次写入：语义与 [`write`] 相同，但目录 fsync 记账到本批次，
    /// 不在这里执行——必须调用 [`Batch::commit`] 才能确认落盘（见结构体文档）。
    pub fn write(&mut self, relative_target: &str, bytes: &[u8]) -> Result<(), AtomicError> {
        let (_target, target_parent) = write_and_rename(self.root, relative_target, bytes)?;
        insert_dir_chain(self.root.path(), &target_parent, &mut self.touched_dirs);
        Ok(())
    }

    /// 本批次目前已经触碰、尚待 `commit` 确认落盘的去重目录数——仅供调用方/
    /// 测试观察批次规模，不影响提交逻辑。
    pub fn pending_dirs(&self) -> usize {
        self.touched_dirs.len()
    }

    /// 收口：fsync 本批次触碰过的每个目录恰好一次。**调用方必须显式调用
    /// 这个方法**——不调用就丢弃 `Batch`，批次内已经 rename 成功的写入仍然
    /// 停留在「内容已落盘、目录项落盘未确认」的状态，等价于单次 `write`
    /// 在 `sync_dir_chain_to_root` 之前就返回（结构体文档已论证这不构成
    /// 半截内容，但也绝不能被当作"已确认落盘"上报）。
    ///
    /// 尽力对每个目录都尝试 fsync（不因为第一个失败就放弃其余的——多确认
    /// 一个目录总比少确认好），失败的目录连同原因一并收进
    /// [`AtomicError::BatchCommitFailed`]。
    pub fn commit(self) -> Result<(), AtomicError> {
        let mut failed = Vec::new();
        for dir in &self.touched_dirs {
            if let Err(e) = sync_dir(dir) {
                failed.push((dir.display().to_string(), e.to_string()));
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(AtomicError::BatchCommitFailed { entries: failed })
        }
    }
}

/// 把 `from` 到 `root`（含两端）的目录链逐层插入 `touched`，供 [`Batch`] 记账。
///
/// 与 [`sync_dir_chain_to_root`] 走同一条路径，但插入去重集合而不是立即
/// fsync。命中已经在集合里的目录就提前返回：`touched` 里的每一次插入都是
/// 沿着「从某个目录一路插到 `root`」这条完整链条做的，所以一旦某个目录已在
/// 集合中，可以归纳地确定它自己到 `root` 的祖先链此前必然也已经插入过，
/// 不需要重新走一遍——对共享同一批父目录的写入（批量归档里极常见：同一个
/// 分片目录下有上千个文件）这是一个有意义的常数级剪枝。
fn insert_dir_chain(root: &Path, from: &Path, touched: &mut BTreeSet<PathBuf>) {
    let mut current = from;
    loop {
        if !touched.insert(current.to_path_buf()) {
            return;
        }
        if current == root {
            return;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return,
        }
    }
}

/// 原子写入到**任意本地路径**——不要求调用方持有 [`StorageRoot`]（tmp → fsync
/// 文件 → rename → fsync 目标所在目录及其祖先直到 `boundary`）。
///
/// # 为什么需要这个变体（M2a tombstone 计划，「为什么这是 M2 的第一块」一节）
///
/// [`write`]/[`Batch::write`] 只能写存储根内部（tmp 建在固定的 `.arca/tmp/`
/// 下，目标是 `root.join(relative_target)`）。但 `arca-cli` 的 `file://`
/// 同步（`sync::execute_download`）把 hub 的内容下载到**本地工作区**——那不是
/// 存储根，没有 `.arca/tmp/` 这样的固定暂存目录，此前用的是一个自己手写的
/// tmp → rename、**完全不 fsync** 的迷你实现。M1d 的切片评审留了一条明确的
/// 前置：那个实现把内容写进工作区就返回，随后基线立刻落盘——崩溃窗口里的
/// 状态是「基线持久、下载的内容丢失」，下次调和会把它误读成「本地把这个
/// 文件删了」，M1 里这只导致一条无处执行的 `TombstoneRemote` 报告（无害），
/// 但 M2 一旦接通 tombstone 的真正执行，这个洞就会变成崩溃引发的 hub 权威
/// 副本销毁。修法就是本函数：把 [`write`] 的四步持久化纪律原样搬到「目标不
/// 在存储根内」的场景，tmp 建在目标的同一目录下（保证与目标同一文件系统，
/// `rename` 才谈得上原子——道理与 `write` 把 tmp 建在 `.arca/tmp/` 而不是
/// `std::env::temp_dir()` 完全一致），`boundary` 由调用方传入（通常是数据集
/// 根），限定「向上 fsync 到哪一层为止」，不会一路 fsync到与本次调用无关的
/// 祖先目录。
///
/// 调用方须保证 `target` 位于 `boundary` 之内——本函数不做路径逃逸校验
/// （`target` 是调用方已经用 `dataset_root.join(..)` 拼好的本地路径，不是
/// 直接来自网络/用户输入的不可信相对路径字符串，与 [`write`] 面向的
/// `relative_target: &str` 场景不同，见 `StorageRoot::join` 校验的是后者）。
pub fn write_local(boundary: &Path, target: &Path, bytes: &[u8]) -> Result<(), AtomicError> {
    let target_parent = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| boundary.to_path_buf());
    fs::create_dir_all(&target_parent).map_err(|e| io_error(&target_parent, &e))?;

    let tmp_name = tmp_file_name(&target.display().to_string());
    let tmp_path = target_parent.join(tmp_name);

    write_and_sync(&tmp_path, bytes)?;

    if let Err(e) = fs::rename(&tmp_path, target) {
        let mapped = if e.kind() == io::ErrorKind::CrossesDevices {
            AtomicError::CrossDevice {
                tmp: tmp_path.display().to_string(),
                target: target.display().to_string(),
            }
        } else {
            io_error(&tmp_path, &e)
        };
        cleanup_tmp(&tmp_path);
        return Err(mapped);
    }

    // rename 成功：与 `write` 同一条分界线，从这里往后任何失败都意味着目标
    // 已经是新内容、只是持久性未确认，报 `CommittedUnsynced` 而不是普通
    // `Io`——调用方绝不能因此重试整个写入（见该变体的文档）。
    sync_dir_chain_to_root(boundary, &target_parent).map_err(|e| AtomicError::CommittedUnsynced {
        target: target.display().to_string(),
        reason: e.to_string(),
    })
}

/// fsync `from` 及其在 `root` 内的每一层祖先目录，一路向上到 `root` 本身
/// （含 `root`）。
///
/// 不去精确记录 `create_dir_all` 到底新建了哪几层——那需要在调用前后各扫
/// 一遍目录树才能确定，多出的复杂度不值得：`relative_target` 的路径深度
/// 通常只有几层，逐层向上 fsync 到 `root` 顶多多付出几次系统调用，但绝不
/// 会漏掉真正新建的那一层。`root` 本身早已存在（`StorageRoot::open` 已经
/// 验证过），fsync 它是多余但无害的一次调用。
fn sync_dir_chain_to_root(root: &Path, from: &Path) -> Result<(), AtomicError> {
    let mut current = from;
    loop {
        sync_dir(current)?;
        if current == root {
            return Ok(());
        }
        match current.parent() {
            Some(parent) => current = parent,
            // 理论上不会发生：`from` 由 `root.join(..)` 派生而来，沿着
            // `parent()` 向上走必然先遇到 `root` 才会耗尽路径分量。防御性
            // 兜底而不是 panic（I5）。
            None => return Ok(()),
        }
    }
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
///
/// `tmp_dir` **本身**先经 [`fs::symlink_metadata`] 校验是真实目录才会
/// `read_dir`——`read_dir` 会跟随符号链接，条目级别再严格的类型判断也救
/// 不回一个整体就建在别处的目录：`<root>/.arca/tmp` 若被换成指向别的真实
/// 数据目录的符号链接（管理员用 `ln -s` 把 tmp 挪到别的卷、或 rsync/网盘
/// 同步带进来的符号链接），链接目标目录里的每个普通文件都会被当成孤儿
/// 临时文件删掉——那正是本函数存在的意义要防止的事（I3）。这是「状态超出
/// 预期」，按 I5 直接停下报告 [`AtomicError::UnexpectedTmpState`]，不做任何
/// 删除，也不静默降级成「本次没什么可清理的」。
pub fn sweep_tmp(root: &StorageRoot) -> Result<SweepReport, AtomicError> {
    let tmp_dir = root.path().join(layout::TMP_DIR);
    let mut report = SweepReport::default();

    let tmp_meta = match fs::symlink_metadata(&tmp_dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(io_error(&tmp_dir, &e)),
    };
    if tmp_meta.is_symlink() {
        return Err(AtomicError::UnexpectedTmpState {
            path: tmp_dir.display().to_string(),
            reason: "是符号链接而不是真实目录",
        });
    }
    if !tmp_meta.is_dir() {
        return Err(AtomicError::UnexpectedTmpState {
            path: tmp_dir.display().to_string(),
            reason: "不是目录",
        });
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 命中 `create_new` 的 `EEXIST` 时，`write_and_sync` 不清理已存在的
    /// 文件——那不是本次调用创建的，可能是另一次写入正在使用的文件（评审
    /// Important #6：pid 回收后重名，`File::create` 会静默截断它，
    /// `create_new` 必须诚实报错且不动它分毫）。
    #[test]
    fn write_and_sync_遇到已存在文件时报错且既不覆盖也不删除它() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.tmp");
        let 既有内容 = "别的调用正在使用的内容，不能被删或覆盖".as_bytes();
        fs::write(&path, 既有内容).unwrap();

        let err = write_and_sync(&path, b"new content").unwrap_err();
        assert!(
            matches!(err, AtomicError::Io { .. }),
            "应报 Io（EEXIST），实得 {err:?}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            既有内容,
            "既有文件必须原样保留：不覆盖、不删除"
        );
    }

    /// `CommittedUnsynced` 的错误消息必须清楚说明「目标已经是新内容，
    /// 不应重试整个写入」——调用方（M1b 调和状态机、M1d 提交路径）靠这条
    /// 语义决定下一步，不能只靠类型名猜（评审 Important #4）。
    #[test]
    fn committed_unsynced_消息说明目标已提交且不应重试整个写入() {
        let err = AtomicError::CommittedUnsynced {
            target: "files/note.txt".to_string(),
            reason: "模拟的 fsync 失败".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("已经写入并 rename 成功"));
        assert!(msg.contains("不应重试整个写入"));
        // 与 Io 是不同变体，调用方可以按类型区分，不必嗅探字符串。
        assert!(!matches!(err, AtomicError::Io { .. }));
    }

    /// `BatchCommitFailed` 的消息必须点出失败的目录数与「不应重试整个批次」
    /// ——与 `CommittedUnsynced` 同一纪律，只是聚合到批次粒度（M1d 批量提交）。
    #[test]
    fn batch_commit_failed_消息说明失败目录数且不应重试整个批次() {
        let err = AtomicError::BatchCommitFailed {
            entries: vec![
                ("files/shared".to_string(), "模拟的 fsync 失败".to_string()),
                ("files/other".to_string(), "另一处模拟失败".to_string()),
            ],
        };
        let msg = err.to_string();
        assert!(msg.contains("2 个目录"));
        assert!(msg.contains("不应重试整个批次"));
        assert!(msg.contains("files/shared"));
        assert!(msg.contains("files/other"));
    }

    /// `write_local` 的基本往返：写入后能读回，且目标同目录下不留 tmp 残留
    /// （rename 成功后 tmp 名字已经不存在了）。
    #[test]
    fn write_local_往返一致且不留tmp残留() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sub/note.txt");

        write_local(dir.path(), &target, b"hello").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"hello");
        let siblings: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(siblings, vec![target.file_name().unwrap().to_owned()]);
    }

    /// `write_local` 覆盖已存在的目标：新内容替换旧内容，不是追加。
    #[test]
    fn write_local_覆盖已有目标() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.txt");
        write_local(dir.path(), &target, b"v1").unwrap();
        write_local(dir.path(), &target, b"v2-longer").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"v2-longer");
    }

    /// `write_local` 的目录落盘确认失败必须报 `CommittedUnsynced`，而不是
    /// 吞掉——这正是 Task 1 要补的洞：旧的手写 tmp→rename 实现完全没有这一步，
    /// 任何 fsync 失败（包括真实断电导致的持久性丢失）都无法被检测到，
    /// 调用方会带着"已经成功"的错觉继续保存基线。这里用 chmod 模拟目录
    /// fsync 失败（`File::open` 读该目录需要读权限，`rename` 本身只需要
    /// 写+执行权限，二者可以分别控制）：chmod 对 root 用户无效、部分文件系统
    /// 也不支持权限位，所以先自证一次假设是否成立，不成立就跳过而不是假装
    /// 测过了（与 `arca-git`/`arca-store` 既有测试同一条纪律）。
    #[test]
    #[cfg(unix)]
    fn write_local_目录落盘确认失败时报committed_unsynced而不是静默成功() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let boundary = dir.path();
        let target = dir.path().join("note.txt");

        // 目录本身只保留写+执行权限，去掉读权限——rename 仍能成功
        // （改目录项不需要读该目录本身），但 `File::open(boundary)` 用于
        // fsync 会因为没有读权限而失败。
        fs::set_permissions(boundary, fs::Permissions::from_mode(0o300)).unwrap();

        if File::open(boundary).is_ok() {
            fs::set_permissions(boundary, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "跳过：当前用户不受 chmod 限制（root 或文件系统不支持权限位），\
                 无法用权限手段模拟目录 fsync 失败"
            );
            return;
        }

        let result = write_local(boundary, &target, b"hello");

        // 恢复权限，否则 tempdir 在 Drop 时清理不掉这个目录。
        fs::set_permissions(boundary, fs::Permissions::from_mode(0o755)).unwrap();

        match result {
            Err(AtomicError::CommittedUnsynced { target: t, .. }) => {
                assert_eq!(t, target.display().to_string());
            }
            other => panic!("应报 CommittedUnsynced，实得 {other:?}"),
        }
        // 目标已经是新内容（rename 已经成功），只是这一事实的落盘确认失败——
        // 绝不能因为这个错误就以为内容也丢了、更不能去删除或回滚它。
        assert_eq!(
            fs::read(&target).unwrap(),
            b"hello",
            "rename 已完成，内容必须仍在目标路径上"
        );
    }

    fn 造存储根(dir: &Path) {
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/trash")).unwrap();
        let format = arca_format::hub_layout::FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    /// `TmpWriter`：分多次 `write_all` 喂入的内容与一次性 `write()` 效果
    /// 等价——落点、内容都一致（M2b 切片评审 C2：PUT 改流式写入的落地基础）。
    #[test]
    fn tmp_writer_分块写入后finish内容与一次性写入等价() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = StorageRoot::open(dir.path(), None).unwrap();

        let mut writer = TmpWriter::create(&root, "files/streamed.txt").unwrap();
        writer.write_all(b"hello, ").unwrap();
        writer.write_all(b"streamed ").unwrap();
        writer.write_all(b"world").unwrap();
        writer.finish(&root, "files/streamed.txt").unwrap();

        assert_eq!(
            fs::read(dir.path().join("files/streamed.txt")).unwrap(),
            b"hello, streamed world"
        );
        // finish 之后 tmp 目录不应该残留任何文件。
        let leftovers: Vec<_> = fs::read_dir(dir.path().join(".arca/tmp"))
            .unwrap()
            .collect();
        assert!(leftovers.is_empty(), "finish 之后不应有 tmp 残留");
    }

    /// `abandon`：中途放弃时临时文件必须被清理，目标路径不应该出现任何
    /// 内容——调用方（HTTP PUT 处理器）在发现请求体超过体积上限时走这条
    /// 路径，绝不能把半份内容留在磁盘上（约束 4）。
    #[test]
    fn tmp_writer_abandon清理临时文件且不产生目标() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = StorageRoot::open(dir.path(), None).unwrap();

        let mut writer = TmpWriter::create(&root, "files/aborted.txt").unwrap();
        writer.write_all(b"partial").unwrap();
        writer.abandon();

        assert!(!dir.path().join("files/aborted.txt").exists());
        let leftovers: Vec<_> = fs::read_dir(dir.path().join(".arca/tmp"))
            .unwrap()
            .collect();
        assert!(leftovers.is_empty(), "abandon 之后不应有 tmp 残留");
    }

    /// 未显式 `finish`/`abandon`（模拟调用方提前 `return`/`?` 传播）——`Drop`
    /// 必须兜底清理，不留孤儿临时文件。
    #[test]
    fn tmp_writer_未收口时drop兜底清理() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = StorageRoot::open(dir.path(), None).unwrap();

        {
            let mut writer = TmpWriter::create(&root, "files/dropped.txt").unwrap();
            writer.write_all(b"never finished").unwrap();
            // writer 在这里离开作用域，既没有 finish 也没有 abandon。
        }

        let leftovers: Vec<_> = fs::read_dir(dir.path().join(".arca/tmp"))
            .unwrap()
            .collect();
        assert!(leftovers.is_empty(), "Drop 应兜底清理，实得 {leftovers:?}");
    }

    /// `rename` 把内容从一个相对路径移到另一个，源不再存在、目标内容不变。
    #[test]
    fn rename_把内容从源移到目标() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = StorageRoot::open(dir.path(), None).unwrap();
        write(&root, "files/a.txt", b"content").unwrap();

        rename(&root, "files/a.txt", ".arca/trash/abc.data").unwrap();

        assert!(!dir.path().join("files/a.txt").exists(), "源应已不存在");
        assert_eq!(
            fs::read(dir.path().join(".arca/trash/abc.data")).unwrap(),
            b"content"
        );
    }

    /// 源不存在时 `rename` 报普通 `Io`（`NotFound`），不做特殊包装。
    #[test]
    fn rename_源不存在时报io错误() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = StorageRoot::open(dir.path(), None).unwrap();

        let err = rename(&root, "files/不存在.txt", ".arca/trash/x.data").unwrap_err();
        assert!(matches!(err, AtomicError::Io { .. }), "实得 {err:?}");
    }

    /// `insert_dir_chain` 白盒验证去重不变量：同一目录链的第二次插入应该
    /// 提前返回而不重复插入祖先，且集合最终恰好是链条本身的长度。
    #[test]
    fn insert_dir_chain_对同一条链去重() {
        let root = Path::new("/root");
        let mut touched = BTreeSet::new();

        insert_dir_chain(root, Path::new("/root/a/b"), &mut touched);
        assert_eq!(touched.len(), 3, "应插入 /root、/root/a、/root/a/b 三层");

        insert_dir_chain(root, Path::new("/root/a/b"), &mut touched);
        assert_eq!(touched.len(), 3, "重复插入同一条链不应增加集合大小");

        insert_dir_chain(root, Path::new("/root/a/c"), &mut touched);
        assert_eq!(
            touched.len(),
            4,
            "只有 /root/a/c 是新目录，/root 与 /root/a 已经在集合里"
        );
    }
}
