//! # arca-chunk
//!
//! 内容寻址原语（spec §4.2、§5.4、§8 兼容矩阵）：
//! - **BLAKE3**：原生内容地址（Merkle 树，支持流式验证与并行）；
//! - **FastCDC**（USENIX ATC'16）：历史版本与传输层的 CDC 分块——
//!   仅服务历史去重与增量传输，`files/` 的 current 永远平放（I1）;
//! - **zstd**（RFC 8878）：chunks 落盘压缩；
//! - SHA-256 懒计算缓存：Git LFS 桥与 Dropbox 导入校验的互操作需要。
//!
//! 参考 lazync：`shared/src/nc_hash.pas`。

#![forbid(unsafe_code)]

pub mod cdc;
pub mod compress;
pub mod hash;
