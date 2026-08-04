//! Git LFS 桥（可选启用，spec §8、PROTOCOL.md §6）：实现 LFS Batch API 与指针格式。
//!
//! 仅用于既有 LFS 仓库迁入；oid 为 SHA-256 → 懒计算缓存（§4.1）。
//!
//! TODO(M5)。
