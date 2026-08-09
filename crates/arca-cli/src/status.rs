//! `arca status`（M1d Task 7）：跑扫描与调和但**不执行**——只把
//! `arca_core::decide` 的判决分类计数，不写任何字节、不落盘基线。
//!
//! 与 `sync.rs`（Task 6）共用同一套输入端（`scan`/`baseline`/`hub`）与同一个
//! 决策源（`arca_core::decide`/`decide_traced`）——**架构约束（CLAUDE.md）
//! 禁止 CLI 另写一套判断逻辑**，`status` 与 `sync` 的差别只在于拿到
//! [`Action`] 之后"只分类，不执行"这一步。
//!
//! Rule of Silence（spec §3.2）：全同步（没有待办、没有冲突）时
//! [`StatusReport::is_silent`] 为真，命令壳据此完全不打印任何东西、退出码 0；
//! 否则把分类结果打到 stderr（诊断，不是数据）。

use crate::transport::{Transport, TransportError};
use crate::{baseline, hub, scan};
use arca_core::reconcile::{decide_traced, Action};
use arca_core::state::{LocalState, RemoteState};
use arca_format::trace::TraceSink;
use arca_store::root::StorageRoot;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

/// 一次 `status` 的分类结果：按 `sync` 若真的执行会落进哪个桶来命名字段
/// （`to_upload`/`to_download`/… 而不是 `sync::SyncReport` 的
/// `uploaded`/`downloaded`/…），强调这些都是"将会"而非"已经"发生的动作。
#[derive(Debug, Default)]
pub struct StatusReport {
    pub to_upload: Vec<String>,
    pub to_download: Vec<String>,
    pub to_adopt: Vec<String>,
    pub to_delete_local: Vec<String>,
    pub tombstone_pending: Vec<String>,
    pub conflicts: Vec<String>,
    pub needs_human: Vec<String>,
    pub scan_rejected: Vec<(String, scan::RejectReason)>,
    pub baseline_reset: bool,
}

impl StatusReport {
    /// 是否没有任何结构化问题（冲突/需要人工介入/未完成的 tombstone 传播/
    /// 扫描阶段被拒绝的路径）——与 `sync::SyncReport::is_clean` 同一定义。
    pub fn is_clean(&self) -> bool {
        self.tombstone_pending.is_empty()
            && self.conflicts.is_empty()
            && self.needs_human.is_empty()
            && self.scan_rejected.is_empty()
    }

    /// 是否有待办的常规动作（上传/下载/零传输认领/本地删除）——`sync` 真的
    /// 跑一次会不会有实际动作。
    pub fn has_pending(&self) -> bool {
        !self.to_upload.is_empty()
            || !self.to_download.is_empty()
            || !self.to_adopt.is_empty()
            || !self.to_delete_local.is_empty()
    }

    /// Rule of Silence：完全同步、没有任何问题——命令壳据此判断是否要安静。
    pub fn is_silent(&self) -> bool {
        self.is_clean() && !self.has_pending()
    }
}

