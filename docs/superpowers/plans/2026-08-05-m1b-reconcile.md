# M1b 调和状态机 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `arca-core` 里实现三态调和决策表——给定「基线 × 本地 × 远端」，输出该做什么动作。**纯状态机，无任何 IO**，客户端与 hub 共用同一份代码。

**Architecture:** `decide(base, local, remote) -> Decision` 是唯一的决策入口，纯函数、可穷举、可属性测试。IO 与执行由 M1d 的 CLI 与 M2 的 arcad 负责。决策的每一步向注入的 `TraceSink` 发一条 `reconcile.decide`。

**Tech Stack:** Rust 2021 / MSRV 1.85 · arca-format（类型与 trace）· arca-chunk（哈希）· proptest（dev）

---

## 为什么这块是核心

spec §3 的第一条架构约束：**arca-core 是同一份对账状态机，两端共用**。客户端与 hub
对路径规则、哈希、调和决策跑**同一段代码**——否则两端会对同一个文件得出不同结论，
同步就会错，而且是那种「各自都认为自己对」的错。

这也是 I3「同步路径无销毁权」唯一能被**可执行地**断言的地方：决策表是纯函数，
可以对任意输入组合穷举，断言「没有任何一条决策路径会导致数据被销毁」。
在别处这只能是一句承诺。

## Global Constraints

- MSRV **1.85**，edition 2021。`Cargo.lock` 已入库；依赖要求高于 1.85 **报告而非降级钉版**。
- **`arca-core` 必须保持 sans-io**：无 `std::fs`、无 `std::net`、无 tokio、无任何异步运行时、
  不读系统时钟。所有外部世界的输入都是函数参数。这条被违反就摧毁了整个设计。
- `arca-core` 保持 `#![forbid(unsafe_code)]`。
- 只在 `main` 分支工作。提交信息用中文。文档与注释一律中文；
  各文件顶部已有的中文 doc comment 必须保留。
- 四项门禁每个任务结束都要绿：`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo +1.85 check --workspace --locked --all-targets`、`cargo fmt --all -- --check`。
- **绝不猜测（I5）**：状态模糊必须停下并可诊断，不得尽力恢复。
  决策表里凡是「说不清该怎么办」的格子，动作必须是 `NeedsHuman`，不是随便挑一个。
- **格式契约**：`FORMAT.md` §10.3 已经钉死了 `reconcile.decide` 的字段名
  （`path` · `item_id` · `local` · `remote` · `base` · `action` · `reason`），
  §10.1 的示例还钉死了几个取值（`action:"conflict"`、`local:"modified"`、
  `remote:"modified"`、`reason:"three_way_divergent"`）。**这些字符串受 I10 约束**——
  agent 对它们做精确匹配。新增取值只能加不能改，且必须同步进 `FORMAT.md`。

## 已交付、可直接使用的接口

```rust
// arca-format
arca_format::model::{ItemId, VersionId, Actor, Version}
arca_format::path_rules::{check, casefold, index_key, PathStatus}
arca_format::error::FormatError
arca_format::trace::{TraceSink, TraceRecord, EventKind, FieldValue, ErrorClass,
                     NullSink, VecSink, RingSink}
EventKind::ReconcileDecide
TraceRecord::new(EventKind, t_abs_us: u64).with(key: &'static str, value: impl Into<FieldValue>)
VecSink::new() / .records() / .kinds()      // 测试里断言决策序列

// arca-chunk
arca_chunk::hash::ContentHash               // Copy + Eq + Ord + Hash；只有 Debug 没有 Display
```

`arca-core` 当前依赖 `arca-format`，dev 依赖 `proptest`。需要 `arca-chunk` 时用 `cargo add`。

---

## File Structure

| 文件 | 职责 |
| --- | --- |
| `crates/arca-core/src/state.rs`（新建） | 三态输入的词汇：`BaseState` / `LocalState` / `RemoteState` 及其 `as_str()` |
| `crates/arca-core/src/reconcile.rs`（改） | `decide()` 决策表 + `Action` / `Reason` |
| `crates/arca-core/src/error.rs`（改） | 错误类型 + `-> ErrorClass` 映射（M0 遗留的 TODO） |
| `crates/arca-core/tests/decision_table.rs`（新建） | 决策表的穷举测试 |
| `crates/arca-core/tests/convergence.rs`（新建） | proptest 收敛性 + I3 可执行断言 |
| `crates/arca-core/tests/simulation.rs`（新建） | 确定性模拟：模拟时钟 + 崩溃注入 + 种子可复现 |

---

