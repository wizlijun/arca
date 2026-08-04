//! # arcad
//!
//! 服务端 daemon——全系统唯一常驻进程（spec §3.1，形态参考 git：
//! 服务端有守护进程，客户端零常驻）。单二进制部署到 ARM NAS，
//! 内存占用平稳可预测（§1.1 目标 9）。
//!
//! 模块：HTTP API（RFC 9110 条件请求）· 库存储（多卷映射）· journal ·
//! 上传会话 · 认证 · GC · Git LFS 桥（可选启用）。
//! 对账与提交决策全部来自 arca-core（两端共用，不在此重写）。

mod api;
mod auth;
mod config;
mod gc;
mod journal_store;
mod lfs_bridge;
mod storage;
mod uploads;

fn main() {
    todo!("arcad 骨架：实现从 M2 开始（spec §12.3）");
}
