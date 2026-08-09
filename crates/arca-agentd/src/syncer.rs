//! 每数据集独立的自动调和回路（M3a Task 2，spec §4.3.2、§5.2）。
//!
//! 参考 lazync：`client/src/nc_sync_engine.pas`、`nc_http_task.pas`。
//!
//! # 本模块**不做**决策
//!
//! 「该传什么、该删什么、冲突怎么落地」全部来自 `arca_core::reconcile`，
//! 执行来自 `arca_cli::sync::sync_transport`——**就是 `arca sync` 调的那一个**。
//! 本模块只回答两个问题：**什么时候调它**，以及**失败了怎么退避**。
//!
//! 这不是洁癖。CLAUDE.md 的分层降级关系写着「agentd 是手动 CLI 的增强，
//! 不是依赖」，而它在代码上的唯一含义就是两者跑同一段代码。如果 agentd
//! 自己写一套调和执行，两条路会分叉——M2d 的评审在 `Transport` 上抓过
//! 同构的问题（「角色在一种传输上被尊重、在另一种上被忽略」）。分叉之后，
//! 用户报「手动同步没事、自动同步丢东西」时没有人能解释。
//!
//! 所以本文件里**不应该出现任何 `RemoteState`/`Decision`/`Action` 的匹配**。
//! 如果你发现自己在这里写「如果远端有而本地没有就下载」，你走错地方了。
//!
//! # 独立故障域（I11）
//!
//! 每个数据集一个 task，各自持有独立的退避状态与健康度。一个 hub 不可达
//! 只让**它承载的**数据集进入离线态，其余数据集完全不受影响。最容易写错的
//! 形态是用一个 `?` 中止整个循环——M2d Task 3 在客户端命令壳上踩过同构的
//! 问题，本模块的结构（一 task 一数据集）从根上排除了它。

use std::path::Path;
use std::time::Duration;

use arca_cli::sync::{SyncActor, SyncReport};
use arca_cli::transport::{ChangesOutcome, Transport};
use arca_format::journal::Cursor;
use tokio::sync::watch;

/// 相邻两轮对账之间的基础间隔。
///
/// 30 秒是「用户改完文件切到另一台机器」这个动作的自然时间尺度——再快
/// 收益递减（人还没走到另一台机器前），再慢就开始像是坏了。真正的低延迟
/// 要靠 longpoll（消费 M2c 建好的 `GET /changes`）与本地 watcher（M3b），
/// 周期对账是**它们之下的地基**，不是它们的替代品：spec §5.2 的三重保险
/// 里，「周期性全量对账」那一层的意义正是「事件丢了也终会收敛」。
pub const BASE_INTERVAL: Duration = Duration::from_secs(30);

/// 退避的上限。封顶而不是无限翻倍：一个离线了两小时的 hub，用户把网插回来
/// 之后不该再等两小时才被发现。
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// 一轮调和的结果——**只用于日志与健康度**，不参与任何决策。
#[derive(Debug)]
pub enum Outcome {
    Ok(Box<SyncReport>),
    /// 这一轮失败了。`retryable` 来自既有的 `ErrorClass` 分类（M1b Task 4），
    /// 本模块不自己判断「这算不算能重试」。
    Failed {
        message: String,
        retryable: bool,
    },
}

/// 一个数据集的回路状态。**每个数据集一份**，绝不共享。
#[derive(Debug)]
pub struct Loop {
    /// 归一化后的相对路径，用于日志（用户认得的那个名字）。
    pub label: String,
    /// 它绑定的 hub 名——离线时必须说清是哪个 hub（M2d 的教训：一个 vault
    /// 有多个数据集分属不同 hub 时，光报路径不足以让用户判断该查哪个）。
    pub hub_name: String,
    /// 连续失败次数，用于计算退避。成功即清零。
    consecutive_failures: u32,
}

impl Loop {
    pub fn new(label: String, hub_name: String) -> Self {
        Self {
            label,
            hub_name,
            consecutive_failures: 0,
        }
    }

