//! trace 失败落盘（M1d Task 8；设计依据 spec §3.3、字节契约 FORMAT.md §10.6）。
//!
//! 进程内用 [`RingSink`] 收集；**成功即丢弃，失败才落盘**——正常退出不写
//! 任何文件（Rule of Silence，spec §3.2），非零退出才把整个环 flush 成一个
//! 会话文件。`ARCA_TRACE_EVENT` 环境变量（非空即视为"要"）可强制落盘，
//! 即便本次成功——与 `GIT_TRACE2_EVENT` 那类"设了就是要"的既有心智一致
//! （PROTOCOL.md §5.2、FORMAT.md §10.6）。
//!
//! # 落盘位置：一处刻意偏离 FORMAT.md §10.6 字面表述，已知且待人工拍板
//!
//! FORMAT.md §10.6 与 spec §3.3 的表格把 trace 的落盘位置定为**全机唯一**的
//! `<state>/trace/`（Linux `$XDG_STATE_HOME/arca`、macOS
//! `~/Library/Logs/arca`、Windows `%LOCALAPPDATA%\arca`），与具体数据集无关。
//! 本模块按 M1d Task 8 brief 的直接指示落到 **`<dataset>/.arca/client/trace/`**
//! ——数据集级别，而不是全机唯一。
//!
//! 这不是无意疏漏：`.arca/client/` 本就是本数据集的、gitignored 的可抛弃
//! 投影（I9，见 `baseline.rs` 顶部同一纪律），trace 挂在同一目录下语义上
//! 说得通；且当前实现里除了"数据集根"没有其它天然可用的落点（这一切分片
//! 命令——`sync`/`adopt`/`status`——都是先解析出一个具体数据集再动手，
//! 从未建立过一个跨数据集的全局状态目录）。**但这与 FORMAT.md §10.6/spec
//! §3.3 的字面表述不一致**，且继承了一个结构性缺口：在"数据集尚未解析出来"
//! 之前发生的失败（例如根本不在任何 vault 里、`.gitarca` 本身解析失败）
//! 没有地方可落盘，全局 `<state>/trace/` 设计不会有这个缺口。是否要按
//! FORMAT.md 原意补一个全局状态目录、或者反过来把 FORMAT.md 改成"数据集
//! 级别"以符合本次实现——留给人工按 I10（格式先于代码）决定，这里不擅自
//! 改规范文档。
//!
//! # 保留策略
//!
//! 保留最近 `keep` 个会话文件（[`DEFAULT_KEEP`]），超出的按文件名（sid 前缀
//! 即紧凑时间戳，字典序即时间序，FORMAT.md §10.2）最旧优先淘汰——GC 由每次
//! 落盘时顺手做（客户端零常驻，没有别的时机能做这件事，spec §3.3）。
//!
//! # 不 fsync
//!
//! 与 FORMAT.md §10.6 同一纪律：真正不能丢的是 `.txn` 与 journal，各自已有
//! fsync 保证；trace 丢了事实仍完整存在于二者中。tmp → rename 只是为了不
//! 留半截写入的文件，不是持久化保证。

use arca_format::trace::{RingSink, Sid};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 默认保留的会话文件数（未在 brief/spec 中钉死具体数字；FORMAT.md §10.6
/// 给全局 `<state>/trace/` 定的是"超过 50 个文件或 14 天"，这里沿用文件数
/// 那一半，数据集级别的目录预期比全局目录小得多，同一个量级足够宽松）。
pub const DEFAULT_KEEP: usize = 50;

const TRACE_SID_ENV: &str = "ARCA_TRACE_SID";
const TRACE_EVENT_ENV: &str = "ARCA_TRACE_EVENT";

/// 本次会话是否应当强制落盘（即便成功）——`ARCA_TRACE_EVENT` 被设置为
/// 任意非空值（PROTOCOL.md §5.2、FORMAT.md §10.6）。
///
/// 只识别"落盘到 `<dataset>/.arca/client/trace/`"这一种取值；FORMAT.md §10.6
/// 还定义了 `=1`/`=2`（实时写 stdout/stderr）与 `=<路径>`（实时写指定文件）
/// 两种取值，本模块只做"失败落盘"这一件事，尚未实现那两种实时镜像——
/// 这里只把它当作"要不要强制落盘"的布尔开关，不解释其值。
pub fn force_flush_requested() -> bool {
    std::env::var_os(TRACE_EVENT_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// 本次是否应当落盘：失败，或调用方要求强制落盘。
pub fn should_flush(succeeded: bool) -> bool {
    !succeeded || force_flush_requested()
}

/// 解析本次会话的 sid：若环境变量 `ARCA_TRACE_SID` 存在且是合法 sid，
/// 派生一个子 sid（层次化会话标识，见 `arca_format::trace::Sid`）；
/// 缺失或不合法则以自身为根，**不报错**——trace 是诊断产物，绝不能因它
/// 而使命令失败（PROTOCOL.md §5.2）。
pub fn resolve_sid() -> Sid {
    if let Ok(parent) = std::env::var(TRACE_SID_ENV) {
        if let Ok(parent_sid) = Sid::parse(&parent) {
            let timestamp = crate::clock::now_compact();
            let random = crate::ids::random_hex32();
            if let Ok(child) = parent_sid.child(&timestamp, &random[..16]) {
                return child;
            }
        }
    }
    new_sid()
}

/// 构造一个全新的根 sid。
pub fn new_sid() -> Sid {
    let timestamp = crate::clock::now_compact();
    let random = crate::ids::random_hex32();
    Sid::new(&timestamp, &random[..16])
        .expect("now_compact 与 random_hex32 产出的形状必然合法（同 ids::new_version_id 的先例）")
}

/// 落盘失败——真正的 IO 故障。**调用方绝不能让这个错误使整个命令失败**
/// （trace 是诊断产物，见模块顶部纪律）；命令壳应当仅把它打到 stderr。
#[derive(Debug)]
pub struct FlushError {
    pub path: String,
    pub reason: String,
}

impl fmt::Display for FlushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "trace 落盘 {} 失败：{}", self.path, self.reason)
    }
}

