//! 每数据集独立的自动调和回路：longpoll 变更流 + arca-core 对账决策 + 传输执行。
//!
//! 决策（该传什么、该删什么、冲突怎么落地）全部来自 `arca_core::reconcile`；
//! 此处只做 IO 执行与退避调度。一个 hub 不可达 → 仅其数据集离线（I11）。
//!
//! 参考 lazync：`client/src/nc_sync_engine.pas`、`nc_http_task.pas`。
//!
//! TODO(M3)。
