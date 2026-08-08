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
use arca_cli::dataset;
use arca_cli::doctor;
use arca_cli::init::{self, HookOutcome};
use arca_cli::register::{self, RegisterOptions};
use arca_cli::status as status_lib;
use arca_cli::sync::{self as sync_lib, SyncActor};
use arca_cli::trace_sink;
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

/// `arca register <path> --hub <name> [--hub-instance-id <id>] [--hub-url <url>] [--root <path>]`。
pub fn register_cmd(
    path: &str,
    hub_name: &str,
    hub_instance_id: Option<&str>,
    hub_url: Option<&str>,
    root: Option<&Path>,
) -> ExitCode {
    let opts = RegisterOptions {
        path,
        hub_name,
        hub_instance_id,
        hub_url,
        root_hint: root,
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

/// `arca sync <path> [--root <path>]`——对一个已 `arca register`/`arca adopt`
/// 过的数据集再跑一轮调和闭环。**不引导全新存储根**（那是 `adopt` 的职责）：
/// 存储根打不开一律按 I11 视为身份不明。
pub fn sync_cmd(path: &str, root: Option<&Path>) -> ExitCode {
    let vault = match vault::open(&cwd()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            // 评审 Minor：与 status/verify/ls/state dump 对同一个 VaultError
            // 统一退出码——"不是 git 仓库"/".gitarca 缺失"不是 I11 意义上的
            // "身份不明"（那专指存储根挂载失败/卷身份不符），不该占用退出码 2。
            return ExitCode::from(1);
        }
    };
    let normalized = match arca_format::path_rules::check(path) {
        Ok(p) => p,
        Err(status) => {
            eprintln!("数据集路径不合规：{}", status.as_str());
            return ExitCode::from(1);
        }
    };
    let dataset_dir = vault.repo.root().join(&normalized);

    let dataset_toml_path = dataset_dir.join(".arca").join("dataset.toml");
    let cfg_text = match std::fs::read_to_string(&dataset_toml_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{normalized} 尚未登记为数据集（{e}）——请先 `arca register`/`arca adopt`");
            return ExitCode::from(1);
        }
    };
    let cfg = match arca_format::dataset::DatasetConfig::parse(&cfg_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dataset.toml 解析失败：{e}");
            return ExitCode::from(2);
        }
    };

    let Some(entry) = vault.registry.datasets().iter().find(|e| {
        arca_format::path_rules::casefold(&e.path) == arca_format::path_rules::casefold(&normalized)
    }) else {
        eprintln!("{normalized} 未在 .gitarca 登记——请先运行 `arca register`");
        return ExitCode::from(1);
    };
    let Some(hub) = vault.registry.hub(&entry.hub) else {
        eprintln!("hub {:?} 未在 .gitarca 登记", entry.hub);
        return ExitCode::from(1);
    };

    let root_path = match vault::resolve_hub_root(hub, root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    // sid/sink 建在挂载检查之前——mount.check 是"全项目最危险的判断"（见
    // `arca_store::root` 模块文档），失败时同样要能落盘诊断，不能只有后面
    // `sync_lib::sync` 内部的决策轨迹被记录下来。
    let sid = trace_sink::resolve_sid();
    let mut sink = RingSink::default();
    let storage_root = match arca_store::root::StorageRoot::open_traced(
        &root_path,
        Some(&cfg.dataset_id),
        0,
        &mut sink,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            flush_trace_if_needed(&sid, &mut sink, false);
            return ExitCode::from(2);
        }
    };

    let (code, succeeded) =
        match sync_lib::sync(&dataset_dir, &storage_root, &default_actor(), &mut sink) {
            Ok(report) => {
                // 评审 Important #2：基线被重置（缺失/损坏，本轮是全量对账）
                // 是"为什么这次结果长这样"的关键线索——`status_cmd` 已经打印
                // 这条提示，`sync_cmd` 此前算出了同一个字段却从不读它，混合
                // 场景下用户会看到一堆 upload/adopt 或一个"结构化冲突"却不知道
                // 起因就是基线被重建。
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
                for p in &report.deleted_local {
                    println!("delete-local\t{p}");
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
                if report.is_clean() {
                    (ExitCode::SUCCESS, true)
                } else {
                    (ExitCode::from(1), false)
                }
            }
            Err(e) => {
                eprintln!("{e}");
                (ExitCode::from(1), false)
            }
        };

    flush_trace_if_needed(&sid, &mut sink, succeeded);
    code
}

/// `arca status <path> [--root <path>]`（M1d Task 7）：跑扫描与调和但**不
/// 执行**——Rule of Silence，全同步时完全安静、退出码 0；有待办（含结构化
/// 问题）时把分类结果打到 stderr（诊断，不是数据）、退出码 1。
pub fn status_cmd(path: &str, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let store_root = match StorageRoot::open(&resolved.root_path, Some(&resolved.cfg.dataset_id)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let mut sink = NullSink;
    match status_lib::status(&resolved.dataset_dir, &store_root, &mut sink) {
        Ok(report) => {
            if report.is_silent() {
                return ExitCode::SUCCESS;
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
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// `arca verify <path> [--root <path>]`（M1d Task 7）：fixity 巡检，复用
/// `arca_store::fsck::check_path`。
pub fn verify_cmd(path: &str, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    // I11：先按已知身份打开一次，确认挂载的是期望的那个数据集——
    // `fsck::check_path` 本身不预设期望身份（诊断工具的设计，服务于任意
    // 存储根路径），但 `verify` 是针对一个具体已登记数据集的巡检，必须先
    // 做这道身份检查；否则挂错了别的空盘会被 `check_path` 老老实实巡检出
    // "零个问题"，把"身份不明"误判成"库是空的、一切正常"（I11）。
    if let Err(e) = StorageRoot::open(&resolved.root_path, Some(&resolved.cfg.dataset_id)) {
        eprintln!("{e}");
        return ExitCode::from(2);
    }

    match arca_store::fsck::check_path(&resolved.root_path) {
        Ok(report) => {
            if report.problems.is_empty() {
                ExitCode::SUCCESS
            } else {
                for problem in &report.problems {
                    eprintln!("{problem:?}");
                }
                eprintln!(
                    "检查 {} 个文件、{} 个块，发现 {} 个问题",
                    report.checked_files,
                    report.checked_chunks,
                    report.problems.len()
                );
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
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
    let store_root = match StorageRoot::open(&resolved.root_path, Some(&resolved.cfg.dataset_id)) {
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
    let store_root = match StorageRoot::open(&resolved.root_path, Some(&resolved.cfg.dataset_id)) {
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
