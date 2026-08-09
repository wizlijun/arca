//! 工作区侧的本地回收站：`<dataset>/.arca/client/trash/<trash_id>.data` +
//! `<trash_id>.meta`（M2d Task 2，FORMAT.md §9.5）。
//!
//! `server` 角色执行 `Action::DeleteLocal` 时的落点（见 `crate::sync` 里
//! `execute_delete_local` 的角色分流）：过了删除四道闸门之后**不 `unlink`**，
//! 把本地副本挪到这里——spec §4.7 的承诺是"server 角色本地永远有完整数据，
//! 任何云侧语义都不会缩减它"，`client` 角色才是"过闸门即移除"。物理销毁
//! 只经未来显式的清理命令（M2 尚未实现），本模块**不提供任何销毁路径**
//! （I3：本切片不得新增销毁路径）。
//!
//! # 与 hub 侧 `.arca/trash/`（`crate::trash`）的关系
//!
//! 同一套字段格式与写入顺序纪律（`.data` 先于 `.meta`；`rename` 天然原子，
//! 绝不 copy+unlink），复用 `crate::trash::{TrashId, TrashMeta}`——这两个
//! 类型的标识编码与 JSON 字段是 root 无关的，没有理由在这里另起一套。
//! 落盘原语不同：hub 侧的 `.arca/trash/` 走 `arca_store::atomic`
//! （`StorageRoot` 专属的 fsync 事务链，服务权威真相）；这里是普通工作区
//! 文件系统上的 `fs::rename` + `fs::write`→`fs::rename`，与姊妹模块
//! `baseline.rs` 同一档持久化强度——本地回收站是"过闸门之后的便利副本"，
//! 它挪进来这件事本身发生的前提是闸门第 4 道已经确认 hub 保留期内可以
//! 找回同一份内容，所以丢失这份本地便利副本（例如 `.meta` 写到一半时
//! 崩溃，留下一个孤儿 `.data`——内容仍在，只是暂时找不到它对应哪个路径）
//! 不构成 I3 意义上的数据损失，不值得为它支付 hub 侧那个级别的持久化成本。
//!
//! # 范围边界（M2e Task 1 起）
//!
//! 本模块负责"挪进来"（[`move_to_trash`]）、"看得见"（[`list`]/[`scan_issues`]/
//! [`usage`]）与"取回来"（[`restore`]，即 `arca restore <dataset> <file> --local`）。
//! **仍然不提供任何销毁路径**：物理销毁只经显式的 `arca gc <dataset> --local
//! --yes`（`crate::gc`，I3）——本模块的每个函数都只读或只写回工作区，绝不
//! `unlink` 回收站里的任何东西。
//!
//! 读侧刻意与 hub 侧 [`crate::trash`] 保持同一套形状与同一套纪律，不发明
//! 第二套语义：
//!
//! | 关注点 | hub 侧 `crate::trash` | 工作区侧（本模块） |
//! | --- | --- | --- |
//! | 条目类型 | `TrashEntry { trash_id, meta }` | [`LocalTrashEntry`]（同字段） |
//! | 列表遇损坏记录 | 整体报错（操作路径要么完整可信要么不用） | 同上（[`list`]） |
//! | 诊断巡检 | `scan_issues` 逐条累积、点名文件 | 同上（[`scan_issues`]） |
//! | 保留期判断 | `trash::within_retention` | **同一个函数**，不另写一份 |
//! | 恢复前核验 | 重开 `.data`、现场重算 BLAKE3 与 `.meta.hash` 比对 | 同上（[`restore`]） |
//! | 覆盖当前占用者 | 先把占用者 `move_to_trash` 再写回（评审 C1） | 同上（[`restore`]） |
//!
//! 两侧唯一真正的差别在写回的目标：hub 侧写回 `files/<path>` 并追加版本链/
//! index/journal（那是**权威真相**的一次新提交）；工作区侧只把字节写回
//! `<dataset>/<path>`，不碰任何 hub 状态——本地回收站里的内容按定义是"hub
//! 早已确认过、过了四道闸门才移进来的那一份"，把它放回工作区不构成一次新
//! 提交，下一次 `arca sync` 会按三态调和自然处理它（可能判为 `AdoptBaseline`
//! 零传输认领，也可能判为待上传，取决于 hub 此刻的状态——这正是让调和表
//! 自己决定、而不是在恢复路径上替它猜的地方，I5）。

