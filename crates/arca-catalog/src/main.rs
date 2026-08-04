//! # arca-catalog
//!
//! 目录卡工具（spec §4.4）：相册 / 标签 / 条目卡片 / embedding 清单——
//! 纯文本、人可编辑、git 可 diff/merge/回滚。
//!
//! **catalog 的格式属于规范，实现移出核心**：核心是 stupid binary tracker，
//! 不内置相册（git 也不内置 bug tracker）；核心只保证引用完整性可校验。
//! catalog 永不内嵌二进制，一切二进制以 `blake3:…` 哈希引用数据集中的 blob。
//!
//! `arca catalog sync`：双向校验 catalog ↔ 数据集引用完整性（悬空引用报告，绝不静默删）。

fn main() {
    todo!("catalog 工具：实现从 M5 开始（spec §12.3）");
}
