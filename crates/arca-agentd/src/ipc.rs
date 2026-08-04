//! 进程间接口：占位符适配层（arca-winfs 直连 / arca-macfs 经 XPC）与 CLI 的通信面。
//!
//! macOS File Provider 扩展是 Swift 沙箱进程，作为薄壳通过 XPC 调本 daemon（spec §3）。
//!
//! TODO(M3 Windows；M4 macOS)：协议定义与传输。
