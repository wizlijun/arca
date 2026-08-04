//! 断点续传会话（spec §5.4，继承 lazync §7 幂等五元组）。
//!
//! 会话落盘于 `.arca/uploads/`；丢失 commit 的 no-op 恢复；
//! 增量上传只收缺失的 CDC 块。
//!
//! 参考 lazync：`server/src/nc_upload_manager.pas`。
//!
//! TODO(M2)。
