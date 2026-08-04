//! # arca-publish
//!
//! 发布与公开 URL 映射（spec §4.9）。目录级数据集使发布退化为一次纯前缀替换。
//!
//! 四条设计约束：
//! 1. 替换只发生在发布时，**绝不改写 vault 里的 md**（I6）——arca 只产出映射，
//!    重写由站点生成器完成（核心不绑定任何生成器）；
//! 2. 两种 URL 风格：`path`（可读/SEO）与 `hash`（不可变 → immutable 缓存）；
//! 3. 默认只发布被引用的资源（`--referenced-only`）——扩大暴露面必须显式；
//! 4. hub 直供或导出到静态托管，用户自选。
//!
//! 副产品：CI 不下载任何 blob 也能构建站点（清单里已有路径/哈希/大小）。

#![forbid(unsafe_code)]

pub mod export;
pub mod map;
pub mod referenced;
