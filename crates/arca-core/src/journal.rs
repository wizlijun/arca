//! journal 事件模型：append-only、`epoch:seq` 游标、actor 归因（I8）。
//!
//! 真相在 hub 的 journal 与库（I9）；客户端一切状态是可重建投影。
//! 压缩后游标失效 → `reset_required` 全量对账兜底（spec §5.2）。
//!
//! 此处定义事件类型与游标语义（两端共用）；持久化在 arcad，
//! 消费在 agentd / CLI。
//!
//! 参考 lazync：`server/src/nc_change_journal.pas`。
//!
//! TODO(M2)：事件枚举、游标类型、序列化（进 PROTOCOL.md §3）。
