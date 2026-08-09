//! 本地变更检测三重保险（M3b，spec §5.2，继承 lazync §5）：
//! 实时事件（Windows `ReadDirectoryChangesW` / macOS FSEvents）→
//! 溢出即全扫 → 周期性全量对账地基。
//!
//! 参考 lazync：`client/src/nc_directory_watcher.pas`。
//!
//! # watcher 说「有动静」，全量扫描说「动了什么」
//!
//! 关键在三层里的中间那层——**实时事件是不可靠的**：内核缓冲区会溢出，
//! 事件会被合并、丢弃、乱序；网络盘、容器挂载、某些文件系统根本不发事件。
//! 三重保险第二层存在的全部理由，就是承认第一层不可靠。
//!
//! 所以本模块**只产生唤醒信号，不产生变更清单**。[`Wake::Changed`] 带的
//! `sample` 只是给日志用的一个样本路径，类型上刻意不做成 `Vec<PathBuf>`——
//! 免得下一个人看见一份「改动清单」就想拿它去决定同步什么。任何
//! 「既然事件说改的是 a.txt，那就只同步 a.txt」的优化都会丢改动。
//!
//! 这与 M3a 对 `Transport::changes` 的处理是同一条纪律，理由也一样：
//! 判断一旦有第二个来源，两个来源就会分叉。
//!
//! # 绝不监听 `.arca/`
//!
//! agentd 每一轮都往 `<ds>/.arca/client/` 写基线与游标。监听它 =
//! 「写基线 → 唤醒 → 调和 → 写基线 → …」的 100% CPU 死循环。
//! 而且它在小库上**看起来能用**（一轮很快，像是在勤奋工作），
//! 只有真机跑一会儿才显形。测试里对着这条有一个直接断言。

use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher as _};

/// 收到一次唤醒之后再等这么久，让一串连续写入落停。
///
/// 编辑器保存一个文件常常产生 3–10 个事件（写临时文件 → rename → chmod）；
/// 不等它落停就调和，会在一次保存里跑好几轮。500ms 对「人保存文件」这个
/// 动作足够短（察觉不到），对事件风暴足够长。
pub const SETTLE: Duration = Duration::from_millis(500);

/// 一次唤醒。**没有变更清单**——见模块文档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake {
    /// 受管目录里有动静。`sample` 是其中一个路径，**只供日志**。
    Changed { sample: Option<String> },
    /// 内核事件队列溢出——事件丢了。这**不是错误**，它正是三重保险第二层
    /// 存在的理由：处置是立刻全量调和一次，而不是报错或忽略。
    Overflow,
}

#[derive(Debug)]
pub struct WatchError {
    pub reason: String,
}

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

/// 活着的监听器。**Drop 即停止监听**，所以调用方要把它拿在手里。
pub struct Watcher {
    _inner: notify::RecommendedWatcher,
    rx: std_mpsc::Receiver<Wake>,
}

impl Watcher {
    /// 监听 `dataset_dir`（递归）。
    ///
    /// 失败**必须由调用方降级处理**，不是致命错误：某些平台/文件系统不支持
    /// 事件监听，那时 agentd 退回纯周期模式照常工作。watcher 是周期对账的
    /// 增强，不是它的替代（分层降级关系，spec §3.1）。
    pub fn start(dataset_dir: &Path) -> Result<Self, WatchError> {
        let (tx, rx) = std_mpsc::channel();
        // **两个根都要留着。** macOS 上 `/var` 是 `/private/var` 的符号链接，
        // FSEvents 报的是**规范化之后**的路径；Linux 上 `/tmp` 之类也可能
        // 经由符号链接。只拿调用方给的那个路径去 `strip_prefix`，在 macOS
        // 上会**每一个事件都关联不上**。第一版就栽在这里，见 `is_internal`。
        let mut roots = vec![dataset_dir.to_path_buf()];
        if let Ok(c) = std::fs::canonicalize(dataset_dir) {
            if c != dataset_dir {
                roots.push(c);
            }
        }
        let mut inner = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let wake = match res {
                Ok(event) => {
                    // 只看真正改变了内容/存在性的事件类型。`Access`（含只读
                    // 打开、mmap）会在备份/杀毒/全库索引扫描时暴涨，而它们
                    // 一个字节都没改——把它们算作变更，等于让一次杀毒扫描
                    // 触发一轮全库调和。
                    if !matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        return;
                    }
                    let paths: Vec<&PathBuf> = event
                        .paths
                        .iter()
                        .filter(|p| !is_internal(&roots, p))
                        .collect();
                    if paths.is_empty() {
                        // 全部落在 `.arca/` 里——agentd 自己写的，绝不自我唤醒。
                        return;
                    }
                    Wake::Changed {
                        sample: paths.first().map(|p| p.display().to_string()),
                    }
                }
                // notify 用 `Error` 报告队列溢出等情况。**不当错误处理**：
                // 事件丢了正是要全扫的时候。
                Err(_) => Wake::Overflow,
            };
            // 发不出去（接收端已经走了）就算了——那说明 agentd 正在退出。
            let _ = tx.send(wake);
        })
        .map_err(|e| WatchError {
            reason: format!("无法创建文件系统监听器：{e}"),
        })?;

        inner
            .watch(dataset_dir, RecursiveMode::Recursive)
            .map_err(|e| WatchError {
                reason: format!("无法监听 {}：{e}", dataset_dir.display()),
            })?;

        Ok(Watcher { _inner: inner, rx })
    }

    /// 取一次唤醒，最多等 `timeout`。返回 `None` 表示这段时间里没有动静。
    ///
    /// 取到之后会等 [`SETTLE`] 再**吸干队列里剩下的**——一次保存产生的
    /// 一串事件合并成一次唤醒。溢出优先级最高：这一批里只要有一个溢出，
    /// 整批就报溢出（宁可多做一次全扫）。
    pub fn wait(&self, timeout: Duration) -> Option<Wake> {
        let first = self.rx.recv_timeout(timeout).ok()?;
        std::thread::sleep(SETTLE);
        let mut wake = first;
        while let Ok(next) = self.rx.try_recv() {
            if matches!(next, Wake::Overflow) {
                wake = Wake::Overflow;
            }
        }
        Some(wake)
    }
}

