# M2a tombstone 与删除安全地基 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让删除真正可以被执行——但先把「绝不销毁」的每一道闸门装好。交付：下载内容的 fsync 纪律、hub 侧 journal 落盘、tombstone 的写入与读取、删除传播的四道闸门、以及保留期内的 `arca restore`。

**Architecture:** tombstone 是 **journal 事件 + `.arca/trash/` 里的内容留存**，不是删除。`read_remote` 从 journal 补出 `RemoteState::Tombstoned`，`arca-core` 的决策表随即激活它那三个已经写好但至今不可达的格子。执行侧在真的移除本地副本之前必须过四道闸门。

**Tech Stack:** Rust 2021 / MSRV 1.85 · arca-format（journal 记录）· arca-store（原子写入）· arca-core（决策表，**不改**）

---

## 为什么这是 M2 的第一块

M1d 的切片评审留了一条明确的前置条件：

> `write_local_atomic` 完全不 fsync，而基线在其后保存。崩溃可能留下「基线持久、
> 下载的内容丢失」。下次 sync 看到 `(base=present, local=absent, remote=present)`
> → `TombstoneRemote`。M1 里只报告所以无害，**但 M2 真的执行 tombstone 之后，
> 这就变成崩溃引发的 hub 副本销毁**。

也就是说：**在删除传播被接通之前，这个洞必须先补上**，否则 M2 的第一个动作就是把
M1 建立的「绝不丢数据」信誉推翻。spec §12.3 把 M0–M2 的排序原则写得很清楚——
先建立信誉，再兑现体验。

M1b 的决策表里 `DeleteLocal` / `TombstoneRemote` / `both_deleted` /
`tombstone_for_unknown_item` 四个格子已经写好并被属性测试覆盖，但 `RemoteState::Tombstoned`
在 M1 里不可达（`read_remote` 只看 index+items）。本切片让它可达——
**`arca-core` 一行都不用改**，这正是当初把决策与执行分开的收益。

## Global Constraints

- MSRV **1.85**，edition 2021。`Cargo.lock` 已入库；依赖要求高于 1.85 **报告而非降级钉版**。
- 各 crate 保持 `#![forbid(unsafe_code)]`；`arca-core` 保持 **sans-io**（本切片不改它）。
- 只在 `main` 分支工作。提交信息用中文。文档与注释一律中文；
  文件顶部已有的中文 doc comment 必须保留。
