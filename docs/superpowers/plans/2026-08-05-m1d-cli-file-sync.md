# M1d CLI 与 file:// 同步闭环 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 M1a（存储根 IO）、M1b（调和决策）、M1c（git 接缝）接成一个**无需任何 daemon 的完整同步闭环**，并跑通 spec §12.3 的 M1 验收演示。

**Architecture:** CLI 是唯一的执行者。扫描本地 → 读基线 → 读存储根 → 交给 `arca_core::decide` 出决策 → 用 `arca_store::atomic` 执行 → 更新基线。`file://` 不是一种「传输协议」，它就是「dataset_root 在本地文件系统上」这一事实——所以同步退化成两个目录之间的调和，没有网络、没有守护进程。

**Tech Stack:** Rust 2021 / MSRV 1.85 · arca-core（决策）· arca-store（存储根 IO）· arca-format（格式）· arca-chunk（哈希）· arca-git（git 接缝）· clap · walkdir

---

## 为什么 `file://` 先于 HTTP

spec §3.1：这让 M1 像早期的 git——**先有对象模型 + 一种最朴素的同步闭环，
HTTP 只是之后加上的另一种 transport**。如果先做 HTTP，对象模型的问题会被网络问题掩盖；
先做 `file://`，任何不对都只能是模型自己的错。

M2 加 `arcad` 时，`file://` 这条路径**必须继续可用**——它是 spec §3.1 分层降级的最底层，
也是「NAS 直插、USB sneakernet」的真实用法。

## Global Constraints

- MSRV **1.85**，edition 2021。依赖要求高于 1.85 **报告而非降级钉版**。
- 各 crate 保持 `#![forbid(unsafe_code)]`；不得引入 tokio 或异步运行时
  （M1 是一次性进程，没有并发 IO 的需求；引入运行时是 M2 的事）。
- **`arca-core` 依旧不得被污染**：CLI 里任何「该做什么」的判断都必须来自
  `arca_core::decide`，不得在 CLI 里另写一套 if-else。这是两端共用的根基。
- 只在 `main` 分支工作。提交信息用中文。文档与注释一律中文；
  文件顶部已有的中文 doc comment 必须保留。
- 四项门禁：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo +1.85 check --workspace --locked --all-targets`、`cargo fmt --all -- --check`。
- **I6 受管文件原地不动**：`adopt` 绝不改名、不移动、不换成指针或符号链接。
- **I3 无销毁权**：CLI 的任何路径都不得删除用户数据，除非是 tombstone 到达后
  移除本地副本（且权威副本在 hub 的保留期内）。
- **CLI 纪律（spec §3.2）**：成功时安静；数据走 stdout、诊断走 stderr；
  处处可加 `--json`；与 git 同名的动词语义必须一致。

## 已交付、可直接使用的接口

```rust
// arca-core（sans-io 决策）
arca_core::state::{BaseState, LocalState, RemoteState}
arca_core::reconcile::{decide, decide_traced, Decision, Action}
arca_core::error::CoreError                       // -> ErrorClass

// arca-store（存储根 IO）
arca_store::root::{StorageRoot, MountError}       // open / open_traced / join -> Result
arca_store::atomic::{write, sweep_tmp, AtomicError}
arca_store::fsck::{check_root, check_path, FsckReport, Problem}

// arca-format
arca_format::{gitarca::Registry, dataset::DatasetConfig, manifest::{Manifest, ManifestEntry}}
arca_format::{items, index, journal, hub_layout::{layout, FormatJson}}
arca_format::path_rules::{check, casefold, index_key, PathStatus}
arca_format::trace::{TraceSink, TraceRecord, EventKind, Sid, RingSink, VecSink, NullSink}

// arca-chunk
arca_chunk::hash::ContentHash                     // from_bytes / hasher() 流式

// arca-git
arca_git::repo::{Repo, GitError}
arca_git::ignore_block::{render, upsert, remove}  // 返回 Result
arca_git::tracking::{check_vault, Issue}
arca_git::hooks::{install_pre_push, uninstall_pre_push}
```

**动手前先读这些模块的实际签名**——它们在各自切片的评审里改过多次，brief 可能落后。

---

## 本切片的范围与不做的事

**做**：扫描 · 基线 · `init` / `register` / `adopt` · `file://` 同步闭环 ·
`status` / `verify` / `doctor` · plumbing（`ls` / `cat` / `resolve` / `state dump`）·
trace 失败落盘 · 验收演示。

