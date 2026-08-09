//! trace 失败落盘（M1d Task 8；设计依据 spec §3.3、字节契约 FORMAT.md §10.6）。
//!
//! 进程内用 [`RingSink`] 收集；**成功即丢弃，失败才落盘**——正常退出不写
//! 任何文件（Rule of Silence，spec §3.2），非零退出才把整个环 flush 成一个
//! 会话文件。`ARCA_TRACE_EVENT` 环境变量（非空即视为"要"）可强制落盘，
//! 即便本次成功——与 `GIT_TRACE2_EVENT` 那类"设了就是要"的既有心智一致
//! （PROTOCOL.md §5.2、FORMAT.md §10.6）。
//!
//! # 落盘位置：全机唯一
//!
//! FORMAT.md §10.6 / spec §3.3 把 trace 的落盘位置定为**全机唯一**的
//! `<state>/trace/`，与具体数据集无关——这正是本模块的落点，[`state_dir`]
//! 负责解析 `<state>` 本身：
//!
//! | 平台 | `<state>` |
//! | --- | --- |
//! | Linux | `$XDG_STATE_HOME/arca`（缺省 `~/.local/state/arca`） |
//! | macOS | `~/Library/Logs/arca` |
//! | Windows | `%LOCALAPPDATA%\arca` |
//!
//! 选"全机唯一"而不是挂在某个数据集目录下，不只是照抄规范：**一个挂在
//! 具体数据集下的落点，天然记录不了"解析数据集失败"这件事本身**——注册表
//! 损坏、数据集嵌套、路径根本不在任何 vault 里——而那恰恰是最需要诊断线索
//! 的时刻。全机位置没有这个结构性缺口：任何一次 `arca` 调用，不论最终有没有
//! 定位到具体数据集，都落在同一个地方，命令壳（`commands/porcelain.rs`）不
//! 再需要"尽力猜一个数据集目录当落点"这种退让。
//!
//! [`flush`] 把 `<state>` 本身作为可注入参数（而不是在函数内部直接调用
//! [`state_dir`]）：调用方负责解析真实的机器位置，本函数只管"给定一个目录，
//! 把 trace 落进它的 `trace/` 子目录"——这也是测试不必依赖用户主目录、也不
//! 必修改 `HOME`/`XDG_STATE_HOME` 就能覆盖落盘路径的原因。
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

/// 默认保留的会话文件数——FORMAT.md §10.6 给全机唯一的 `<state>/trace/`
/// 定的是"超过 50 个文件或 14 天"；本模块只实现按文件数的那一半（时间维度
/// 的淘汰未实现，见 [`gc`] 文档），50 与规范文字直接对应。
pub const DEFAULT_KEEP: usize = 50;

/// 解析全机唯一的 trace 状态目录 `<state>`（FORMAT.md §10.6 / spec §3.3
/// 表格定义的字面路径，末段的 `arca` 已经包含在返回值里）——[`flush`] 在
/// 这个目录下再建 `trace/` 子目录存放会话文件。
///
/// 用 [`directories::BaseDirs`]（不是 `ProjectDirs`）取宿主机的标准目录：
/// `ProjectDirs` 会在各平台约定的组织/应用段之间插入额外的路径分量（如
/// macOS 的反向域名 bundle id），与 FORMAT.md 钉死的字面路径不符；
/// `BaseDirs` 只给"这一类目录在这台机器上的位置"，由本函数自己拼上
/// `arca`：
///
/// - **Linux**：`BaseDirs::state_dir()` 就是 XDG state 目录本身
///   （`$XDG_STATE_HOME`，缺省 `~/.local/state`）——`directories` crate 对
///   Linux 原生支持这个概念，直接采用。
/// - **macOS**：`directories` 在 macOS 上没有对应"state"的标准目录
///   （`state_dir()` 恒为 `None`——Apple 的标准目录体系里没有这一类），
///   按 FORMAT.md 字面路径手工拼 `home_dir()/Library/Logs`。
/// - **其他平台（含 Windows）**：同样没有"state"概念；
///   `BaseDirs::data_local_dir()`（同样是 `BaseDirs` 而非 `ProjectDirs`
///   版本）在 Windows 上直接返回裸的 `%LOCALAPPDATA%`，不附加任何组织/
///   应用子目录——正好是 FORMAT.md 要的那个值。
///
/// 宿主机连 home/profile 目录都解析不出来时返回 `None`
/// （[`directories::BaseDirs::new`] 的既有约定，通常只在极度精简的容器
/// 环境发生）——调用方据此放弃落盘，不能报错让命令本身失败（trace 是
/// 诊断产物，见模块顶部纪律）。
pub fn state_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    let dir = if let Some(state) = base.state_dir() {
        state.to_path_buf()
    } else if cfg!(target_os = "macos") {
        base.home_dir().join("Library").join("Logs")
    } else {
        base.data_local_dir().to_path_buf()
    };
    Some(dir.join("arca"))
}

const TRACE_SID_ENV: &str = "ARCA_TRACE_SID";
const TRACE_EVENT_ENV: &str = "ARCA_TRACE_EVENT";

/// 本次会话是否应当强制落盘（即便成功）——`ARCA_TRACE_EVENT` 被设置为
/// 任意非空值（PROTOCOL.md §5.2、FORMAT.md §10.6）。
///
/// 只识别"落盘到 `<state>/trace/`"这一种取值；FORMAT.md §10.6
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