use crate::trash::{TrashId, TrashIssue, TrashMeta};
use arca_chunk::hash::ContentHash;
use arca_core::state::BaseState;
use arca_format::error::FormatError;
use arca_format::model::ItemId;
use arca_format::path_rules::{self, PathStatus};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const CLIENT_DIR: &str = ".arca/client";
const TRASH_SUBDIR: &str = "trash";

/// 本地回收站的失败——与 [`crate::trash::TrashError`] 同一套区分纪律：
/// IO 故障与格式故障是不同性质的失败（I5）。
#[derive(Debug)]
pub enum LocalTrashError {
    Io { path: String, reason: String },
    Format(FormatError),
}

impl fmt::Display for LocalTrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocalTrashError::Io { path, reason } => write!(f, "本地回收站 {path}：{reason}"),
            LocalTrashError::Format(e) => write!(f, "本地回收站记录：{e}"),
        }
    }
}

impl std::error::Error for LocalTrashError {}

fn io_err(path: &Path, e: io::Error) -> LocalTrashError {
    LocalTrashError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

fn trash_dir(dataset_root: &Path) -> PathBuf {
    dataset_root.join(CLIENT_DIR).join(TRASH_SUBDIR)
}

fn data_path(dir: &Path, id: TrashId) -> PathBuf {
    dir.join(format!("{}.data", id.to_hex()))
}

fn meta_path(dir: &Path, id: TrashId) -> PathBuf {
    dir.join(format!("{}.meta", id.to_hex()))
}

/// 把工作区里 `source`（对应逻辑路径 `path`）的内容移进本地回收站，返回
/// 分配到的 `trash_id`。
///
/// `source` 此刻已经不存在时返回 `Ok(None)`——与
/// `gates::check_delete_transport` 第 3 道闸门"本地文件此刻已经不存在也算
/// 通过"、以及 `sync.rs` 里 `client` 角色分支对 `fs::remove_file` 的
/// `NotFound` 处理同一条幂等纪律：不管是用户手动删了、还是这次调用是重跑，
/// "现在没有可挪的源"这件事本身不是错误。
///
/// `deleted_at` 由调用方注入（不在本函数内读系统时钟）——与
/// `trash::move_to_trash`、`items::Version` 的 `committed_at` 同一条纪律：
/// 确定性测试需要能固定这个值。
pub fn move_to_trash(
    dataset_root: &Path,
    source: &Path,
    path: &str,
    item_id: ItemId,
    deleted_at: &str,
) -> Result<Option<TrashId>, LocalTrashError> {
    let dir = trash_dir(dataset_root);
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;

    let trash_id = TrashId::new_random();
    let data = data_path(&dir, trash_id);

    // `.data` 先于 `.meta`（FORMAT.md §9.5，与 hub 侧 §7.3 同一条纪律）：
    // `rename` 同文件系统内天然原子，绝不 copy+unlink（复制完成与删除源
    // 之间不留窗口）。
    match fs::rename(source, &data) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(source, e)),
    }

    // hash/size 算的是刚移动到位的 `.data`——读的是 rename 已经落地的那份
    // 内容（与 `trash::move_to_trash` 同一条纪律，评审 Critical #2 的镜像）。
    let bytes = fs::read(&data).map_err(|e| io_err(&data, e))?;
    let hash = ContentHash::from_bytes(&bytes);
    let size = bytes.len() as u64;

    let meta = TrashMeta {
        path: path.to_string(),
        item_id,
        deleted_at: deleted_at.to_string(),
        hash,
        size,
    };
    let text = meta.to_json().map_err(LocalTrashError::Format)?;
    let meta_p = meta_path(&dir, trash_id);
    let tmp = dir.join(format!("{}.meta.tmp", trash_id.to_hex()));
    fs::write(&tmp, text.as_bytes()).map_err(|e| io_err(&tmp, e))?;
    fs::rename(&tmp, &meta_p).map_err(|e| io_err(&meta_p, e))?;

    Ok(Some(trash_id))
}

/// 读一条本地回收站记录的 `.data` 内容——[`restore`]、[`content_hash`] 与
/// `crate::gc` 的本地分支共用的读原语。**只读**：不删除、不移动。
pub fn read_content(dataset_root: &Path, id: TrashId) -> Result<Vec<u8>, LocalTrashError> {
    let path = data_path(&trash_dir(dataset_root), id);
    fs::read(&path).map_err(|e| io_err(&path, e))
}

