//! porcelain 命令（plumbing 之上的薄壳，spec §3.2、§12.3 M1–M2）：
//!
//! | 命令 | 语义 |
//! | --- | --- |
//! | `setup` | 一次性引导：读 `.gitarca` → 建绑定 → 选角色（对齐 `git lfs install` 的角色） |
//! | `adopt` | 就地纳管既有附件：算哈希、上传、写 `.gitignore` 块，**文件原地不动**（只阻止未来膨胀，不瘦身历史——输出里必须讲清楚） |
//! | `add` / `register` | 新数据集声明 / 孤儿数据集显式登记 |
//! | `status` | 比对本地与 hub，不动数据；按数据集分别报告健康度与 server 副本数 |
//! | `fetch` / `pull` / `push` / `sync` | 与 git 动词语义对齐；file:// 或 https:// |
//! | `verify` | fixity 巡检（BLAKE3 重算对账），机器可读报告 |
//! | `history` / `restore` | 版本链查看 / 保留期内一条命令找回 |
//! | `gc` | 显式销毁（I3）：`--dry-run` 先出清单 |
//! | `bundle` | 自包含归档交付（含 `--verify` 离线校验，§4.4.3） |
//! | `doctor` | 一致性断言：`.gitignore` 反选块（`git check-ignore` 实测）、孤儿数据集、缺失文件统计 |
//! | `rebuild` | 投影删掉重建 + adopt 认领（I9） |
//! | `pin` / `unpin` | 驻留策略（M3） |
//! | `import` | Dropbox / Google Drive / LFS 迁入，厂商校验和验证 + 审计报告（M5） |
//! | `publish-map` / `export` | 发布（M5，委托 arca-publish） |
//!
//! M1d Task 4/5/6 落地了 `init`/`register`/`adopt`/`sync` 四个命令壳；
//! 本文件另加 Task 7 的 `status`/`verify`/`doctor`（plumbing 的 `ls`/`cat`/
//! `resolve`/`state dump` 落在 `commands/plumbing.rs`）。
//!
//! 退出码约定（spec §3.2，与 `arca fsck` 已定的一致）：0 = 干净，
//! 1 = 有问题/有未完成的工作，2 = 身份不明（存储根挂载失败/身份不符，I11）。