impl std::error::Error for FlushError {}

/// 一次落盘的结果，供命令壳打印诊断信息。
#[derive(Debug)]
pub struct FlushOutcome {
    pub path: PathBuf,
    pub events: usize,
    /// 落盘前，环形缓冲已经因容量上限挤掉的事件数（`sink.dropped()`）。
    /// 落盘文件本身也会含一条 `trace.dropped` 事件如实记录同一数字
    /// （`RingSink::drain` 的职责，见其文档），这里额外返回是为了让命令壳
    /// 不必重新解析落盘文件就能在 stderr 报一句摘要。
    pub dropped: u64,
}

fn trace_dir(dataset_root: &Path) -> PathBuf {
    dataset_root.join(".arca").join("client").join("trace")
}

/// 把 `sink` 里留存的事件 flush 成 `<dataset_root>/.arca/client/trace/<sid 末段>.jsonl`，
/// 并淘汰超出 `keep` 保留数量的最旧会话文件。
///
/// `sink` 取可变引用是因为 [`RingSink::drain`] 会清空缓冲——调用方不应该在
/// 一次 flush 之后继续复用同一个 sink 塞入下一个会话的事件（每次命令调用
/// 各自持有一个 sink，语义上就是一次会话，`sync.rs` 顶部注释里"一次 `arca`
/// 调用就是一次会话"的说法在这里同样适用）。
pub fn flush(
    dataset_root: &Path,
    sid: &Sid,
    sink: &mut RingSink,
    keep: usize,
) -> Result<FlushOutcome, FlushError> {
    let dir = trace_dir(dataset_root);
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;

    let dropped = sink.dropped();
    let events = sink.drain(sid);

    let mut content = String::new();
    for event in &events {
        content.push_str(&event.to_json_line());
        content.push('\n');
    }

    let file_name = format!("{}.jsonl", sid.leaf());
    let path = dir.join(&file_name);
    write_atomic_no_fsync(&path, &content).map_err(|e| io_err(&path, e))?;

    // GC 失败不应该让本次落盘本身失败——落盘的核心义务（"这次失败的线索被
    // 保住了"）已经达成，淘汰旧文件只是空间管理，尽力而为即可（下次落盘时
    // 还会再试一次）。
    let _ = gc(&dir, keep);

    Ok(FlushOutcome {
        path,
        events: events.len(),
        dropped,
    })
}

