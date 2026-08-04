//! # arca-conformance
//!
//! 一致性测试套件（spec §11.2、§12.1）——对任何第三方实现开放，
//! 鼓励替代实现：格式活得比实现久。
//!
//! 覆盖面（骨架，按里程碑填充）：
//! - **格式一致性**：golden vectors 回放（arca-format）；
//! - **恢复演示**：纯 shell + coreutils（不含任何 arca 代码）从测试库完整取回
//!   `files/` 并校验哈希——逃生舱（I1）进 CI，每晚验证（见 `tests/escape-hatch/`）；
//! - **收敛性**：任意操作交错 + 崩溃注入 → 收敛且零销毁（I3 可执行断言）；
//! - **噩梦路径**：spec §6.3 清单 1–12 逐条自动化（占位符层、`.gitignore` 反选、
//!   多 hub 故障隔离、数据集搬迁等）。

#![forbid(unsafe_code)]

// TODO(M0 起)：pub mod golden; pub mod convergence; pub mod nightmare;