### Task 1: 三态词汇与输入类型

**Files:**
- Create: `crates/arca-core/src/state.rs`
- Modify: `crates/arca-core/src/lib.rs`
- Test: 内联 tests 模块

**Interfaces:**
- Produces:
  - `BaseState`：`Absent` | `Present { item_id: ItemId, version_id: VersionId, hash: ContentHash, size: u64 }`
  - `LocalState`：`Absent` | `Present { hash: ContentHash, size: u64 }`
  - `RemoteState`：`Absent` | `Present { item_id: ItemId, version_id: VersionId, hash: ContentHash, size: u64 }` | `Tombstoned { item_id: ItemId, version_id: VersionId }`
  - 三者各有 `as_str(&self) -> &'static str`，取值受 I10 约束（见下）。

- [ ] **Step 1: 定下 trace 词汇并同步进 FORMAT.md**

`FORMAT.md` §10.1 的示例已经用了 `local:"modified"` 与 `remote:"modified"`，
所以词汇必须**包含**这些取值。定下三组：

```
base   ∈ absent | present
local  ∈ absent | unchanged | modified | added
remote ∈ absent | unchanged | modified | tombstoned
```

注意 `local` 与 `remote` 的取值是**相对基线**的判断，不是裸状态——
`unchanged` 意味着「与基线记录的哈希一致」，`modified` 意味着「存在但哈希与基线不同」，
`added` 意味着「基线里没有但本地有」。`RemoteState` 没有 `added`，因为远端新增在
基线缺失时表现为 `present`——这个不对称是有意的，在类型的 doc comment 里写明理由。

在 `FORMAT.md` §10.3 的事件表下方补一段，把这三组取值逐个列出并说明语义。
**这是本计划唯一允许改 `FORMAT.md` 的地方**（Task 4 若需补 `action`/`reason` 取值时再改一次）。

- [ ] **Step 2: 写失败的测试**

覆盖：三个枚举的 `as_str()` 取值与上面的表逐字一致；`LocalState` 与 `BaseState`
的哈希比较能正确导出 `unchanged` / `modified`（写一个 `LocalState::classify(base, observed)`
之类的构造器，或在 Task 2 里由 `decide` 内部完成——你决定，但要在报告里说明）。

- [ ] **Step 3: 实现，跑通，提交**

```bash
git add crates/arca-core FORMAT.md
git commit -m "arca-core: 三态调和的输入词汇（取值受 I10 约束，已同步 FORMAT.md §10.3）"
```

---

### Task 2: 决策表核心

**Files:**
- Modify: `crates/arca-core/src/reconcile.rs`
- Test: `crates/arca-core/tests/decision_table.rs`

**Interfaces:**
- Produces:
  - `Action`：`Noop` | `Upload { parent: Option<VersionId> }` | `Download { version_id: VersionId }` |
    `AdoptBaseline { hash: ContentHash }` | `DeleteLocal { item_id: ItemId }` |
    `TombstoneRemote { item_id: ItemId, parent: VersionId }` | `Conflict { .. }` | `NeedsHuman { .. }`
  - `Reason`：稳定的短标识（`&'static str`），受 I10 约束
  - `Decision { action: Action, reason: Reason }`
  - `decide(base: &BaseState, local: &LocalState, remote: &RemoteState) -> Decision`

- [ ] **Step 1: 把决策表写进模块 doc comment，再写代码**

这张表是本切片的全部价值所在，必须先以人可读的形式写下来，且代码逐格对应：

