# M2b arcad 与 HTTP CAS 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 arcad 跑起来——HTTP API + CAS（If-Match / 412）+ 挂载缺失即 503，并让既有的同步闭环第一次跑在网络上而不是 `file://`。

**Architecture:** arcad 是薄壳：HTTP 表面 + 存储根 IO，**决策全部来自 `arca-core`**。客户端侧引入 `Transport` 抽象，`file://` 与 `http://` 是它的两个实现——既有的 `sync.rs` 闭环不因传输方式改变而分叉。

**Tech Stack:** Rust 2021 / MSRV 1.85 · axum + tokio（**仅 arcad**）· arca-store · arca-core · reqwest（客户端）

---

## 为什么先抽传输，再写服务端

M2a 的切片评审留了一条建议：

> 第 4 道闸门现在直接读 hub 的文件系统（`trash::list`），HTTP CAS 下它需要变成一次
> 远端查询——`DeleteCheck` 拿 `&StorageRoot` 这个签名会挡路，且 O(n·m) 在网络往返下
> 会从「慢」变成「不可用」。趁现在把第 4 道的接口抽成一个「这个 item 的内容此刻
> 是否可取回（附哈希）」的 trait，比 M2b 再动它便宜得多。

这条对整个客户端侧都成立，不只是闸门：`hub.rs` 的 `read_remote`、`sync.rs` 的上传下载、
`trash.rs` 的恢复——全都在直接摸文件系统。**先抽 `Transport`，再让 `file://` 成为它的
第一个实现**，然后 HTTP 只是第二个实现。反过来做，就要在写 HTTP 的同时改所有调用点。

## Global Constraints

- MSRV **1.85**，edition 2021。依赖要求高于 1.85 **报告而非降级钉版**。
- **tokio / axum 只允许进 `arcad`**。`arca-core` 保持 sans-io；`arca-cli` 是一次性进程，
  用**阻塞**的 HTTP 客户端（`reqwest` 的 blocking feature 或 `ureq`），
  **绝不把异步运行时带进 CLI**——spec §3.1 的「客户端零常驻」是形态约束，
  一个为了发三个 HTTP 请求而启动的 tokio 运行时违背它。
- 各 crate 保持 `#![forbid(unsafe_code)]`。
- 只在 `main` 分支工作。提交信息用中文。文档与注释一律中文；已有 doc comment 保留。
- 四项门禁：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo fmt --all -- --check`、`cargo +1.85 check --workspace --locked --all-targets`。
- **I4 一切写入走 CAS**：提交必须携带父版本（If-Match），过期即 412，绝不静默覆盖。
- **I11 挂载缺失即离线**：存储根未挂载或身份不符 → **503**，绝不呈现为空库。
- **I3**：服务端不得有任何销毁路径。删除仍是 tombstone（M2a 已建好），
  物理销毁只经显式 `arca gc`（后续切片）。
- **协议契约**：`PROTOCOL.md` §1.2 目前只有要点，端点表是 `TODO`。
  **本切片要把它写出来**——按 I10，先写协议再写实现。

## 已交付、可直接使用的接口

```rust
arca_store::root::{StorageRoot, MountError}          // open / open_traced / join
arca_store::atomic::{write, write_local, rename, Batch}
arca_store::fsck::{check_root, check_path}
arca_core::state::{BaseState, LocalState, RemoteState}
arca_core::reconcile::{decide, decide_traced, Action, Decision}
arca_format::journal::{JournalEvent, Op, Cursor}
arca_format::trace::{TraceSink, EventKind, ErrorClass, Sid}
```

客户端侧（`crates/arca-cli/src/`）：`hub.rs`（`read_remote`）、`sync.rs`、`gates.rs`、
`trash.rs`、`journal.rs` —— 这些是 Task 1 要抽象的对象。

---

## File Structure

| 文件 | 职责 |
| --- | --- |
| `crates/arca-cli/src/transport/mod.rs`（新建） | `Transport` trait：客户端看 hub 的唯一接口 |
| `crates/arca-cli/src/transport/local.rs`（新建） | `file://` 实现——把既有直接摸文件系统的代码搬进来 |
| `crates/arca-cli/src/transport/http.rs`（新建） | `http://` 实现（阻塞客户端） |
| `crates/arcad/src/main.rs`（改） | 启动、配置、优雅关闭 |
| `crates/arcad/src/api.rs`（填充） | 路由与处理器 |
| `crates/arcad/src/storage.rs`（填充） | 多存储根管理 + 挂载检查 → 503 |
| `PROTOCOL.md` §1.2 | 端点表、请求/响应格式、错误码——**先写它** |

