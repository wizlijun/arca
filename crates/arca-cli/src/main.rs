//! # arca CLI
//!
//! 手动同步一等公民（spec §3.1）：一次性进程，如 git，无需任何 daemon。
//! 双形态：独立 `arca …` 与子命令 `git arca …`。
//!
//! CLI 纪律（spec §3.2）：
//! - plumbing / porcelain 分层：一切能力先以输出稳定、可脚本化的 plumbing 存在；
//! - Rule of Silence：成功时安静；数据走 stdout，进度与诊断走 stderr；处处可加 `--json`；
//! - 与 git 同名的动词语义必须一致：status / fetch / pull / push；`sync` = pull + push。

#![forbid(unsafe_code)]

mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "arca", about = "git 仓库的二进制附件层")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 巡检一个存储根的完整性（只读，绝不修改任何文件）
    Fsck {
        /// 存储根路径（含 files/ 与 .arca/）
        root: std::path::PathBuf,
    },
    /// 在 vault 根建 `.gitarca`（若已存在则只校验、不覆盖）、装 pre-push 钩子
    Init {
        /// vault 内任意路径，默认当前目录
        path: Option<std::path::PathBuf>,
        /// 跳过 pre-push 钩子安装
        #[arg(long)]
        no_hook: bool,
    },
    /// 把一个目录登记为数据集：建 dataset.toml、更新 .gitarca、更新 .gitignore
    Register {
        /// 数据集路径，相对 vault 根
        path: String,
        /// hub 名（.gitarca 里的 `[hub.<name>]`）
        #[arg(long)]
        hub: String,
        /// hub 不存在时用它创建；已存在则必须与登记的一致
        #[arg(long = "hub-instance-id")]
        hub_instance_id: Option<String>,
        /// hub 不存在时用它创建（file:// 或裸本地路径）；已存在则更新其 url
        #[arg(long = "hub-url")]
        hub_url: Option<String>,
        /// hub 是新建的且未给 --hub-url 时，从这个路径推导 file:// 地址
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 就地纳管一个已登记的数据集：算哈希、上传、写 .gitignore 块（文件原地不动）
    Adopt {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径（外置盘换挂载点等场景）
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        /// 显式承认"就是要在这个（当前打不开的）路径上新建一个全新存储根"
        /// ——数据集此前已被纳管过（.arca/manifest 存在）而存储根缺失时，
        /// 默认按 I11 拒绝凭空新建，必须用这个开关明确表态（评审 Critical #2）
        #[arg(long = "create-root")]
        create_root: bool,
    },
    /// 对一个已纳管的数据集跑一轮 file:// 调和闭环
    Sync {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 比对本地与 hub，不动数据；全同步时安静，退出码 0
    Status {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// fixity 巡检（BLAKE3 重算对账），复用 arca fsck 的巡检逻辑
    Verify {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 一致性巡检：.gitarca 一致性 + 本地存在但 hub 尚无副本的文件告警
    Doctor {
        /// 覆盖从 .gitarca 解析出的存储根路径（对本次巡检的全部数据集生效）
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// plumbing：hub 侧当前清单（--json 输出，格式见 PROTOCOL.md §5）
    Ls {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        /// 目前唯一支持的输出格式；保留该开关是为了未来扩展其它格式时不破坏调用方
        #[arg(long)]
        json: bool,
    },
    /// plumbing：按内容哈希取字节，原样写 stdout
    Cat {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 内容哈希（blake3:<hex> 形式）
        hash: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// plumbing：路径 → hub 侧身份/版本
    Resolve {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 数据集内的文件路径
        file: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// plumbing：客户端本地投影（基线）检视
    State {
        #[command(subcommand)]
        action: StateCommand,
    },
}

#[derive(Subcommand)]
enum StateCommand {
    /// 导出当前基线（--json 输出，格式见 PROTOCOL.md §5）
    Dump {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径（basline dump 本身不需要打开
        /// 存储根，这个参数只是为了与其它 plumbing 命令的调用形状保持一致）
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        /// 目前唯一支持的输出格式
        #[arg(long)]
        json: bool,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Fsck { root } => {
            // 挂载失败（根不存在、身份读不出来等）与「巡检发现问题」是两种
            // 不同性质的结果：前者连身份都不明，退出码 2；与逃生舱脚本的
            // 约定一致（2 = 身份不明，1 = 有问题，0 = 干净）。
            let report = match arca_store::fsck::check_path(&root) {
                Ok(report) => report,
                Err(e) => {
                    eprintln!("{e}");
                    return std::process::ExitCode::from(2);
                }
            };
            // Rule of Silence（spec §3.2）：成功时安静，退出码 0 本身就是答案。
            // 摘要行是诊断，不是可脚本消费的数据，连同逐条问题一起走 stderr；
            // 干净时完全不打印任何东西。
            if report.problems.is_empty() {
                std::process::ExitCode::SUCCESS
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
                std::process::ExitCode::from(1)
            }
        }
        Command::Init { path, no_hook } => commands::porcelain::init_cmd(path, no_hook),
        Command::Register {
            path,
            hub,
            hub_instance_id,
            hub_url,
            root,
        } => commands::porcelain::register_cmd(
            &path,
            &hub,
            hub_instance_id.as_deref(),
            hub_url.as_deref(),
            root.as_deref(),
        ),
        Command::Adopt {
            path,
            root,
            create_root,
        } => commands::porcelain::adopt_cmd(&path, root.as_deref(), create_root),
        Command::Sync { path, root } => commands::porcelain::sync_cmd(&path, root.as_deref()),
        Command::Status { path, root } => commands::porcelain::status_cmd(&path, root.as_deref()),
        Command::Verify { path, root } => commands::porcelain::verify_cmd(&path, root.as_deref()),
        Command::Doctor { root } => commands::porcelain::doctor_cmd(root.as_deref()),
        Command::Ls {
            path,
            root,
            json: _,
        } => commands::plumbing::ls_cmd(&path, root.as_deref()),
        Command::Cat { path, hash, root } => {
            commands::plumbing::cat_cmd(&path, &hash, root.as_deref())
        }
        Command::Resolve { path, file, root } => {
            commands::plumbing::resolve_cmd(&path, &file, root.as_deref())
        }
        Command::State { action } => match action {
            StateCommand::Dump {
                path,
                root,
                json: _,
            } => commands::plumbing::state_dump_cmd(&path, root.as_deref()),
        },
    }
}
