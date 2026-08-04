//! # arca-winfs
//!
//! Windows Cloud Files API（CfAPI，`cldflt.sys` 微过滤驱动，Win10 1709+）适配层，
//! Rust（windows-rs）直接驱动（spec §6.2）。仅 NTFS；仅 client 角色绑定使用。
//!
//! - 注册：`CfRegisterSyncRoot` + `CfCreatePlaceholders`；
//! - 按需下载：`FETCH_DATA` 回调 → agentd 流式喂数据（支持按区间响应——
//!   元数据读/文件头读不触发全量水化，§4.8 要求 2）；
//! - 释放空间：置 dehydrate / UNPINNED 属性。
//!
//! 工程纪律（I9）：OS 占位符状态永远是投影；同步根损坏的标准处置 =
//! 注销 → 重建 → 全量对账 + adopt 认领（一等公民测试，非灾难恢复脚注）。
//!
//! 本 crate 在非 Windows 平台编译为空（占位符层是可选增强，§3.1）。

#![forbid(unsafe_code)]
// 注：实现 CfAPI FFI 时此 crate 是 unsafe 边界（spec §11.2 的例外），届时改为局部 allow。

// TODO(M3)：#[cfg(windows)] mod sync_root; mod fetch_data; mod dehydrate;
