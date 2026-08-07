//! # arca-core
//!
//! 对账与提交状态机——**sans-io 纯状态机，无任何 IO**，客户端与 hub 共用同一份代码
//! （继承 lazync `shared/` 的纪律：两端对路径规则、哈希、过滤器、调和决策跑同一段代码）。
//!
//! 正确性基础设施（spec §11.2，第一天就建）：
//! - 确定性模拟测试：模拟时钟/网络/文件系统 + 随机事件序列 + 崩溃注入 + 种子可复现；
//! - 收敛性属性测试（proptest）：任意操作交错 + 任意崩溃点 → 最终收敛，
//!   且**无任何路径销毁数据**（I3 作为可执行断言）。
//!
//! 参考 lazync：`client/src/nc_sync_engine.pas`（调和回路）、
//! `shared/src/nc_file_ops.pas`（原子文件操作语义）。

#![forbid(unsafe_code)]

pub mod commit;
pub mod conflict;
pub mod error;
pub mod journal;
pub mod reconcile;
pub mod state;
pub mod tombstone;
