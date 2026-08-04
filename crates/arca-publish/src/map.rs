//! `arca publish-map` 的输出：publish-map.json 生成（spec §4.9）。
//!
//! 结构：`{schema, datasets: {前缀 → base_url/style}, items: {路径 → hash/size}}`。
//! 格式属于对外契约（站点生成器过滤器消费），进 PROTOCOL.md。
//!
//! TODO(M5)：结构定义与序列化。
