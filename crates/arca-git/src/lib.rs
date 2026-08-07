//! # arca-git
//!
//! git 集成层（spec §4.3–§4.4）。arca 与 git **并行**工作，不做 clean/smudge filter
//! （寄生 git 管道正是 LFS 的失败根源，§1.2）——只共享目录、`.gitignore` 与清单。
//!
//! 职责：`.gitignore` 反选块的生成与维护、pre-push 一致性钩子、
//! 追踪冲突检测（已被 git 追踪的文件落入数据集 → 报告，绝不静默）。

#![forbid(unsafe_code)]

pub mod hooks;
pub mod ignore_block;
pub mod repo;
pub mod tracking;