**不做**（spec §12.3 的 M1 行里有，但依赖尚未就绪，留作 M1e 或并入 M2）：
`history` / `restore` / `gc` / `bundle`——它们都依赖 `.arca/trash/` 与 journal 的
完整实现，而那两块的格式在 `FORMAT.md` 里标着「M2 定义」。
**在计划里明确写出来，而不是悄悄少做。**

---

## File Structure

| 文件 | 职责 |
| --- | --- |
| `crates/arca-cli/src/scan.rs`（新建） | 遍历数据集目录、算哈希、产出 `LocalState` 集合 |
| `crates/arca-cli/src/baseline.rs`（新建） | `.arca/client/` 基线的读写（可抛弃投影，I9） |
| `crates/arca-cli/src/hub.rs`（新建） | 从存储根读出 `RemoteState` 集合 |
| `crates/arca-cli/src/sync.rs`（新建） | 闭环：scan → decide → execute → 更新基线 |
| `crates/arca-cli/src/commands/*.rs` | 各命令的薄壳 |
| `crates/arca-cli/src/trace_sink.rs`（新建） | trace 的失败落盘 |
| `crates/arca-cli/tests/e2e.rs`（新建） | 端到端：两个目录之间的完整同步 |

---

### Task 1: 本地扫描

**Interfaces:** `scan::{scan_dataset(root: &Path, sink: &mut dyn TraceSink) -> ScanResult, ScanResult { files: BTreeMap<String, LocalState>, rejected: Vec<(String, PathStatus)>, bytes: u64 }}`

- 遍历数据集目录，**跳过 `.arca/`**（元数据不是受管内容）
- 每个文件过 `path_rules::check`——不合规的进 `rejected` 并发 `path.reject` trace 事件
  （事件与字段见 `FORMAT.md` §10.3），**绝不静默跳过**
- 算 BLAKE3（流式，别把整个文件读进内存——1 万张照片的验收标准在这里）
- 产出按路径排序的 `BTreeMap`，保证确定性
- 发一条 `scan.summary`（`files` · `bytes` · `rejected`）
- [ ] 测试：正常目录 · 含不合规路径 · 空目录 · `.arca/` 被跳过 · 符号链接的处置
  （**决定并说明**：跟随还是跳过？跟随会导致同一份内容被算两次且可能逃出数据集，
  建议跳过并计入 `rejected`）

### Task 2: 基线（客户端投影）

**Interfaces:** `baseline::{load(dataset_root: &Path) -> Result<Baseline, BaselineError>, Baseline::{get(path) -> BaseState, set(path, BaseState), remove(path), save(&self, dataset_root) -> Result<(), BaselineError>}}`

- 落在 `<dataset>/.arca/client/baseline.jsonl`——**gitignored**（设备差异不进共享配置）
- 格式用 JSON Lines，与 hub 侧的 items 同风格；**首行带版本号**，损坏时按 I5 报错
- **I9：基线是可抛弃投影**。损坏或缺失时不是灾难——`load` 返回空基线并**告知调用方**
  （返回一个 `was_reset: bool` 或类似信号），让 `arca status` 能提示「基线已重建，
  本轮会做全量对账」。悄悄当成空基线是不可接受的（那会让所有文件看起来像新增）
- [ ] 测试：往返 · 损坏行 · 缺失文件 · 版本号高于已知时拒绝

### Task 3: 从存储根读远端状态

**Interfaces:** `hub::{read_remote(root: &StorageRoot) -> Result<BTreeMap<String, RemoteState>, HubError>}`

- 读 `.arca/index/` 得到路径 → item_id，读 `.arca/items/` 得到版本链的当前版本
- tombstone 的表达：当前版本是 tombstone 记录时产出 `RemoteState::Tombstoned`
- **损坏的记录按 I5 报错，不跳过**——这与 fsck 的纪律一致
- [ ] 测试：健康存储根 · 空存储根 · 损坏的 items · index 与 items 不一致

### Task 4: `arca init` / `arca register`

- `init`：在 vault 根建 `.gitarca`（若已存在则校验后不覆盖）、装 pre-push 钩子（可 `--no-hook` 跳过）
- `register <path> --hub <name>`：把一个目录登记为数据集——建 `<path>/.arca/dataset.toml`、
  更新 `.gitarca`、**更新 `.gitignore` 反选块**
- 两个命令都要在做任何写入前跑 `tracking::check_vault`，有 `Issue` 就停下报告（I5）
- [ ] 测试：真建 git 仓库跑完整流程，然后断言 `git check-ignore` 的实际结果