- 四项门禁：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo fmt --all -- --check`、`cargo +1.85 check --workspace --locked --all-targets`。
- **I3 是本切片的全部主题**：删除 = tombstone；物理销毁只经显式 `arca gc`（M2 后续切片）。
  本切片不得出现任何**销毁 hub 内容**的代码路径。移除本地副本是允许的——
  但只在四道闸门全过之后，且 hub 的权威副本仍在保留期内。
- **I5**：闸门任一条不满足就停下并可诊断，绝不「尽力删」。
- 格式契约：`FORMAT.md` §7.2（journal 事件）已定义 `op ∈ upsert | tombstone | rename`
  与字段表。**按它实现，不要改它**；确有缺口先报告。

## 已交付、可直接使用的接口

```rust
arca_format::journal::{JournalEvent, Op, Cursor}        // parse_line / to_line / parse_stream
arca_format::hub_layout::{layout, parse_epoch_pointer}  // layout::{JOURNAL_DIR, EPOCH_FILE, TRASH_DIR}
arca_format::items::{parse_chain, to_line}
arca_store::root::StorageRoot                            // open / open_traced / join
arca_store::atomic::{write, Batch, sync_dir, sweep_tmp}
arca_core::state::{BaseState, LocalState, RemoteState}
arca_core::reconcile::{decide, decide_traced, Action, Decision}
```

`crates/arca-cli/src/{sync.rs, hub.rs, baseline.rs, scan.rs}` 是 M1d 交付的执行侧。

---

## File Structure

| 文件 | 职责 |
| --- | --- |
| `crates/arca-cli/src/sync.rs`（改） | 下载内容 fsync；接通删除执行 |
| `crates/arca-cli/src/hub.rs`（改） | 读 journal 补出 `RemoteState::Tombstoned` |
| `crates/arca-cli/src/journal.rs`（新建） | hub 侧 journal 的读写（append-only + epoch 指针） |
| `crates/arca-cli/src/gates.rs`（新建） | 删除传播的四道闸门 |
| `crates/arca-cli/src/trash.rs`（新建） | `.arca/trash/` 的写入与保留期查询 |
| `crates/arca-cli/src/commands/porcelain.rs`（改） | `arca restore` |

---

### Task 1: 下载内容的 fsync 纪律（关闭 M1d 的隐患）

**Files:** `crates/arca-cli/src/sync.rs`

M1d 的 `write_local_atomic` 把内容写进工作区却不 fsync，而基线在其后保存。
崩溃窗口里的状态是「基线持久、内容丢失」——下次调和会把它读成「本地被删了」。

- [ ] **写失败的测试先**：构造「基线已保存但内容缺失」的状态，断言下一轮
  `decide` 得到的是 `TombstoneRemote`（**证明隐患真实存在**），
  再断言修复后这个窗口关上了。
- [ ] 修法：下载的内容必须 **fsync 之后**才允许保存基线。
  复用 `arca_store::atomic` 的写入路径（它已经做了 tmp → fsync → rename → fsync 父目录），
  不要在 CLI 里另写一套。工作区不是存储根，所以可能需要一个不依赖 `StorageRoot` 的
  变体——**先看 `atomic` 现有的 API 能不能直接用**，不能的话在 `arca-store` 里加，
  别在 CLI 里复制粘贴一份 fsync 逻辑。
- [ ] 在代码注释里写明这个顺序防的是什么，引用本计划这一节。

---

### Task 2: hub 侧 journal 的读写

**Files:** `crates/arca-cli/src/journal.rs`（新建）

**Interfaces:**
- `journal::append(root: &StorageRoot, event: &JournalEvent) -> Result<(), JournalError>`
  ——整行原子追加，写完 fsync
- `journal::read_all(root: &StorageRoot) -> Result<(Cursor, Vec<JournalEvent>), JournalError>`
- `journal::current_epoch(root: &StorageRoot) -> Result<Option<String>, JournalError>`
  ——用 `parse_epoch_pointer`，三态处置照 `FORMAT.md` §4

**纪律**（`FORMAT.md` §7.2 已定）：末行不完整 → 截断到最后一个完整行；
**中间行损坏 → 失败**（journal 是真相，读错一行等于伪造历史）。
`arca_format::journal::parse_stream` 已经实现了这条与 `seq` 连续性校验，直接用。

- [ ] epoch 指针不存在时：按 §4，那是**合法的未初始化态**，第一次追加时创建。
- [ ] 测试：追加后可读回；末行撕裂被截断；中间行损坏失败；`seq` 空洞被拒绝；
  epoch 缺失时的初始化。

---

### Task 3: tombstone 的写入与读取

**Files:** `crates/arca-cli/src/trash.rs`（新建）、`crates/arca-cli/src/hub.rs`（改）

**tombstone 是什么**：一条 `op="tombstone"` 的 journal 事件 + 内容被移进 `.arca/trash/`。
**不是删除**——`files/` 下的内容移走而不是 unlink，保留期内 `arca restore` 能找回。

- [ ] `trash::move_to_trash(root, path, item_id) -> Result<TrashId, ..>`：
  内容从 `files/<path>` **移动**到 `.arca/trash/<trash_id>.data`，
  旁边写 `.arca/trash/<trash_id>.meta` 记录原逻辑路径、item_id、删除时间。
  用 rename（同一文件系统，原子），**绝不 copy+unlink**（那有丢数据的窗口）。
- [ ] `hub::read_remote` 扩展：读 journal，若某 item 的最后一条事件是 `tombstone`，
  产出 `RemoteState::Tombstoned{item_id, version_id}`。
  **这一步让决策表那三个格子第一次可达**。
- [ ] 测试：写 tombstone 后 `read_remote` 产出 `Tombstoned`；`files/` 下内容已不在
  但 `.arca/trash/` 里在；`decide` 对 `(present, unchanged, tombstoned)` 给出 `DeleteLocal`。

---

### Task 4: 删除传播的四道闸门

**Files:** `crates/arca-cli/src/gates.rs`（新建）、`crates/arca-cli/src/sync.rs`（改）

spec §5.3 与 §6：远端 tombstone 到达本地时，**四道闸门全过才允许移除本地副本**。
这是 I3 在执行侧的形态——决策表说「可以删」，闸门说「现在真的安全吗」。

四道（按 spec §6 继承 lazync）：

1. **read_roots 范围**：要删的路径必须在本次调和实际扫描过的范围内。
   没扫到就删，等于拿一份不完整的观察去销毁数据。
2. **单点确认**：远端明确给出了 tombstone，而不是「查不到记录」。
   `remote_vanished_without_tombstone` 那格已经在决策层挡住了，闸门是第二道。
3. **基线一致性**：本地内容必须与基线记录的哈希一致——即「本地没有未同步的修改」。
   决策表用 `LocalClass` 判过一次，闸门在执行前**重新读一次实际字节**再判一次
   （调和与执行之间有窗口，文件可能被改了）。
4. **保留期存在**：hub 的 `.arca/trash/` 里确实有这份内容且未过保留期。
   本地副本被移除后，权威副本必须仍然可取回——否则这就是销毁。

- [ ] `gates::check_delete(...) -> Result<(), GateFailure>`，`GateFailure` 逐条可区分。
- [ ] 任一闸门不过 → **不删**，报告并计入退出码 1（I5：停下并可诊断）。
- [ ] 测试：四道闸门各有一条「该拦住」的用例 + 一条全过的用例。
  第 3 道要构造「调和后、执行前文件被改」的竞态。

---

### Task 5: `arca restore` 与保留期

**Files:** `crates/arca-cli/src/commands/porcelain.rs`（改）、`trash.rs`（改）

spec §7：保留期默认 180 天，期内 `arca restore` 一条命令找回。

- [ ] `arca restore <path>`：从 `.arca/trash/` 取回内容，写回 `files/`，
  在 journal 追加一条 `upsert`（**新版本，不是复活旧版本**——
  spec §4.1「删除后重建 = 新身份」的同构物，但 restore 保持 item_id 不变、
  只是新版本；如果这与 §4.1 冲突，**先停下报告**）。
- [ ] `arca restore --list`：列出保留期内可恢复的条目。
- [ ] 保留期只是**元数据里的一个时间戳**——本切片不做任何过期清理。
  物理销毁属 `arca gc`，是 M2 后续切片，且**必须显式触发**（I3）。

---

## Self-Review

**范围**：本切片只做 `file://` 直连下的 tombstone。HTTP API、longpoll、arcad 本体
属 M2b/M2c。多卷映射与角色属 M2d。

**`arca-core` 不改**：决策表的四个 tombstone 格子已经就位且被属性测试覆盖。
如果实现过程中发现需要改它，**先停下报告**——那说明决策表有缺口，需要单独评审。

**验收对齐**：spec §12.3 的 M2 验收要求「两机改名/删除/冲突全场景（纯手动命令完成）」，
本切片交付其中的删除场景；改名与冲突在 M2b/M2c。