    /// 下一轮之前该等多久。成功 → 基础间隔；失败 → 指数退避并封顶。
    ///
    /// **没有随机抖动**：抖动是给「大量客户端同时冲击一台服务器」准备的，
    /// 而 arca 的部署形态是个人的几台设备对自己的一台 NAS（spec §1.1）。
    /// 引入抖动会让「等多久」变得不可预测，从而让本函数无法被确定性测试
    /// 覆盖——为一个本项目形态下不存在的问题牺牲一条可测性，不值得。
    /// 如果将来出现多客户端共用 hub 的场景，这里加抖动，并同时给测试
    /// 注入随机源。
    pub fn next_delay(&self) -> Duration {
        if self.consecutive_failures == 0 {
            return BASE_INTERVAL;
        }
        // 1s, 2s, 4s, ... 封顶 MAX_BACKOFF。`min(16)` 挡住移位溢出。
        let shift = self.consecutive_failures.saturating_sub(1).min(16);
        let secs = 1u64 << shift;
        Duration::from_secs(secs).min(MAX_BACKOFF)
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// 记录一轮的结果，更新退避状态。
    pub fn record(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Ok(_) => self.consecutive_failures = 0,
            Outcome::Failed { .. } => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1)
            }
        }
    }
}

/// 跑一轮调和。**阻塞 IO 全部在 `spawn_blocking` 里**。
///
/// M2c 的评审 I6 教训：全部阻塞工作直接跑在 tokio worker 上，12 并发时一个
/// 零 IO 的纯路由 404 首次耗时 4.45 秒；部署目标是 2–4 核 ARM NAS，届时
/// 几个大数据集就能把 worker 占满。agentd 与 arcad 经常跑在同一台机器上，
/// 同一条纪律照抄。
///
/// `make_transport` 是个闭包而不是现成的 `Transport`：`Transport` 的实现
/// 不保证 `Send`，而 `spawn_blocking` 要求闭包 `Send`——所以传输在**阻塞
/// 线程内部**现造，不跨线程搬运。每轮重建一个 HTTP 客户端的代价远小于把
/// `Send + Sync` 约束强加给整个 `Transport` trait 的代价。
/// 一轮调和的失败。`retryable` 来自既有的 `ErrorClass` 分类（M1b Task 4），
/// **本模块不自己判断「这算不算能重试」**。
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub retryable: bool,
}

