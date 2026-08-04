//! 错误分类与「绝不猜测」处置（I5）。
//!
//! 原则：状态模糊 → 停下并可诊断，而不是尽力恢复。
//! 错误须区分：可重试（网络/锁竞争）、需人工介入（一致性冲突、孤儿数据集）、
//! 协议错误（CAS 412 → 结构化冲突，走 [`crate::conflict`]）。
//!
//! 分类的落地处已定：[`arca_format::trace::ErrorClass`]（`retryable` / `needs_human` /
//! `protocol` / `bug`），码表在 PROTOCOL.md §7。本模块的错误类型只需**映射**到它，
//! 不重新发明一套分类——同一套 `class` 同时出现在 trace 事件、HTTP 错误体与 `--json` 输出，
//! agent 只看 `class` 就知道该重试、该停下、还是该报 bug（FORMAT.md §10.4）。
//!
//! 参考 lazync：`shared/src/nc_errors.pas`、`shared/src/nc_error_codes.pas`。
//!
//! TODO(M0)：错误类型层次 + `-> ErrorClass` 映射；码表随里程碑增补进 PROTOCOL.md §7。
