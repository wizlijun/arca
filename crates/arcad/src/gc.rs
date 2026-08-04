//! GC 与 fsck（spec §7）：物理销毁**只经显式触发**（I3），默认安装不销毁任何东西。
//!
//! - `--dry-run` 先出清单；gc 报告列出销毁清单；
//! - 只清理超过保留期（默认 180 天）的 tombstone 与失引用块；
//! - fsck 巡检本身实现在 `arca-store::fsck`（`check_root`）——只读诊断，
//!   `arca-cli` 与本 crate 共用同一份逻辑，不在这里重写；gc 与它共享引用计数校验：
//!   悬空/多余引用 → 停下报告（I5）；
//! - `verify --all`：逐文件 BLAKE3 重算对账（fixity 巡检，防 bit rot）。
//!
//! TODO(M2 GC)。
