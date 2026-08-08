//! 基线（客户端投影）：`<dataset>/.arca/client/baseline.jsonl`（M1d Task 2）。
//!
//! 三态调和（`arca_core::reconcile::decide`）的第二个输入端——记录客户端上一次
//! 对账时双方都曾确认过的状态（[`arca_core::state::BaseState`]）。落在
//! `.arca/client/` 下，这个目录被 `arca-git` 的反选块规则整体排除在 git 追踪
//! 之外（见 `crates/arca-git/src/ignore_block.rs`：`/<dataset>/.arca/client/`），
//! 设备差异不进共享配置。
//!
//! 格式是行式 JSON Lines，风格与 hub 侧 `items`/`index` 一致：首行是版本头
//! （`{"v":1}`），之后每行一条 [`arca_core::state::BaseState::Present`] 记录。
//! `Absent` 不落盘——一个路径不在文件里就是 `Absent`，不需要显式记一条「不存在」。
//!
//! # I9：基线是可抛弃投影，但「基线丢了」必须被告知
//!
//! 基线损坏或缺失不是灾难——[`load`] 在这两种情况下都返回一个空基线，而不是
//! 把调用方晾在一个 `Err` 里逼它自己决定怎么办（那会让每个调用点各写一套
//! 「捕获错误后当空处理」的逻辑，恰恰是本模块要收敛掉的东西）。但"悄悄"当成
//! 空基线是不可接受的：那会让所有本地文件在这一轮调和里看起来都是新增
//! （`LocalClass::Added`），而用户不知道为什么——所以 [`Baseline`] 随身带着
//! [`Baseline::was_reset`]（细节見 [`ResetReason`]），供 `arca status`
//! 之类的调用方提示「基线已重建，本轮做全量对账」。
//!
//! 于是 `load` 返回的 `Result<Baseline, BaselineError>` 里，`Err` 只保留给一种
//! 情况：既不是"文件不存在"也不是"内容读不懂"，而是连"到底能不能读"这件事
//! 本身都不确定的真正 IO 故障（权限被拒、路径某一级类型不对等）。把这种情况
//! 也当成"重置"处理是危险的：我们并不知道那份基线内容到底还在不在，静默当
//! 空可能比实际情况更乐观或更悲观，这正是 I5「绝不猜测」要拦住的场景——与
//! `arca_store::root::MountError` 区分 `Absent`/`Io` 是同一条纪律。

use arca_chunk::hash::ContentHash;
use arca_core::state::BaseState;
use arca_format::model::{ItemId, VersionId};
use arca_format::path_rules;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 本实现能写出、也能读懂的最高基线记录格式版本。
const RECORD_VERSION: u32 = 1;

/// 基线文件相对数据集根的路径（分量已知，供 [`baseline_path`] 拼接）。
const CLIENT_DIR: &str = ".arca/client";
const BASELINE_FILE: &str = "baseline.jsonl";

/// 基线损坏/缺失时的具体原因，供调用方给出诊断信息（比单纯的 `bool` 更有用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResetReason {
    /// 文件从未落盘——例如这个数据集第一次跑同步。不是损坏，是正常的初始状态。
    Missing,
    /// 文件存在但读不出可信的基线：结构损坏、字段编码不合法，或版本号高于
    /// 本实现已知的最高版本（I10：不尽力解析未来格式）。
    Corrupt(String),
}

impl fmt::Display for ResetReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResetReason::Missing => write!(f, "基线文件不存在（从未同步过，或已被清空）"),
            ResetReason::Corrupt(reason) => write!(f, "基线文件损坏，已放弃：{reason}"),
        }
    }
}

/// 客户端投影：路径 → 基线状态。可抛弃（I9）。
#[derive(Debug, Clone, Default)]
pub struct Baseline {
    entries: BTreeMap<String, BaseState>,
    reset: Option<ResetReason>,
}

impl Baseline {
    /// 构造一个空基线（未重置——用于全新创建、尚未 `load` 过的场景）。
    pub fn empty() -> Self {
        Baseline {
            entries: BTreeMap::new(),
            reset: None,
        }
    }

    fn with_reset(reason: ResetReason) -> Self {
        Baseline {
            entries: BTreeMap::new(),
            reset: Some(reason),
        }
    }

