//! # arca CLI
//!
//! 手动同步一等公民（spec §3.1）：一次性进程，如 git，无需任何 daemon。
//! 双形态：独立 `arca …` 与子命令 `git arca …`。
//!
//! CLI 纪律（spec §3.2）：
//! - plumbing / porcelain 分层：一切能力先以输出稳定、可脚本化的 plumbing 存在；
//! - Rule of Silence：成功时安静；数据走 stdout，进度与诊断走 stderr；处处可加 `--json`；
//! - 与 git 同名的动词语义必须一致：status / fetch / pull / push；`sync` = pull + push。

mod commands;

fn main() {
    todo!("CLI 骨架：M1 交付 setup/adopt/add/verify/history/restore/gc/bundle + plumbing（spec §12.3）");
}
