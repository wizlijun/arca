//! vault 根注册表 `/.gitarca` 的解析与序列化（spec §4.3）。
//!
//! 只做两件事：声明哪些目录归 arca（`[[dataset]]`：path + hub 名），
//! 以及登记可用的 hub 端点（`[hub.<name>]`：instance_id + url）。
//!
//! 端点无关身份：`instance_id` 是稳定身份，`url` 只是"当前怎么连过去"。
//!
//! TODO(M0)：数据结构定义、TOML 解析/序列化、schema 版本校验、
//! 一致性检查素材（同一路径登记两次 → 拒绝，§4.3.2）。
