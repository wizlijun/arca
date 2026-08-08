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
//! 本轮（M1d Task 4/5/6）落地 `init`/`register`/`adopt`/`sync` 四个命令壳；
//! `status`/`verify`/`doctor`/plumbing 属 Task 7，TODO(M1 起)。
//!
//! 退出码约定（spec §3.2，与 `arca fsck` 已定的一致）：0 = 干净，
//! 1 = 有问题/有未完成的工作，2 = 身份不明（存储根挂载失败/身份不符，I11）。

use arca_cli::adopt::{self, AdoptOptions};
use arca_cli::init::{self, HookOutcome};
use arca_cli::register::{self, RegisterOptions};
use arca_cli::sync::{self as sync_lib, SyncActor};
use arca_cli::vault;
use arca_format::trace::NullSink;
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

/// `arca adopt <path> [--root <path>]`。
pub fn adopt_cmd(path: &str, root: Option<&Path>) -> ExitCode {
    let opts = AdoptOptions {
        path,
        root_override: root,
        actor: default_actor(),
    };
    let mut sink = NullSink;
    match adopt::adopt(&cwd(), opts, &mut sink) {
        Ok(outcome) => {
            for p in &outcome.report.uploaded {
                println!("upload\t{p}");
            }
            for p in &outcome.report.adopted {
                println!("adopt\t{p}");
            }
            for p in &outcome.untracked_from_git {
                eprintln!("从 git index 逐出（工作树文件未改动）：{p}");
            }
            // I5：能力缺失/需要人工介入的情况绝不静默。
            for p in &outcome.report.tombstone_pending {
                eprintln!("删除传播属 M2，本轮未执行：{p}");
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
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(adopt::AdoptError::Mount(_)) => {
            eprintln!("存储根身份不明，已按 I11 拒绝——数据集离线");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// `arca sync <path> [--root <path>]`——对一个已 `arca register`/`arca adopt`
/// 过的数据集再跑一轮调和闭环。**不引导全新存储根**（那是 `adopt` 的职责）：
/// 存储根打不开一律按 I11 视为身份不明。
pub fn sync_cmd(path: &str, root: Option<&Path>) -> ExitCode {
    let vault = match vault::open(&cwd()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
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

    let storage_root = match arca_store::root::StorageRoot::open(&root_path, Some(&cfg.dataset_id))
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let mut sink = NullSink;
    match sync_lib::sync(&dataset_dir, &storage_root, &default_actor(), &mut sink) {
        Ok(report) => {
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
            for p in &report.tombstone_pending {
                eprintln!("删除传播属 M2，本轮未执行：{p}");
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
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}
