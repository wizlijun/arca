//! # arca-agentd
//!
//! 可选客户端 daemon（spec §3.1）：自动同步（周期对账 + 退避重试，M3a；
//! longpoll 与本地 watcher 见 M3b）与占位符投影供给（M3c/M3d）。
//!
//! **上层永远是下层的增强，不是依赖**——agentd 崩了，手动命令照常工作；
//! 占位符注册失败，退回全量物化。这条在代码上的落地方式是：agentd 是
//! `arca-cli` 的**消费者**，它调的 `sync_transport` 就是 `arca sync` 调的
//! 那一个。本 crate 里没有任何一行「该传什么」的判断，见 `syncer` 的模块文档。
//!
//! daemon 为每个数据集跑独立的调和回路、传输队列与退避状态
//! （多 hub 独立故障域，§4.3.2）。
//!
//! # 用法
//!
//! ```text
//! arca-agentd [--vault <路径>] [--interval <秒>] [--once]
//! ```
//!
//! `--once` 跑一轮就退出——给演练脚本与 CI 用，让「自动同步确实在工作」
//! 可以被断言，而不必去 sleep 一个不确定的时长再猜。

mod cursor;
mod hydration;
mod ipc;
mod lock;
mod projection;
mod syncer;
mod watcher;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use arca_cli::dataset::HubTarget;
use syncer::{Loop, Outcome};
use tokio::sync::watch;

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("无法启动 tokio 运行时：{e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(args))
}

#[derive(Debug)]
struct Args {
    vault: PathBuf,
    interval: Duration,
    once: bool,
}

impl Args {
    fn parse<I: Iterator<Item = String>>(mut it: I) -> Result<Self, String> {
        let mut vault = None;
        let mut interval = syncer::BASE_INTERVAL;
        let mut once = false;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--vault" => {
                    vault = Some(PathBuf::from(
                        it.next().ok_or("`--vault` 后面要跟一个路径")?,
                    ))
                }
                "--interval" => {
                    let raw = it.next().ok_or("`--interval` 后面要跟秒数")?;
                    let secs: u64 = raw
                        .parse()
                        .map_err(|_| format!("`--interval` 需要一个整数秒数，实得 {raw:?}"))?;
                    interval = Duration::from_secs(secs);
                }
                "--once" => once = true,
                "-h" | "--help" => {
                    return Err(
                        "arca-agentd [--vault <路径>] [--interval <秒>] [--once]".to_string()
                    )
                }
                other => return Err(format!("无法识别的参数 {other:?}")),
            }
        }
        Ok(Args {
            vault: match vault {
                Some(v) => v,
                None => std::env::current_dir().map_err(|e| format!("无法取得当前目录：{e}"))?,
            },
            interval,
            once,
        })
    }
}

