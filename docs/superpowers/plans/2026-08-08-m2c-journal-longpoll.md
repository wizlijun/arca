# M2c journal 变更流、longpoll 与 sid 闭环 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让同步第一次真的跑在网络上——补齐 `Transport` 的四条接口缺口、把 journal 做成可消费的变更流（`epoch:seq` 游标 + longpoll）、打通 `sid` 从客户端 trace 到服务端 journal 的审计闭环，最后用 `http://` 实现跑通两机端到端。

**Architecture:** `http.rs` 是 `Transport` 的第二个实现。**先把 trait 补到能表达流式与批量，再写 HTTP**——否则 HTTP 实现会把 M2b 评审列出的四条缺口全部继承下来（其中一条是客户端侧的内存无界，即服务端 C2 的镜像）。

**Tech Stack:** Rust 2021 / MSRV 1.85 · axum + tokio（**仅 arcad**）· 阻塞式 HTTP 客户端（**绝不给 CLI 引入异步运行时**）

---

## 为什么第一个任务是补 trait 而不是写 HTTP

M2b 的切片评审在「Readiness for M2c / M2e」里列了四条缺口：

1. **没有按哈希寻址的读**——`arca cat <hash>`（PROTOCOL §5）没有 HTTP 对应
2. **没有批量提交**——每文件一次往返，而 `sync.rs` 本地已用 `atomic::Batch`；
   1 万文件的 sweep 会变成 1 万次往返
3. **`Transport` 没有 Range/续传方法**——服务端已实现并验证过的 206 经 trait 够不着
4. **`read_content -> Vec<u8>` 强制客户端也整文件缓冲**——服务端 C2 的镜像。
   两端要一起修，否则 M2e 会在客户端继承同样的内存曲线，
   而 spec §3.1 刻意让客户端保持轻量（一次性进程）

**这四条都是 trait 形状的问题。** 先补 trait、让 `local.rs` 跟上，
`http.rs` 才有一个正确的靶子可打。反过来先写 HTTP，等于把缺口固化进两个实现。

## Global Constraints

- MSRV **1.85**，edition 2021。依赖要求高于 1.85 **报告而非降级钉版**。
- **tokio / axum 只允许进 `arcad`**。`arca-core` 保持 sans-io；
  `arca-cli` 用**阻塞**客户端——spec §3.1「客户端零常驻」是形态约束。
  若选的 HTTP 库强制异步，**停下报告**，那需要换库不是妥协。
- 各 crate 保持 `#![forbid(unsafe_code)]`；**`arca-core` 一行不改**。
- 只在 `main` 分支工作。提交信息用中文。文档与注释一律中文；已有 doc comment 保留。
- 四项门禁：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo fmt --all -- --check`、`cargo +1.85 check --workspace --locked --all-targets`。
- **协议契约是 `PROTOCOL.md`**：§1.2 端点表、§3 journal 与游标、§7 错误码。
  新增端点与码**先写进协议再实现**（I10）。
- **I11**：数据集离线 → 503，绝不 200 加空列表。longpoll 期间数据集掉线同样要 503，
  **不能挂到超时再返回空**——那与「空库」等价。
- **I8 每个事件可归因**：`sid` 闭环是本切片的验收点之一。

## 已交付、可直接使用的接口

```rust
// 客户端
arca_cli::transport::{Transport, CommitRequest, CommitOutcome, TombstoneRequest,
                      Recoverable, TransportError}