/// 读一条本地回收站记录的 `.meta`。
pub fn read_meta(dataset_root: &Path, id: TrashId) -> Result<TrashMeta, LocalTrashError> {
    let path = meta_path(&trash_dir(dataset_root), id);
    let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
    TrashMeta::parse(&text).map_err(LocalTrashError::Format)
}

/// 重新打开 `.data` 并现场重算 BLAKE3——与 `crate::trash::content_hash`
/// 逐字同一条纪律（FORMAT.md §7.3/§9.5）：`.meta`/`.data` 两个文件都"存在"
/// 不代表 `.data` 里此刻的字节没有被外部工具截断、替换或换成悬空符号链接。
/// [`restore`] 与 `crate::gc` 的本地分支写回/销毁之前都必须调用它。
pub fn content_hash(dataset_root: &Path, id: TrashId) -> Result<ContentHash, LocalTrashError> {
    Ok(ContentHash::from_bytes(&read_content(dataset_root, id)?))
}

// ---------------------------------------------------------------------------
// 读侧：列表、巡检、占用统计（M2e Task 1）
// ---------------------------------------------------------------------------

/// 一条本地回收站条目——字段与 `crate::trash::TrashEntry` 逐一对应（模块
/// 顶部的对照表：不发明第二套语义）。刻意不复用 `TrashEntry` 类型本身：
/// 两侧的 `trash_id` 命名空间彼此独立（一个在存储根、一个在工作区），让
/// 类型系统挡住"把工作区的条目喂给 hub 侧函数"这种串台，比省一个类型定义
/// 更值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTrashEntry {
    pub trash_id: TrashId,
    pub meta: TrashMeta,
}

/// 列出 `<dataset>/.arca/client/trash/` 下全部条目，按 `trash_id` 排序
/// （确定性输出）。
///
/// 目录不存在视为空列表（从未发生过 server 角色删除的正常状态）。目录存在
/// 但其中某条 `.meta` 读不懂则**整体报错**——与 `crate::trash::list` 完全
/// 同一条纪律：列表是"这里到底还能找回什么"的权威依据，读错一条就是对它
/// 撒谎（I5）；要逐条定位是哪一条坏了用 [`scan_issues`]（诊断路径）。
pub fn list(dataset_root: &Path) -> Result<Vec<LocalTrashEntry>, LocalTrashError> {
    let dir = trash_dir(dataset_root);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(&dir, e)),
    };

    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".meta") else {
            continue;
        };
        let trash_id = TrashId::parse(stem).map_err(|e| {
            LocalTrashError::Format(FormatError::Malformed {
                line: 0,
                reason: e.to_string(),
            })
        })?;
        let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
        let meta = TrashMeta::parse(&text).map_err(LocalTrashError::Format)?;
        out.push(LocalTrashEntry { trash_id, meta });
    }
    out.sort_by(|a, b| a.trash_id.cmp(&b.trash_id));
    Ok(out)
}

/// 逐条巡检 `.meta`，**不因为某一条损坏就整体放弃**——只服务诊断
/// （`arca doctor`），与 `crate::trash::scan_issues` 同一分工（操作路径要
/// "要么完整可信、要么不用"，诊断路径要"尽量多点名几条具体问题"）。
/// 复用 `crate::trash::TrashIssue` 这个纯诊断值类型：它只有"哪个文件、
/// 什么原因"两个字符串字段，没有任何 root/dataset 语义，两侧共用不会串台。
pub fn scan_issues(dataset_root: &Path) -> Result<Vec<TrashIssue>, LocalTrashError> {
    let dir = trash_dir(dataset_root);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(&dir, e)),
    };

    let mut issues = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".meta") else {
            continue;
        };
        if let Err(e) = TrashId::parse(stem) {
            issues.push(TrashIssue {
                file_name: name.to_string(),
                reason: e.to_string(),
            });
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(text) => {
                if let Err(e) = TrashMeta::parse(&text) {
                    issues.push(TrashIssue {
                        file_name: name.to_string(),
                        reason: e.to_string(),
                    });
                }
            }
            Err(e) => issues.push(TrashIssue {
                file_name: name.to_string(),
                reason: e.to_string(),
            }),
        }
    }
    issues.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(issues)
}