async fn run(args: Args) -> ExitCode {
    // 1. 打开 vault 并**先拿单实例锁**——两个 agentd 同时对账会互相覆盖
    //    基线，那是比不同步更糟的状态。
    let vault = match arca_cli::vault::open(&args.vault) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}：无法打开 vault：{e}", args.vault.display());
            return ExitCode::from(2);
        }
    };
    let vault_root = vault.repo.root().to_path_buf();
    let _guard = match lock::acquire(&vault_root) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // 2. 解析出所有数据集。**一个数据集解析失败不影响其余**（I11）——
    //    这里就是最容易写成 `?` 的地方，写成 `?` 会让一个坏掉的 `.gitarca`
    //    条目导致整个 daemon 起不来，而那正是 M2d Task 3 在命令壳上抓过的形态。
    let mut loops = Vec::new();
    for entry in vault.registry.datasets() {
        match arca_cli::dataset::resolve(&vault_root, &entry.path, None) {
            Ok(r) => loops.push((Loop::new(r.normalized_path.clone(), r.hub_name.clone()), r)),
            Err(e) => eprintln!(
                "数据集 {}（hub={}）解析失败，本次不为它启动回路：{e}",
                entry.path, entry.hub
            ),
        }
    }
    if loops.is_empty() {
        eprintln!(
            "{}：没有任何可同步的数据集——agentd 没有事情可做，退出。\
             （用 `arca register` 注册一个数据集，或检查上面的解析失败。）",
            vault_root.display()
        );
        return ExitCode::from(2);
    }

    eprintln!(
        "arca-agentd 已启动：{} 个数据集，间隔 {} 秒，单实例锁 {}{}",
        loops.len(),
        args.interval.as_secs(),
        _guard.path().display(),
        if args.once {
            "（--once：跑一轮即退出）"
        } else {
            ""
        }
    );

    // 3. 停止信号。用 watch 而不是 broadcast：每个回路只关心「当前是不是该停了」
    //    这个**状态**，不关心信号历史，watch 的语义正好是状态而非事件。
    let (stop_tx, stop_rx) = watch::channel(false);
    let signals = tokio::spawn(watch_signals(stop_tx));

    // 4. 每个数据集一个 task。一个 task panic **不能**带走其余——`spawn` 的
    //    `JoinHandle` 把 panic 收进 `Err`，下面 join 时按数据集分别报告。
    let actor = default_actor();
    let mut handles = Vec::new();
    for (lp, resolved) in loops {
        let mut rx = stop_rx.clone();
        let actor = actor.clone();
        let interval = args.interval;
        let once = args.once;
        handles.push(tokio::spawn(async move {
            run_loop(lp, resolved, actor, interval, once, &mut rx).await
        }));
    }

    let mut failed = false;
    for h in handles {
        match h.await {
            Ok(ok) => failed |= !ok,
            Err(e) => {
                eprintln!("一个数据集的回路异常终止：{e}");
                failed = true;
            }
        }
    }
    signals.abort();

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// 一个数据集的完整回路。返回 `true` 表示「这个数据集没有留下需要人处理的问题」。
///
/// `--once` 下只跑一轮，返回这一轮是否成功——演练脚本据此断言「自动同步
/// 确实在工作」，不必 sleep 一个不确定的时长再猜。
async fn run_loop(
    mut lp: Loop,
    resolved: arca_cli::dataset::ResolvedDataset,
    actor: arca_cli::sync::SyncActor,
    interval: Duration,
    once: bool,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    // 增量游标：只对 http(s):// 有意义（`file://` 没有可挂起的对象，
    // 见 `LocalTransport::changes` 的说明）。
    let longpoll = matches!(resolved.target, HubTarget::Http { .. });

    // 本地 watcher（M3b）。**失败必须降级而不是中止**：某些平台/文件系统
    // 不支持事件监听，那时退回纯周期模式照常工作——watcher 是周期对账的
    // 增强，不是它的替代（spec §3.1 的分层降级关系）。
    let mut local_wakes = match watcher::spawn_forwarder(&resolved.dataset_dir) {
        Ok(rx) => Some(rx),
        Err(e) => {
            eprintln!(
                "{}：本地文件监听不可用（{e}），本数据集退回纯周期模式——                 本地改动仍会被发现，只是最多晚一个间隔。",
                lp.label
            );
            None
        }
    };
    let loaded = cursor::load(&resolved.dataset_dir);
    if !matches!(loaded, cursor::Loaded::Cursor(_)) && longpoll {
        // 读不懂的游标要留一句诊断——「第一次跑」和「上次写坏了」对排障
        // 的人是两件事（FORMAT.md §9.6）。
        eprintln!("{}：{loaded}", lp.label);
    }
    let mut since = loaded.as_cursor().cloned();

    let mut healthy;
    loop {
        let outcome = reconcile_once(&resolved, actor.clone()).await;
        match &outcome {
            Outcome::Ok(report) => {
                // Rule of Silence：没变化就不说话。agentd 是长期运行的进程，
                // 每 30 秒打一行「一切正常」会让日志变成没人看的噪音，而
                // 真正的异常就淹没在里面了。
                let 动了 = report.uploaded.len()
                    + report.downloaded.len()
                    + report.renamed.len()
                    + report.deleted_local.len()
                    + report.conflicts.len();
                if 动了 > 0 {
                    eprintln!(
                        "{}：上传 {} · 下载 {} · 改名 {} · 本地删除 {} · 冲突 {}",
                        lp.label,
                        report.uploaded.len(),
                        report.downloaded.len(),
                        report.renamed.len(),
                        report.deleted_local.len(),
                        report.conflicts.len()
                    );
                }
                healthy = true;
            }
            Outcome::Failed { message, retryable } => {
                healthy = false;
                eprintln!(
                    "{}（hub={}）：{message}{}",
                    lp.label,
                    lp.hub_name,
                    if *retryable {
                        format!("——将在 {} 秒后重试", lp.next_delay().as_secs())
                    } else {
                        "——这不是能靠重试解决的问题，需要人看一眼".to_string()
                    }
                );
            }
        }
        lp.record(&outcome);

        if once {
            // `--once` 也要把游标推进并落盘——否则每次脚本化调用（演练、CI、
            // cron）都从头做一次全量对账，而**游标持久化这条路径也就永远
            // 不会被这些流程走到**：一个只在长驻模式下才生效的持久化，
            // 等于一个没被日常验证覆盖的持久化。
            if longpoll && healthy {
                if let Some(c) = probe_changes(&resolved, since.clone(), Duration::ZERO).await {
                    persist(&resolved, &lp, c.as_ref());
                }
            }
            return healthy;
        }

        let delay = if lp.consecutive_failures() == 0 {
            interval
        } else {
            lp.next_delay()
        };

        // 失败之后走退避睡眠，**既不进 longpoll 也不接受 watcher 唤醒**——
        // 一个连不上的 hub，本地每存一次盘就重试一次，等于把退避架空了。
        if lp.consecutive_failures() > 0 {
            if !syncer::wait_next(delay, shutdown).await {
                return healthy;
            }
            continue;
        }

        // `file://`：没有 longpoll 可等，等本地 watcher 或者等到点。
        if !longpoll {
            match wait_local_or_tick(local_wakes.as_mut(), delay, shutdown, &lp).await {
                syncer::Wakeup::Stop => return healthy,
                _ => continue,
            }
        }

        // 这一轮调和成功了。**先把游标快进过我们自己刚写进 journal 的那些
        // 事件**（`wait=0`，立刻返回），否则下一次 longpoll 会被自己的写入
        // 立刻唤醒，白跑一轮空调和。
        if let Some(c) = probe_changes(&resolved, since.clone(), Duration::ZERO).await {
            since = c;
            persist(&resolved, &lp, since.as_ref());
        }

        // 挂起等待对面的动静——同时盯着本地 watcher。
        let probe = make_probe(&resolved);
        let (wakeup, next) = {
            let remote = syncer::wait_for_change(since.clone(), delay, probe, shutdown);
            match local_wakes.as_mut() {
                None => remote.await,
                Some(rx) => {
                    tokio::select! {
                        r = remote => r,
                        w = rx.recv() => {
                            报告本地唤醒(&lp, w.as_ref());
                            // 本地有动静：立刻去调和。游标保持原值——本地改动
                            // 不会推进远端 journal 的游标，硬要在这里动它就是
                            // 在编造一个服务端没说过的位置。
                            (syncer::Wakeup::Reconcile, since.clone())
                        }
                    }
                }
            }
        };
        match wakeup {
            syncer::Wakeup::Stop => return healthy,
            syncer::Wakeup::ResetAndReconcile => {
                // 服务端说这个游标没法续接。丢掉它做一次全量对账——**绝不
                // 当作「从头开始」静默重下全库**，那正是 I5 要挡住的东西。
                eprintln!(
                    "{}（hub={}）：hub 报告增量游标已失效，本轮改做一次全量对账。",
                    lp.label, lp.hub_name
                );
                if let Err(e) = cursor::clear(&resolved.dataset_dir) {
                    eprintln!("{}：清除失效游标失败：{e}", lp.label);
                }
                since = next;
                persist(&resolved, &lp, since.as_ref());
            }
            syncer::Wakeup::Reconcile => {
                if let Some(c) = next {
                    since = Some(c);
                    persist(&resolved, &lp, since.as_ref());
                }
            }
        }
    }
}