### Task 5: `arca adopt`——就地纳管

**这是 M1 验收的核心命令。**

- 扫描数据集目录 → 对每个文件算哈希 → 写进存储根（`files/` 平放 + `items/` + `index/`）
  → 生成清单 → 更新 `.gitignore` 块
- **I6：文件原地不动**——不改名、不移动、不换成符号链接。测试要断言 inode/mtime 不变
- 验收断言（spec §12.3）：**`git status` 干净**、清单进 git、受管二进制不进 git
- 内容相同的文件走**零传输认领**（`AdoptBaseline` 那两格的执行侧）
- [ ] 测试：真实 git 仓库 + 若干文件 → adopt → 断言上面每一条

### Task 6: `file://` 同步闭环

**Interfaces:** `sync::{sync(dataset: &Path, root: &StorageRoot, sink: &mut dyn TraceSink) -> Result<SyncReport, SyncError>}`

对每个路径：`decide(base, local, remote)` → 按 `Action` 执行：

| Action | 执行 |
| --- | --- |
| `Noop` | 无 |
| `Upload{parent}` | 写 `files/` + 追加 `items/` + 更新 `index/`；parent 用于 CAS 校验（本地直连时校验远端当前版本是否仍是 parent，不符则重新调和） |
| `Download{version_id}` | 从存储根读出内容写到本地（`atomic::write`） |
| `AdoptBaseline{hash, version_id}` | **零传输**，只更新基线 |
| `DeleteLocal{item_id}` | 移除本地副本（权威副本在 hub） |
| `TombstoneRemote{item_id, parent}` | 在 `items/` 追加 tombstone 记录 |
| `Conflict{..}` | **不动数据**，计入报告，退出码非 0 |
| `NeedsHuman{..}` | **停下**，计入报告，退出码非 0 |

- 每个决策发一条 `reconcile.decide`（用 `decide_traced`）
- 执行完更新基线并保存
- **决策全部来自 `arca_core::decide`**——CLI 里不得有第二套判断逻辑
- [ ] 端到端测试：两个临时目录，制造各种三态组合，跑 sync，断言收敛且文件内容正确

### Task 7: `status` / `verify` / `doctor` + plumbing

- `status`：跑扫描与调和但**不执行**，按数据集报告；Rule of Silence——全同步时安静
- `verify`：fixity 巡检，复用 `arca_store::fsck::check_path`
- `doctor`：`tracking::check_vault` + **「本地存在但 hub 尚无副本」的告警**
  （这是 M1c 留下的义务：`git clean -xdf` 风险的唯一缓解措施）；
  `Issue::CheckIncomplete` 必须显式呈现，不能当成干净
- plumbing：`ls --json` · `cat <hash>` · `resolve <path>` · `state dump --json`；
  输出格式进 `PROTOCOL.md` §5
- [ ] 测试：每个命令的退出码与静默性；`doctor` 对未上传文件的告警

### Task 8: trace 失败落盘 + 验收演示

- trace：用 `RingSink` 收集，**仅在失败时**落盘到 `<dataset>/.arca/client/trace/`；
  保留最近 N 次（spec §3.3）；`ARCA_TRACE_EVENT` 环境变量可强制落盘
- **验收演示**（spec §12.3 的 M1 行）：
  - 在一个真实的 Obsidian 式目录上 `arca adopt`：文件原地不动、`git status` 干净、
    清单进 git、后续提交不再增长
  - **1 万个文件 2 分钟内**去重归档 + 全量校验——写成一个 `#[ignore]` 的基准测试
    （不进常规 CI，但可手工跑），报告实测耗时
- [ ] 若 1 万文件跑不进 2 分钟：**报告实测数字**，不要改标准。可能的瓶颈是逐文件
  fsync（M1a 已知的「每文件一次 create_dir_all + 一次目录 fsync」），
  那说明该做批量 sync，属于真实发现

---

## Self-Review

**验收标准的诚实注解**（spec §12.3 已写明，此处重申）：`arca adopt` 让**后续提交不再增长**，
但已经 commit 过的二进制仍留在 git 历史里——`git rm --cached` 只影响索引与未来提交，
仓库体积不会自动回落。存量瘦身需要用户自行 `git filter-repo`，arca 只提供指引不代劳。
**这一条必须出现在 `arca adopt` 的输出里**，否则用户预期必然落空。

**范围边界**：`history` / `restore` / `gc` / `bundle` 明确不做，理由见上。
