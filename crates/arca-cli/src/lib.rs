//! `arca-cli` 的库面：CLI 二进制（`src/main.rs`）与集成测试共用的构件。
//!
//! M1d（spec §12.3 M1 行「`file://` 直连同步」）把 M1a/M1b/M1c 接成一个
//! 无需任何 daemon 的完整同步闭环，CLI 是唯一的执行者。三个输入端各自
//! 独立成模块，供 [`sync`]（尚未实现，Task 6）与各命令壳（Task 4/5/7）复用：
//!
//! - [`scan`]：本地扫描，产出 `LocalState` 集合；
//! - [`baseline`]：客户端投影（可抛弃，I9），产出/持久化 `BaseState` 集合；
//! - [`hub`]：从存储根读出 `RemoteState` 集合（M1 结构上不产出 `Tombstoned`，
//!   见该模块 doc comment 的详细说明与连带后果）。
//!
//! 单独拆出 `lib.rs`（而不是把这些模块塞进 `main.rs` 的私有 `mod`）是刻意的：
//! 这三个模块的公开函数在 Task 4-8 落地前不会被 `main.rs` 调用到，若仍嵌在
//! 二进制目标里，`dead_code` 会在每一步单任务提交时都告警；作为库目标的
//! 公开 API，它们对编译器而言天然「可达」（外部 crate 可能使用），不依赖
//! 内部调用点提前存在。这也让 `tests/e2e.rs`（Task 6）能以集成测试的身份
//! 直接调用这些模块，不必依赖二进制本身的进程边界。

#![forbid(unsafe_code)]

pub mod adopt;
pub mod baseline;
pub mod clock;
pub mod dataset;
pub mod doctor;
pub mod gates;
pub mod gc;
pub mod hub;
pub mod ids;
pub mod import_lfs;
pub mod init;
pub mod journal;
pub mod local_trash;
pub mod register;
pub mod role;
pub mod scan;
pub mod status;
pub mod sync;
pub mod tls;
pub mod trace_sink;
pub mod transport;
pub mod trash;
pub mod vault;
