//! `arca gc`——**本项目第一个被授权物理销毁数据的命令**（spec §7、I3、
//! FORMAT.md §7.3「销毁顺序」/§9.5）。
//!
//! README 第一屏那句承诺是「arca 里没有任何一条代码路径能在你不知情时销毁
//! 数据」。这个模块就是那句话的边界——它是唯一一处 `fs::remove_file` 会作用
//! 在**用户内容字节**上的地方。因此它的默认行为不是"少销毁一点"，而是
//! **什么都不销毁**。
//!
//! # 五条不可谈判的纪律
//!
//! 1. **默认不销毁**。没有 [`GcOptions::confirmed`]（命令行 `--yes`）就是
//!    一次 dry-run：出清单、算字节数、退出，**一次 `unlink` 都不发生**。
//!    `--dry-run` 是一个显式承认默认行为的开关，不是打开它的开关。
//! 2. **保留期内一律不动，即使加了 `--yes`**。要越过保留期需要第二道更显式
//!    的开关（[`GcOptions::include_unexpired`]，命令行
//!    `--include-unexpired`），它的帮助文本必须把后果写明。两道开关是"与"
//!    关系，不是"或"。
//! 3. **销毁前必列清单**。dry-run 与真的销毁产出的是**同一份**
//!    [`GcReport`]：真跑一遍看到的清单，与 dry-run 那次看到的形状一致，
//!    用户不需要在"预览"和"真跑"之间重新建立信任。
//! 4. **发现悬空/多余引用就停下，什么都不销毁**（I5）。判据见
//!    [`Blocker`]：任何一条 blocker 存在，`destroyed` 就恒为空——不是"跳过
//!    有问题的那条、继续处理其余"，是整体停手。gc 只销毁它能**完整解释**
//!    的东西。
//! 5. **绝不自动触发**。本模块没有任何定时器、没有任何"顺手清理"，
//!    `crate::sync`/`crate::adopt`/`arcad` 一行都不调用它（有一条测试专门
//!    钉死这一点，见本文件底部 `没有任何自动触发路径`）。cron 里写
//!    `arca gc` 是用户的主动决策。
//!
//! # 销毁范围：只有内容字节，不含历史
//!
//! 销毁的是**已过保留期的回收站条目的内容字节**（`.data`）与它的记录
//! （`.meta`）。`items/<item_id>.jsonl` 的版本链、`journal/` 的事件一律不动
//! ——理由见 FORMAT.md §7.3「`arca gc` 只销毁内容字节，不销毁历史」：它们
//! 是审计闭环（I8）与 `arca history` 的依据，每条一行 JSON，销毁它们既回收
//! 不了空间又会制造无法解释的空洞。
//!
//! # 块（`.arca/chunks/`）：本版本一个都不动，这是刻意的
//!
//! spec §7 说 gc 回收"失引用块"。**本版本不实现块回收**，而且这不是"没来得及
//! 做"，是"做了会错"：CDC 块的哈希是**切块之后每一段**的 BLAKE3，而
//! `items/` 版本链里记的是**整份文件**的 BLAKE3——两个命名空间完全不重叠。
//! 用"版本链里出现过的哈希"当作引用集合去判断块是否失引用，会把**每一个块
//! 都判成失引用**，一次 `--yes` 就能清空整个历史版本库。真正的引用模型
//! （每个版本 → 它的块清单，FORMAT.md §8 为此预留了 `chunks/refs/`）在写入
//! 侧还不存在（本仓库目前没有任何代码往 `chunks/` 写东西），所以这里的正确
//! 做法是**不猜**（I5）：一个块都不动，并在报告里如实说明还有多少块没被
//! 回收过。见 [`GcReport::chunks_untouched`]。

use crate::local_trash;
use crate::trash::{self, TrashId, TrashMeta};
use arca_chunk::hash::ContentHash;
use arca_format::hub_layout::layout;
use arca_format::model::ItemId;
use arca_store::fsck;
use arca_store::root::StorageRoot;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 一次 gc 的输入参数。`now` 由调用方注入（不在本模块读系统时钟）——一个
/// 被授权销毁数据的命令，它对"什么算过期"的判断必须能在测试里被完全固定。
#[derive(Debug, Clone)]
pub struct GcOptions {
    pub now: String,
    /// 保留期天数，默认见 [`trash::DEFAULT_RETENTION_DAYS`]（spec §7：180 天）。
    pub retention_days: i64,
    /// 第一道开关（`--yes`）：没有它就是 dry-run，**一次 `unlink` 都不发生**。
    pub confirmed: bool,
    /// 第二道开关（`--include-unexpired`）：连**保留期内**的条目也一起销毁。
    /// 与 `confirmed` 是"与"关系——单独给这个开关不会销毁任何东西。
    pub include_unexpired: bool,
}

impl GcOptions {
    /// 默认形态：**dry-run、默认保留期、绝不碰保留期内的条目**。构造一个
    /// gc 参数的起点必须是"什么都不销毁"，要销毁得逐个字段显式打开。
    pub fn dry_run(now: impl Into<String>) -> Self {
        GcOptions {
            now: now.into(),
            retention_days: trash::DEFAULT_RETENTION_DAYS,
            confirmed: false,
            include_unexpired: false,
        }
    }
}