    /// 本次 `load` 是否因为文件缺失/损坏而重置为空基线。
    pub fn was_reset(&self) -> bool {
        self.reset.is_some()
    }

    /// 重置原因；`was_reset()` 为 `false` 时为 `None`。
    pub fn reset_reason(&self) -> Option<&ResetReason> {
        self.reset.as_ref()
    }

    /// 查询一个路径的基线状态。不在基线里即 `Absent`。
    pub fn get(&self, path: &str) -> BaseState {
        self.entries.get(path).cloned().unwrap_or(BaseState::Absent)
    }

    /// 设置一个路径的基线状态。`BaseState::Absent` 等价于 [`Self::remove`]——
    /// 基线里从不显式记录"不存在"，不在 map 里本身就是 `Absent` 的表达。
    pub fn set(&mut self, path: impl Into<String>, state: BaseState) {
        let path = path.into();
        match state {
            BaseState::Absent => {
                self.entries.remove(&path);
            }
            present => {
                self.entries.insert(path, present);
            }
        }
    }

    pub fn remove(&mut self, path: &str) {
        self.entries.remove(path);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按路径排序的条目迭代——确定性输出，供 `sync`/`status` 遍历。
    pub fn iter(&self) -> impl Iterator<Item = (&String, &BaseState)> {
        self.entries.iter()
    }

    /// 落盘到 `<dataset_root>/.arca/client/baseline.jsonl`。
    ///
    /// 用 tmp → rename（同目录内，rename 原子），但不做 hub 提交那一整套
    /// fsync 事务性保证（`arca_store::atomic`）——基线是可抛弃投影（I9），
    /// 崩溃时最坏情况是下次 `load()` 读到不完整内容、判定为损坏并安全重置，
    /// 触发一次全量对账，这本就是设计允许的正常恢复路径，不需要为它支付
    /// 权威数据那个级别的持久化成本。
    pub fn save(&self, dataset_root: &Path) -> Result<(), BaselineError> {
        let path = baseline_path(dataset_root);
        let dir = path
            .parent()
            .expect("baseline_path 总在 .arca/client 目录下，必有 parent");
        fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;

        let mut content = String::new();
        content.push_str(&to_header_line()?);
        content.push('\n');
        for (path_key, state) in &self.entries {
            content.push_str(&to_entry_line(path_key, state)?);
            content.push('\n');
        }

        let tmp_path = dir.join(format!("{BASELINE_FILE}.tmp"));
        fs::write(&tmp_path, content.as_bytes()).map_err(|e| io_err(&tmp_path, e))?;
        fs::rename(&tmp_path, &path).map_err(|e| io_err(&path, e))?;
        Ok(())
    }
}

/// 基线的低层 IO 故障——与"内容读不懂"是不同性质的失败，见模块顶部 doc comment。
#[derive(Debug)]
pub enum BaselineError {
    Io { path: String, reason: String },
}

impl fmt::Display for BaselineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaselineError::Io { path, reason } => write!(f, "基线 {path} 读写失败：{reason}"),
        }
    }
}

impl std::error::Error for BaselineError {}

fn io_err(path: &Path, e: io::Error) -> BaselineError {
    BaselineError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

fn baseline_path(dataset_root: &Path) -> PathBuf {
    dataset_root.join(CLIENT_DIR).join(BASELINE_FILE)
}

/// 读取基线。**几乎不返回 `Err`**——文件缺失或内容损坏都被吸收成
/// `Ok(Baseline::with_reset(..))`，只有真正的 IO 故障（非 NotFound）才向上
/// 传播，见模块顶部 doc comment「I9」一节。
pub fn load(dataset_root: &Path) -> Result<Baseline, BaselineError> {
    let path = baseline_path(dataset_root);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Baseline::with_reset(ResetReason::Missing));
        }
        Err(e) => return Err(io_err(&path, e)),
    };

    match parse(&text) {
        Ok(entries) => Ok(Baseline {
            entries,
            reset: None,
        }),
        Err(reason) => Ok(Baseline::with_reset(ResetReason::Corrupt(reason))),
    }
}

