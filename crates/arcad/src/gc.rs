//! GC 与 fsck（spec §7）：物理销毁**只经显式触发**（I3），默认安装不销毁任何东西。
//!
//! - `--dry-run` 先出清单；gc 报告列出销毁清单；
//! - 只清理超过保留期（默认 180 天）的 tombstone 与失引用块；
//! - gc 与 fsck 共享引用计数校验：悬空/多余引用 → 停下报告（I5）；
//! - `verify --all`：逐文件 BLAKE3 重算对账（fixity 巡检，防 bit rot）。
//!
//! TODO(M0 fsck 骨架；M2 GC)。