/// 本地回收站的占用概况——`arca doctor`/`arca bugreport` 用它把这个此前
/// 完全不可见的目录呈现出来（M2e Task 1：「让它可见」；M2d 评审原话：
/// 「server 设备的 trash 会无界增长」）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Usage {
    /// 条目数（有 `.meta` 的记录数，不含孤儿 `.data`）。
    pub entries: usize,
    /// `.data` 在磁盘上此刻实际占用的字节总数——用 `symlink_metadata` 逐个
    /// 量，不是把 `.meta.size` 加起来：后者是"移进来时记录的大小"，被外部
    /// 工具截断/替换过就不再等于真实占用，而这个数字要回答的恰恰是"这个
    /// 目录现在吃了我多少空间"。
    pub bytes: u64,
    /// 最老一条的 `deleted_at`（RFC 3339 原文）——`None` 表示没有任何条目。
    pub oldest_deleted_at: Option<String>,
    /// 其中已经**超过保留期**的条目数（`crate::trash::within_retention` 取反）
    /// ——这些是 `arca gc --local` 未来会列进销毁清单的候选，不代表它们
    /// 此刻不可恢复（没跑过 `arca gc` 就一条都不会消失，I3）。
    pub expired: usize,
}

/// 统计本地回收站占用。`now`/`retention_days` 由调用方注入（不在这里读系统
/// 时钟——与本模块其余函数同一条确定性测试纪律）。
///
/// 用 [`list`]（遇损坏记录整体报错）而不是尽力而为：占用统计出现在
/// `doctor`/`bugreport` 里，一个"少算了几条"的数字比报错更有害——用户会
/// 据它判断"这台设备的回收站还好"。损坏记录本身由 [`scan_issues`] 单独
/// 点名，两条通路各司其职。
pub fn usage(
    dataset_root: &Path,
    now: &str,
    retention_days: i64,
) -> Result<Usage, LocalTrashError> {
    let entries = list(dataset_root)?;
    let dir = trash_dir(dataset_root);
    let mut out = Usage {
        entries: entries.len(),
        ..Usage::default()
    };
    for entry in &entries {
        // `symlink_metadata`：不跟随符号链接——`.data` 被换成悬空链接时
        // 这里如实记 0 字节而不是报错（它是不是还能取回由 `restore` 的
        // 三方核验回答，占用统计不该因为一条坏记录整个失败）。
        if let Ok(meta) = fs::symlink_metadata(data_path(&dir, entry.trash_id)) {
            out.bytes += meta.len();
        }
        if !crate::trash::within_retention(&entry.meta, now, retention_days) {
            out.expired += 1;
        }
    }
    out.oldest_deleted_at = oldest_deleted_at(&entries);
    Ok(out)
}

/// 最老的 `deleted_at`：优先按解析出的 UNIX 秒比较；一条都解析不出来时
/// 退回字典序最小的原文——`deleted_at` 结构上总是出自 `clock::now_rfc3339`
/// 的统一格式，解析失败意味着记录被外部改过，这时给出"某个原文"仍然比
/// 给出 `None`（"回收站是空的"）更诚实。
fn oldest_deleted_at(entries: &[LocalTrashEntry]) -> Option<String> {
    let parsed = entries
        .iter()
        .filter_map(|e| {
            crate::clock::parse_rfc3339(&e.meta.deleted_at).map(|t| (t, &e.meta.deleted_at))
        })
        .min_by_key(|(t, _)| *t)
        .map(|(_, s)| s.clone());
    parsed.or_else(|| entries.iter().map(|e| e.meta.deleted_at.clone()).min())
}

// ---------------------------------------------------------------------------
// `arca restore <dataset> <file> --local`（M2e Task 1）
// ---------------------------------------------------------------------------

/// 从本地回收站找回失败——彼此可区分（I5），形状对齐
/// `crate::trash::RestoreError`。
#[derive(Debug)]
pub enum LocalRestoreError {
    /// 本地回收站里没有这个路径的记录——这台设备从未以 server 角色删除过
    /// 它（也可能它只在 hub 侧回收站里：那要用不带 `--local` 的
    /// `arca restore`），或者记录已被显式的 `arca gc --local` 销毁。
    NotFound {
        path: String,
    },
    /// `.data` 的现场哈希与 `.meta.hash` 不一致——内容已被截断/替换/篡改，
    /// 拒绝把可能损坏的字节当作"找回成功"写回工作区（FORMAT.md §9.5）。
    ContentMismatch {
        path: String,
        trash_id: TrashId,
    },
    PathInvalid(PathStatus),
    Trash(LocalTrashError),
    Baseline(crate::baseline::BaselineError),
    Io {
        path: String,
        reason: String,
    },
}

