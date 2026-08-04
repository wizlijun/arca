//! BLAKE3 内容哈希（原生地址，`blake3:…` 前缀）+ SHA-256 懒计算（互操作）。
//!
//! ETag = BLAKE3 内容哈希（PROTOCOL.md）；流式计算支持大文件与 Range 验证。
//!
//! TODO(M0)：哈希类型、流式计算接口、`blake3:` 文本表示的解析/格式化。