/// `file://` 的等待：本地 watcher 唤醒、等到点、或者停止信号。
async fn wait_local_or_tick(
    local: Option<&mut tokio::sync::mpsc::Receiver<watcher::Wake>>,
    delay: Duration,
    shutdown: &mut watch::Receiver<bool>,
    lp: &Loop,
) -> syncer::Wakeup {
    let Some(rx) = local else {
        return if syncer::wait_next(delay, shutdown).await {
            syncer::Wakeup::Reconcile
        } else {
            syncer::Wakeup::Stop
        };
    };
    tokio::select! {
        w = rx.recv() => {
            报告本地唤醒(lp, w.as_ref());
            syncer::Wakeup::Reconcile
        }
        // 周期地基仍在：即使 watcher 一声不吭，也要按时全量对账一次
        // （spec §5.2 三重保险的第三层）。
        go_on = syncer::wait_next(delay, shutdown) => {
            if go_on { syncer::Wakeup::Reconcile } else { syncer::Wakeup::Stop }
        }
    }
}

/// 本地唤醒的日志。**溢出要说出来**——它意味着有事件被内核丢掉了，
/// 而这正是三重保险第二层「溢出即全扫」在起作用的时刻，值得留痕。
fn 报告本地唤醒(lp: &Loop, wake: Option<&watcher::Wake>) {
    if let Some(watcher::Wake::Overflow) = wake {
        eprintln!(
            "{}：文件系统事件队列溢出（有事件被丢弃）——按三重保险第二层的\
             约定立刻做一次全量对账。",
            lp.label
        );
    }
}