/// 起一个专用线程持有 [`Watcher`]，把唤醒转发进 tokio 通道。
///
/// 为什么要多一层线程：`Watcher::wait` 是阻塞的，而 `notify` 的 watcher 句柄
/// 不保证 `Sync`——每轮都把它搬进 `spawn_blocking` 是不成立的。让它待在一个
/// 自己的线程里、只把「唤醒」这个**值**送出来，两边的约束都满足了。
///
/// 返回 `None` 表示这台机器/这个文件系统不支持事件监听——调用方据此
/// **降级到纯周期模式**，不是失败（spec §3.1 的分层降级关系）。
///
/// 通道容量 1 且满了就丢：队列里已经躺着一个唤醒时，第二个唤醒没有任何
/// 新信息——「有动静」不是可累加的量。这也顺带让事件风暴天然收敛。
pub fn spawn_forwarder(
    dataset_dir: &Path,
) -> Result<tokio::sync::mpsc::Receiver<Wake>, WatchError> {
    let watcher = Watcher::start(dataset_dir)?;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        loop {
            // 1 秒一轮：`wait` 内部会阻塞在 `recv_timeout` 上，超时只是让这个
            // 线程有机会发现「接收端已经走了」并退出，不是轮询文件系统。
            let Some(wake) = watcher.wait(Duration::from_secs(1)) else {
                if tx.is_closed() {
                    return;
                }
                continue;
            };
            match tx.try_send(wake) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
    });
    Ok(rx)
}