/// `status` 失败——真正的 IO/格式故障，与"决策落在需要报告的终态"不同性质
/// （后者进 [`StatusReport`]，不是 `Err`），与 `sync::SyncError` 同一纪律。
#[derive(Debug)]
pub enum StatusError {
    Scan(scan::ScanError),
    Baseline(baseline::BaselineError),
    Hub(hub::HubError),
    /// M2e Task 3：经 [`Transport`] 读远端状态失败（`http(s)://` hub）。
    /// **保留完整的 [`TransportError`]，不折成字符串**——命令壳要靠它区分
    /// `Offline`（I11，退出码 2）与其它失败（退出码 1），折成字符串就没法
    /// 再分辨了。
    Transport(TransportError),
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatusError::Scan(e) => write!(f, "{e}"),
            StatusError::Baseline(e) => write!(f, "{e}"),
            StatusError::Hub(e) => write!(f, "{e}"),
            StatusError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StatusError {}

/// 跑一次只读的三态调和：`dataset_root` ↔ 本地存储根。**不修改任何文件**
/// ——不写本地文件、不写存储根、不落盘基线（与 `sync::sync` 的关键差别）。
pub fn status(
    dataset_root: &Path,
    root: &StorageRoot,
    sink: &mut dyn TraceSink,
) -> Result<StatusReport, StatusError> {
    status_with(dataset_root, sink, |_| {
        hub::read_remote(root).map_err(StatusError::Hub)
    })
}

/// 同一次只读调和，但远端状态经 [`Transport`] 读取（M2e Task 3：`arcad` 是
/// M2 的主线，主健康检查命令必须对主 hub 类型工作）。
///
/// 与 [`status`] 共用**同一段** [`status_with`] 判断逻辑，只换远端状态的
/// 来源——`status` 与 `sync` 不许各写一套判断（CLAUDE.md 架构约束），
/// `file://` 与 `http://` 之间同理：两种传输下"这个数据集有什么待办"的
/// 答案必须出自同一段代码。
pub fn status_transport(
    dataset_root: &Path,
    transport: &dyn Transport,
    sink: &mut dyn TraceSink,
) -> Result<StatusReport, StatusError> {
    status_with(dataset_root, sink, |_| {
        transport.read_remote().map_err(StatusError::Transport)
    })
}

/// 两种传输共用的判断本体。`read_remote` 是个闭包而不是一个已经算好的
/// map：**读远端要发生在扫描与基线加载之后**（与 M1d 起的既有顺序逐字
/// 一致），这样"本地就已经读不下去"这类失败不会被一次可能很慢、可能超时
/// 的网络往返挡在后面才报出来。
fn status_with(
    dataset_root: &Path,
    sink: &mut dyn TraceSink,
    read_remote: impl FnOnce(&mut dyn TraceSink) -> Result<BTreeMap<String, RemoteState>, StatusError>,
) -> Result<StatusReport, StatusError> {
    let scan_result = scan::scan_dataset(dataset_root, sink).map_err(StatusError::Scan)?;
    let baseline = baseline::load(dataset_root).map_err(StatusError::Baseline)?;
    let baseline_reset = baseline.was_reset();
    let remote = read_remote(sink)?;

    let mut report = StatusReport {
        scan_rejected: scan_result.rejected.clone(),
        baseline_reset,
        ..StatusReport::default()
    };

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(scan_result.files.keys().cloned());
    paths.extend(baseline.iter().map(|(p, _)| p.clone()));
    paths.extend(remote.keys().cloned());

    for (idx, path) in paths.iter().enumerate() {
        let base = baseline.get(path);
        let local = scan_result
            .files
            .get(path)
            .cloned()
            .unwrap_or(LocalState::Absent);
        let remote_state = remote.get(path).cloned().unwrap_or(RemoteState::Absent);

        let decision = decide_traced(&base, &local, &remote_state, path, idx as u64, sink);

        match decision.action {
            Action::Noop => {}
            Action::Upload { .. } => report.to_upload.push(path.clone()),
            Action::Download { .. } => report.to_download.push(path.clone()),
            Action::AdoptBaseline { .. } => report.to_adopt.push(path.clone()),
            Action::DeleteLocal { .. } => report.to_delete_local.push(path.clone()),
            Action::TombstoneRemote { .. } => report.tombstone_pending.push(path.clone()),
            Action::Conflict { .. } => report.conflicts.push(path.clone()),
            Action::NeedsHuman { .. } => report.needs_human.push(path.clone()),
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::hub_layout::FormatJson;
    use arca_format::trace::NullSink;
    use std::fs;

    fn 造存储根(dir: &Path) {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        let format = FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    fn open_root(dir: &Path) -> StorageRoot {
        StorageRoot::open(dir, None).unwrap()
    }

    #[test]
    fn 空目录对空存储根完全静默() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        let root = open_root(store.path());
        let mut sink = NullSink;
        let report = status(dataset.path(), &root, &mut sink).unwrap();
        assert!(report.is_silent());
    }

    #[test]
    fn 本地新增文件被归为待上传_且不写任何字节() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("a.txt"), b"hello").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let report = status(dataset.path(), &root, &mut sink).unwrap();

        assert_eq!(report.to_upload, vec!["a.txt".to_string()]);
        assert!(!report.is_silent());
        // status 绝不执行：存储根里不应该出现任何文件。
        assert!(!store.path().join("files/a.txt").exists());
        // 基线也不应该被创建（status 是只读操作）。
        assert!(!dataset.path().join(".arca/client/baseline.jsonl").exists());
    }

    #[test]
    fn 两次连续status不改变任何状态且结果一致() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("a.txt"), b"hello").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let first = status(dataset.path(), &root, &mut sink).unwrap();
        let second = status(dataset.path(), &root, &mut sink).unwrap();
        assert_eq!(first.to_upload, second.to_upload);
        assert_eq!(first.to_upload, vec!["a.txt".to_string()]);
    }

    #[test]
    fn 不合规路径进入scan_rejected且不静默() {
        let dataset = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        造存储根(store.path());
        fs::write(dataset.path().join("CON.txt"), b"bad").unwrap();

        let root = open_root(store.path());
        let mut sink = NullSink;
        let report = status(dataset.path(), &root, &mut sink).unwrap();
        assert_eq!(report.scan_rejected.len(), 1);
        assert!(!report.is_clean());
        assert!(!report.is_silent());
    }
}