/// 把游标落盘。失败**不中断回路**——丢了它的后果只是下次多做一次全量对账
/// （FORMAT.md §9.6），为它停掉自动同步不划算。
fn persist(
    resolved: &arca_cli::dataset::ResolvedDataset,
    lp: &Loop,
    cursor: Option<&arca_format::journal::Cursor>,
) {
    let Some(c) = cursor else { return };
    if let Err(e) = cursor::save(&resolved.dataset_dir, c) {
        eprintln!(
            "{}：增量游标写入失败（不影响本轮同步，下次会多做一次全量对账）：{e}",
            lp.label
        );
    }
}

/// 造一个「探测变更流」的闭包，形状与 `reconcile_once` 里造传输的那一套一致。
fn make_probe(
    resolved: &arca_cli::dataset::ResolvedDataset,
) -> impl FnOnce(
    Option<arca_format::journal::Cursor>,
    Duration,
) -> Result<arca_cli::transport::ChangesOutcome, String>
       + Send
       + 'static {
    let target = resolved.target.clone();
    let dataset_id = resolved.cfg.dataset_id.clone();
    move |since, wait| {
        let HubTarget::Http { base_url, tls_pin } = &target else {
            // `file://` 不走这条路（`longpoll` 为假），到这里说明调用点写错了。
            return Err("file:// hub 不支持 longpoll 唤醒".to_string());
        };
        let transport =
            build_http(base_url, &dataset_id, tls_pin.as_deref()).map_err(|f| f.message)?;
        use arca_cli::transport::Transport;
        transport
            .changes(since.as_ref(), wait, 1000)
            .map_err(|e| e.to_string())
    }
}

/// 一次不挂起的探测，只为把游标快进到当前位置。返回 `None` 表示这次探测
/// 没能给出可用的游标（网络抖动等）——保持原值，下一轮再说。
async fn probe_changes(
    resolved: &arca_cli::dataset::ResolvedDataset,
    since: Option<arca_format::journal::Cursor>,
    wait: Duration,
) -> Option<Option<arca_format::journal::Cursor>> {
    let probe = make_probe(resolved);
    match tokio::task::spawn_blocking(move || probe(since, wait)).await {
        Ok(Ok(arca_cli::transport::ChangesOutcome::Events { cursor, .. })) => Some(cursor),
        // `ResetRequired` 交给下面真正的 longpoll 那一轮去处理——在这里
        // 顺手清游标会让「为什么重下全库」少一条日志。
        _ => None,
    }
}