use arca_cli::adopt::{self, AdoptOptions};
use arca_cli::clock;
use arca_cli::dataset::{self, HubTarget};
use arca_cli::doctor;
use arca_cli::gc;
use arca_cli::init::{self, HookOutcome};
use arca_cli::local_trash;
use arca_cli::register::{self, RegisterOptions};
use arca_cli::role;
use arca_cli::status as status_lib;
use arca_cli::sync::{self as sync_lib, SyncActor, SyncError};
use arca_cli::trace_sink;
use arca_cli::transport::http::HttpTransport;
use arca_cli::transport::TransportError;
use arca_cli::trash;
use arca_cli::vault;
use arca_format::trace::{NullSink, RingSink};
use arca_store::root::StorageRoot;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// 归因上下文（I8）：从环境尽力推断，推不出来就留空——`Actor` 的字段都
/// 允许为空字符串（`arca_format::model::Actor` 全部字段 `#[serde(default)]`），
/// 不是必须有值才能同步。`session` 用每次进程调用各自生成的随机段：一次
/// `arca` 调用就是一次会话，语义上与"进程生命周期"对齐。
fn default_actor() -> SyncActor {
    let account = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let device = Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    arca_format::model::Actor {
        account,
        device,
        session: arca_cli::ids::random_hex32(),
    }
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// `arca init`。
pub fn init_cmd(path: Option<PathBuf>, no_hook: bool) -> ExitCode {
    let start = path.unwrap_or_else(cwd);
    match init::init(&start, !no_hook) {
        Ok(outcome) => {
            if outcome.stopped() {
                for issue in &outcome.issues {
                    eprintln!("{issue}");
                }
                eprintln!(
                    "`arca init` 已停止：先处理以上 {} 个问题",
                    outcome.issues.len()
                );
                return ExitCode::from(1);
            }
            if let Some(HookOutcome::Refused { existing_path }) = &outcome.hook {
                eprintln!(
                    "已存在非 arca 安装的 pre-push 钩子（{}），未覆盖——如需接管请手工处理后重跑",
                    existing_path.display()
                );
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

/// `arca register <path> --hub <name> [--hub-instance-id <id>] [--hub-url <url>] [--root <path>] [--dataset-id <id>]`。
pub fn register_cmd(
    path: &str,
    hub_name: &str,
    hub_instance_id: Option<&str>,
    hub_url: Option<&str>,
    root: Option<&Path>,
    dataset_id: Option<&str>,
) -> ExitCode {
    let opts = RegisterOptions {
        path,
        hub_name,
        hub_instance_id,
        hub_url,
        root_hint: root,
        dataset_id,
    };
    match register::register(&cwd(), opts) {
        Ok(outcome) => {
            println!(
                "{path} dataset_id={} hub={hub_name} hub_instance_id={}",
                outcome.dataset_id, outcome.hub_instance_id
            );
            ExitCode::SUCCESS
        }
        Err(register::RegisterError::Issues(issues)) => {
            for issue in &issues {
                eprintln!("{issue}");
            }
            eprintln!("`arca register` 已停止：先处理以上 {} 个问题", issues.len());
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// `arca role <path> [--set server|client] [--root <path>]`（M2d Task 1，
/// FORMAT.md §9.5）：查看或设置一个数据集在**本机**的存储角色。只读写
/// `<dataset>/.arca/client/role.toml`——不打开存储根、不联网，纯本地决策
/// （spec §4.7）；`--root` 参数只是为了与其它命令的调用形状保持一致，
/// `dataset::resolve` 解析出的 `dataset_dir` 才是本命令唯一关心的东西。
///
/// 不带 `--set`：把当前角色打印到 stdout（数据，可脚本消费）——文件缺失时
/// 打印的是默认角色 `client`，不是空输出（`role::read` 的语义，见其文档）。
/// 带 `--set`：写入新角色；设为 `server` 时额外在 stderr 提示这意味着什么
/// （M2d Task 1 brief 明确要求：这是一个"永不主动释放空间"的承诺，用户
/// 应该在设置的那一刻就被提醒，而不是要等到第一次删除传播时才发现）。
pub fn role_cmd(path: &str, set: Option<&str>, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    match set {
        None => match role::read(&resolved.dataset_dir) {
            Ok(current) => {
                println!("{}", current.as_str());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(1)
            }
        },
        Some(value) => {
            let new_role = match role::Role::parse(value) {
                Some(r) => r,
                None => {
                    eprintln!("--set 只接受 server 或 client，实得 {value:?}");
                    return ExitCode::from(1);
                }
            };
            match role::write(&resolved.dataset_dir, new_role) {
                Ok(()) => {
                    if matches!(new_role, role::Role::Server) {
                        eprintln!(
                            "{path} 已设为 server 角色：本设备承诺为这个数据集永久保留一份完整\
                             副本——远端删除到达、过闸门之后，本地副本只会移入本地回收站\
                             （.arca/client/trash/），不释放空间。物理销毁只经显式的\
                             `arca gc {path} --local --yes`（默认只出清单，绝不自动触发，I3）；\
                             要找回用 `arca restore {path} <文件> --local`，要查看占用用\
                             `arca doctor`。"
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

/// trace 只在失败时落盘，`ARCA_TRACE_EVENT` 可强制——见 `trace_sink` 模块
/// 文档。落点是全机唯一的 `<state>/trace/`（`trace_sink::state_dir`），
/// 与具体数据集是否解析成功无关——即便"数据集还没登记好"这类失败也有地方
/// 落盘（这正是全机位置相对数据集级别位置的优势，见模块文档）。宿主机连
/// home/profile 目录都解析不出来（`state_dir` 返回 `None`，极罕见的精简
/// 容器环境）时放弃落盘，不报错（trace 是诊断产物，绝不能反过来变成命令
/// 失败的原因）。
fn flush_trace_if_needed(sid: &arca_format::trace::Sid, sink: &mut RingSink, succeeded: bool) {
    if !trace_sink::should_flush(succeeded) {
        return;
    }
    let Some(dir) = trace_sink::state_dir() else {
        eprintln!("无法解析全机 trace 目录（home/profile 目录不可用），本次跳过 trace 落盘");
        return;
    };
    match trace_sink::flush(&dir, sid, sink, trace_sink::DEFAULT_KEEP) {
        Ok(outcome) => eprintln!(
            "trace 已落盘：{}（{} 条事件，其中 {} 条因环形缓冲溢出被丢弃）",
            outcome.path.display(),
            outcome.events,
            outcome.dropped
        ),
        Err(e) => eprintln!("{e}"),
    }
}

/// `arca adopt <path> [--root <path>] [--create-root]`。
pub fn adopt_cmd(path: &str, root: Option<&Path>, allow_create_root: bool) -> ExitCode {
    let sid = trace_sink::resolve_sid();
    let mut sink = RingSink::default();

    let opts = AdoptOptions {
        path,
        root_override: root,
        actor: default_actor(),
        allow_create_root,
    };
    let (code, succeeded) = match adopt::adopt(&cwd(), opts, &mut sink) {
        Ok(outcome) => {
            for p in &outcome.report.uploaded {
                println!("upload\t{p}");
            }
            for p in &outcome.report.adopted {
                println!("adopt\t{p}");
            }
            for p in &outcome.report.tombstone_submitted {
                println!("tombstone\t{p}");
            }
            for p in &outcome.untracked_from_git {
                eprintln!("从 git index 逐出（工作树文件未改动）：{p}");
            }
            // I5：能力缺失/需要人工介入的情况绝不静默。
            for (p, failure) in &outcome.report.delete_blocked {
                eprintln!("删除闸门拦下，未移除本地副本：{p}：{failure}");
            }
            for p in &outcome.report.tombstone_pending {
                eprintln!("本该提交为删除（tombstone）但本轮未能执行：{p}");
            }
            for p in &outcome.report.conflicts {
                eprintln!("结构化冲突，未动数据：{p}");
            }
            for p in &outcome.report.needs_human {
                eprintln!("状态模糊，需要人工介入：{p}");
            }
            for (p, reason) in &outcome.report.scan_rejected {
                eprintln!("扫描阶段被拒绝（{}）：{p}", reason.as_str());
            }
            // 诚实注解（spec §12.3）：无论本次是否有实际变化都要讲清楚。
            eprintln!("{}", adopt::HISTORY_NOTE);

            if outcome.report.is_clean() {
                (ExitCode::SUCCESS, true)
            } else {
                (ExitCode::from(1), false)
            }
        }
        Err(adopt::AdoptError::Mount(_)) => {
            eprintln!("存储根身份不明，已按 I11 拒绝——数据集离线");
            (ExitCode::from(2), false)
        }
        Err(e @ adopt::AdoptError::RootMissingButAdopted { .. }) => {
            eprintln!("{e}");
            (ExitCode::from(2), false)
        }
        Err(e) => {
            eprintln!("{e}");
            (ExitCode::from(1), false)
        }
    };

    flush_trace_if_needed(&sid, &mut sink, succeeded);
    code
}

/// 打印一次 [`sync_lib::SyncReport`]——`arca sync` 与两种 hub target 共用
/// 同一份措辞（M2c Task 5：`file://`/`http://` 走的是不同的
/// `sync_lib::sync`/`sync_transport` 引擎，但报告的呈现方式必须一致，不能
/// 让用户从输出上感觉出"这是两个不同的命令"）。返回 `report.is_clean()`。
fn print_sync_report(report: &sync_lib::SyncReport) -> bool {
    // 评审 Important #2：基线被重置（缺失/损坏，本轮是全量对账）是"为什么
    // 这次结果长这样"的关键线索——`status_cmd` 已经打印这条提示，`sync_cmd`
    // 此前算出了同一个字段却从不读它，混合场景下用户会看到一堆
    // upload/adopt 或一个"结构化冲突"却不知道起因就是基线被重建。
    if report.baseline_reset {
        eprintln!("基线已重建（此前缺失或损坏）——本轮是一次全量对账");
    }
    for p in &report.uploaded {
        println!("upload\t{p}");
    }
    for p in &report.downloaded {
        println!("download\t{p}");
    }
    for p in &report.adopted {
        println!("adopt\t{p}");
    }
    // `renamed`：M2c Task 5 新增——只有 `sync_transport`（`http://`，以及
    // 未来任何走 `Transport` 的路径）才会填充这个桶，`file://` 的旧
    // `sync()` 引擎本切片不变，恒空（`SyncReport::default()`）。
    for (from, to) in &report.renamed {
        println!("rename\t{from}\t{to}");
    }
    for p in &report.deleted_local {
        println!("delete-local\t{p}");
    }
    // M2d Task 2：`server` 角色下 `DeleteLocal` 不移除本地副本，而是移进
    // 工作区侧本地回收站——机器可读的 tag 与 `delete-local` 刻意不同
    // （`delete-local-trash`），供脚本区分；额外补一条人类可读的 stderr
    // 说明（brief 原话要求区分文案：client「已移除本地副本」/ server
    // 「已移入本地回收站（server 角色永不释放空间）」），只在这个桶非空时
    // 打一次，不逐路径重复。
    for p in &report.deleted_to_local_trash {
        println!("delete-local-trash\t{p}");
    }
    if !report.deleted_to_local_trash.is_empty() {
        eprintln!(
            "以上 {} 个路径已移入本地回收站（.arca/client/trash/），不是移除——\
             本设备对这个数据集是 server 角色，永不主动释放空间；找回可直接读取\
             该目录下对应的 .data/.meta 文件（`arca role` 查看/切换角色）",
            report.deleted_to_local_trash.len()
        );
    }
    for p in &report.tombstone_submitted {
        println!("tombstone\t{p}");
    }
    for (p, failure) in &report.delete_blocked {
        eprintln!("删除闸门拦下，未移除本地副本：{p}：{failure}");
    }
    for p in &report.tombstone_pending {
        eprintln!("本该提交为删除（tombstone）但本轮未能执行：{p}");
    }
    for p in &report.conflicts {
        eprintln!("结构化冲突，未动数据：{p}");
    }
    for p in &report.needs_human {
        eprintln!("状态模糊，需要人工介入：{p}");
    }
    for (p, reason) in &report.scan_rejected {
        eprintln!("扫描阶段被拒绝（{}）：{p}", reason.as_str());
    }
    report.is_clean()
}

/// `SyncError` → 退出码——**I11**：`Transport::Offline`（数据集离线）与
/// `StorageRoot::open` 打不开是同一严重性，走退出码 2；其余（含
/// `Network`/`Protocol`）是命令本身的失败，退出码 1——命令壳这一层不区分
/// `retryable`/`bug`（那是 agent/更上层调用方看 `TransportError::class()`
/// 该做的事，见其文档），只保证"离线"这一种情形不会被和"随便什么问题"
/// 混在同一个退出码里，与 `status_cmd`/`verify_cmd` 对 `StorageRoot::open`
/// 失败的既有处置保持一致的信号强度。
/// `SyncError` → 退出码严重度（0 干净不可能出现在这里/1 有问题/2 身份不明）
/// ——返回 `u8` 而不是直接返回 `ExitCode`：`std::process::ExitCode` 不透明，
/// 取不出内部值，没法在 [`sync_all`] 里跟其它数据集的结果比大小取"最严重
/// 的那个"。所有单数据集内部逻辑改用 `u8`，只在最外层命令壳边界转一次
/// `ExitCode`（M2d Task 3）。
fn sync_error_exit_level(e: &SyncError) -> u8 {
    match e {
        SyncError::Transport(TransportError::Offline { .. }) => 2,
        _ => 1,
    }
}

/// `arca sync <path> [--root <path>]` 单个数据集的核心逻辑——对一个已
/// `arca register`/`arca adopt` 过的数据集再跑一轮调和闭环。**不引导全新
/// 存储根**（那是 `adopt` 的职责）：存储根打不开/数据集离线一律按 I11
/// 视为身份不明。返回严重度而不是 `ExitCode`，理由见
/// [`sync_error_exit_level`]；[`sync_cmd`]（单数据集）与 [`sync_all`]
/// （M2d Task 3，全部数据集）都调用这一个函数，不重复实现。
///
/// # M2c Task 5：`file://` 与 `http://` 分流
///
/// `dataset::resolve` 产出的 [`HubTarget`] 决定走哪条引擎：`Local` 沿用
/// M1d 起就有的 `sync_lib::sync`（`StorageRoot` + `Batch`/`AppendBatch` 批量
/// 收口，性能设计只对本地磁盘有意义）；`Http` 走本切片新增的
/// `sync_lib::sync_transport` + [`HttpTransport`]——`Arca-Session` 头带的是
/// 这次命令解析出的 `sid`（M2c Task 4 sid 闭环：客户端 trace 里的 sid 与
/// 服务端 journal 里的 `actor.session` 由此串起来）。两条路径共享
/// [`print_sync_report`]，用户看到的输出格式不因 hub 类型而分裂。
fn sync_one(path: &str, root: Option<&Path>) -> u8 {
    let resolved = match dataset::resolve(&cwd(), path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    // sid/sink 建在挂载检查/连接之前——mount.check 是"全项目最危险的判断"
    // （见 `arca_store::root` 模块文档），失败时同样要能落盘诊断，不能只有
    // 后面 `sync_lib::sync`/`sync_transport` 内部的决策轨迹被记录下来。
    let sid = trace_sink::resolve_sid();
    let mut sink = RingSink::default();

    let result = match &resolved.target {
        HubTarget::Local(root_path) => {
            let storage_root = match arca_store::root::StorageRoot::open_traced(
                root_path,
                Some(&resolved.cfg.dataset_id),
                0,
                &mut sink,
            ) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("数据集 {path}（hub={}）离线：{e}", resolved.hub_name);
                    flush_trace_if_needed(&sid, &mut sink, false);
                    return 2;
                }
            };
            sync_lib::sync(
                &resolved.dataset_dir,
                &storage_root,
                &default_actor(),
                &mut sink,
            )
        }
        HubTarget::Http { base_url } => {
            let transport =
                HttpTransport::new(base_url, &resolved.cfg.dataset_id, Some(sid.clone()));
            sync_lib::sync_transport(
                &resolved.dataset_dir,
                &transport,
                &default_actor(),
                &mut sink,
            )
        }
    };

    let (level, succeeded) = match result {
        Ok(report) => {
            let clean = print_sync_report(&report);
            if clean {
                (0, true)
            } else {
                (1, false)
            }
        }
        Err(e) => {
            // 评审 Minor #3：此前只有 `Offline` 点名数据集与 hub，其它
            // `Transport` 失败（例如 http:// hub 进程根本没起来，连接被拒绝，
            // 分类为 `TransportError::Network`）打出的是一句光秃秃的
            // "网络故障：Connection refused"，没有数据集名也没有 hub 名——
            // `sync_all` 靠 `== path ==` 表头兜底，但单数据集调用
            // （`arca sync <path>`）没有这层兜底，用户拿到的诊断无法归因是
            // 哪个数据集出的问题。`Transport` 这一类失败本就都与"这个数据集
            // 绑定的 hub 打交道"相关，统一带上归属；只有 `Offline` 才额外
            // 加"离线"这个更具体的判词（I11 语义，其它 `Transport` 变体不是
            // "离线"，不能张冠李戴）。
            match &e {
                SyncError::Transport(TransportError::Offline { .. }) => {
                    eprintln!("数据集 {path}（hub={}）离线：{e}", resolved.hub_name);
                }
                SyncError::Transport(_) => {
                    eprintln!("数据集 {path}（hub={}）：{e}", resolved.hub_name);
                }
                _ => {
                    eprintln!("{e}");
                }
            }
            (sync_error_exit_level(&e), false)
        }
    };

    flush_trace_if_needed(&sid, &mut sink, succeeded);
    level
}

/// `arca sync [<path>] [--root <path>]`：带路径同步单个数据集；**不带路径
/// 同步 vault 里全部已登记数据集**（M2d Task 3，spec §4.3.2）。
pub fn sync_cmd(path: Option<&str>, root: Option<&Path>) -> ExitCode {
    match path {
        Some(p) => ExitCode::from(sync_one(p, root)),
        None => sync_all(root),
    }
}

/// `arca sync`（不带路径）：对 vault 里全部已登记数据集各跑一轮同步。
///
/// spec §4.3.2：「daemon 为每个数据集维护独立的绑定、独立的 journal 游标、
/// 独立的传输队列与退避状态。一个 hub 不可达时，只有它承载的数据集进入
/// 离线态（I11），其余数据集完全不受影响。」——这里是这条纪律在**客户端**
/// 手动同步下的体现（arcad 侧的独立故障域已在 M2b 验证）。
///
/// **循环体绝不能用 `?`/`return` 提前退出**——那是这类"多个独立单元、一个
/// 失败不该拖累其它"场景最容易写错的形态（M1b 在 `into_result` 上踩过同构
/// 的问题：一个冲突文件中止了整轮 sweep）。每个数据集的结果都要被
/// [`sync_one`] 完整跑完并捕获，循环体本身不包含任何可能提前退出的
/// `?`/`return`；最终退出码取全体数据集里最严重的那个（0 < 1 < 2），
/// 不是"第一个失败就代表全部"。
///
/// # 对 Rule of Silence 的一处刻意收窄
///
/// `sync_one`（单数据集）继续 100% 遵守 Rule of Silence——`arca sync <path>`
/// 全同步时不打印任何东西，这条路径完全不变。这里（`arca sync` 不带路径、
/// 真的有 2+ 个数据集）额外给每个数据集打一行 `== path ==` 头，哪怕那个
/// 数据集本身干净：当"一个单元"变成"N 个独立单元各自成败"时，完全沉默会
/// 制造新的歧义——用户分不清"扫过了 3 个数据集且都干净"与"注册表是空的、
/// 根本没扫到东西"，这正是 `git submodule foreach` 类工具即便子模块干净也
/// 打印 `Entering '<name>'` 的同一个理由。只有单数据集恰好等于 1 个时不打
/// （`paths.len() > 1`），保持与"单数据集命令"完全一致的观感。
fn sync_all(root_override: Option<&Path>) -> ExitCode {
    if root_override.is_some() {
        eprintln!(
            "--root 只在指定单个数据集路径时生效——全量同步（不带路径）请对需要覆盖存储根的\
             数据集单独运行 `arca sync <path> --root <path>`"
        );
        return ExitCode::from(1);
    }

    let vault = match vault::open(&cwd()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    let paths: Vec<String> = vault
        .registry
        .datasets()
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    if paths.is_empty() {
        // Rule of Silence：还没有任何已登记的数据集，没什么可同步的。
        return ExitCode::SUCCESS;
    }

    let mut worst: u8 = 0;
    for path in &paths {
        if paths.len() > 1 {
            eprintln!("== {path} ==");
        }
        // 关键纪律（见本函数文档）：这里只累积严重度，绝不因为某个数据集
        // 的结果就跳过或中止其余数据集的循环。
        let level = sync_one(path, None);
        worst = worst.max(level);
    }
    ExitCode::from(worst)
}

/// `arca status <path> [--root <path>]` 单个数据集的核心逻辑（M1d Task 7）：
/// 跑扫描与调和但**不执行**——Rule of Silence，全同步时完全安静、退出码 0；
/// 有待办（含结构化问题）时把分类结果打到 stderr（诊断，不是数据）、
/// 退出码 1；数据集离线（I11）退出码 2，且明确点出是哪个 hub（M2d Task 3）。
/// 返回严重度而不是 `ExitCode`，理由与 [`sync_error_exit_level`] 一致：
/// [`status_cmd`]（单数据集）与 [`status_all`]（M2d Task 3，全部数据集）
/// 都调用这一个函数。
fn status_one(path: &str, root: Option<&Path>) -> u8 {
    let resolved = match dataset::resolve(&cwd(), path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // 修复 I2：`known_server_copies` 只读 `role::read(dataset_root)`，从不碰
    // `StorageRoot`，因此这道副本告警不需要等到 `local_root()`/
    // `StorageRoot::open` 都通过才跑。提到这两道 gate 之前，`http://` 绑定
    // 的数据集（M2b/M2c 的主线配置，此前 `local_root()` 直接 bail 导致告警
    // 永远走不到）与卷离线的数据集（此前同样被跳过）都能收到 spec §4.5
    // 承诺的告警，不再是 file:// 且在线时才有的特权。
    //
    // 修复 I1：只打印，不再把返回值折进 `level`——理由见
    // `report_replica_warning_if_any` 的文档：这条告警不是"命令本身失败"，
    // 也不是"有待办"，混进退出码会让一个刚 `adopt`、完全同步的默认角色
    // 数据集的 `arca status` 也退出非零，破坏 PROTOCOL.md 记载的三态语义，
    // 还会通过 `status_all`/`sync_all` 的取最大值传染到整个 vault。
    report_replica_warning_if_any(path, &resolved.dataset_dir);

    // M2e Task 3：两种 hub 类型都走同一段 `status_lib` 判断（M2d 评审：
    // 「arcad 是 M2 的主线，而主健康检查命令对主 hub 类型不工作」）。
    // `file://` 仍先按已知身份 `StorageRoot::open` 一次（I11）；`http://`
    // 的离线由 `TransportError::Offline`（503）表达，翻译成同一个退出码 2、
    // 同一句措辞——拔盘演练脚本对三个命令做同一种断言，措辞分裂会让它要么
    // 漏判要么各写一套规则。
    let mut sink = NullSink;
    let result = match &resolved.target {
        HubTarget::Local(root_path) => {
            let store_root = match StorageRoot::open(root_path, Some(&resolved.cfg.dataset_id)) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("数据集 {path}（hub={}）离线：{e}", resolved.hub_name);
                    return 2;
                }
            };
            status_lib::status(&resolved.dataset_dir, &store_root, &mut sink)
        }
        HubTarget::Http { base_url } => {
            let transport = HttpTransport::new(base_url, &resolved.cfg.dataset_id, None);
            status_lib::status_transport(&resolved.dataset_dir, &transport, &mut sink)
        }
    };

    let mut level: u8 = 0;
    match result {
        Ok(report) => {
            if report.is_silent() {
                return level;
            }
            if report.baseline_reset {
                eprintln!("基线已重建（此前缺失或损坏）——本轮是一次全量对账");
            }
            for p in &report.to_upload {
                eprintln!("待上传：{p}");
            }
            for p in &report.to_download {
                eprintln!("待下载：{p}");
            }
            for p in &report.to_adopt {
                eprintln!("待认领（零传输）：{p}");
            }
            for p in &report.to_delete_local {
                eprintln!("待删除本地副本：{p}");
            }
            for p in &report.tombstone_pending {
                eprintln!("待提交删除（tombstone）：{p}");
            }
            for p in &report.conflicts {
                eprintln!("结构化冲突：{p}");
            }
            for p in &report.needs_human {
                eprintln!("状态模糊，需要人工介入：{p}");
            }
            for (p, reason) in &report.scan_rejected {
                eprintln!("扫描阶段被拒绝（{}）：{p}", reason.as_str());
            }
            level = level.max(1);
            level
        }
        // I11：`http(s)://` hub 回 503（数据集离线）与本地存储根打不开是
        // 同一严重性，走同一个退出码 2、同一句措辞。其它 `Transport` 失败
        // （连不上、协议错）是命令本身的失败，退出码 1——与 `sync_one` 的
        // `sync_error_exit_level` 完全一致的分类。
        Err(status_lib::StatusError::Transport(TransportError::Offline { message })) => {
            eprintln!("数据集 {path}（hub={}）离线：{message}", resolved.hub_name);
            2
        }
        Err(e @ status_lib::StatusError::Transport(_)) => {
            eprintln!("数据集 {path}（hub={}）：{e}", resolved.hub_name);
            level.max(1)
        }
        Err(e) => {
            eprintln!("{e}");
            level.max(1)
        }
    }
}

/// M2d Task 4（spec §4.5）：「`arca status` 报告每个数据集的 server 副本数，
/// 低于阈值（默认 2）即告警——致敬 git-annex 的 numcopies。」
const DEFAULT_MIN_SERVER_COPIES: u32 = 2;

/// 算出**本设备目前能知道的下限**，不是全局真相——诚实的边界（M2d Task 4
/// brief 原话）：hub 自己的存储根即隐式 server 角色（spec §4.7），记 1 份；
/// 本设备若把这个数据集也声明为 server 角色（`crate::role`），再记 1 份。
/// **本切片没有办法知道其它设备的角色**——那需要 hub 侧登记每个绑定设备的
/// 角色（属 M2e 或更后的切片），所以这里算出来的数字只是"已知的下界"，
/// 不代表"全局一共有几份"；调用方（[`report_replica_warning_if_any`]）的
/// 措辞必须把这个边界讲清楚，不能让用户误以为这是权威计数。
fn known_server_copies(dataset_root: &Path) -> Result<u32, role::RoleError> {
    let mut copies = 1; // hub 自己的存储根
    if role::read(dataset_root)? == role::Role::Server {
        copies += 1; // 本设备也承诺永久保留一份
    }
    Ok(copies)
}

/// 副本数低于阈值时打一条 stderr 警告——**只打印，不影响退出码**（修复
/// I1）。这条告警既不是"命令本身失败"，也不是"有待办要处理"（PROTOCOL.md
/// §3.2 记载的三态语义：0 干净 / 1 有问题待办 / 2 身份不明），它是一条独立
/// 于"这次调和跑得干不干净"的、关于长期数据安全策略的建议——`arca sync`
/// 从不因为它退出非零，`status` 不该在这一点上分裂出不同的信号强度。
///
/// 之前的实现把返回值（0/1）叠加进 `status_one` 的 `level`，后果是一个刚
/// `adopt`、完全同步的默认角色（`client`）数据集 `arca status` 也退出 1，
/// 且 `status_all`/`sync_all` 取最大值会让**一个**默认角色的数据集拖累整个
/// vault 的退出码——反过来变相强迫用户为了消掉这个退出码去声明 `server`
/// （一个"永不主动释放空间"的强承诺），本末倒置。选择"打印但不动 level"
/// 而不是"加一个显式开关"：默认就该看到告警（Rule of Silence 只保证干净
/// 时安静，这条告警恰恰是在提醒一件本该被注意到的事），开关只会制造"新装
/// 用户默认看不到这条提示"的新问题，不值得为此多一个 flag。
///
/// 角色声明本身读不出来（role.toml 损坏）也一并打印（I5：不静默吞掉），但
/// 同样不影响退出码——按 `client`（未声明的默认角色）继续算下限，不因为
/// 这一步失败就放弃整个副本数提示。
fn report_replica_warning_if_any(path: &str, dataset_root: &Path) {
    let copies = match known_server_copies(dataset_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("数据集 {path} 的角色声明读取失败，副本数按未声明（client）估算：{e}");
            1
        }
    };
    if copies < DEFAULT_MIN_SERVER_COPIES {
        eprintln!(
            "数据集 {path}：已知的 server 副本数为 {copies}，低于阈值 {DEFAULT_MIN_SERVER_COPIES}\
             ——这只是本设备目前能看到的下限（hub 自己的存储根记 1 份，本设备若是 server 角色\
             再记 1 份），并非全局真相：其它设备是否也承诺了 server 角色，本版本还没有办法\
             得知（需要 hub 侧登记每个绑定设备的角色，未实现）。如果这台设备应该作为一份永久\
             保留的副本，运行 `arca role {path} --set server`。"
        );
    }
}

/// `arca status [<path>] [--root <path>]`：带路径报告单个数据集；**不带
/// 路径报告 vault 里全部已登记数据集**（M2d Task 3）。
pub fn status_cmd(path: Option<&str>, root: Option<&Path>) -> ExitCode {
    match path {
        Some(p) => ExitCode::from(status_one(p, root)),
        None => status_all(root),
    }
}

/// `arca status`（不带路径）：按数据集分别报告健康度——与 [`sync_all`]
/// 同一条纪律（M2d Task 3，spec §4.3.2）：一个 hub 离线只让它承载的数据集
/// 离线，循环体不因此中止，其余数据集照常报告；退出码取全体里最严重的
/// 那个；对 Rule of Silence 的收窄与 [`sync_all`] 同一处理——`status_one`
/// 单数据集调用路径不变，只有真的 2+ 个数据集时才逐个打 `== path ==` 头。
fn status_all(root_override: Option<&Path>) -> ExitCode {
    if root_override.is_some() {
        eprintln!(
            "--root 只在指定单个数据集路径时生效——全量状态查看（不带路径）请对需要覆盖存储根的\
             数据集单独运行 `arca status <path> --root <path>`"
        );
        return ExitCode::from(1);
    }

    let vault = match vault::open(&cwd()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    let paths: Vec<String> = vault
        .registry
        .datasets()
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    if paths.is_empty() {
        return ExitCode::SUCCESS;
    }

    let mut worst: u8 = 0;
    for path in &paths {
        if paths.len() > 1 {
            eprintln!("== {path} ==");
        }
        let level = status_one(path, None);
        worst = worst.max(level);
    }
    ExitCode::from(worst)
}

/// `arca verify <path> [--deep] [--root <path>]`（M1d Task 7；M2e Task 3
/// 补 `http(s)://`）：fixity 巡检。
///
/// # 两种 hub 类型下"verify 到底验了什么"完全不同——这一条必须讲清楚
///
/// 「一个自称 verify 却只对了对元数据的命令，比没有更危险」（M2e Task 3
/// brief 原话）。所以本命令**永远在 stderr 说明这次验了什么、没验什么**，
/// 即便结果是干净的（这是对 Rule of Silence 的一处刻意收窄，与
/// `sync_all` 的 `== path ==` 同一先例：当"成功"有多种强度时，沉默本身
/// 就是歧义）。
///
/// | hub | 默认 | `--deep` |
/// | --- | --- | --- |
/// | `file://` | **全量 fixity**：逐文件重算 BLAKE3 与版本链比对（`arca_store::fsck`） | 同左（本地读取不经网络，默认就已经是最强的那档，没有可省的余地） |
/// | `http(s)://` | **只验元数据一致性**：`GET .../state` 能读通、每条记录结构合法、本地基线与 hub 的哈希/大小声明一致。**不重算任何内容**——位腐、被外部改写的 `files/` 字节，这一档一个都发现不了 | **全量 fixity**：把每个路径的内容**整份拉下来**重算 BLAKE3，与 hub 声明的哈希比对 |
///
/// `http(s)://` 默认不做深度校验是一个明确的取舍而不是遗漏：一次深度巡检
/// 要把整个数据集的字节从网络上拉一遍（个人照片库轻易上百 GB），把它设成
/// 默认会让 `arca verify` 变成一条没人敢跑的命令，最终效果是根本没人巡检。
/// 代价是默认这一档**验不出位腐**——所以它必须被说出来，而不是让用户从
/// 一个安静的 exit 0 里自行推断。
///
/// 真正的 fixity 权威永远在 hub 那台机器上：在那里直接对存储根跑
/// `arca fsck <存储根>` 比隔着网络拉全部字节便宜几个数量级，输出里也会这么
/// 建议。
pub fn verify_cmd(path: &str, deep: bool, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    match &resolved.target {
        HubTarget::Local(root_path) => {
            // I11：先按已知身份打开一次，确认挂载的是期望的那个数据集——
            // `fsck::check_path` 本身不预设期望身份（诊断工具的设计，服务于
            // 任意存储根路径），但 `verify` 是针对一个具体已登记数据集的巡检，
            // 必须先做这道身份检查；否则挂错了别的空盘会被 `check_path`
            // 老老实实巡检出"零个问题"，把"身份不明"误判成"库是空的、
            // 一切正常"（I11）。
            //
            // M2d Task 5（拔盘演练）：诊断文案与 `status_one`/`sync_one` 统一
            // 用"数据集 {path}（hub=...）离线：{e}"这个形状——拔盘演练脚本
            // 需要对三个命令做同一种断言（都报"离线"、都点名哪个 hub），
            // 措辞分裂会让演练脚本要么漏判、要么各写一套匹配规则。
            if let Err(e) = StorageRoot::open(root_path, Some(&resolved.cfg.dataset_id)) {
                eprintln!("数据集 {path}（hub={}）离线：{e}", resolved.hub_name);
                return ExitCode::from(2);
            }
            verify_local(path, root_path)
        }
        HubTarget::Http { base_url } => {
            let transport = HttpTransport::new(base_url, &resolved.cfg.dataset_id, None);
            verify_remote(path, &resolved, &transport, deep)
        }
    }
}

/// `file://`：沿用 M1d 起的全量 fixity（`arca_store::fsck`）——`--deep`
/// 对它是空操作，因为默认就已经是最强的那一档。
fn verify_local(path: &str, root_path: &Path) -> ExitCode {
    match arca_store::fsck::check_path(root_path) {
        Ok(report) => {
            eprintln!(
                "已对 {path} 做**全量 fixity 巡检**：逐文件重算 BLAKE3 与版本链比对，\
                 检查 {} 个文件、{} 个块。",
                report.checked_files, report.checked_chunks
            );
            if report.problems.is_empty() {
                ExitCode::SUCCESS
            } else {
                for problem in &report.problems {
                    eprintln!("{problem:?}");
                }
                eprintln!("发现 {} 个问题", report.problems.len());
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

/// 一个把写入的字节直接喂给 BLAKE3、**不保留任何一份内容**的 `io::Write`
/// ——`Transport::read_content_into` 要的是 `&mut dyn Write`，而
/// `arca_chunk::hash::Hasher` 只有 `update`/`finish`，两者之间需要这一层。
///
/// 存在的意义是让深度巡检的内存占用与文件体积**完全无关**：`read_content`
/// 那条路会把整份内容攒成 `Vec<u8>`，一个 2 GB 的视频就是 2 GB 常驻。
struct HashingSink(arca_chunk::hash::Hasher);

impl HashingSink {
    fn new() -> Self {
        HashingSink(arca_chunk::hash::ContentHash::hasher())
    }
    fn finish(self) -> arca_chunk::hash::ContentHash {
        self.0.finish()
    }
}

impl std::io::Write for HashingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `http(s)://`：默认只验元数据一致性，`--deep` 才拉内容重算——两档各自
/// 验了什么、没验什么，见 [`verify_cmd`] 的表格。
fn verify_remote(
    path: &str,
    resolved: &dataset::ResolvedDataset,
    transport: &HttpTransport,
    deep: bool,
) -> ExitCode {
    use arca_cli::transport::Transport;
    use arca_core::state::RemoteState;

    let remote = match transport.read_remote() {
        Ok(r) => r,
        Err(TransportError::Offline { message }) => {
            // I11：与 file:// 侧逐字同一句措辞（拔盘演练脚本靠它做统一断言）。
            eprintln!("数据集 {path}（hub={}）离线：{message}", resolved.hub_name);
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("数据集 {path}（hub={}）：{e}", resolved.hub_name);
            return ExitCode::from(1);
        }
    };

    let mut problems: Vec<String> = Vec::new();
    let mut present = 0usize;

    // 第一档：元数据一致性——把 hub 声明的哈希/大小与**本地基线**记的那份
    // 对照。这两份数据来自完全独立的两条通路（基线是上次同步时本地算出来
    // 并落盘的，hub 的声明来自它自己的 index/items），不一致就意味着其中
    // 一侧被改过。发现不了的是"两侧记录一致、但 hub 的 files/ 字节已经腐
    // 烂"——那需要重算内容，是 `--deep` 那一档的事。
    let baseline = match arca_cli::baseline::load(&resolved.dataset_dir) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取本地基线失败，无法做元数据对照：{e}");
            return ExitCode::from(1);
        }
    };
    for (p, state) in &remote {
        let RemoteState::Present { hash, size, .. } = state else {
            continue;
        };
        present += 1;
        if let arca_core::state::BaseState::Present {
            hash: base_hash,
            size: base_size,
            ..
        } = baseline.get(p)
        {
            if base_hash != *hash {
                problems.push(format!(
                    "{p}：hub 声明的哈希 {} 与本地基线记录的 {} 不一致",
                    hash.to_text(),
                    base_hash.to_text()
                ));
            } else if base_size != *size {
                problems.push(format!(
                    "{p}：hub 声明的大小 {size} 与本地基线记录的 {base_size} 不一致"
                ));
            }
        }
    }

    // 第二档（`--deep`）：整份拉下来重算 BLAKE3 与 hub 声明的哈希比对——
    // 这一档才真的能发现位腐。
    let mut deep_checked = 0usize;
    let mut deep_bytes = 0u64;
    if deep {
        for (p, state) in &remote {
            let RemoteState::Present { hash, .. } = state else {
                continue;
            };
            // 流式读：不把整份内容攒在内存里（`read_content_into` 的存在
            // 理由，见 `transport/mod.rs` 缺口第 1 条）；哈希是增量算的，
            // 内存占用与文件体积无关——一次深度巡检可能要过几百 GB，
            // 这一点不是优化而是可行性前提。
            let mut sink = HashingSink::new();
            match transport.read_content_into(p, &mut sink) {
                Ok(n) => {
                    deep_checked += 1;
                    deep_bytes += n;
                    let actual = sink.finish();
                    if actual != *hash {
                        problems.push(format!(
                            "{p}：hub 上的内容重算得到 {}，与它自己声明的 {} 不一致\
                             （位腐或被外部改写）",
                            actual.to_text(),
                            hash.to_text()
                        ));
                    }
                }
                Err(e) => problems.push(format!("{p}：拉取内容失败，无法校验：{e}")),
            }
        }
    }

    // 无论干净与否都说明这次验了什么、没验什么（见 `verify_cmd` 文档）。
    if deep {
        eprintln!(
            "已对 {path} 做**深度 fixity 巡检**（--deep）：拉取并重算了 {deep_checked} 个\
             文件、共 {deep_bytes} 字节的 BLAKE3，与 hub 声明的哈希逐一比对。"
        );
    } else {
        eprintln!(
            "已对 {path} 做**元数据一致性检查**（默认档）：hub 报告 {present} 个存活路径，\
             逐条与本地基线记录的哈希/大小对照。\
             **这一档不重算任何内容字节**——hub 上的位腐、被外部工具改写过的 \
             files/ 内容，它一个都发现不了。要真正验内容：加 `--deep`（会把整个\
             数据集的字节从网络上拉一遍），或者更省事的办法——在 hub 那台机器上\
             直接对存储根运行 `arca fsck <存储根>`。"
        );
    }

    if problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        for p in &problems {
            eprintln!("{p}");
        }
        eprintln!("发现 {} 个问题", problems.len());
        ExitCode::from(1)
    }
}

/// `arca doctor [--root <path>]`（M1d Task 7）：`tracking::check_vault` 的
/// 结果**原样呈现**（含 `Issue::CheckIncomplete`——它意味着"检查没跑成功"，
/// 不是"检查通过"，doctor 绝不能把它折叠成安静）+ 「本地存在但 hub 尚无
/// 副本」的显著告警（`git clean -xdf` 风险的唯一缓解措施，见 `doctor.rs`
/// 模块文档）。doctor 是全 vault 巡检，不针对单个数据集，因此没有 `<path>`
/// 参数——与 `status`/`verify`/plumbing 四个命令不同。
pub fn doctor_cmd(root: Option<&Path>) -> ExitCode {
    let vault = match vault::open(&cwd()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            // 评审 Minor：与 status/verify/ls/state dump 统一退出码，理由同
            // `sync_cmd` 上方的同一处改动。
            return ExitCode::from(1);
        }
    };
    let report = doctor::doctor(&vault.repo, &vault.registry, root);

    // check_vault 的每一条 Issue（含 CheckIncomplete）原样打印——Display
    // 本身已经把语义讲清楚，这里不做任何过滤或降级。
    for issue in &report.vault_issues {
        eprintln!("{issue}");
    }

    for dataset_health in &report.datasets {
        match dataset_health {
            doctor::DatasetHealth::Checked {
                path,
                local_only,
                ignore_issues,
                manifest_issue,
                trash_issues,
                possible_lost_server_role,
                local_trash_usage,
                local_trash_issues,
                hub_trash_scan_skipped,
            } => {
                if !local_only.is_empty() {
                    eprintln!();
                    eprintln!(
                        "！！！警告：数据集 {path} 下以下文件本地存在，但 hub 尚无副本——\
                         `git clean -xdf`（含 `-Xdf`）会把它们永久删除且无法找回："
                    );
                    for p in local_only {
                        eprintln!("  {path}/{p}");
                    }
                    eprintln!(
                        "！！！在确认已同步（`arca sync {path}` 或 `arca adopt {path}`）之前，\
                         请勿运行 `git clean -xdf`。"
                    );
                    eprintln!();
                }
                // 评审 Important #1：`.gitignore` 反选块的实测问题——受管
                // 二进制可能会被下一次 `git add -A` 提交进 git，或协作者
                // 拿不到清单/配置。
                for issue in ignore_issues {
                    eprintln!("数据集 {path} 的 .gitignore 反选块有问题：{issue}");
                }
                // 评审 Important #4：清单与基线漂移——协作者从 git 拿到的
                // 清单可能已经不反映当前受管路径集合。
                if let Some(issue) = manifest_issue {
                    eprintln!("数据集 {path} 的清单与基线不一致：{issue}");
                }
                // 评审 Minor：`.arca/trash/` 里损坏的记录——点名具体是哪个
                // 文件，不能只笼统报"删除/restore --list 已失效"。
                for issue in trash_issues {
                    eprintln!("数据集 {path} 的 .arca/trash/ 记录损坏：{issue}");
                }
                // M2e Task 3：I5——"没查"绝不能被呈现成"查过了没问题"。
                if *hub_trash_scan_skipped {
                    eprintln!(
                        "数据集 {path}：hub 侧回收站（.arca/trash/）的逐条巡检**未执行**\
                         ——这个数据集绑定的是 http(s):// hub，而协议里没有「枚举回收站\
                         全部记录」这个端点（PROTOCOL.md §1.2）。这不代表那边没问题，\
                         只代表这次没查；要巡检它请在 hub 那台机器上直接对存储根运行\
                         `arca fsck <存储根>`。"
                    );
                }
                // 评审 Minor #1：本地回收站非空但角色声明缺失——大概率是
                // `role.toml` 意外丢失，而不是用户主动把 server 降级成
                // client（后者不会留下这种组合）；不是数据丢失（hub 侧 trash
                // 保留期仍持有内容），但设备下次收到删除事件会真的移除本地
                // 副本，必须提醒用户核实。
                if *possible_lost_server_role {
                    eprintln!(
                        "数据集 {path}：本地回收站（.arca/client/trash/）非空，但角色声明\
                         （.arca/client/role.toml）缺失——这台设备可能曾经是 server 角色、\
                         声明意外丢失了（不是数据丢失，hub 侧保留期内的内容仍在）；如果这台\
                         设备本该继续承诺永久保留一份完整副本，请重新运行\
                         `arca role {path} --set server`"
                    );
                }
                // M2e Task 1：本地回收站里损坏的记录——与 hub 侧同一严重性，
                // 一条读不懂的 `.meta` 会让 `arca restore --local`/
                // `arca gc --local` 对整个数据集失效。
                for issue in local_trash_issues {
                    eprintln!(
                        "数据集 {path} 的 .arca/client/trash/ 记录损坏：{issue}\
                         （`arca restore {path} --local --list` 会因此整体报错）"
                    );
                }
                // M2e Task 1：让本地回收站可见。这是一条**信息**，不是问题
                // ——`server` 角色下这里有东西完全正常，所以只在非空时打印、
                // 且不影响退出码（见 `doctor::DatasetHealth::Checked::
                // local_trash_usage` 文档）。Rule of Silence 因此不被破坏：
                // 一个从未触发过 server 角色删除的数据集这里恒为空。
                if let Some(u) = local_trash_usage {
                    if u.entries > 0 {
                        eprintln!(
                            "数据集 {path} 的本地回收站（.arca/client/trash/）：{} 条记录、\
                             占用 {} 字节、最老一条删除于 {}；其中 {} 条已过默认保留期\
                             （{} 天）。这些内容**不会**被自动清理（I3），要销毁请显式运行\
                             `arca gc {path} --local`（默认只出清单）；要找回请运行\
                             `arca restore {path} <文件> --local`。",
                            u.entries,
                            u.bytes,
                            u.oldest_deleted_at.as_deref().unwrap_or("(未知)"),
                            u.expired,
                            arca_cli::trash::DEFAULT_RETENTION_DAYS,
                        );
                    }
                }
            }
            doctor::DatasetHealth::Offline { path, reason } => {
                eprintln!("数据集 {path} 离线（I11：挂载缺失或卷身份不符）：{reason}");
            }
            doctor::DatasetHealth::CheckFailed { path, reason } => {
                eprintln!("数据集 {path} 巡检失败：{reason}");
            }
            doctor::DatasetHealth::ResolveFailed { path, reason } => {
                eprintln!("数据集 {path} 解析失败：{reason}");
            }
        }
    }

    if report.has_offline() {
        ExitCode::from(2)
    } else if report.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// `arca restore <dataset> <file> [--root <path>]`（M2a tombstone 计划
/// Task 5，spec §7）：保留期内一条命令找回被删除的文件——从 hub 的
/// `.arca/trash/` 取回内容写回 `files/`，在 journal 追加一条新版本
/// （`item_id` 延续，理由见 `trash::restore` 的文档）。
///
/// 与 `status`/`verify` 同样先经 [`dataset::resolve`] 定位数据集与存储根，
/// 再用 [`StorageRoot::open`] 严格校验身份（I11）——恢复是写操作，绝不能
/// 在身份不明的挂载点上执行。
pub fn restore_cmd(dataset_path: &str, file: &str, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), dataset_path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let normalized_file = match arca_format::path_rules::check(file) {
        Ok(p) => p,
        Err(status) => {
            eprintln!("路径 {file:?} 不合规：{}", status.as_str());
            return ExitCode::from(1);
        }
    };
    // M2c Task 5：`restore` 尚未 Transport 化——`http://` hub 报明确的
    // "这条命令不支持"。
    let root_path = match resolved.local_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let store_root = match StorageRoot::open(root_path, Some(&resolved.cfg.dataset_id)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    match trash::restore(
        &store_root,
        &normalized_file,
        &default_actor(),
        &clock::now_rfc3339(),
    ) {
        Ok(version) => {
            // 评审 Minor：`version.parent == None` 只可能来自
            // `trash::last_version_id` 找不到版本链——按其文档，这在结构上
            // 不应该发生（能进回收站的 item 必然有过至少一条 upsert 版本）。
            // `restore` 仍然照常把内容找回给用户（防御性合理，不该因为一处
            // 不该出现的缺失就拒绝找回内容本身），但这个异常本身必须被看见
            // （I5），不能悄悄产出一条 `parent: null` 的孤立首版就当无事发生。
            if version.parent.is_none() {
                eprintln!(
                    "警告：{normalized_file} 的版本链缺失（结构上不应该发生）——\
                     刚恢复出的版本没有 parent，`arca history` 会把它显示成一条孤立首版；\
                     内容已经正常找回，建议之后跑一次 `arca doctor`"
                );
            }
            println!(
                "restore\t{normalized_file}\t{}",
                version.version_id.as_str()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// `arca restore <dataset> --list [--root <path>]`：列出回收站里的全部条目
/// ——数据永远走 stdout（plumbing 同一条 Rule of Silence 纪律：即便结果是
/// 空列表也不算"安静"，因为这是用户明确请求的数据，不是诊断）。
///
/// # 输出的是全部条目，不是"只有保留期内的"（评审 Important #4：文案与实现对齐）
///
/// 本切片没有实现 `arca gc`，回收站里任何一条记录的内容都还实实在在地
/// 可以被 `arca restore <path>` 找回——过了 spec §7 默认 180 天保留期的
/// 记录**不会**因此从这份列表里消失或变得不可恢复，物理销毁只经显式
/// `arca gc`（I3，未实现）。这里额外打印一列 `within_retention`（`true`/
/// `false`）——`deleted_at + 180 天 > 现在`（[`trash::within_retention`]，
/// spec §7）——告诉用户"这条记录是否已经过了默认保留期"，供运维判断哪些
/// 记录未来 `arca gc` 落地后会成为清理候选，不是"能不能恢复"的判断依据。
pub fn restore_list_cmd(dataset_path: &str, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), dataset_path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    // M2c Task 5：`restore --list` 尚未 Transport 化——`http://` hub 报明确的
    // "这条命令不支持"。
    let root_path = match resolved.local_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let store_root = match StorageRoot::open(root_path, Some(&resolved.cfg.dataset_id)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let now = clock::now_rfc3339();
    match trash::list(&store_root) {
        Ok(entries) => {
            for entry in &entries {
                let retained =
                    trash::within_retention(&entry.meta, &now, trash::DEFAULT_RETENTION_DAYS);
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    entry.meta.path,
                    entry.meta.item_id.to_hex(),
                    entry.meta.deleted_at,
                    entry.trash_id,
                    retained
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// `arca gc`（M2e Task 2，spec §7、I3）
// ---------------------------------------------------------------------------

/// `arca gc <dataset> [--local] [--dry-run] [--yes] [--include-unexpired]
/// [--retention-days N] [--root <path>]`
///
/// **本项目第一个被授权物理销毁数据的命令。** 纪律与判断全部在
/// [`arca_cli::gc`]，这里只负责"把参数翻译成 [`gc::GcOptions`]、把
/// [`gc::GcReport`] 呈现给人"，命令壳里**没有任何一处**自己决定要不要删。
///
/// # 退出码
///
/// - `0`：本次没有任何需要人处理的东西（dry-run 出了清单也算 0——预览不是
///   问题，用户就是来看清单的）。
/// - `1`：有 blocker（gc 停手了，什么都没销毁），或参数组合本身不合法。
/// - `2`：数据集离线（I11），与 `status`/`verify`/`sync` 同一信号。
pub fn gc_cmd(
    dataset_path: &str,
    local: bool,
    yes: bool,
    include_unexpired: bool,
    retention_days: Option<i64>,
    root: Option<&Path>,
) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), dataset_path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    let retention = retention_days.unwrap_or(trash::DEFAULT_RETENTION_DAYS);
    if retention < 0 {
        eprintln!("--retention-days 不接受负数（实得 {retention}）");
        return ExitCode::from(1);
    }
    let opts = gc::GcOptions {
        now: clock::now_rfc3339(),
        retention_days: retention,
        confirmed: yes,
        include_unexpired,
    };

    // 最响的那条提示，放在最前面：这个组合会销毁**还在保留期内**的内容。
    if include_unexpired && yes {
        eprintln!(
            "！！！--include-unexpired 已生效：本次会连**仍在保留期内**的回收站条目一起\
             物理销毁。保留期（{retention} 天）存在的意义就是给「删错了」留一段可以反悔的\
             时间，越过它之后这些内容在本地就**再也找不回来**了（除非另一台设备或备份里\
             还有）。如果你只是想清掉过期的东西，去掉这个开关重跑。"
        );
    }

    let report = if local {
        gc::local(&resolved.dataset_dir, &opts)
    } else {
        let root_path = match resolved.local_root() {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "{e}\n（`arca gc` 的 hub 侧回收站清理必须直接操作存储根文件系统，\
                     没有对应的 HTTP 端点——请在 hub 那台机器上运行，或改用 `--local` \
                     清理本机工作区侧的回收站）"
                );
                return ExitCode::from(1);
            }
        };
        // I11：销毁是最重的写操作，绝不能在身份不明的挂载点上执行。
        let store_root = match StorageRoot::open(root_path, Some(&resolved.cfg.dataset_id)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "数据集 {dataset_path}（hub={}）离线：{e}",
                    resolved.hub_name
                );
                return ExitCode::from(2);
            }
        };
        gc::hub(&store_root, &opts)
    };

    let report = match report {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    print_gc_report(dataset_path, local, &report, &opts)
}

/// 呈现一次 [`gc::GcReport`]。
///
/// 清单走 **stdout**（这是用户明确请求的数据，可脚本消费：
/// `gc-plan`/`gc-destroyed` 两种 tag + trash_id + 路径 + 字节数 + 删除时刻）；
/// 解释性文字与警告走 stderr。dry-run 与真跑打的是**同一份清单**，只有 tag
/// 不同——见 `gc` 模块顶部纪律 3。
fn print_gc_report(
    dataset_path: &str,
    local: bool,
    report: &gc::GcReport,
    opts: &gc::GcOptions,
) -> ExitCode {
    let 侧 = if local {
        "本地回收站（.arca/client/trash/）"
    } else {
        "hub 回收站（.arca/trash/）"
    };

    // 1. blocker 优先——它决定了这次什么都不会被销毁（I5）。
    if !report.blockers.is_empty() {
        for blocker in &report.blockers {
            eprintln!("{blocker}");
        }
        eprintln!(
            "`arca gc {dataset_path}` 已停止：{侧} 里有 {} 处无法解释的状态，\
             本次**一个字节都没有销毁**（包括那些本身健康、已经过期的条目）。\
             gc 只销毁它能完整解释的东西——先处理以上问题再重跑。",
            report.blockers.len()
        );
        // 清单仍然打出来，让用户知道修好之后能回收多少。
        print_gc_candidates(report, false);
        return ExitCode::from(1);
    }

    if report.executed {
        print_gc_candidates(report, true);
        eprintln!(
            "已从 {侧} 物理销毁 {} 条已过保留期的记录，回收 {} 字节。\
             这些内容**已经不可恢复**。",
            report.destroyed.len(),
            report.freed_bytes()
        );
    } else {
        print_gc_candidates(report, false);
        if report.candidates.is_empty() {
            // Rule of Silence：没有任何可清理的东西就安静。
            if report.retained.is_empty() {
                return ExitCode::SUCCESS;
            }
            eprintln!(
                "{侧} 里的 {} 条记录全部仍在保留期内（{} 天），没有可清理的条目——\
                 本次什么都没做。",
                report.retained.len(),
                opts.retention_days
            );
        } else {
            eprintln!(
                "以上是 **dry-run** 清单：{} 条已过保留期的记录、共 {} 字节，\
                 本次**没有销毁任何东西**。确认无误后加 `--yes` 重跑才会真的销毁。",
                report.candidates.len(),
                report.reclaimable_bytes()
            );
        }
        if !report.retained.is_empty() {
            eprintln!(
                "另有 {} 条仍在保留期内（{} 天），**即使加 `--yes` 也不会被销毁**。",
                report.retained.len(),
                opts.retention_days
            );
        }
    }

    if report.chunks_untouched > 0 {
        eprintln!(
            "注意：`.arca/chunks/` 下有 {} 个块，本版本**一个都不会回收**——\
             块级引用模型（每个版本用了哪些块）在写入侧还不存在，凭现有信息判断\
             「哪个块失引用」必然出错，因此这里选择不猜（I5）。",
            report.chunks_untouched
        );
    }

    ExitCode::SUCCESS
}

/// 逐条打印清单到 stdout。`destroyed` 为真时打的是"已销毁"的那份。
fn print_gc_candidates(report: &gc::GcReport, destroyed: bool) {
    let (tag, list) = if destroyed {
        ("gc-destroyed", &report.destroyed)
    } else {
        ("gc-plan", &report.candidates)
    };
    for c in list {
        println!(
            "{tag}\t{}\t{}\t{}\t{}",
            c.trash_id, c.path, c.bytes, c.deleted_at
        );
    }
}

// ---------------------------------------------------------------------------
// `arca restore … --local`（M2e Task 1，FORMAT.md §9.5）
// ---------------------------------------------------------------------------

/// `arca restore <dataset> <file> --local`：从**本设备工作区侧**的本地回收站
/// （`<dataset>/.arca/client/trash/`）找回内容。
///
/// # 与不带 `--local` 的 `arca restore` 是两个不同的回收站
///
/// | | 不带 `--local`（默认） | `--local` |
/// | --- | --- | --- |
/// | 读哪里 | **hub 侧** `<存储根>/.arca/trash/`（FORMAT.md §7.3） | **本设备** `<dataset>/.arca/client/trash/`（§9.5） |
/// | 内容怎么进去的 | 任一设备删除 → hub 记 tombstone 时移进来 | 本设备是 `server` 角色、远端 tombstone 过了四道闸门时移进来 |
/// | 恢复的效果 | 写回 hub 的 `files/` + 追加版本链/index/journal（一次**权威**提交，所有设备都会看到） | 只把字节写回本设备工作区，**不碰 hub 任何状态** |
/// | 需要什么 | 存储根在线且身份相符（I11） | 什么都不需要——纯本地操作，hub 离线也能跑 |
///
/// 换句话说：**默认那条是"把这个文件在整个数据集范围内找回来"，`--local`
/// 是"把这台机器上被删掉的那份副本捞回来"**。两者不互斥——同一次删除通常
/// 在两边各留了一份记录，从哪边找回取决于你想要什么效果。
///
/// 纯本地操作因此不打开存储根、不做 I11 身份校验：没有任何字节会被写到 hub。
pub fn restore_local_cmd(dataset_path: &str, file: &str, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), dataset_path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    match local_trash::restore(&resolved.dataset_dir, file, &clock::now_rfc3339()) {
        Ok(restored) => {
            // I5：被顶替下去的内容去了哪里必须说清楚，不能只报"恢复成功"。
            if let Some(protected) = restored.protected {
                eprintln!(
                    "{} 此前在工作区里已有一份**不同**的内容——它已被移入本地回收站\
                     （trash_id={protected}），没有被这次恢复销毁；要把它换回来运行\
                     `arca restore {dataset_path} {} --local`（会再次触发同样的保护）。",
                    restored.path, restored.path,
                );
            }
            println!("restore-local\t{}\t{}", restored.path, restored.trash_id);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// `arca restore <dataset> --list --local`：列出本地回收站里的全部条目。
///
/// 输出与 hub 侧 `restore --list` **逐列相同**（`path`/`item_id`/`deleted_at`/
/// `trash_id`/`within_retention`）——两个回收站的列表是同一种数据，脚本不该
/// 因为多了个 `--local` 就要换一套解析规则。同样列出的是**全部**条目而不是
/// "只有保留期内的"：没跑过 `arca gc` 就一条都不会消失（I3），
/// `within_retention` 那一列回答的是"未来 `arca gc` 会不会把它列进候选"，
/// 不是"现在还能不能恢复"。
pub fn restore_local_list_cmd(dataset_path: &str, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), dataset_path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    let now = clock::now_rfc3339();
    match local_trash::list(&resolved.dataset_dir) {
        Ok(entries) => {
            for entry in &entries {
                let retained =
                    trash::within_retention(&entry.meta, &now, trash::DEFAULT_RETENTION_DAYS);
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    entry.meta.path,
                    entry.meta.item_id.to_hex(),
                    entry.meta.deleted_at,
                    entry.trash_id,
                    retained
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}