fn io_err(path: &Path, e: io::Error) -> FlushError {
    FlushError {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// tmp → rename，不留半截写入的文件；**刻意不 fsync**（模块顶部纪律）。
fn write_atomic_no_fsync(path: &Path, content: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .expect("trace 文件路径总在 trace 目录下，必有 parent");
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session.jsonl");
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    fs::write(&tmp, content.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// 保留最近 `keep` 个会话文件，其余按文件名（sid 前缀是紧凑时间戳，字典序
/// 即时间序）最旧优先删除。
fn gc(dir: &Path, keep: usize) -> io::Result<()> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    if files.len() > keep {
        for old in &files[..files.len() - keep] {
            // 单个文件删不掉（并发进程占用等）不应该拖垮整次 GC——尽力而为，
            // 下次落盘会再试一次；trace 本就不是权威数据（模块顶部纪律）。
            let _ = fs::remove_file(old);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::trace::{EventKind, TraceRecord, TraceSink};
    use std::sync::Mutex;

    /// `cargo test` 默认多线程并发跑测试，而 `std::env::set_var`/`remove_var`
    /// 操作的是整个进程共享的环境——本模块里操作 `ARCA_TRACE_EVENT`/
    /// `ARCA_TRACE_SID` 的测试必须互斥，否则会彼此踩踏产生间歇性失败。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn sid_n(n: u8) -> Sid {
        // 时间戳前缀递增，保证字典序即插入顺序——GC 测试依赖这一点。
        Sid::new(&format!("202608{n:02}T090000Z"), "0123456789abcdef").unwrap()
    }

    #[test]
    fn 成功且未强制时不应落盘() {
        assert!(!should_flush(true));
    }

    #[test]
    fn 失败时应当落盘() {
        assert!(should_flush(false));
    }

    #[test]
    fn 强制环境变量设置时即便成功也应落盘() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(TRACE_EVENT_ENV, "1");
        let result = should_flush(true);
        std::env::remove_var(TRACE_EVENT_ENV);
        assert!(result);
    }

    #[test]
    fn 空字符串的强制环境变量不视为要求强制落盘() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(TRACE_EVENT_ENV, "");
        let result = force_flush_requested();
        std::env::remove_var(TRACE_EVENT_ENV);
        assert!(!result);
    }

    #[test]
    fn flush落盘的文件是合法jsonl且事件数与sink一致() {
        let dataset = tempfile::tempdir().unwrap();
        let mut sink = RingSink::new(16);
        sink.record(TraceRecord::new(EventKind::Start, 0));
        sink.record(TraceRecord::new(EventKind::ScanSummary, 10).with("files", 3u64));

        let sid = sid_n(1);
        let outcome = flush(dataset.path(), &sid, &mut sink, DEFAULT_KEEP).unwrap();

        assert_eq!(outcome.events, 2);
        assert_eq!(outcome.dropped, 0);
        assert!(outcome.path.is_file());
        let text = fs::read_to_string(&outcome.path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "每行必须是合法 JSON：{line}"
            );
        }
    }

    #[test]
    fn flush后sink被清空() {
        let dataset = tempfile::tempdir().unwrap();
        let mut sink = RingSink::new(16);
        sink.record(TraceRecord::new(EventKind::Start, 0));
        let sid = sid_n(1);
        flush(dataset.path(), &sid, &mut sink, DEFAULT_KEEP).unwrap();
        assert!(sink.is_empty());
    }

    #[test]
    fn 挤出事件时落盘文件包含trace_dropped且dropped计数如实() {
        let dataset = tempfile::tempdir().unwrap();
        let mut sink = RingSink::new(2);
        for i in 0..5u64 {
            sink.record(TraceRecord::new(EventKind::ScanSummary, i).with("files", i));
        }
        let sid = sid_n(1);
        let outcome = flush(dataset.path(), &sid, &mut sink, DEFAULT_KEEP).unwrap();
        assert_eq!(outcome.dropped, 3);
        let text = fs::read_to_string(&outcome.path).unwrap();
        assert!(text.contains("\"trace.dropped\""));
    }

    #[test]
    fn 落盘目录落在dataset_arca_client_trace下() {
        let dataset = tempfile::tempdir().unwrap();
        let mut sink = RingSink::new(4);
        sink.record(TraceRecord::new(EventKind::Start, 0));
        let sid = sid_n(1);
        let outcome = flush(dataset.path(), &sid, &mut sink, DEFAULT_KEEP).unwrap();
        assert_eq!(
            outcome.path.parent().unwrap(),
            dataset.path().join(".arca/client/trace")
        );
    }

    #[test]
    fn 超出保留数量时最旧的会话文件被淘汰() {
        let dataset = tempfile::tempdir().unwrap();
        for n in 1..=5u8 {
            let mut sink = RingSink::new(4);
            sink.record(TraceRecord::new(EventKind::Start, 0));
            flush(dataset.path(), &sid_n(n), &mut sink, 3).unwrap();
        }
        let dir = dataset.path().join(".arca/client/trace");
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 3, "只应保留最近 3 个：{names:?}");
        // 最早的两个（n=1,2）应该已经被淘汰，保留 n=3,4,5。
        assert!(!names.iter().any(|n| n.contains("20260801")));
        assert!(!names.iter().any(|n| n.contains("20260802")));
        assert!(names.iter().any(|n| n.contains("20260805")));
    }

    #[test]
    fn resolve_sid在没有父sid环境变量时以自身为根() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(TRACE_SID_ENV);
        let sid = resolve_sid();
        assert_eq!(sid.depth(), 1);
    }

    #[test]
    fn resolve_sid在父sid合法时派生子sid() {
        let _guard = ENV_LOCK.lock().unwrap();
        let parent = new_sid();
        std::env::set_var(TRACE_SID_ENV, parent.as_str());
        let child = resolve_sid();
        std::env::remove_var(TRACE_SID_ENV);
        assert_eq!(child.depth(), 2);
        assert_eq!(child.root(), parent.as_str());
    }

    #[test]
    fn resolve_sid在父sid不合法时不报错而是以自身为根() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(TRACE_SID_ENV, "不是合法的sid");
        let sid = resolve_sid();
        std::env::remove_var(TRACE_SID_ENV);
        assert_eq!(sid.depth(), 1);
    }
}