/// 按 hub 类型造传输并跑一轮。/// 按 hub 类型造传输并跑一轮。两种传输走**同一个** `run_once`——差别只在
/// 怎么造传输，不在怎么调和。
async fn reconcile_once(
    resolved: &arca_cli::dataset::ResolvedDataset,
    actor: arca_cli::sync::SyncActor,
) -> Outcome {
    let dataset_dir = resolved.dataset_dir.clone();
    let dataset_id = resolved.cfg.dataset_id.clone();
    match &resolved.target {
        HubTarget::Local(path) => {
            let path = path.clone();
            syncer::run_once(move || {
                // `StorageRoot::open` 每轮重做一次不是浪费：它就是 I11 的
                // 检查点——外置盘被拔掉之后，正是这里把「数据集离线」与
                // 「数据集空了」区分开。缓存住它等于把这个检查关掉。
                let root = arca_store::root::StorageRoot::open(&path, Some(&dataset_id))
                    .map_err(|e| syncer::Failure::not_retryable(e.to_string()))?;
                let transport = arca_cli::transport::local::LocalTransport::new(&root);
                syncer::do_sync(&dataset_dir, &transport, &actor)
            })
            .await
        }
        HubTarget::Http { base_url, tls_pin } => {
            let base = base_url.clone();
            let pin = tls_pin.clone();
            syncer::run_once(move || {
                let transport = build_http(&base, &dataset_id, pin.as_deref())?;
                syncer::do_sync(&dataset_dir, &transport, &actor)
            })
            .await
        }
    }
}

/// 与 `arca-cli` 命令壳里的同名 helper 同一形状：明文 `http://` 直接构造，
/// `https://` 先经 `tls::decide_for_url` 拿到信任配置（系统根 / pin 过的
/// 那一张 / 拒绝），再 `with_trust`。**指纹不符或未 pin 的自签名在这里就被
/// 挡住**，绝不 TOFU（spec §9，M2e Task 4）。
fn build_http(
    base_url: &str,
    dataset_id: &str,
    tls_pin: Option<&str>,
) -> Result<arca_cli::transport::http::HttpTransport, syncer::Failure> {
    use arca_cli::transport::http::HttpTransport;
    if !base_url.starts_with("https://") {
        return Ok(HttpTransport::new(base_url, dataset_id, None));
    }
    let trust = arca_cli::tls::decide_for_url(base_url, tls_pin)
        .map_err(|e| syncer::Failure::not_retryable(e.to_string()))?;
    Ok(HttpTransport::with_trust(
        base_url, dataset_id, None, &trust,
    ))
}

/// 等 `SIGTERM`/`SIGINT`，收到就把停止状态置真。
///
/// **优雅停止的含义是「不在一轮调和中途被杀」**：信号只是让回路在当前这轮
/// 跑完之后不再开始下一轮。中途硬停正是 M2c 抓到的「批量提交中途 kill -9」
/// 那类中间态的来源，而我们自己有能力不制造它。
async fn watch_signals(stop: watch::Sender<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("无法注册 SIGTERM 处理：{e}");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    eprintln!("收到停止信号：当前这一轮跑完之后退出（不会中途打断调和）。");
    let _ = stop.send(true);
}

fn default_actor() -> arca_cli::sync::SyncActor {
    arca_format::model::Actor {
        account: std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_default(),
        device: hostname(),
        session: String::new(),
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn 默认间隔是基础间隔且不是once() {
        let a = parse(&["--vault", "/tmp"]).unwrap();
        assert_eq!(a.interval, syncer::BASE_INTERVAL);
        assert!(!a.once);
        assert_eq!(a.vault, PathBuf::from("/tmp"));
    }

    /// I5：绝不「看不懂就用默认值」继续——那会让用户以为自己的配置生效了。
    #[test]
    fn 非法间隔被明确拒绝而不是当成默认值() {
        let msg = parse(&["--interval", "很快"]).unwrap_err();
        assert!(msg.contains("整数秒数"), "{msg}");
    }

    #[test]
    fn 无法识别的参数被拒绝() {
        let msg = parse(&["--turbo"]).unwrap_err();
        assert!(msg.contains("--turbo"), "{msg}");
    }

    #[test]
    fn 缺参数值时报错而不是panic() {
        assert!(parse(&["--vault"]).is_err());
        assert!(parse(&["--interval"]).is_err());
    }
}