| base | local | remote | action | reason | 理由 |
| --- | --- | --- | --- | --- | --- |
| absent | absent | absent | `Noop` | `nothing_anywhere` | 无事发生 |
| absent | added | absent | `Upload{parent:None}` | `local_new` | 本地新增 → 上传，CAS 的 parent 为 None（仅创建） |
| absent | absent | present | `Download` | `remote_new` | 远端新增 → 下载 |
| absent | added | present | 哈希相同 → `AdoptBaseline`；否则 `Conflict` | `converged_independently` / `both_new_divergent` | **零传输认领**：两端各自产生了同一内容（例如同一张照片从两台设备导入）。这是 §4.3 「内容一致的本地文件走认领」的落地处 |
| present | unchanged | unchanged | `Noop` | `all_in_sync` | |
| present | modified | unchanged | `Upload{parent:Some(base.version)}` | `local_modified` | CAS 带父版本（I4） |
| present | unchanged | modified | `Download` | `remote_modified` | |
| present | modified | modified | 哈希相同 → `AdoptBaseline`；否则 `Conflict` | `converged_independently` / `three_way_divergent` | `three_way_divergent` 这个取值已被 `FORMAT.md` §10.1 的示例钉死，**逐字使用** |
| present | absent | unchanged | `TombstoneRemote` | `local_deleted` | 本地删除 → 传播为 tombstone（**不是**物理销毁，I3） |
| present | absent | modified | `Download` | `delete_vs_modify` | **本地删除撞上远端修改**：按 I3，删除绝不能赢——重新下载远端版本并报告。用户想删就再删一次，那是一次新的、明确的意图 |
| present | unchanged | tombstoned | `DeleteLocal` | `remote_tombstoned` | 远端删除且本地无改动 → 移除本地副本（四道闸门在 M1d 的执行侧，此处只出决策） |
| present | modified | tombstoned | `Conflict` | `modify_vs_delete` | 本地有未同步修改 → **绝不删**，升级为冲突副本（spec §5.3） |
| present | absent | tombstoned | `Noop` | `both_deleted` | 两端都删了，清基线即可 |
| present | absent | absent | `NeedsHuman` | `remote_vanished_without_tombstone` | **远端记录凭空消失**：基线说它存在过，远端却既无记录也无 tombstone。这不该发生——可能是 journal 被截断、存储根被换掉、或 bug。按 I5 停下，绝不推断成「远端删了」（那会导致删除本地数据） |
| absent | absent | tombstoned | `Noop` | `tombstone_for_unknown_item` | 收到一个我们从没见过的 item 的 tombstone，无事可做 |
| absent | added | tombstoned | `Upload{parent:None}` | `local_new_over_tombstone` | 删除后重建 = **新身份**（spec §4.1），所以按新增上传 |
| present | modified | absent | `NeedsHuman` | `remote_vanished_without_tombstone` | 同上，且本地还有未同步的修改，更不能猜 |
| present | unchanged | absent | `NeedsHuman` | `remote_vanished_without_tombstone` | 同上 |

**两条贯穿全表的纪律**，在 doc comment 里写明：

1. **没有任何一格的动作是「删除数据」**。`DeleteLocal` 移除的是本地副本，
   而权威副本在 hub 的 trash 保留期内；`TombstoneRemote` 记的是墓碑不是销毁。
   物理销毁只经显式 `arca gc`。这就是 I3 在决策层的形态。
2. **模糊必停**：`remote_vanished_without_tombstone` 三格宁可停下要人介入，
   也不推断成删除。这是 I5 最贵也最重要的一次应用。

- [ ] **Step 2: 穷举测试**

`crates/arca-core/tests/decision_table.rs`：对 `BaseState` × `LocalState` × `RemoteState`
的**每一个合法组合**断言其 `Decision`。表里 18 格，测试也应是 18 条（或一张数据驱动的表）。
额外断言：
- 遍历所有组合，**没有任何一个 `Action` 是会销毁数据的**（I3 的可执行断言——
  给 `Action` 加一个 `fn destroys_data(&self) -> bool` 恒返回 `false` 太廉价，
  改为断言 `Action` 的判别式集合不含任何销毁语义的变体，并在注释里说明这条测试的意义）
- 所有 `Reason` 取值互不重复且与 `FORMAT.md` 一致

- [ ] **Step 3: 实现，跑通，提交**

---

### Task 3: reconcile.decide trace 发射

**Files:**
- Modify: `crates/arca-core/src/reconcile.rs`
- Test: `crates/arca-core/tests/decision_table.rs`（追加）

**Interfaces:**
- Produces: `decide_traced(base, local, remote, path: &str, t_abs_us: u64, sink: &mut dyn TraceSink) -> Decision`；
  `decide` 保留为 `decide_traced(..., &mut NullSink)` 的薄壳。

字段按 `FORMAT.md` §10.3：`path` · `item_id` · `local` · `remote` · `base` · `action` · `reason`。
`item_id` 从 base 或 remote 取（都没有则空字符串——**缺失与空值是两回事**，
M1a 已经踩过这条，照那边的纪律办）。

**时钟注入**：`t_abs_us` 是参数，函数内绝不读系统时钟——sans-io 约束加上
spec §11.2 的确定性模拟测试要求可重放。

若 `Action` / `Reason` 的取值需要补进 `FORMAT.md` §10.3，本任务一并做。

- [ ] 测试：每种 action 至少一条，断言七个字段齐全且取值正确；
  `decide` 与 `decide_traced` + `NullSink` 对同一输入返回相同结果（可失败的薄壳断言）。

---

### Task 4: 错误类型与 ErrorClass 映射