impl Failure {
    /// 传输连造都造不出来（URL 非法、pin 不符、存储根身份不认识）——
    /// 重试不会让它自己变好，等人处理。
    pub fn not_retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

/// **本 crate 里唯一允许调用调和的地方。**
///
/// 抽成一个函数不是为了少写几行，而是为了让「agentd 调的就是 `arca sync`
/// 调的那一个」这件事有一个可以被 grep 到的单点：两种传输、将来的 longpoll
/// 唤醒路径、watcher 唤醒路径，全部经过这里。
pub fn do_sync<T: Transport>(
    dataset_dir: &Path,
    transport: &T,
    actor: &SyncActor,
) -> Result<SyncReport, Failure> {
    // trace 用 NullSink：agentd 每 30 秒跑一轮，把每一轮都留痕会让 trace
    // 目录变成噪音场。失败时的诊断由 `Failure::message` 承担，真正需要
    // 完整 trace 的场景是人手动重跑一次 `arca sync`。
    let mut sink = arca_format::trace::NullSink;
    // 就是这一行——`arca sync` 调的同一个函数。
    arca_cli::sync::sync_transport(dataset_dir, transport, actor, &mut sink).map_err(|e| Failure {
        message: e.to_string(),
        retryable: matches!(sync_error_class(&e), ErrorClass::Retryable),
    })
}

/// 在阻塞线程里跑一轮。
///
/// M2c 的评审 I6 教训：全部阻塞工作直接跑在 tokio worker 上，12 并发时一个
/// 零 IO 的纯路由 404 首次耗时 4.45 秒；部署目标是 2–4 核 ARM NAS，届时
/// 几个大数据集就能把 worker 占满。agentd 与 arcad 经常跑在同一台机器上，
/// 同一条纪律照抄。
///
/// `body` 自己负责「造传输 + 调 [`do_sync`]」，而不是由本函数接过一个现成的
/// `Transport`：`LocalTransport<'a>` 借用 `StorageRoot`，而 `spawn_blocking`
/// 要求闭包 `Send + 'static`——把两者都关在同一个闭包作用域里，借用就不必
/// 跨线程活着了。代价是每轮重建一次传输，远小于把 `'static` 约束强加给整个
/// `Transport` trait 的代价。
pub async fn run_once<F>(body: F) -> Outcome
where
    F: FnOnce() -> Result<SyncReport, Failure> + Send + 'static,
{
    match tokio::task::spawn_blocking(body).await {
        Ok(Ok(report)) => Outcome::Ok(Box::new(report)),
        Ok(Err(f)) => Outcome::Failed {
            message: f.message,
            retryable: f.retryable,
        },
        // 阻塞任务 panic 了。**不能让它带走整个 daemon**——把它降级成这个
        // 数据集这一轮的失败，其余数据集照常（I11 的进程内镜像）。
        Err(e) => Outcome::Failed {
            message: format!("调和任务异常终止：{e}"),
            retryable: false,
        },
    }
}

use arca_format::trace::ErrorClass;

/// 把 `SyncError` 映射到既有的 `ErrorClass`。
///
/// 只认 `Transport` 那一支：网络抖动、hub 暂时不可达属于「等一会儿再来」。
/// 其余（扫描失败、基线损坏、角色文件非法）都是需要人看一眼的状态——
/// 对它们指数退避重试只会把同一个错误刷满日志（I5：状态模糊就停下）。
fn sync_error_class(e: &arca_cli::sync::SyncError) -> ErrorClass {
    match e {
        arca_cli::sync::SyncError::Transport(t) => t.class(),
        _ => ErrorClass::NeedsHuman,
    }
}

/// 一次「等着有事发生」的结果（M3a Task 3）。
#[derive(Debug, PartialEq, Eq)]
pub enum Wakeup {
    /// 有新事件（或者等待超时到点了）——去跑一轮调和。
    Reconcile,
    /// 服务端说这个游标没法续接：丢掉它，做一次全量对账。
    ResetAndReconcile,
    /// 该停了。
    Stop,
}

/// 用 `Transport::changes` 的 longpoll 当唤醒器。
///
/// # 为什么 `changes` 只用来**唤醒**，不用来驱动调和
///
/// 拿到事件之后「按事件增量地改本地」是很诱人的写法，但那等于在 agentd 里
/// 重新实现一遍调和——而调和的正确性全部由 `arca-core` 的 18 格决策表与
/// `arca_cli::sync` 的四道闸门保证。事件流告诉我们的只是**「那边有动静」**；
/// 「该怎么办」仍然要交给同一段代码去算。
///
/// 代价是每次唤醒都做一次全量对账（扫本地 + 读远端 state）。收益是
/// **agentd 与手动命令永远不会给出不同的结果**——这正是 M2d 评审在
/// `Transport` 上抓过的那类分叉，本切片从一开始就不留口子。
///
/// 真正的增量优化（只对变动路径做调和）要等到调和本身支持「只看这几条路径」，
/// 那是 `arca-core` 的接口问题，不是 agentd 该私自解决的问题。
pub async fn wait_for_change<F>(
    since: Option<Cursor>,
    wait: Duration,
    probe: F,
    shutdown: &mut watch::Receiver<bool>,
) -> (Wakeup, Option<Cursor>)
where
    F: FnOnce(Option<Cursor>, Duration) -> Result<ChangesOutcome, String> + Send + 'static,
{
    if *shutdown.borrow() {
        return (Wakeup::Stop, since);
    }
    let probe_task = tokio::task::spawn_blocking(move || probe(since.clone(), wait));
    tokio::select! {
        joined = probe_task => match joined {
            // 有事件、以及 longpoll 空转超时，处置**相同**：都跑一轮调和。
            // 空转也跑不是浪费——周期性全量对账是 spec §5.2 三重保险的最后
            // 一层，事件流漏了也要收敛。这一层的存在，正是上面「changes 只
            // 用来唤醒」那个取舍能够成立的原因。
            Ok(Ok(ChangesOutcome::Events { cursor, .. })) => (Wakeup::Reconcile, cursor),
            Ok(Ok(ChangesOutcome::ResetRequired { cursor })) => (Wakeup::ResetAndReconcile, cursor),
            // 探测失败（网络抖动、hub 掉线）不是致命的：照常跑一轮调和，
            // 让 `sync_transport` 去给出准确的错误与分类。在这里自己判断
            // 「这算不算能重试」会与 `sync_error_class` 形成第二套判据。
            Ok(Err(_)) | Err(_) => (Wakeup::Reconcile, None),
        },
        _ = shutdown.changed() => (Wakeup::Stop, None),
    }
}

/// 等待「下一轮该开始了」或者「该停了」，返回 `true` 表示继续跑。
///
/// 关键是**不睡满整个间隔再看信号**——那会让 `SIGTERM` 之后最多等 30 秒
/// 才退出，用户会以为它卡死了转而 `kill -9`，而 `kill -9` 恰恰是我们
/// 想让用户不必用的东西。
pub async fn wait_next(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = shutdown.changed() => !*shutdown.borrow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 空回路() -> Loop {
        Loop::new("assets".into(), "home".into())
    }

    fn 失败() -> Outcome {
        Outcome::Failed {
            message: "x".into(),
            retryable: true,
        }
    }

    #[test]
    fn 成功之后回到基础间隔() {
        let mut l = 空回路();
        l.record(&失败());
        assert_ne!(l.next_delay(), BASE_INTERVAL);
        l.record(&Outcome::Ok(Box::default()));
        assert_eq!(l.consecutive_failures(), 0);
        assert_eq!(l.next_delay(), BASE_INTERVAL);
    }

    #[test]
    fn 退避指数增长且封顶() {
        let mut l = 空回路();
        let mut seen = Vec::new();
        for _ in 0..12 {
            l.record(&失败());
            seen.push(l.next_delay());
        }
        assert_eq!(seen[0], Duration::from_secs(1));
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[2], Duration::from_secs(4));
        assert!(
            seen.iter().all(|d| *d <= MAX_BACKOFF),
            "退避必须封顶，实得 {seen:?}"
        );
        assert_eq!(
            *seen.last().unwrap(),
            MAX_BACKOFF,
            "连续失败足够多次之后应当停在上限"
        );
    }

