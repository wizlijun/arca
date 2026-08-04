//! 认证与令牌（spec §9）：密码（PBKDF2 → argon2id）→ 设备令牌（只存哈希，LRU 上限）
//! → 内存会话（重启即失效）；agent 令牌 `{scope, caps, ttl, actor_label}` 为第四形态，
//! 独立撤销，journal actor 直接引用（I8 审计闭环）。
//!
//! 参考 lazync：`server/src/nc_server_auth.pas`、`nc_server_identity.pas`、
//! `nc_server_security.pas`。
//!
//! TODO(M2；agent 令牌 M6)。