**Files:**
- Modify: `crates/arca-core/src/error.rs`

这是 M0 遗留的 `TODO(M0)`。`error.rs` 的 doc comment 已经定好了方向：
**不重新发明分类**，映射到 `arca_format::trace::ErrorClass`（`retryable` / `needs_human` /
`protocol` / `bug`），码表在 `PROTOCOL.md` §7。

- [ ] 定义 `CoreError` 及其变体，每个变体有 `fn class(&self) -> ErrorClass` 与
  `fn code(&self) -> &'static str`（对应 `PROTOCOL.md` §7 的码）。
  `NeedsHuman` 决策对应的错误必须是 `needs_human`——agent 只看 class 就知道要停下。
- [ ] `PROTOCOL.md` §7 补上本切片新增的码。
- [ ] 测试：每个变体的 class 与 code 都被断言；code 互不重复。

---

### Task 5: 收敛性属性测试（proptest）

**Files:**
- Create: `crates/arca-core/tests/convergence.rs`

spec §11.2 要求的「收敛性属性测试：任意操作交错 + 任意崩溃点，最终三态收敛，
且**无任何路径销毁数据**（I3 作为可执行断言）」。

- [ ] **性质 1（决策全域性）**：对任意 `(base, local, remote)` 组合，`decide` 必须返回
  一个 `Decision`，绝不 panic。用 proptest 生成任意三态。
- [ ] **性质 2（I3：无销毁）**：遍历任意输入，产出的 `Action` 永远不属于「销毁数据」类。
  在注释里写明这条测试守护的是什么承诺，以及为什么它必须是属性测试而不是几个例子。
- [ ] **性质 3（幂等）**：应用一个决策的效果之后再 `decide` 一次，必须得到 `Noop`。
  这需要一个「应用决策到三态」的模型函数（纯函数，测试内定义即可）——
  它本身也是对决策表语义的第二次表述，两者不一致就说明表有问题。
- [ ] **性质 4（收敛）**：从任意初始三态出发，反复「decide → 应用」，
  必须在有限步内到达 `Noop`（不震荡）。给一个明确的步数上限（例如 8），
  超过就是 bug。

**这四条性质里，2 和 4 是本切片最有价值的产出**——它们把 spec 里的两句承诺
变成了机器每次提交都会检查的断言。

---

### Task 6: 确定性模拟测试

**Files:**
- Create: `crates/arca-core/tests/simulation.rs`

spec §11.2 的第一条：「确定性模拟测试：sans-io 状态机 + 模拟时钟/网络/文件系统，
随机事件序列 + 崩溃注入 + 种子可复现——Dropbox Nucleus 的核心教训」。

- [ ] 建一个测试内的模拟世界：一个 `SimClock`（单调递增的 `u64` 微秒）、
  一个 `SimStore`（`HashMap<路径, 三态>`）、一个种子驱动的事件生成器
  （本地改动 / 远端改动 / 本地删除 / 远端 tombstone / 崩溃）。
- [ ] **种子可复现**：测试失败时必须打印种子，且用同一种子重跑必然复现。
  这一条要有它自己的测试（跑两次同一种子，断言事件序列逐条相同）。
- [ ] **崩溃注入**：在「决策已产出但尚未应用」的点注入崩溃，重启后重新 `decide`，
  断言不会导致数据销毁、且最终仍收敛。
- [ ] 断言整个模拟过程中 `VecSink` 收到的 `reconcile.decide` 事件序列与实际决策一一对应
  （trace 不能漏事件——漏了就等于事故现场少了线索）。

---

## Self-Review

**范围**：M1b 只做**决策**，不做执行。上传/下载/改名/删除的真实 IO 属于 M1d 的 CLI
与 M2 的 arcad。这个边界就是 sans-io 约束本身。

**有意留给后续的**：
- 改名检测（同一 item_id 换了路径）需要索引参与，属 M1d 扫描侧的职责；
  本切片的决策表以「单个路径的三态」为单位，改名在那一层表现为
  「旧路径 tombstone + 新路径新增」，M1d 负责在扫描时把它们识别成一次改名
- 冲突副本的命名（`原名 (设备名 的冲突副本 日期).ext`）属 M2 的 `conflict.rs`
- 四道闸门的执行侧（read_roots 范围检查、单点确认）属 M1d

**类型一致性**：`BaseState` / `LocalState` / `RemoteState` 在 Task 1 定义，
Task 2–6 全部使用；`Decision` 在 Task 2 定义，Task 3 的 trace 发射与 Task 5/6 的
属性测试都基于它。