/// 一条候选（或已销毁）的回收站条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub trash_id: TrashId,
    /// 这条记录对应的原逻辑路径（`.meta.path`）。
    pub path: String,
    pub item_id: ItemId,
    pub deleted_at: String,
    /// `.data` 在磁盘上此刻实际占用的字节数——销毁它能回收的就是这些。
    /// `.data` 已经不在（gc 自己上次崩溃留下的合法中间态，FORMAT.md §7.3）
    /// 时为 0。
    pub bytes: u64,
    /// `.data` 此刻是否还在——`false` 表示这条候选只需要补删 `.meta`
    /// （gc 崩溃残留的自愈路径）。
    pub data_present: bool,
}

/// 让 gc **整体停手**的发现（I5：状态模糊就停下，不猜测该怎么继续）。
///
/// 任何一条 blocker 存在，这次 gc 就一个字节都不销毁——包括那些本身完全
/// 健康、已经过期的条目。这是刻意的：一个被授权销毁数据的命令，在对存储根
/// 的理解出现任何裂缝时，正确的行为是把手拿开，而不是"处理我看得懂的部分"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// `arca fsck` 报出的存储根问题（gc 与 fsck 共享同一份巡检，spec §7）。
    ///
    /// **一个例外**：`Problem::OrphanIndex` 在"该 item 确实有 tombstone 事件"
    /// 时是 tombstone 执行留下的**预期形态**（执行时主动删掉了 index 记录，
    /// 见 `sync.rs::remove_index_record` 与 FORMAT.md §7.3），不是损坏——
    /// 否则任何发生过一次删除的存储根都会永久无法 gc，而那恰恰是唯一需要
    /// gc 的存储根。判据是 `hub::item_is_tombstoned`，不是"看起来像"。
    Fsck(fsck::Problem),
    /// 一个 `.data` 没有任何 `.meta` 认领它——写入侧崩溃的残留
    /// （FORMAT.md §7.3 明文允许的中间态）。gc 对它**无法判断保留期**
    /// （`deleted_at` 随 `.meta` 一起不存在）也**无法验证完整性**
    /// （没有 `hash` 可比），所以既不能销毁也不能假装没看见：停下报告，
    /// 由人确认这份字节是什么之后手工处理。
    OrphanData { file_name: String },
    /// 一条候选的 `.data` 现场哈希与它 `.meta.hash` 记录的不一致——这份
    /// 字节**不是**当初移进来的那份（被截断、被替换、或被换成了指向别处
    /// 的符号链接）。gc 不知道它现在是什么，因此不销毁它：一个"我不知道
    /// 这是什么，所以我删掉它"的清理程序，正是 I3 要挡住的东西。
    ContentMismatch {
        trash_id: TrashId,
        path: String,
        expected: String,
        actual: String,
    },
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::Fsck(p) => write!(f, "存储根巡检发现问题：{p:?}"),
            Blocker::OrphanData { file_name } => write!(
                f,
                "{file_name}：回收站里有一份没有 .meta 认领的内容——无法判断它的保留期\
                 与完整性，gc 拒绝对它做任何判断。请先确认这份字节是什么（它可能是一次\
                 写入中途崩溃的残留，也可能是别的东西），处理掉之后再重跑"
            ),
            Blocker::ContentMismatch {
                trash_id,
                path,
                expected,
                actual,
            } => write!(
                f,
                "{trash_id}（原路径 {path}）：.data 的现场哈希 {actual} 与 .meta 记录的 \
                 {expected} 不一致——这份字节已经不是当初移进来的那份，gc 不知道它现在\
                 是什么，因此拒绝销毁它"
            ),
        }
    }
}

/// 一次 gc 的完整报告——**dry-run 与真跑产出同一个形状**，唯一的差别是
/// `destroyed` 是否为空（见模块顶部纪律 3）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// 本次的销毁候选：已过保留期（或 `include_unexpired` 下的全部）。
    /// dry-run 时这就是"如果你加上 `--yes` 会被销毁的东西"。
    pub candidates: Vec<Candidate>,
    /// 因**仍在保留期内**而被保护、本次一律不动的条目（纪律 2）。
    pub retained: Vec<Candidate>,
    /// 让 gc 整体停手的发现（纪律 4）。非空时 `destroyed` 恒为空。
    pub blockers: Vec<Blocker>,
    /// 实际被销毁的条目——**dry-run 恒为空**，有 blocker 时也恒为空。
    pub destroyed: Vec<Candidate>,
    /// `.arca/chunks/` 下的文件数——本版本一个都不会动，见模块顶部
    /// 「块：本版本一个都不动」。工作区侧的 gc 恒为 0（工作区没有块存储）。
    pub chunks_untouched: usize,
    /// 本次是否真的执行了销毁（`confirmed` 且无 blocker）。
    pub executed: bool,
}

impl GcReport {
    /// 实际回收的字节数（dry-run 恒为 0）。
    pub fn freed_bytes(&self) -> u64 {
        self.destroyed.iter().map(|c| c.bytes).sum()
    }

    /// dry-run 下"如果确认会回收多少字节"。
    pub fn reclaimable_bytes(&self) -> u64 {
        self.candidates.iter().map(|c| c.bytes).sum()
    }
}

/// gc 本身跑不下去的失败（与"发现了 blocker"是两回事：后者是正常产出的
/// 报告，见 [`Blocker`]）。
#[derive(Debug)]
pub enum GcError {
    Trash(trash::TrashError),
    LocalTrash(local_trash::LocalTrashError),
    Hub(crate::hub::HubError),
    Lock(arca_store::lock::LockError),
    Io { path: String, reason: String },
}

