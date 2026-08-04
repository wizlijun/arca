//! journal 持久化：append-only 事件流 + `epoch:seq` 游标 + 压缩（spec §5.2）。
//!
//! 事件模型定义在 `arca_core::journal`（两端共用）；此处只负责落盘、
//! 游标服务与压缩后的 `reset_required` 语义。
//!
//! 参考 lazync：`server/src/nc_change_journal.pas`。
//!
//! TODO(M2)。