/// 这条路径是不是 agentd 自己的内部状态（`<ds>/.arca/`）。
///
/// 判据是**顶层路径分量**而不是字符串包含：一个名叫 `my.arca-notes` 的用户
/// 目录不该被当成内部状态，而 `contains(".arca")` 会误判它。
///
/// `roots` 含数据集目录本身与它的规范化形式——见 [`Watcher::start`] 里的
/// 说明（macOS 的 `/var` → `/private/var`）。
///
/// # 关联不上任何一个根时，**唤醒**而不是忽略
///
/// 第一版这里写的是「关联不上就当作内部，不唤醒」，看起来保守，实际是
/// **反的**：漏一次唤醒 = 一次静默丢失的改动（要等到周期对账才被发现）；
/// 多一次唤醒 = 一轮白跑的调和。两者代价差着数量级。
///
/// 而且它造成了一个教科书式的假绿：macOS 上每个事件都关联不上，于是
/// 所有事件被吞掉，「绝不自我唤醒」那条测试**真空通过**，而三条正例
/// 同时失败。如果当时只写了那条负例，这个 bug 会被当成「功能正常」。
fn is_internal(roots: &[PathBuf], path: &Path) -> bool {
    for root in roots {
        if let Ok(rel) = path.strip_prefix(root) {
            return rel
                .components()
                .next()
                .map(|c| c.as_os_str() == ".arca")
                .unwrap_or(false);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 建数据集() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".arca/client")).unwrap();
        d
    }

    #[test]
    fn 内部路径判定按分量而不是字符串包含() {
        let roots = vec![PathBuf::from("/v/assets")];
        assert!(is_internal(
            &roots,
            Path::new("/v/assets/.arca/client/baseline.jsonl")
        ));
        assert!(!is_internal(&roots, Path::new("/v/assets/a.png")));
        // 名字里带 `.arca` 的普通用户文件/目录**不是**内部状态。
        assert!(!is_internal(
            &roots,
            Path::new("/v/assets/my.arca-notes/x.png")
        ));
        assert!(!is_internal(&roots, Path::new("/v/assets/notes.arca.md")));
        // 嵌在深处的 `.arca` 也不是顶层内部状态（那是用户自己的目录）。
        assert!(!is_internal(&roots, Path::new("/v/assets/sub/.arca/x")));
    }

    /// 规范化之后的根也要认——macOS 上 FSEvents 报 `/private/var/...`，
    /// 而调用方给的是 `/var/...`。只认其中一个会让**每一个事件**都关联不上。
    #[test]
    fn 规范化前后的根都被认作同一个数据集() {
        let roots = vec![
            PathBuf::from("/var/x/assets"),
            PathBuf::from("/private/var/x/assets"),
        ];
        assert!(is_internal(
            &roots,
            Path::new("/private/var/x/assets/.arca/client/baseline.jsonl")
        ));
        assert!(!is_internal(
            &roots,
            Path::new("/private/var/x/assets/photo.png")
        ));
    }

    /// 关联不上任何一个根 → **唤醒**（返回 false），不是忽略。
    /// 漏一次唤醒是静默丢改动；多一次唤醒只是一轮白跑的调和。
    #[test]
    fn 关联不上根的路径倾向于唤醒而不是忽略() {
        let roots = vec![PathBuf::from("/v/assets")];
        assert!(!is_internal(&roots, Path::new("/完全不相干/x.png")));
    }

    /// 受管文件的写入必须产生唤醒。
    #[test]
    fn 受管文件改动产生唤醒() {
        let d = 建数据集();
        let w = Watcher::start(d.path()).expect("本机应当支持文件系统事件");
        std::fs::write(d.path().join("photo.png"), b"hello").unwrap();
        assert!(
            w.wait(Duration::from_secs(5)).is_some(),
            "写入受管文件应当唤醒"
        );
    }

    /// **本文件里最重要的一条。** agentd 每轮都往 `.arca/client/` 写基线与
    /// 游标；如果它们能唤醒自己，就是一个 100% CPU 的死循环——而且在小库上
    /// 「看起来能用」，只有真机跑一会儿才显形。
    #[test]
    fn 写入arca内部状态绝不唤醒自己() {
        let d = 建数据集();
        let w = Watcher::start(d.path()).expect("本机应当支持文件系统事件");

        // 模拟 agentd 一轮之后的全部内部写入。
        std::fs::write(d.path().join(".arca/client/baseline.jsonl"), b"{}\n").unwrap();
        std::fs::write(d.path().join(".arca/client/changes-cursor"), b"x\n").unwrap();
        std::fs::write(d.path().join(".arca/client/role.toml"), b"schema=1\n").unwrap();

        assert_eq!(
            w.wait(Duration::from_secs(2)),
            None,
            "agentd 自己的内部写入绝不能唤醒自己——那是 100% CPU 的死循环"
        );
    }

    /// 内部写入不唤醒，**但同一批里的受管文件仍然要唤醒**——不能因为
    /// 「顺手过滤」把真正的改动一起吞掉。
    #[test]
    fn 内部写入与受管改动混在一起时仍然唤醒() {
        let d = 建数据集();
        let w = Watcher::start(d.path()).expect("本机应当支持文件系统事件");
        std::fs::write(d.path().join(".arca/client/baseline.jsonl"), b"{}\n").unwrap();
        std::fs::write(d.path().join("real.png"), b"bytes").unwrap();
        assert!(
            w.wait(Duration::from_secs(5)).is_some(),
            "混在内部写入里的真实改动不能被一起吞掉"
        );
    }

    /// 一次保存产生的一串事件合并成**一次**唤醒——否则一次保存会跑好几轮调和。
    #[test]
    fn 连续写入合并成一次唤醒() {
        let d = 建数据集();
        let w = Watcher::start(d.path()).expect("本机应当支持文件系统事件");
        for i in 0..10 {
            std::fs::write(d.path().join(format!("f{i}.bin")), b"x").unwrap();
        }
        assert!(w.wait(Duration::from_secs(5)).is_some());
        // 落停之后队列应当已被吸干：再等一小会儿不该还有唤醒。
        assert_eq!(
            w.wait(Duration::from_millis(300)),
            None,
            "一串连续写入应当已被合并进上一次唤醒"
        );
    }

    /// 没有动静时 `wait` 超时返回 `None`，不阻塞到天荒地老。
    #[test]
    fn 无动静时超时返回none() {
        let d = 建数据集();
        let w = Watcher::start(d.path()).expect("本机应当支持文件系统事件");
        assert_eq!(w.wait(Duration::from_millis(300)), None);
    }

    /// 监听一个不存在的目录必须**报错而不是 panic**——调用方据此降级到
    /// 纯周期模式。
    #[test]
    fn 监听不存在的目录时报错供调用方降级() {
        let d = tempfile::tempdir().unwrap();
        let Err(err) = Watcher::start(&d.path().join("根本没有这个目录")) else {
            panic!("监听不存在的目录应当报错");
        };
        assert!(err.to_string().contains("无法监听"), "{err}");
    }
}
