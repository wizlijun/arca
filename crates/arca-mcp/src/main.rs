//! # arca-mcp
//!
//! MCP server（spec §10）。工具面：
//!
//! | 工具 | 语义 | 底层保证 |
//! | --- | --- | --- |
//! | `vault_search` | 检索条目 | 只读，受令牌 scope 约束 |
//! | `vault_get` | 取内容/元数据（可指定版本） | Range 流式；版本钉住 |
//! | `vault_put` | 写入，必须带 expected_parent | CAS（I4）：过期即结构化失败 |
//! | `vault_history` | 版本链 + 每版 actor | I8 审计 |
//! | `vault_subscribe` | SSE 变更流 | agent 间协作事件总线 |
//! | `vault_checkpoint` | 命名快照创建/回滚 | 投机执行-丢弃工作模式 |
//! | `vault_conflicts` | 结构化冲突列表 | 冲突解决可编排 |
//!
//! 每 agent 独立 scope 令牌；每次写入可归因、可回滚、可拒绝。

mod tools;

fn main() {
    todo!("MCP server：实现从 M6 开始（spec §12.3）");
}
