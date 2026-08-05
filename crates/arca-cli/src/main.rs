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
    }
}
