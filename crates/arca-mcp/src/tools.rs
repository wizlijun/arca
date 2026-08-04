//! MCP 工具定义与到 arcad API 的映射（spec §10.1）。
//!
//! 多 agent 并发正确性：`vault_put` 的 expected_parent 过期 → 412 结构化冲突
//! `{base, theirs, yours}`，agent fail-fast 重读再试——没有静默覆盖、没有丢失更新（§10.2）。
//!
//! TODO(M6)。