---

### Task 1: `Transport` 抽象 + `file://` 实现

**Files:** `crates/arca-cli/src/transport/{mod.rs,local.rs}`（新建）、`hub.rs` / `sync.rs` / `gates.rs` / `trash.rs`（改为经 trait）

**这是纯重构，行为必须不变。** 判据：改完后 512 个测试**一条不改**全部照常通过。
若某条测试必须改才能过，那说明行为变了——**停下报告**。

`Transport` 至少要覆盖客户端现在对 hub 做的每件事：

```rust
pub trait Transport {
    /// 读一个 item 的当前远端状态（含 tombstone 判定）
    fn read_remote(&self, path: &str) -> Result<RemoteState, TransportError>;
    /// 取内容
    fn read_content(&self, path: &str) -> Result<Vec<u8>, TransportError>;
    /// 提交新版本（CAS：parent 过期即失败）
    fn commit(&self, req: &CommitRequest) -> Result<CommitOutcome, TransportError>;
    /// 提交 tombstone
    fn tombstone(&self, req: &TombstoneRequest) -> Result<CommitOutcome, TransportError>;
    /// 第 4 道闸门要问的：这个 item 的内容此刻是否可取回（附哈希）
    fn recoverable(&self, item_id: &ItemId) -> Result<Option<Recoverable>, TransportError>;
    /// 枚举远端全部路径（status/verify 用）
    fn list(&self) -> Result<Vec<String>, TransportError>;
}
```

**`recoverable` 是评审点名要抽的那个**——它让第 4 道闸门不再需要 `&StorageRoot`，
HTTP 下变成一次远端查询而不是全量扫回收站。返回值要带哈希，
这样三方核验（基线期望 = 远端记录 = 现场重算）在两种传输下形状一致。

- [ ] 先写 trait 与 `local.rs`，让既有代码经它走。
- [ ] `CommitOutcome` 要能表达 **CAS 冲突**（parent 过期）——那是协议层的正常结果，
      不是错误（`PROTOCOL.md` §7 对 `class=protocol` 的定义：走结构化冲突流程，
      不作为错误处理）。M1b 已经踩过这个形状问题，别再犯。
- [ ] 判据：512 个测试全部照常通过，一条都不改。

---

### Task 2: `PROTOCOL.md` §1.2 端点表定稿

**Files:** `PROTOCOL.md`

**只写文档，不写代码**——I10 要求协议先于实现。

要定的：

- **端点表**：路径、方法、请求头、请求体、响应码、响应体。至少覆盖
  `GET /v1/datasets/{id}/files/{path}`（取内容，支持 Range 与 If-None-Match）、
  `PUT /v1/datasets/{id}/files/{path}`（提交，**必须带 If-Match**）、
  `DELETE`（提交 tombstone，同样带 If-Match）、
  `GET /v1/datasets/{id}/state`（枚举远端状态）、
  `GET /v1/datasets/{id}/trash/{item_id}`（第 4 道闸门的可取回查询）。
- **ETag = BLAKE3 内容哈希**（spec §8 已定）。`If-Match` 认的是**版本号**还是 ETag？
  M1b 的教训是 CAS 认版本号——**这里要写清楚，并说明与 ETag 的关系**。