/// 会话文件的实际落点：`<state>/trace/`。
///
/// 公开出来是因为 `arca bugreport` 要列出最近的落盘文件——它必须列
/// **这个**目录而不是 `<state>` 本身。第一版就踩了这个坑：列 `<state>`
/// 只会得到一行「`trace` 目录」，几十个真正的会话文件一个都看不见，
/// 而 bugreport 里那一节的全部价值就在于让人知道**有哪些会话可以附上**。
pub fn trace_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("trace")
}

/// 把 `sink` 里留存的事件 flush 成 `<state_dir>/trace/<sid 末段>.jsonl`，
/// 并淘汰超出 `keep` 保留数量的最旧会话文件。
///
/// `state_dir` 是全机唯一的 `<state>` 目录（见模块文档「落盘位置」一节），
/// 由调用方解析后传入——通常是 [`state_dir`] 函数的返回值，但本函数刻意
/// 不在内部调用它：调用方负责"这台机器上真正的位置在哪"，本函数只管
/// "给定一个目录，把 trace 落进它的 `trace/` 子目录"，这也是测试能够
/// 不依赖用户主目录、直接注入一个临时目录来覆盖落盘路径的原因。
///
/// `sink` 取可变引用是因为 [`RingSink::drain`] 会清空缓冲——调用方不应该在
/// 一次 flush 之后继续复用同一个 sink 塞入下一个会话的事件（每次命令调用
/// 各自持有一个 sink，语义上就是一次会话，`sync.rs` 顶部注释里"一次 `arca`
/// 调用就是一次会话"的说法在这里同样适用）。
pub fn flush(
    state_dir: &Path,
    sid: &Sid,
    sink: &mut RingSink,
    keep: usize,
) -> Result<FlushOutcome, FlushError> {
    let dir = trace_dir(state_dir);
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
        let state = tempfile::tempdir().unwrap();
        let mut sink = RingSink::new(16);
        sink.record(TraceRecord::new(EventKind::Start, 0));
        sink.record(TraceRecord::new(EventKind::ScanSummary, 10).with("files", 3u64));

        let sid = sid_n(1);
        let outcome = flush(state.path(), &sid, &mut sink, DEFAULT_KEEP).unwrap();

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
        let state = tempfile::tempdir().unwrap();
        let mut sink = RingSink::new(16);
        sink.record(TraceRecord::new(EventKind::Start, 0));
        let sid = sid_n(1);
        flush(state.path(), &sid, &mut sink, DEFAULT_KEEP).unwrap();
        assert!(sink.is_empty());
    }

    #[test]
    fn 挤出事件时落盘文件包含trace_dropped且dropped计数如实() {
        let state = tempfile::tempdir().unwrap();
        let mut sink = RingSink::new(2);
        for i in 0..5u64 {
            sink.record(TraceRecord::new(EventKind::ScanSummary, i).with("files", i));
        }
        let sid = sid_n(1);
        let outcome = flush(state.path(), &sid, &mut sink, DEFAULT_KEEP).unwrap();
        assert_eq!(outcome.dropped, 3);
        let text = fs::read_to_string(&outcome.path).unwrap();
        assert!(text.contains("\"trace.dropped\""));
    }

    #[test]
    fn 落盘目录落在注入的state目录下的trace子目录() {
        let state = tempfile::tempdir().unwrap();
        let mut sink = RingSink::new(4);
        sink.record(TraceRecord::new(EventKind::Start, 0));
        let sid = sid_n(1);
        let outcome = flush(state.path(), &sid, &mut sink, DEFAULT_KEEP).unwrap();
        assert_eq!(outcome.path.parent().unwrap(), state.path().join("trace"));
    }

    #[test]
    fn 超出保留数量时最旧的会话文件被淘汰() {
        let state = tempfile::tempdir().unwrap();
        for n in 1..=5u8 {
            let mut sink = RingSink::new(4);
            sink.record(TraceRecord::new(EventKind::Start, 0));
            flush(state.path(), &sid_n(n), &mut sink, 3).unwrap();
        }
        let dir = state.path().join("trace");
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

    /// `state_dir` 本身：解析出的路径必须以 `arca` 结尾（每个平台分支都
    /// 拼接了这一段），且宿主机在测试环境里通常能解析出 home/profile 目录
    /// （CI/本机都设了 HOME 或等价变量），不应返回 `None`。
    #[test]
    fn state_dir解析出的路径以arca结尾() {
        let dir = state_dir().expect("测试环境应能解析出 home/profile 目录");
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("arca"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn state_dir在linux上遵循xdg_state_home环境变量() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", "/tmp/arca-test-custom-state");
        let dir = state_dir();
        match original {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        assert_eq!(dir, Some(PathBuf::from("/tmp/arca-test-custom-state/arca")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn state_dir在linux上缺省落在本地state目录() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original = std::env::var_os("XDG_STATE_HOME");
        std::env::remove_var("XDG_STATE_HOME");
        let dir = state_dir().unwrap();
        if let Some(v) = original {
            std::env::set_var("XDG_STATE_HOME", v);
        }
        assert!(
            dir.ends_with(".local/state/arca"),
            "缺省应落在 ~/.local/state/arca，实得 {dir:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn state_dir在macos上落在library_logs下() {
        let dir = state_dir().unwrap();
        assert!(
            dir.ends_with("Library/Logs/arca"),
            "应落在 ~/Library/Logs/arca，实得 {dir:?}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn state_dir在windows上等于localappdata下的arca() {
        let dir = state_dir().unwrap();
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            assert_eq!(dir, PathBuf::from(local_app_data).join("arca"));
        }
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