// ---------------------------------------------------------------------------
// 行式解析/序列化（内部）
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct HeaderWire {
    v: u32,
}

#[derive(Serialize, Deserialize)]
struct EntryWire {
    path: String,
    item_id: String,
    version_id: String,
    hash: String,
    size: u64,
}

/// 序列化版本头。判断点同 `arca_format` 里 `items::to_line` 等一贯的纪律：
/// 返回 `Result` 而非 `unwrap_or_default()`（`Wire` 全是标量字段，`Err` 分支
/// 当前不可达，保留 `Result` 签名为未来加字段留防线）。
fn to_header_line() -> Result<String, BaselineError> {
    serde_json::to_string(&HeaderWire { v: RECORD_VERSION }).map_err(|e| BaselineError::Io {
        path: "<baseline header>".to_string(),
        reason: format!("序列化失败：{e}"),
    })
}

fn to_entry_line(path: &str, state: &BaseState) -> Result<String, BaselineError> {
    let BaseState::Present {
        item_id,
        version_id,
        hash,
        size,
    } = state
    else {
        // set() 已经保证 entries 里不会出现 Absent，这里属于内部不变量，
        // 真出现时按同一种 Result 纪律报告而不是 panic（I5）。
        return Err(BaselineError::Io {
            path: path.to_string(),
            reason: "内部不变量被破坏：基线条目不应是 Absent".to_string(),
        });
    };
    let wire = EntryWire {
        path: path.to_string(),
        item_id: item_id.to_hex(),
        version_id: version_id.as_str().to_string(),
        hash: hash.to_text(),
        size: *size,
    };
    serde_json::to_string(&wire).map_err(|e| BaselineError::Io {
        path: path.to_string(),
        reason: format!("序列化失败：{e}"),
    })
}

/// 解析整份基线文本。任何一行解析失败都让整体解析失败（`Err(原因)`）——
/// 调用方 [`load`] 会把这个 `Err` 转换成"重置为空基线 + 记录原因"，不在这里
/// 做"跳过坏行、保留好行"的宽松处理：基线一旦部分丢失，哪些条目还可信已经
/// 无法判断，宁可整体重置触发全量对账，也不要在一份自己都不完全信任的基线
/// 上继续工作（I5）。
fn parse(text: &str) -> Result<BTreeMap<String, BaseState>, String> {
    let mut lines = text.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| "基线文件为空，缺少版本头".to_string())?;
    let header: HeaderWire =
        serde_json::from_str(header_line).map_err(|e| format!("版本头解析失败：{e}"))?;
    if header.v > RECORD_VERSION {
        return Err(format!(
            "基线格式版本 {} 高于本实现支持的 {RECORD_VERSION}；请升级 arca",
            header.v
        ));
    }

    let mut entries = BTreeMap::new();
    for (offset, raw) in lines.enumerate() {
        let line_no = offset + 2; // 版本头占第 1 行
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let (path, state) = parse_entry(line).map_err(|e| format!("第 {line_no} 行：{e}"))?;
        entries.insert(path, state);
    }
    Ok(entries)
}

fn parse_entry(line: &str) -> Result<(String, BaseState), String> {
    let wire: EntryWire = serde_json::from_str(line).map_err(|e| format!("JSON 解析失败：{e}"))?;
    let path = path_rules::check(&wire.path)
        .map_err(|status| format!("path {:?} 不合规：{}", wire.path, status.as_str()))?;
    let item_id = ItemId::parse(&wire.item_id)
        .map_err(|e| format!("item_id {:?} 不合法：{e}", wire.item_id))?;
    let version_id = parse_version_id(&wire.version_id)
        .map_err(|e| format!("version_id {:?} 不合法：{e}", wire.version_id))?;
    let hash = ContentHash::parse(&wire.hash).map_err(|e| format!("哈希不合规：{e}"))?;
    Ok((
        path,
        BaseState::Present {
            item_id,
            version_id,
            hash,
            size: wire.size,
        },
    ))
}

