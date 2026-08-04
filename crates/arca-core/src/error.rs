//! 错误分类与「绝不猜测」处置（I5）。
//!
//! 原则：状态模糊 → 停下并可诊断，而不是尽力恢复。
//! 错误须区分：可重试（网络/锁竞争）、需人工介入（一致性冲突、孤儿数据集）、
//! 协议错误（CAS 412 → 结构化冲突，走 [`crate::conflict`]）。
//!
//! 参考 lazync：`shared/src/nc_errors.pas`、`shared/src/nc_error_codes.pas`。
//!
//! TODO(M0)：错误类型层次与错误码表（进 PROTOCOL.md）。