    /// 连续失败次数**不会溢出**——agentd 是长期运行的进程，一个永久离线的
    /// hub 会在几个月里累积到很大的计数。`saturating_add` 与 `min(16)` 的
    /// 组合要真的挡住 `1u64 << shift` 的移位溢出（`shift >= 64` 会 panic）。
    #[test]
    fn 极端失败次数下不panic也不溢出() {
        let mut l = 空回路();
        for _ in 0..200 {
            l.record(&失败());
        }
        assert_eq!(l.next_delay(), MAX_BACKOFF);
        assert_eq!(l.consecutive_failures(), 200);
    }

    #[tokio::test]
    async fn 收到停止信号时不等满间隔就返回false() {
        let (tx, mut rx) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send(true).ok();
        });
        let start = std::time::Instant::now();
        // 间隔给足 30 秒——如果实现是"先睡满再看信号"，这里会等满。
        let go_on = wait_next(Duration::from_secs(30), &mut rx).await;
        assert!(!go_on, "收到停止信号后不该继续");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "不能睡满整个间隔才响应停止：实耗 {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn 已经停止时直接返回false() {
        let (_tx, mut rx) = watch::channel(true);
        assert!(!wait_next(Duration::from_secs(30), &mut rx).await);
    }

    /// 传输造不出来时，这一轮失败且**标为不可重试**——URL 非法、pin 不符
    /// 这类问题重试一万次也不会自己好，指数退避只会把同一条错误刷满日志。
    #[tokio::test]
    async fn 传输构造失败被标为不可重试() {
        let outcome = run_once(|| Err(Failure::not_retryable("URL 非法"))).await;
        match outcome {
            Outcome::Failed { retryable, message } => {
                assert!(!retryable, "{message}");
                assert!(message.contains("URL 非法"));
            }
            other => panic!("应当失败，实得 {other:?}"),
        }
    }

    /// 一轮调和 panic **不能带走整个 daemon**——它必须被降级成这个数据集
    /// 这一轮的失败（I11 的进程内镜像）。这条如果不成立，一个数据集上的
    /// 边界情况会让用户其余所有数据集集体停止同步。
    #[tokio::test]
    async fn 一轮调和panic被降级为本轮失败而不是带走进程() {
        let outcome = run_once(|| panic!("模拟调和过程中的 panic")).await;
        match outcome {
            Outcome::Failed { retryable, message } => {
                assert!(!retryable);
                assert!(message.contains("异常终止"), "{message}");
            }
            other => panic!("应当被降级成失败，实得 {other:?}"),
        }
    }
}
