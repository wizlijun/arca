//! 三层数据模型：身份 → 版本 → 内容（spec §4.1）。
//!
//! - `ItemId`：随机 128-bit，创建时分配，永不复用；路径是索引键，身份跨改名稳定（I7）；
//! - `Version`：`{version_id, item_id, parent_version, content_hash, size, mtime, actor, committed_at}`，
//!   hub 上线性历史；
//! - `ContentHash`：BLAKE3 原生地址（I2：blob 不可变）；SHA-256 懒计算缓存（互操作，§8）；
//! - `Actor`：账号 + 设备/agent + 会话（I8：每个事件可归因）。
//!
//! 参考 lazync：`shared/src/nc_version.pas` 的版本模型，此处升级为身份/版本/内容三层。
//!
//! TODO(M0)：类型定义与序列化。
