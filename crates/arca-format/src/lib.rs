//! # arca-format
//!
//! 磁盘格式的纯数据结构 + 解析/序列化，是 `FORMAT.md` 的唯一落地处（I10：格式先于代码）。
//!
//! 纪律（spec §11.2–§11.3）：
//! - 零重依赖、可嵌入——像 libgit2 一样成为别人构建的地基；
//! - cargo-fuzz 持续跑解析器：损坏输入 → 明确错误，绝不 panic、绝不猜测（I5）;
//! - golden vectors 进 `tests/golden/`，跨版本兼容性回归。

#![forbid(unsafe_code)]

pub mod dataset;
pub mod error;
pub mod gitarca;
pub mod hub_layout;
pub mod index;
pub mod items;
pub mod journal;
pub mod manifest;
pub mod model;
pub mod path_rules;
pub mod trace;