impl fmt::Display for LocalRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocalRestoreError::NotFound { path } => write!(
                f,
                "{path}：本地回收站（.arca/client/trash/）里没有这个路径的记录\
                 ——若要从 hub 侧回收站找回，去掉 --local 重跑"
            ),
            LocalRestoreError::ContentMismatch { path, trash_id } => write!(
                f,
                "{path}：本地回收站记录 {trash_id} 的内容与其 .meta 记录的哈希不一致\
                 （可能已损坏或被篡改），拒绝写回"
            ),
            LocalRestoreError::PathInvalid(s) => write!(f, "路径不合规：{}", s.as_str()),
            LocalRestoreError::Trash(e) => write!(f, "{e}"),
            LocalRestoreError::Baseline(e) => write!(f, "{e}"),
            LocalRestoreError::Io { path, reason } => write!(f, "{path}：{reason}"),
        }
    }
}

impl std::error::Error for LocalRestoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LocalRestoreError::Trash(e) => Some(e),
            LocalRestoreError::Baseline(e) => Some(e),
            _ => None,
        }
    }
}

/// 一次本地找回的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRestored {
    /// 找回所依据的回收站记录（**没有被删除**，本模块无销毁路径）。
    pub trash_id: TrashId,
    /// 写回的逻辑路径（归一化后）。
    pub path: String,
    pub size: u64,
    /// 写回前把工作区里那份"另一份鲜活内容"保护性地移进本地回收站时分配到
    /// 的 `trash_id`——`None` 表示写回前该路径没有内容、或内容与要写回的
    /// 完全一致（无需保护）。见下方「恢复不该比删除拥有更大的销毁权」。
    pub protected: Option<TrashId>,
}

