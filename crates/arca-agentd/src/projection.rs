//! 客户端投影（`<dataset>/.arca/client/`，SQLite）：角色 · 驻留策略 · 基线。
//!
//! **可抛弃投影**（I9）：客户端库必须假设会损坏，「删掉重建」是一等公民——
//! `arca rebuild` 从 hub 全量对账重建，内容一致的本地文件走 adopt 认领零传输接管。
//! 逃生舱承诺只约束 hub，故此处允许 SQLite（配 `arca state dump --json`）。
//!
//! 参考 lazync：`client/src/nc_sync_state.pas`。
//!
//! TODO(M3)：schema、基线读写、rebuild/adopt 流程。