/// `VersionId` 目前只暴露 `new(timestamp, random)`，没有单独解析
/// `"<timestamp>-<random>"` 整串的入口——与 `arca_format::items` 里私有的
/// 同名辅助函数逻辑一致（该函数未导出，这里按同样的规则本地重写一份，
/// 两处都只有几行，暂不为此在 `arca_format` 开一个新的公开 API）。
fn parse_version_id(text: &str) -> Result<VersionId, String> {
    let (timestamp, random) = text
        .split_once('-')
        .ok_or_else(|| format!("version_id {text:?} 缺少分隔符"))?;
    VersionId::new(timestamp, random).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_id() -> ItemId {
        ItemId::from_bytes([0x3f; 16])
    }

    fn version_id(seed: u8) -> VersionId {
        VersionId::new("20260805T093012Z", &format!("{:032x}", seed as u128)).unwrap()
    }

    fn sample(seed: u8) -> BaseState {
        BaseState::Present {
            item_id: item_id(),
            version_id: version_id(seed),
            hash: ContentHash::from_bytes(&[seed]),
            size: seed as u64,
        }
    }

    #[test]
    fn 往返一致() {
        let dir = tempfile::tempdir().unwrap();
        let mut baseline = Baseline::empty();
        baseline.set("京都/鸭川.png", sample(1));
        baseline.set("notes/a.md", sample(2));
        baseline.save(dir.path()).unwrap();

        let loaded = load(dir.path()).unwrap();
        assert!(!loaded.was_reset());
        assert_eq!(loaded.get("京都/鸭川.png"), sample(1));
        assert_eq!(loaded.get("notes/a.md"), sample(2));
        assert_eq!(loaded.get("从不存在.txt"), BaseState::Absent);
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn 缺失文件返回空基线且标记was_reset() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(dir.path()).unwrap();
        assert!(loaded.is_empty());
        assert!(loaded.was_reset());
        assert_eq!(loaded.reset_reason(), Some(&ResetReason::Missing));
    }

    #[test]
    fn 损坏行导致整体重置而不是静默丢弃部分内容() {
        let dir = tempfile::tempdir().unwrap();
        let client_dir = dir.path().join(".arca/client");
        fs::create_dir_all(&client_dir).unwrap();
        fs::write(
            client_dir.join("baseline.jsonl"),
            "{\"v\":1}\n不是合法json\n",
        )
        .unwrap();

        let loaded = load(dir.path()).unwrap();
        assert!(loaded.is_empty());
        assert!(loaded.was_reset());
        assert!(matches!(
            loaded.reset_reason(),
            Some(ResetReason::Corrupt(_))
        ));
    }

    #[test]
    fn 版本号高于已知时拒绝并重置() {
        let dir = tempfile::tempdir().unwrap();
        let client_dir = dir.path().join(".arca/client");
        fs::create_dir_all(&client_dir).unwrap();
        fs::write(client_dir.join("baseline.jsonl"), "{\"v\":99}\n").unwrap();

        let loaded = load(dir.path()).unwrap();
        assert!(loaded.was_reset());
        match loaded.reset_reason() {
            Some(ResetReason::Corrupt(reason)) => {
                assert!(
                    reason.contains("99"),
                    "原因应提及实际读到的版本号：{reason}"
                )
            }
            other => panic!("应为 Corrupt，实得 {other:?}"),
        }
    }

    #[test]
    fn set_absent等价于remove() {
        let mut baseline = Baseline::empty();
        baseline.set("a.txt", sample(1));
        assert_eq!(baseline.len(), 1);
        baseline.set("a.txt", BaseState::Absent);
        assert!(baseline.is_empty());
        assert_eq!(baseline.get("a.txt"), BaseState::Absent);
    }

    #[test]
    fn 保存后基线文件首行是版本头() {
        let dir = tempfile::tempdir().unwrap();
        let mut baseline = Baseline::empty();
        baseline.set("a.txt", sample(1));
        baseline.save(dir.path()).unwrap();

        let text = fs::read_to_string(dir.path().join(".arca/client/baseline.jsonl")).unwrap();
        assert_eq!(text.lines().next(), Some("{\"v\":1}"));
    }

    #[test]
    fn 空基线保存后重新读取仍是空且未重置() {
        let dir = tempfile::tempdir().unwrap();
        Baseline::empty().save(dir.path()).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert!(loaded.is_empty());
        assert!(!loaded.was_reset(), "文件存在且内容合法，不应判定为重置");
    }
}
