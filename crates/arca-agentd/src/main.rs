//! # arca-agentd
//!
//! 可选客户端 daemon（spec §3.1）：自动同步（watcher + longpoll + 退避重试）
//! 与占位符投影供给。**上层永远是下层的增强，不是依赖**——agentd 崩了，
//! 手动命令照常工作；占位符注册失败，退回全量物化。
//!
//! daemon 为每个数据集跑独立的调和回路、journal 游标、传输队列与退避状态
//! （多 hub 独立故障域，§4.3.2）。

mod hydration;
mod ipc;
mod projection;
mod syncer;
mod watcher;

fn main() {
    todo!("agentd 骨架：实现从 M3 开始（spec §12.3）");
}