impl fmt::Display for GcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GcError::Trash(e) => write!(f, "{e}"),
            GcError::LocalTrash(e) => write!(f, "{e}"),
            GcError::Hub(e) => write!(f, "{e}"),
            GcError::Lock(e) => write!(f, "{e}"),
            GcError::Io { path, reason } => write!(f, "{path}：{reason}"),
        }
    }
}

impl std::error::Error for GcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GcError::Trash(e) => Some(e),
            GcError::LocalTrash(e) => Some(e),
            GcError::Hub(e) => Some(e),
            GcError::Lock(e) => Some(e),
            GcError::Io { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// hub 侧：`<存储根>/.arca/trash/`
// ---------------------------------------------------------------------------

/// 对一个已打开、身份已确认（I11）的存储根跑一次 gc。
///
/// 全程持有 `arca_store::lock` 的跨进程排他锁——一个正在销毁字节的命令绝不
/// 能与 `arcad` 的写入（或另一个 `arca gc`）交错：`arcad` 的 `DELETE` 会往
/// 回收站里放新东西，而 gc 正在遍历同一个目录。这是本模块唯一一处比
/// `LocalTransport` 更强的并发要求，理由也更硬：读写交错最坏是一次 CAS 冲突，
/// 销毁与写入交错最坏是删掉一份刚移进来的内容。
pub fn hub(root: &StorageRoot, opts: &GcOptions) -> Result<GcReport, GcError> {
    let _lock = arca_store::lock::acquire(root).map_err(GcError::Lock)?;

    let dir = root.path().join(layout::TRASH_DIR);
    let entries = trash::list(root).map_err(GcError::Trash)?;
    let plan_entries: Vec<(TrashId, TrashMeta)> =
        entries.into_iter().map(|e| (e.trash_id, e.meta)).collect();

    let mut report = plan(&dir, &plan_entries, opts)?;
    report.chunks_untouched = count_chunks(&root.path().join(layout::CHUNKS_DIR));

    // gc 与 fsck 共享引用计数校验（spec §7）——放在最后追加 blocker，
    // 这样即便存储根有问题，用户也仍能在报告里看到"本来会销毁什么"，
    // 知道修好之后能回收多少（清单本身不销毁任何东西）。
    for problem in fsck::check_root(root).problems {
        if is_expected_after_tombstone(root, &problem)? {
            continue;
        }
        report.blockers.push(Blocker::Fsck(problem));
    }

    execute_if_authorized(&dir, &mut report, opts)?;
    Ok(report)
}

/// `Problem::OrphanIndex` 是不是 tombstone 执行留下的预期形态——见
/// [`Blocker::Fsck`] 的文档。只有这一个变体有例外，其余问题一律是 blocker。
fn is_expected_after_tombstone(
    root: &StorageRoot,
    problem: &fsck::Problem,
) -> Result<bool, GcError> {
    let fsck::Problem::OrphanIndex { key } = problem else {
        return Ok(false);
    };
    // `key` 是 item_id 的十六进制（见 `fsck::check_root`）。解析不出来就
    // 不是我们认识的形态，按 blocker 处理（不猜）。
    let Ok(item_id) = ItemId::parse(key) else {
        return Ok(false);
    };
    crate::hub::item_is_tombstoned(root, item_id).map_err(GcError::Hub)
}

fn count_chunks(chunks_dir: &Path) -> usize {
    let Ok(shards) = fs::read_dir(chunks_dir) else {
        return 0;
    };
    shards
        .filter_map(|s| s.ok())
        .filter_map(|s| fs::read_dir(s.path()).ok())
        .map(|files| files.filter_map(|f| f.ok()).count())
        .sum()
}

// ---------------------------------------------------------------------------
// 工作区侧：`<dataset>/.arca/client/trash/`
// ---------------------------------------------------------------------------

/// 对一个数据集的**工作区侧本地回收站**跑一次 gc（`arca gc <ds> --local`）。
///
/// 与 [`hub`] 完全同一套纪律与同一份 [`GcReport`]，两处差别：
///
/// - **不跑 fsck、不加锁**：工作区侧没有存储根、没有 `index/`/`items/`
///   引用模型可校验，也没有常驻进程会与之并发（`arca-cli` 是一次性进程，
///   spec §3.1）。引用校验退化成"每条 `.meta` 都能读懂、每份 `.data` 的
///   现场哈希都与记录一致、没有无人认领的 `.data`"——即 [`plan`] 本身。
/// - **不碰 hub**：这条命令不需要存储根在线，hub 拔掉了照样能清理本机
///   回收站。
///
/// 这台设备是 `server` 角色时，本地回收站是它承诺"永不主动释放空间"的
/// 落点——**这条命令是用户主动放弃那份承诺的唯一出口**，所以它同样需要
/// 两道显式开关，一道都不能省。
pub fn local(dataset_root: &Path, opts: &GcOptions) -> Result<GcReport, GcError> {
    let dir = dataset_root.join(".arca/client/trash");
    let entries = local_trash::list(dataset_root).map_err(GcError::LocalTrash)?;
    let plan_entries: Vec<(TrashId, TrashMeta)> =
        entries.into_iter().map(|e| (e.trash_id, e.meta)).collect();

    let mut report = plan(&dir, &plan_entries, opts)?;
    execute_if_authorized(&dir, &mut report, opts)?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// 两侧共用的计划与执行
// ---------------------------------------------------------------------------

fn data_file(dir: &Path, id: TrashId) -> PathBuf {
    dir.join(format!("{}.data", id.to_hex()))
}

fn meta_file(dir: &Path, id: TrashId) -> PathBuf {
    dir.join(format!("{}.meta", id.to_hex()))
}

/// 只做判断，**不碰任何文件**：把回收站条目分成"候选"与"受保留期保护"，
/// 并收集 blocker。两侧（hub / 工作区）共用同一段逻辑，因为"什么算过期、
/// 什么算不可解释"这两个判断绝不该有两个答案。
fn plan(
    dir: &Path,
    entries: &[(TrashId, TrashMeta)],
    opts: &GcOptions,
) -> Result<GcReport, GcError> {
    let mut report = GcReport::default();

    for (trash_id, meta) in entries {
        let data = data_file(dir, *trash_id);
        // `symlink_metadata`：不跟随符号链接——一条被换成指向别处的符号
        // 链接的 `.data`，绝不能让 gc 顺着它去 `unlink` 链接目标。
        let (data_present, bytes) = match fs::symlink_metadata(&data) {
            Ok(m) if m.file_type().is_file() => (true, m.len()),
            // 存在但不是普通文件（符号链接、目录、设备节点……）：这不是
            // 我们放进去的东西，按"内容对不上"处理，停下（I5）。
            Ok(_) => {
                report.blockers.push(Blocker::ContentMismatch {
                    trash_id: *trash_id,
                    path: meta.path.clone(),
                    expected: meta.hash.to_text(),
                    actual: "(不是普通文件)".to_string(),
                });
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (false, 0),
            Err(e) => {
                return Err(GcError::Io {
                    path: data.display().to_string(),
                    reason: e.to_string(),
                })
            }
        };

        let candidate = Candidate {
            trash_id: *trash_id,
            path: meta.path.clone(),
            item_id: meta.item_id,
            deleted_at: meta.deleted_at.clone(),
            bytes,
            data_present,
        };

        // **纪律 2**：保留期内一律不动，`include_unexpired` 是唯一的例外，
        // 而它自己还需要 `confirmed` 才有效（见 `execute_if_authorized`）。
        let within = trash::within_retention(meta, &opts.now, opts.retention_days);
        if within && !opts.include_unexpired {
            report.retained.push(candidate);
            continue;
        }

        // 只对**真要销毁的候选**做现场哈希核验（FORMAT.md §7.3）：受保留期
        // 保护的条目本就不会被动，不该因为它内容坏了就阻止清理别的东西
        // ——它的损坏由 `arca doctor` 报告，不是 gc 的职责。
        if data_present {
            let actual = read_hash(&data)?;
            if actual != meta.hash {
                report.blockers.push(Blocker::ContentMismatch {
                    trash_id: *trash_id,
                    path: meta.path.clone(),
                    expected: meta.hash.to_text(),
                    actual: actual.to_text(),
                });
                continue;
            }
        }
        report.candidates.push(candidate);
    }

    // 无人认领的 `.data`（写入侧崩溃残留）——见 `Blocker::OrphanData`。
    for name in orphan_data_files(dir, entries)? {
        report
            .blockers
            .push(Blocker::OrphanData { file_name: name });
    }

    report
        .candidates
        .sort_by(|a, b| a.trash_id.cmp(&b.trash_id));
    report.retained.sort_by(|a, b| a.trash_id.cmp(&b.trash_id));
    Ok(report)
}

fn read_hash(path: &Path) -> Result<ContentHash, GcError> {
    let bytes = fs::read(path).map_err(|e| GcError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    Ok(ContentHash::from_bytes(&bytes))
}

/// 目录里有 `.data`、但 `entries` 里没有对应 `.meta` 的那些文件名。
fn orphan_data_files(dir: &Path, entries: &[(TrashId, TrashMeta)]) -> Result<Vec<String>, GcError> {
    let known: std::collections::BTreeSet<String> =
        entries.iter().map(|(id, _)| id.to_hex()).collect();
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(GcError::Io {
                path: dir.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| GcError::Io {
            path: dir.display().to_string(),
            reason: e.to_string(),
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".data") else {
            continue;
        };
        if !known.contains(stem) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// **唯一一处真的会 `unlink` 用户内容的代码。** 三道闸门全过才动手。
fn execute_if_authorized(
    dir: &Path,
    report: &mut GcReport,
    opts: &GcOptions,
) -> Result<(), GcError> {
    // 闸门 1（纪律 4）：有任何无法解释的东西 → 整体停手。
    if !report.blockers.is_empty() {
        return Ok(());
    }
    // 闸门 2（纪律 1）：没有显式确认 → dry-run，一次 unlink 都不发生。
    if !opts.confirmed {
        return Ok(());
    }
    // 闸门 3：真的没有候选就什么都不做（也不把 `executed` 置真——没有发生
    // 任何销毁行为，报告不该暗示发生过）。
    if report.candidates.is_empty() {
        return Ok(());
    }

    for candidate in &report.candidates {
        // FORMAT.md §7.3「销毁顺序」：**先 `.data` 后 `.meta`**（与写入顺序
        // 刚好相反）。中途崩溃留下的是一条 `.data` 已不在的 `.meta`——合法
        // 中间态，下一次 gc 会把它当作同一条已过期候选补删掉（自愈）；反过来
        // 先删 `.meta` 会留下永远无法自动处理的无主字节。
        let data = data_file(dir, candidate.trash_id);
        match fs::remove_file(&data) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(GcError::Io {
                    path: data.display().to_string(),
                    reason: e.to_string(),
                })
            }
        }
        let meta = meta_file(dir, candidate.trash_id);
        match fs::remove_file(&meta) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(GcError::Io {
                    path: meta.display().to_string(),
                    reason: e.to_string(),
                })
            }
        }
        report.destroyed.push(candidate.clone());
    }
    report.executed = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::hub_layout::FormatJson;
    use std::collections::BTreeMap;

    const NOW: &str = "2026-08-09T00:00:00Z";
    /// 早于 `NOW` 180 天以上——移进回收站时写这个 `deleted_at` 就是"已过期"。
    const LONG_AGO: &str = "2020-01-01T00:00:00Z";
    /// 距 `NOW` 很近——保留期内。
    const RECENTLY: &str = "2026-08-08T00:00:00Z";

    fn item(n: u8) -> ItemId {
        ItemId::from_bytes([n; 16])
    }

    // ---- 文件系统指纹：dry-run 前后必须逐字节一致 --------------------

    /// 递归拍一张目录树的快照：相对路径 → 内容哈希。`arca gc --dry-run`
    /// 前后这张快照必须**完全相等**——这是本模块最重要的一条断言，比
    /// "报告里写着 destroyed 是空的"强得多（报告是我们自己写的，快照不是）。
    ///
    /// **唯一的排除项是 `.arca/locks/`**：`gc::hub` 全程持有
    /// `arca_store::lock` 的跨进程排他锁，获取锁会在存储根从未被加锁过时
    /// 创建一个**零字节**的 `arca.lock`。这是一次纯粹的协调副作用——它不
    /// 在 `files/`、不在 `trash/`、不含任何用户字节、也不会改变任何已有
    /// 文件；把它算进"dry-run 改变了文件系统"会让这条断言失焦。排除的是
    /// 这一个精确路径前缀，不是"`.arca/` 下随便什么"。
    fn 目录指纹(dir: &Path) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    walk(base, &path, out);
                } else {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    if rel.starts_with(".arca/locks/") {
                        continue;
                    }
                    let hash = fs::read(&path)
                        .map(|b| ContentHash::from_bytes(&b).to_text())
                        .unwrap_or_else(|e| format!("<读取失败：{e}>"));
                    out.insert(rel, hash);
                }
            }
        }
        walk(dir, dir, &mut out);
        out
    }

    // ---- hub 侧脚手架 ------------------------------------------------

    fn 造存储根(dir: &Path) -> StorageRoot {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        for sub in [".arca/tmp", ".arca/trash", ".arca/journal", ".arca/locks"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let format = FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-09T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
        StorageRoot::open(dir, None).unwrap()
    }

    /// 往 hub 回收站里放一条记录（走真实的 `trash::move_to_trash`，
    /// 不手工拼字节）。
    fn hub移入(root: &StorageRoot, path: &str, content: &[u8], at: &str, n: u8) -> TrashId {
        let full = root.path().join(format!("files/{path}"));
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, content).unwrap();
        trash::move_to_trash(root, path, item(n), at).unwrap()
    }

    fn 工作区移入(dataset: &Path, path: &str, content: &[u8], at: &str, n: u8) -> TrashId {
        let full = dataset.join(path);
        fs::create_dir_all(full.parent().unwrap_or(dataset)).unwrap();
        fs::write(&full, content).unwrap();
        local_trash::move_to_trash(dataset, &full, path, item(n), at)
            .unwrap()
            .unwrap()
    }

    fn expired() -> GcOptions {
        GcOptions::dry_run(NOW)
    }

    fn confirmed() -> GcOptions {
        GcOptions {
            confirmed: true,
            ..GcOptions::dry_run(NOW)
        }
    }

    // =================================================================
    // 纪律 1：默认 dry-run 不销毁任何东西
    // =================================================================

    #[test]
    fn dry_run前后文件系统逐字节一致_hub侧() {
        let store = tempfile::tempdir().unwrap();
        let root = 造存储根(store.path());
        hub移入(&root, "expired.png", b"expired bytes", LONG_AGO, 1);
        hub移入(&root, "fresh.png", b"fresh bytes", RECENTLY, 2);

        let before = 目录指纹(store.path());
        let report = hub(&root, &expired()).unwrap();
        let after = 目录指纹(store.path());

        assert_eq!(before, after, "--dry-run 绝不能改变文件系统的任何一个字节");
        assert!(!report.executed);
        assert!(report.destroyed.is_empty());
        assert_eq!(report.freed_bytes(), 0);
        // 但清单必须出来——用户要能看到"确认之后会销毁什么"。
        assert_eq!(report.candidates.len(), 1, "{report:?}");
        assert_eq!(report.candidates[0].path, "expired.png");
        assert_eq!(report.reclaimable_bytes(), "expired bytes".len() as u64);
        assert_eq!(report.retained.len(), 1);
        assert_eq!(report.retained[0].path, "fresh.png");
    }

    #[test]
    fn dry_run前后文件系统逐字节一致_工作区侧() {
        let dataset = tempfile::tempdir().unwrap();
        工作区移入(dataset.path(), "expired.png", b"expired bytes", LONG_AGO, 1);
        工作区移入(dataset.path(), "fresh.png", b"fresh bytes", RECENTLY, 2);

        let before = 目录指纹(dataset.path());
        let report = local(dataset.path(), &expired()).unwrap();
        let after = 目录指纹(dataset.path());

        assert_eq!(before, after);
        assert!(!report.executed);
        assert!(report.destroyed.is_empty());
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.retained.len(), 1);
    }

    /// `include_unexpired` **单独**给不销毁任何东西——两道开关是"与"关系。
    #[test]
    fn 只给include_unexpired而不确认时仍然是dry_run() {
        let dataset = tempfile::tempdir().unwrap();
        工作区移入(dataset.path(), "fresh.png", b"fresh", RECENTLY, 1);

        let before = 目录指纹(dataset.path());
        let opts = GcOptions {
            include_unexpired: true,
            ..GcOptions::dry_run(NOW)
        };
        let report = local(dataset.path(), &opts).unwrap();
        assert_eq!(before, 目录指纹(dataset.path()));
        assert!(!report.executed);
        assert_eq!(report.candidates.len(), 1, "清单里应该列出它会被销毁");
    }

    // =================================================================
    // 纪律 2：保留期内一律不动，即使加了 --yes
    // =================================================================

    #[test]
    fn 未过保留期的条目在yes下仍然存活_hub侧() {
        let store = tempfile::tempdir().unwrap();
        let root = 造存储根(store.path());
        let fresh = hub移入(&root, "fresh.png", b"precious", RECENTLY, 1);
        let old = hub移入(&root, "expired.png", b"junk", LONG_AGO, 2);

        let report = hub(&root, &confirmed()).unwrap();

        assert!(report.executed);
        assert_eq!(report.destroyed.len(), 1);
        assert_eq!(report.destroyed[0].trash_id, old);
        // 保留期内那条必须**逐字节**还在。
        assert_eq!(
            trash::read_content(&root, fresh).unwrap(),
            b"precious",
            "保留期内的条目在 --yes 下必须原封不动"
        );
        assert!(trash::data_exists(&root, fresh));
        // 过期那条真的没了。
        assert!(!trash::data_exists(&root, old));
        assert!(!store
            .path()
            .join(format!(".arca/trash/{old}.meta"))
            .exists());
    }

    #[test]
    fn 未过保留期的条目在yes下仍然存活_工作区侧() {
        let dataset = tempfile::tempdir().unwrap();
        let fresh = 工作区移入(dataset.path(), "fresh.png", b"precious", RECENTLY, 1);
        let old = 工作区移入(dataset.path(), "expired.png", b"junk", LONG_AGO, 2);

        let report = local(dataset.path(), &confirmed()).unwrap();

        assert!(report.executed);
        assert_eq!(report.destroyed.len(), 1);
        assert_eq!(report.destroyed[0].trash_id, old);
        assert_eq!(
            local_trash::read_content(dataset.path(), fresh).unwrap(),
            b"precious"
        );
        assert!(local_trash::read_content(dataset.path(), old).is_err());
    }

    /// 第二道显式开关确实能越过保留期——但它必须与 `--yes` 同时出现。
    #[test]
    fn include_unexpired加yes才会销毁保留期内的条目() {
        let dataset = tempfile::tempdir().unwrap();
        工作区移入(dataset.path(), "fresh.png", b"precious", RECENTLY, 1);

        let opts = GcOptions {
            confirmed: true,
            include_unexpired: true,
            ..GcOptions::dry_run(NOW)
        };
        let report = local(dataset.path(), &opts).unwrap();
        assert!(report.executed);
        assert_eq!(report.destroyed.len(), 1);
        assert!(report.retained.is_empty());
        assert!(local_trash::list(dataset.path()).unwrap().is_empty());
    }

    // =================================================================
    // 纪律 3：销毁前列清单——dry-run 与真跑是同一份清单
    // =================================================================

    #[test]
    fn dry_run的清单与真跑销毁的清单逐条一致() {
        let dataset = tempfile::tempdir().unwrap();
        工作区移入(dataset.path(), "a.png", b"aaa", LONG_AGO, 1);
        工作区移入(dataset.path(), "b.png", b"bbbb", LONG_AGO, 2);
        工作区移入(dataset.path(), "c.png", b"ccccc", RECENTLY, 3);

        let preview = local(dataset.path(), &expired()).unwrap();
        let real = local(dataset.path(), &confirmed()).unwrap();

        assert_eq!(
            preview.candidates, real.destroyed,
            "预览看到的清单必须与真跑销毁的逐条一致——否则 --dry-run 建立的信任是假的"
        );
        assert_eq!(preview.reclaimable_bytes(), real.freed_bytes());
        assert_eq!(preview.retained, real.retained);
    }

    // =================================================================
    // 纪律 4：悬空/多余引用 → 停下报告，什么都不销毁
    // =================================================================

    /// 无人认领的 `.data`（写入侧崩溃残留）——gc 必须停手，**连那些完全
    /// 健康、已经过期的条目也不动**。
    #[test]
    fn 无人认领的data让gc整体停手而不是跳过它继续() {
        let dataset = tempfile::tempdir().unwrap();
        let old = 工作区移入(dataset.path(), "expired.png", b"junk", LONG_AGO, 1);
        // 手工放一份没有 `.meta` 的 `.data`——等价于 `move_to_trash` 在
        // rename 之后、写 `.meta` 之前崩溃。
        let orphan = TrashId::new_random();
        fs::write(
            dataset
                .path()
                .join(".arca/client/trash")
                .join(format!("{orphan}.data")),
            b"unaccounted bytes",
        )
        .unwrap();

        let before = 目录指纹(dataset.path());
        let report = local(dataset.path(), &confirmed()).unwrap();

        assert_eq!(
            before,
            目录指纹(dataset.path()),
            "有 blocker 时即便加了 --yes 也不能动任何一个字节"
        );
        assert!(!report.executed);
        assert!(report.destroyed.is_empty());
        assert_eq!(report.blockers.len(), 1, "{report:?}");
        assert!(matches!(report.blockers[0], Blocker::OrphanData { .. }));
        // 那条本来会被销毁的候选仍在清单里（用户看得到"修好之后会清什么"），
        // 但确实没被销毁。
        assert_eq!(report.candidates.len(), 1);
        assert!(local_trash::read_content(dataset.path(), old).is_ok());
    }

    /// `.data` 被外部工具替换过（现场哈希 ≠ `.meta.hash`）——gc 不知道这份
    /// 字节现在是什么，因此拒绝销毁它，并整体停手。
    #[test]
    fn data被篡改时gc停手不销毁() {
        let dataset = tempfile::tempdir().unwrap();
        let id = 工作区移入(dataset.path(), "expired.png", b"original", LONG_AGO, 1);
        fs::write(
            dataset
                .path()
                .join(".arca/client/trash")
                .join(format!("{id}.data")),
            b"SOMETHING ELSE ENTIRELY",
        )
        .unwrap();

        let before = 目录指纹(dataset.path());
        let report = local(dataset.path(), &confirmed()).unwrap();

        assert_eq!(before, 目录指纹(dataset.path()));
        assert!(!report.executed);
        assert_eq!(report.blockers.len(), 1, "{report:?}");
        assert!(matches!(
            report.blockers[0],
            Blocker::ContentMismatch { .. }
        ));
    }

    /// `.data` 被换成符号链接——gc 绝不能顺着它去 `unlink` 链接目标。
    #[cfg(unix)]
    #[test]
    fn data是符号链接时gc停手且不碰链接目标() {
        let dataset = tempfile::tempdir().unwrap();
        let victim_dir = tempfile::tempdir().unwrap();
        let victim = victim_dir.path().join("重要文件.txt");
        fs::write(&victim, "绝不能被 gc 删掉".as_bytes()).unwrap();

        let id = 工作区移入(dataset.path(), "expired.png", b"original", LONG_AGO, 1);
        let data = dataset
            .path()
            .join(".arca/client/trash")
            .join(format!("{id}.data"));
        fs::remove_file(&data).unwrap();
        std::os::unix::fs::symlink(&victim, &data).unwrap();

        let report = local(dataset.path(), &confirmed()).unwrap();

        assert!(!report.executed);
        assert!(matches!(
            report.blockers[0],
            Blocker::ContentMismatch { .. }
        ));
        assert!(victim.exists(), "链接目标必须完好无损");
        assert_eq!(fs::read(&victim).unwrap(), "绝不能被 gc 删掉".as_bytes());
    }

    /// 存储根本身有 fsck 问题（这里造一个 `files/` 下无人认领的孤儿文件）
    /// → gc 停手。
    #[test]
    fn 存储根有fsck问题时gc停手() {
        let store = tempfile::tempdir().unwrap();
        let root = 造存储根(store.path());
        hub移入(&root, "expired.png", b"junk", LONG_AGO, 1);
        // `files/` 下放一个没有任何 index 记录认领的物理文件——
        // `fsck::Problem::OrphanFile`。
        fs::write(store.path().join("files/orphan.bin"), b"who put this here").unwrap();

        let before = 目录指纹(store.path());
        let report = hub(&root, &confirmed()).unwrap();

        assert_eq!(before, 目录指纹(store.path()));
        assert!(!report.executed);
        assert!(
            report
                .blockers
                .iter()
                .any(|b| matches!(b, Blocker::Fsck(fsck::Problem::OrphanFile { .. }))),
            "{report:?}"
        );
    }

    /// **回归防线**：tombstone 执行会主动删掉 index 记录，于是 fsck 必然报
    /// 一条 `OrphanIndex`——如果把它当成 blocker，任何发生过一次删除的存储
    /// 根都会**永久无法 gc**，而那恰恰是唯一需要 gc 的存储根。这条测试走
    /// 真实的 `sync()` 删除传播，钉死这个例外。
    #[test]
    fn tombstone留下的orphan_index不算blocker否则gc永远跑不起来() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let root = StorageRoot::create(
            store.path(),
            "9c41000000000000000000000000abcd",
            "2026-08-09T09:00:00Z",
        )
        .unwrap();
        let actor = arca_format::model::Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        };
        let mut sink = arca_format::trace::NullSink;

        fs::write(dataset.path().join("a.txt"), b"hello").unwrap();
        crate::sync::sync(dataset.path(), &root, &actor, &mut sink).unwrap();
        fs::remove_file(dataset.path().join("a.txt")).unwrap();
        let r = crate::sync::sync(dataset.path(), &root, &actor, &mut sink).unwrap();
        assert_eq!(r.tombstone_submitted, vec!["a.txt".to_string()]);

        // 前置条件：fsck 确实报了 OrphanIndex（如果哪天 fsck 不再这么报，
        // 这条测试的前提就变了，应该重新审视 `is_expected_after_tombstone`）。
        assert!(
            fsck::check_root(&root)
                .problems
                .iter()
                .any(|p| matches!(p, fsck::Problem::OrphanIndex { .. })),
            "前置条件：tombstone 之后 fsck 应报 OrphanIndex"
        );

        let report = hub(&root, &GcOptions::dry_run(NOW)).unwrap();
        assert!(
            report.blockers.is_empty(),
            "tombstone 留下的 OrphanIndex 是预期形态，不该阻止 gc：{report:?}"
        );
    }

    // =================================================================
    // 崩溃残留的自愈：`.data` 已不在的 `.meta`
    // =================================================================

    /// FORMAT.md §7.3「销毁顺序」：gc 先删 `.data` 后删 `.meta`，中途崩溃
    /// 留下的是一条 `.data` 已不在的 `.meta`。下一次 gc 必须把它当作同一条
    /// 已过期候选补删掉（自愈），而不是停手。
    #[test]
    fn 上次gc崩溃残留的孤儿meta会被下一次gc补删掉() {
        let dataset = tempfile::tempdir().unwrap();
        let id = 工作区移入(dataset.path(), "expired.png", b"junk", LONG_AGO, 1);
        // 手工制造"data 已删、meta 还在"的中间态。
        fs::remove_file(
            dataset
                .path()
                .join(".arca/client/trash")
                .join(format!("{id}.data")),
        )
        .unwrap();

        let report = local(dataset.path(), &confirmed()).unwrap();
        assert!(report.executed, "{report:?}");
        assert_eq!(report.destroyed.len(), 1);
        assert!(!report.destroyed[0].data_present);
        assert_eq!(report.destroyed[0].bytes, 0, "内容早就没了，回收 0 字节");
        assert!(local_trash::list(dataset.path()).unwrap().is_empty());
    }

    /// 同一个中间态但**仍在保留期内**：不动它（纪律 2 优先），由
    /// `arca doctor`/`arca restore` 去报告它读不到内容。
    #[test]
    fn 保留期内的孤儿meta不被gc补删() {
        let dataset = tempfile::tempdir().unwrap();
        let id = 工作区移入(dataset.path(), "fresh.png", b"junk", RECENTLY, 1);
        fs::remove_file(
            dataset
                .path()
                .join(".arca/client/trash")
                .join(format!("{id}.data")),
        )
        .unwrap();

        let report = local(dataset.path(), &confirmed()).unwrap();
        assert!(report.destroyed.is_empty());
        assert_eq!(report.retained.len(), 1);
        assert_eq!(local_trash::list(dataset.path()).unwrap().len(), 1);
    }

    // =================================================================
    // 块：一个都不动
    // =================================================================

    #[test]
    fn chunks下的文件一个都不会被动且报告如实计数() {
        let store = tempfile::tempdir().unwrap();
        let root = 造存储根(store.path());
        hub移入(&root, "expired.png", b"junk", LONG_AGO, 1);
        let shard = store.path().join(".arca/chunks/ab");
        fs::create_dir_all(&shard).unwrap();
        fs::write(shard.join(format!("{}.zst", "ab".repeat(32))), b"chunk").unwrap();

        let report = hub(&root, &confirmed()).unwrap();
        assert_eq!(report.chunks_untouched, 1);
        assert!(
            shard.join(format!("{}.zst", "ab".repeat(32))).exists(),
            "本版本没有块级引用模型，一个块都不该被动（见模块文档）"
        );
    }

    // =================================================================
    // 纪律 5：绝不自动触发
    // =================================================================

    /// 全仓库只有命令壳（`commands/porcelain.rs`）可以调用本模块——
    /// `sync`/`adopt`/`arcad` 里出现任何一处调用，就意味着存在一条用户没有
    /// 显式要求的销毁路径（I3）。这条测试直接读源码文本来断言，因为这是
    /// "不存在某种调用"这类命题唯一能自动化验证的方式。
    #[test]
    fn 没有任何自动触发路径() {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let arcad_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("arcad/src");

        let mut offenders = Vec::new();
        let mut check = |dir: &Path, allow: &[&str]| {
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                let Ok(entries) = fs::read_dir(&d) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let p = entry.path();
                    if p.is_dir() {
                        stack.push(p);
                        continue;
                    }
                    if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                        continue;
                    }
                    let name = p.file_name().unwrap().to_string_lossy().to_string();
                    if allow.contains(&name.as_str()) {
                        continue;
                    }
                    let Ok(text) = fs::read_to_string(&p) else {
                        continue;
                    };
                    // 只找真正的调用（`gc::hub(` / `gc::local(`），不找
                    // doc comment 里对 `arca gc` 的文字提及。
                    for needle in ["gc::hub(", "gc::local(", "crate::gc::", "arca_cli::gc::"] {
                        if text.contains(needle) {
                            offenders.push(format!("{}：{needle}", p.display()));
                        }
                    }
                }
            }
        };
        // `gc.rs` 自己（内部调用）与命令壳是仅有的合法调用点。
        check(&src_dir, &["gc.rs", "porcelain.rs"]);
        check(&arcad_dir, &[]);

        assert!(
            offenders.is_empty(),
            "gc 只能由用户显式运行 `arca gc` 触发，绝不能有任何自动路径（I3）；\
             发现这些调用点：{offenders:?}"
        );
    }

    // =================================================================
    // 空回收站 / 幂等
    // =================================================================

    #[test]
    fn 空回收站下gc什么都不做且不报错() {
        let dataset = tempfile::tempdir().unwrap();
        let report = local(dataset.path(), &confirmed()).unwrap();
        assert!(!report.executed, "没有发生任何销毁行为，不该声称执行过");
        assert!(report.candidates.is_empty());
        assert!(report.blockers.is_empty());
    }

    #[test]
    fn 连跑两次gc第二次是空操作() {
        let dataset = tempfile::tempdir().unwrap();
        工作区移入(dataset.path(), "expired.png", b"junk", LONG_AGO, 1);

        let first = local(dataset.path(), &confirmed()).unwrap();
        assert_eq!(first.destroyed.len(), 1);

        let before = 目录指纹(dataset.path());
        let second = local(dataset.path(), &confirmed()).unwrap();
        assert_eq!(before, 目录指纹(dataset.path()));
        assert!(second.destroyed.is_empty());
    }
}