- **412 的响应体**：必须是结构化冲突（`{base, theirs, yours}`），不是一句错误文本。
  agent 要靠它决定重读再试。
- **503**：数据集离线（未挂载/身份不符）时返回，响应体带 `code=mount.absent` 或
  `mount.identity_mismatch`（§7 已有这两个码）。
- **`sid` 进协议头**（spec §12.3 的 M2 行点名）：客户端把自己的 trace `sid` 放进请求头，
  服务端把它记进 journal 的 `actor.session`——这条闭环让「谁在何时改了什么」
  可以从客户端 trace 一路追到服务端 journal。定下头名。

---

### Task 3: arcad 服务端骨架 + 挂载检查

**Files:** `crates/arcad/src/{main.rs,config.rs,storage.rs}`、`Cargo.toml`

- [ ] `hub.toml` 配置：`instance_id` + 每数据集的存储根路径映射（spec §4.6）。
- [ ] 启动时对每个存储根做 `StorageRoot::open_traced`，**失败不等于启动失败**——
      该数据集进入离线态，其余照常服务（spec §4.3.2 的独立故障域）。
- [ ] 对离线数据集的任何请求 → **503**，响应体带 code。
      **绝不返回 200 加一个空列表**（I11）。
- [ ] `arcad --check` 子命令：只做挂载检查并报告，不起服务——运维排障用。
- [ ] 测试：健康根可服务；根被移走后该数据集 503 而其余数据集照常 200；
      身份不符时 503 且 code 不同。

---

### Task 4: 读取端点 + 条件请求

**Files:** `crates/arcad/src/api.rs`

- [ ] `GET .../files/{path}`：200 + 内容 + `ETag`；`If-None-Match` 命中 → 304；
      `Range` → 206。
- [ ] `GET .../state`：枚举，供客户端构造 `RemoteState`。
- [ ] `GET .../trash/{item_id}`：第 4 道闸门的可取回查询，返回哈希与大小。
- [ ] 路径必须过 `arca_format::path_rules::check`——**HTTP 是不可信输入的入口**，
      一条 `../../etc/passwd` 必须在进文件系统之前被拒。这条要有测试。

---

### Task 5: CAS 写入端点

**Files:** `crates/arcad/src/api.rs`

- [ ] `PUT .../files/{path}`：**没有 `If-Match` 头 → 400**（I4：一切写入走 CAS，
      不允许无条件写）。`If-Match: *` 表示「仅当不存在时创建」。
- [ ] parent 过期 → **412 + 结构化冲突体**。
- [ ] 写入顺序照 M1d 的 C1 教训：**内容先落、指针最后发布**。
- [ ] `DELETE`：提交 tombstone，复用 M2a 的 `move_to_trash` + journal，
      **服务端同样不得物理销毁**。
- [ ] `sid` 从请求头取出，写进 journal 的 `actor.session`。
- [ ] 测试：无 If-Match → 400；过期 parent → 412 且响应体可解析出三方哈希；
      并发两个客户端提交同一路径 → 一个成功一个 412（**这条要真的并发跑**）。

---

## Self-Review

**范围**：本切片交付服务端 + CAS + 客户端的传输抽象。**longpoll 与游标属 M2c**；
多卷与角色属 M2d；`https://`（TLS）与 bugreport 属 M2e——本切片用明文 `http://`
在本机测试即可，TLS 是部署问题不是协议问题。

**Task 1 是纯重构，必须行为不变**：512 个测试一条不改全过。这是本切片风险最高的一步——
重构时顺手「改进」行为是最容易引入回归的方式。

**不要在 `arca-cli` 引入 tokio**。CLI 是一次性进程（spec §3.1）。
如果发现选的 HTTP 客户端库强制要求异步运行时，**停下报告**——那需要换库，不是妥协。
