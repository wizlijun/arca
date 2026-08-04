//! 本地变更检测三重保险（spec §5.2，继承 lazync §5）：
//! 实时事件（Windows `ReadDirectoryChangesW` / macOS FSEvents）→
//! 溢出即全扫 → 周期性全量对账地基。
//!
//! 参考 lazync：`client/src/nc_directory_watcher.pas`。
//!
//! TODO(M3)。