arca_cli::transport::local::LocalTransport
// 服务端
crates/arcad/src/api.rs         // 端点已有：GET files/state/trash、PUT、DELETE
arca_store::lock                // 跨进程排他锁（M2b 新增）
// 格式
arca_format::journal::{JournalEvent, Op, Cursor}   // Cursor::parse / to_string
arca_format::trace::Sid
```

---

### Task 1: 补齐 `Transport` 的四条缺口

**Files:** `crates/arca-cli/src/transport/mod.rs`、`local.rs`；`crates/arcad/src/api.rs`（补端点）

- [ ] **流式读**：`read_content` 之外加一个把内容写进 `impl Write` 的方法，
      调用方不必整文件驻留内存。`local.rs` 用有界缓冲拷贝，`http.rs`（Task 5）用流式响应体。
- [ ] **Range / 续传**：加一个带字节区间的读方法。服务端的 206 已经能用且经评审验证，
      只是 trait 够不着。
- [ ] **按哈希寻址的读**：`arca cat <hash>` 需要它。服务端补
      `GET /v1/datasets/{id}/blobs/{hash}`（**先写进 `PROTOCOL.md` §1.2**）。
- [ ] **批量提交**：一次往返提交多个版本。服务端补一个批量端点；
      语义上要么整批成功要么整批不生效（**不要做成「部分成功」**——
      那会让客户端无法判断该从哪里重试，与 I5 相悖）。
      CAS 仍然逐条校验：任一条 parent 过期 → 整批 412 并指明是哪一条。

**判据**：现有 581 个测试**一条不改**全部通过（这是接口扩展不是行为变更）。
若某条必须改才能过，**停下报告**。

### Task 2: journal 变更流端点与游标

**Files:** `PROTOCOL.md` §3（先写）、`crates/arcad/src/api.rs`、`journal_store.rs`

- [ ] `GET /v1/datasets/{id}/changes?since=<epoch:seq>`：返回该游标之后的事件与新游标。
- [ ] **游标早于保留区间 → `reset_required`**（spec §5.2），客户端据此做全量对账。
      这条要在协议里写清楚：返回什么状态码、响应体什么形状。
- [ ] epoch 轮转的语义：M2a 已让 tombstone 在 index 侧留痕，所以 journal 理论上可截断——
      **但本切片不做压缩**，只把「游标失效怎么办」的路径打通并测试。
- [ ] 测试：正常增量拉取；游标失效 → `reset_required`；空增量；
      **游标语法非法 → 400 而不是当成从头开始**（I5：别猜）。

### Task 3: longpoll

**Files:** `crates/arcad/src/api.rs`

spec §5.2：客户端挂起 30–90 秒，有事件立即返回；2 秒短轮询仅作降级路径。

- [ ] `GET .../changes?since=...&wait=<秒>`：无新事件时挂起，有事件立即返回，
      超时返回空增量与原游标。
- [ ] **挂起期间数据集掉线 → 立即 503**，不要挂到超时再返回空（与「空库」等价，违反 I11）。
- [ ] 挂起的连接不得占用写锁，也不得阻塞其它请求——
      测试要能证明：一个客户端挂着 longpoll 时，另一个客户端的 PUT 照常完成并**唤醒**前者。
- [ ] 上限与退避：`wait` 超过上限要钳制而不是照单全收（资源耗尽面，M2b 已有教训）。

### Task 4: `sid` 闭环

**Files:** `crates/arcad/src/api.rs`、`crates/arca-cli/src/transport/http.rs`（Task 5 落地）

spec §12.3 的 M2 行点名：「`sid` 进协议头与 journal 的 `actor.session` 闭环」。

- [ ] 客户端把本次会话的 trace `sid` 放进 `Arca-Session` 头（§1.2 已定头名）。
- [ ] 服务端取出、**校验格式**（不可信输入！），写进 journal 事件的 `actor.session`。
- [ ] 格式非法或缺失时的行为要明确：**拒绝还是记为空**？我倾向缺失记空、非法拒绝——
      缺失是老客户端的正常情形，非法是 bug 或攻击。你判断并写进协议。
- [ ] 测试：一次带 sid 的 PUT 之后，journal 事件里能读回同一个 sid；
      **端到端**：客户端 trace 里的 sid 与服务端 journal 里的 `actor.session` 对得上——
      这条闭环让「谁在何时改了什么」可以从客户端一路追到服务端。

### Task 5: `http://` Transport 实现 + 两机端到端

**Files:** `crates/arca-cli/src/transport/http.rs`（新建）、`dataset.rs`（解析 `http://` 的 hub url）

- [ ] 用**阻塞**客户端实现 `Transport` 全部方法。
- [ ] `dataset::resolve` 认 `http://`（M1d 时它会报「该 transport 属 M2」）。
- [ ] **两机端到端演示**（spec §12.3 的 M2 验收要求「纯手动命令完成」）：
      两个工作区 + 一个 arcad，走完 `adopt → sync → 改名 → 删除 → 冲突` 全场景。
      **改名与冲突这两个场景本切片必须跑通**——它们是 M2 验收的明文要求。
- [ ] 网络故障的处置：连不上、超时、502——按 `ErrorClass` 分类，
      `retryable` 的要能重试，`needs_human` 的要停下。**别把网络抖动和协议错误混为一谈。**

---

## Self-Review

**范围**：多卷映射与 server/client 角色属 M2d；TLS 与 `arca bugreport` 属 M2e。
本切片用明文 `http://` 在本机测试——TLS 是部署问题不是协议问题。

**Task 1 是接口扩展不是行为变更**：581 个测试一条不改全过。这是本切片最容易出错的地方——
扩接口时顺手改语义，是引入回归的常见方式。

**风险点**：longpoll 引入了「挂起的连接」这一新的资源维度。M2b 已经在内存上栽过一次
（单请求 1.86 GB），这次要提前想：多少个并发挂起连接会耗尽什么？测试要覆盖。
