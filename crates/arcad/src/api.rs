//! HTTP API（PROTOCOL.md §1.2）：RFC 9110 条件请求 · longpoll 变更流 · Range 续传。
//!
//! - ETag = BLAKE3；If-Match CAS，过期 → 412 + 结构化冲突体；
//! - longpoll 挂起 30–90 秒；SSE 供 agent 场景；
//! - 数据集离线（卷未挂载 / 身份不符）→ 503，绝不呈现为空库（I11）。
//!
//! TODO(M2)：路由表、请求处理器（决策委托 arca-core）。