/// 把本地回收站里的内容写回 `<dataset_root>/<path>`。
///
/// # 三方核验（FORMAT.md §9.5，hub 侧评审 C2 的镜像）
///
/// 写回前重新打开 `.data`、现场重算 BLAKE3，与 `.meta.hash` 比对——不一致
/// 即 [`LocalRestoreError::ContentMismatch`]，绝不把可能损坏的字节当作
/// "找回成功"。
///
/// # 恢复不该比删除拥有更大的销毁权（hub 侧评审 C1 的镜像）
///
/// 工作区里 `<path>` 此刻完全可能已经有了一份**别的**内容（用户手工放了
/// 新文件、或另一台设备的改动被 `arca sync` 下载了下来）。若直接覆盖，
/// 这份内容会被静默销毁——不进任何回收站、无提示、exit 0，比这个项目里
/// 任何一条删除路径都危险（删除要过四道闸门）。因此：占用者的内容哈希与
/// 即将写回的不同时，先把占用者自己 [`move_to_trash`] 一遍再覆盖，
/// `protected` 字段带回它的 `trash_id`，命令壳负责告诉用户。
///
/// 占用者的 `item_id` 取自基线（`<dataset>/.arca/client/baseline.jsonl`）里
/// 这个路径的记录；基线没有（从未同步过的本地新文件）则铸一个全新身份，
/// **绝不借用即将写回的那个 `item_id`**——那会把一份不相关的内容错误地并进
/// 另一个 item 的历史（与 `crate::trash::restore` 同一处理）。
///
/// # 只读回收站
///
/// 找回**不删除**回收站记录：同一条记录可以被找回多次，物理销毁只经显式
/// `arca gc --local`（I3）。
///
/// `now` 由调用方注入（保护性移入需要一个 `deleted_at`），与本模块其余
/// 函数同一条确定性纪律。
pub fn restore(
    dataset_root: &Path,
    path: &str,
    now: &str,
) -> Result<LocalRestored, LocalRestoreError> {
    let normalized = path_rules::check(path).map_err(LocalRestoreError::PathInvalid)?;

    let entries = list(dataset_root).map_err(LocalRestoreError::Trash)?;
    // 同一路径可能有多条历史记录（删除→重建→再删除）：取 `deleted_at`
    // 最晚的一条——与 `crate::trash::restore` 同一条选择规则（用户敲下
    // 恢复命令时想找回的通常是"最近一次删除"）。
    let chosen = entries
        .iter()
        .filter(|e| e.meta.path == normalized)
        .max_by(|a, b| a.meta.deleted_at.cmp(&b.meta.deleted_at))
        .ok_or_else(|| LocalRestoreError::NotFound {
            path: normalized.clone(),
        })?;

    let bytes = read_content(dataset_root, chosen.trash_id).map_err(LocalRestoreError::Trash)?;
    let hash = ContentHash::from_bytes(&bytes);
    if hash != chosen.meta.hash {
        return Err(LocalRestoreError::ContentMismatch {
            path: normalized,
            trash_id: chosen.trash_id,
        });
    }

    let target = dataset_root.join(&normalized);

    // 保护当前占用者（见函数文档）。
    let mut protected = None;
    match fs::read(&target) {
        Ok(current) => {
            if ContentHash::from_bytes(&current) != hash {
                let item_id = baseline_item_id(dataset_root, &normalized)?
                    .unwrap_or_else(crate::ids::new_item_id);
                protected = move_to_trash(dataset_root, &target, &normalized, item_id, now)
                    .map_err(LocalRestoreError::Trash)?;
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(LocalRestoreError::Io {
                path: target.display().to_string(),
                reason: e.to_string(),
            })
        }
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| LocalRestoreError::Io {
            path: parent.display().to_string(),
            reason: e.to_string(),
        })?;
    }
    // tmp → rename（同目录内原子）——与 `baseline::save`/`role::write` 同一
    // 档持久化强度：写到一半崩溃不会在工作区留下半个文件，而 `arca sync`
    // 的下一轮扫描看到的要么是旧内容要么是完整的新内容。
    let tmp = target.with_extension(format!(
        "{}arca-restore-tmp",
        target
            .extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    fs::write(&tmp, &bytes).map_err(|e| LocalRestoreError::Io {
        path: tmp.display().to_string(),
        reason: e.to_string(),
    })?;
    fs::rename(&tmp, &target).map_err(|e| LocalRestoreError::Io {
        path: target.display().to_string(),
        reason: e.to_string(),
    })?;

    Ok(LocalRestored {
        trash_id: chosen.trash_id,
        path: normalized,
        size: bytes.len() as u64,
        protected,
    })
}

/// 基线里这个路径的 `item_id`（若有）——见 [`restore`]「恢复不该比删除拥有
/// 更大的销毁权」。基线损坏/读不出来时**报错而不是当作"没有"**：那会让
/// 保护性移入用一个全新铸的身份，把一份本有归属的内容写进错误的历史里，
/// 而基线本身读不出来这件事用户理应知道（I5）。
fn baseline_item_id(dataset_root: &Path, path: &str) -> Result<Option<ItemId>, LocalRestoreError> {
    let baseline = crate::baseline::load(dataset_root).map_err(LocalRestoreError::Baseline)?;
    Ok(match baseline.get(path) {
        BaseState::Present { item_id, .. } => Some(item_id),
        BaseState::Absent => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::model::ItemId;

    fn item_id() -> ItemId {
        ItemId::from_bytes([0x3f; 16])
    }

    #[test]
    fn 移入回收站后原路径不存在_内容可从回收站读回() {
        let dir = tempfile::tempdir().unwrap();
        let dataset_root = dir.path();
        let source = dataset_root.join("note.txt");
        fs::write(&source, b"hello arca").unwrap();

        let trash_id = move_to_trash(
            dataset_root,
            &source,
            "note.txt",
            item_id(),
            "2026-08-09T00:00:00Z",
        )
        .unwrap()
        .expect("源文件存在时必须分配 trash_id");

        assert!(!source.exists(), "内容应已被移走，原路径不应再存在");
        assert_eq!(read_content(dataset_root, trash_id).unwrap(), b"hello arca");

        let meta = read_meta(dataset_root, trash_id).unwrap();
        assert_eq!(meta.path, "note.txt");
        assert_eq!(meta.item_id, item_id());
        assert_eq!(meta.size, 10);
    }

    #[test]
    fn 源已不存在时返回none而不是报错() {
        let dir = tempfile::tempdir().unwrap();
        let dataset_root = dir.path();
        let source = dataset_root.join("从未创建过.txt");

        let outcome = move_to_trash(
            dataset_root,
            &source,
            "从未创建过.txt",
            item_id(),
            "2026-08-09T00:00:00Z",
        )
        .unwrap();
        assert_eq!(outcome, None);
    }

    #[test]
    fn 两次移入分配不同的trash_id() {
        let dir = tempfile::tempdir().unwrap();
        let dataset_root = dir.path();
        fs::write(dataset_root.join("a.txt"), b"a").unwrap();
        fs::write(dataset_root.join("b.txt"), b"b").unwrap();

        let id_a = move_to_trash(
            dataset_root,
            &dataset_root.join("a.txt"),
            "a.txt",
            item_id(),
            "2026-08-09T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        let id_b = move_to_trash(
            dataset_root,
            &dataset_root.join("b.txt"),
            "b.txt",
            item_id(),
            "2026-08-09T00:00:00Z",
        )
        .unwrap()
        .unwrap();

        assert_ne!(id_a, id_b);
    }

    // -----------------------------------------------------------------
    // 读侧：list / scan_issues / usage（M2e Task 1）
    // -----------------------------------------------------------------

    const NOW: &str = "2026-08-09T00:00:00Z";

    fn 移入(dataset_root: &Path, rel: &str, content: &[u8], deleted_at: &str) -> TrashId {
        let source = dataset_root.join(rel);
        if let Some(parent) = source.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&source, content).unwrap();
        move_to_trash(dataset_root, &source, rel, item_id(), deleted_at)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn list对不存在的回收站目录返回空列表而不是报错() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn list列出全部条目且按trash_id排序() {
        let dir = tempfile::tempdir().unwrap();
        let a = 移入(dir.path(), "a.txt", b"aaa", NOW);
        let b = 移入(dir.path(), "b.txt", b"bbbb", NOW);

        let entries = list(dir.path()).unwrap();
        assert_eq!(entries.len(), 2);
        let ids: Vec<TrashId> = entries.iter().map(|e| e.trash_id).collect();
        assert!(ids.contains(&a) && ids.contains(&b));
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "输出必须确定性排序");
    }

    #[test]
    fn list遇到损坏的meta整体报错而不是跳过() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "a.txt", b"aaa", NOW);
        let phantom = TrashId::new_random();
        fs::write(
            dir.path()
                .join(".arca/client/trash")
                .join(format!("{phantom}.meta")),
            "不是合法json",
        )
        .unwrap();

        let err = list(dir.path()).unwrap_err();
        assert!(matches!(err, LocalTrashError::Format(_)), "实得 {err:?}");
    }

    /// 与 hub 侧同名场景一致：`list()` 整体报错时，`scan_issues()` 必须还能
    /// **点名**具体是哪个文件坏了，且不把健康记录一并当成有问题。
    #[test]
    fn scan_issues点名损坏的meta且不影响健康记录() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "a.txt", b"aaa", NOW);
        let phantom = TrashId::new_random();
        fs::write(
            dir.path()
                .join(".arca/client/trash")
                .join(format!("{phantom}.meta")),
            "不是合法json",
        )
        .unwrap();

        assert!(list(dir.path()).is_err());
        let issues = scan_issues(dir.path()).unwrap();
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].file_name, format!("{phantom}.meta"));
    }

    #[test]
    fn scan_issues对健康的回收站返回空列表() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "a.txt", b"aaa", NOW);
        assert!(scan_issues(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn usage报告条目数_实际占用字节_最老条目() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "new.txt", b"12345", "2026-08-08T00:00:00Z");
        移入(dir.path(), "old.txt", b"123", "2026-07-01T00:00:00Z");

        let u = usage(dir.path(), NOW, crate::trash::DEFAULT_RETENTION_DAYS).unwrap();
        assert_eq!(u.entries, 2);
        assert_eq!(u.bytes, 8, "应是 .data 在磁盘上的实际字节数之和");
        assert_eq!(u.oldest_deleted_at.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(u.expired, 0, "180 天保留期内两条都没过期");
    }

    #[test]
    fn usage统计超过保留期的条目数() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "old.txt", b"123", "2020-01-01T00:00:00Z");
        移入(dir.path(), "new.txt", b"456", NOW);

        let u = usage(dir.path(), NOW, crate::trash::DEFAULT_RETENTION_DAYS).unwrap();
        assert_eq!(u.expired, 1, "只有 2020 年那条超出了 180 天保留期");
    }

    #[test]
    fn usage对空回收站是全零() {
        let dir = tempfile::tempdir().unwrap();
        let u = usage(dir.path(), NOW, crate::trash::DEFAULT_RETENTION_DAYS).unwrap();
        assert_eq!(u, Usage::default());
    }

    // -----------------------------------------------------------------
    // restore（`arca restore <dataset> <file> --local`）
    // -----------------------------------------------------------------

    #[test]
    fn restore把内容逐字节写回工作区且不删除回收站记录() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"\x00\x01\xff\xfe photo bytes";
        let id = 移入(dir.path(), "photo.png", content, NOW);
        assert!(!dir.path().join("photo.png").exists());

        let restored = restore(dir.path(), "photo.png", NOW).unwrap();

        assert_eq!(restored.trash_id, id);
        assert_eq!(restored.path, "photo.png");
        assert_eq!(restored.size, content.len() as u64);
        assert_eq!(restored.protected, None);
        assert_eq!(fs::read(dir.path().join("photo.png")).unwrap(), content);
        // I3：本模块没有销毁路径——记录必须还在，可以再恢复一次。
        assert_eq!(list(dir.path()).unwrap().len(), 1);
        assert!(restore(dir.path(), "photo.png", NOW).is_ok());
    }

    #[test]
    fn restore能写回嵌套目录里的路径() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "京都/鸭川.png", b"kamo", NOW);
        // 移走之后目录本身可能还在，这里连目录一起删掉，验证 restore 会重建。
        fs::remove_dir_all(dir.path().join("京都")).unwrap();

        restore(dir.path(), "京都/鸭川.png", NOW).unwrap();
        assert_eq!(fs::read(dir.path().join("京都/鸭川.png")).unwrap(), b"kamo");
    }

    #[test]
    fn restore找不到记录时报not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = restore(dir.path(), "从未删除过.png", NOW).unwrap_err();
        assert!(
            matches!(err, LocalRestoreError::NotFound { .. }),
            "实得 {err:?}"
        );
    }

    /// FORMAT.md §9.5 的三方核验：`.data` 被外部工具截断/替换之后，`restore`
    /// 必须拒绝写回，绝不能把损坏的字节当作"找回成功"。
    #[test]
    fn restore在data被篡改时拒绝写回() {
        let dir = tempfile::tempdir().unwrap();
        let id = 移入(dir.path(), "a.txt", b"original", NOW);
        fs::write(
            dir.path()
                .join(".arca/client/trash")
                .join(format!("{id}.data")),
            b"TAMPERED",
        )
        .unwrap();

        let err = restore(dir.path(), "a.txt", NOW).unwrap_err();
        assert!(
            matches!(err, LocalRestoreError::ContentMismatch { .. }),
            "实得 {err:?}"
        );
        assert!(
            !dir.path().join("a.txt").exists(),
            "拒绝写回时绝不能在工作区留下损坏的内容"
        );
    }

    /// hub 侧评审 C1 的工作区镜像：恢复不该比删除拥有更大的销毁权。
    /// 路径上此刻有一份**别的**内容时，先把它保护性地移进本地回收站，
    /// 再写回——被顶替下去的那份必须仍能找回来。
    #[test]
    fn restore覆盖当前占用者前先把占用者移进回收站() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "photo.png", b"OLD bytes", NOW);
        // 同名重建为完全不相关的新内容（spec §4.1 明文预期的场景）。
        fs::write(dir.path().join("photo.png"), b"NEW bytes").unwrap();

        let restored = restore(dir.path(), "photo.png", "2026-08-09T01:00:00Z").unwrap();

        assert_eq!(
            fs::read(dir.path().join("photo.png")).unwrap(),
            b"OLD bytes",
            "用户显式要求的恢复必须照常生效"
        );
        let protected = restored.protected.expect("占用者应被保护性移入回收站");
        assert_eq!(
            read_content(dir.path(), protected).unwrap(),
            b"NEW bytes",
            "被顶替下去的内容必须仍能找回，不能被静默销毁"
        );
    }

    #[test]
    fn restore对内容完全相同的占用者不做多余的保护性移入() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "a.txt", b"same", NOW);
        fs::write(dir.path().join("a.txt"), b"same").unwrap();

        let restored = restore(dir.path(), "a.txt", NOW).unwrap();
        assert_eq!(restored.protected, None);
        assert_eq!(list(dir.path()).unwrap().len(), 1, "不该多出一条记录");
    }

    #[test]
    fn restore命中同路径多条历史记录时取最晚删除的一条() {
        let dir = tempfile::tempdir().unwrap();
        移入(dir.path(), "a.txt", b"older", "2026-08-01T00:00:00Z");
        移入(dir.path(), "a.txt", b"newer", "2026-08-08T00:00:00Z");

        restore(dir.path(), "a.txt", NOW).unwrap();
        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"newer");
    }

    #[test]
    fn restore拒绝不合规的路径() {
        let dir = tempfile::tempdir().unwrap();
        let err = restore(dir.path(), "../逃出去.txt", NOW).unwrap_err();
        assert!(
            matches!(err, LocalRestoreError::PathInvalid(_)),
            "实得 {err:?}"
        );
    }
}
