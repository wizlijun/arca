//! # arca-store
//!
//! hub 存储根（dataset_root）的 IO 层。
//!
//! **为什么单独成 crate**：读写存储根的不止服务端。
//! - `arcad`（M2）：hub 侧的全部读写；
//! - `arca-cli`（M1）：`file://` 直连同步——dataset_root 本地挂载时，
//!   无任何 daemon 也要完成同步（spec §3.1）；`arca verify` 的 fixity 巡检同理。
//!
//! 两个消费者都在，所以这段逻辑属于它们共同的下层，而不该住在其中任何一个里。
//! 依赖方向：`arca-format`（格式解析）+ `arca-chunk`（哈希与压缩）→ 本 crate → arcad / arca-cli。
//!
//! **与 arca-core 的分工**：`arca-core` 是 sans-io 的纯状态机（决定"该做什么"），
//! 本 crate 负责"怎么落盘"（原子提交、事务、巡检）。core 不依赖本 crate。
//!
//! 布局规范见 `FORMAT.md` §4–§8。

#![forbid(unsafe_code)]

pub mod atomic;
pub mod fsck;
pub mod root;

// TODO(M2)：pub mod txn;      —— .txn 事务日志与前滚/回滚（继承 lazync §4）
