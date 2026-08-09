# M3c 分级驻留策略与水化调度 实现计划

**Goal:** 落地 spec §4.8——**占位符是给「大而冷」的文件用的，不是给所有文件用的**。
并把 §6.3 第 7 条「全库索引水化字节数 = 0」做成必过测试。

**Architecture:** 策略引擎是**纯函数**，与 OS 占位符层完全解耦。OS 那一侧由一个
`Provider` trait 抽象，本切片交付它的**全量物化实现**（spec §3.1 强制的降级路径），
CfAPI/File Provider 分别是 M3d/M4 的另外两个实现。

**Tech Stack:** Rust 2021 / MSRV 1.85 · 无新依赖

---

## 为什么策略引擎必须先于占位符层落地

spec §4.8 的三条实现要求里，**只有第二条依赖 OS 能力**（按区间响应 `FETCH_DATA`）。
体积分级、热度预取、pin/LRU、并发上限与批量合并——全部是**纯判断**，
不需要任何占位符 API 就能写出来、测出来。

而 §6.3 第 7 条那条必过测试（「全库索引水化字节数 = 0」）真正在验的东西是
**策略引擎会不会因为一次元数据遍历就决定去拉内容**。这条在全量物化实现下
同样成立、同样有意义：全量物化时字节数恒为 0（本来就都在本地），
占位符实现接上后这条测试**一个字都不用改**就能继续守着它。

先写策略、再接 OS，顺序不能反——反过来会让「策略」散落在 CfAPI 回调里，
到 M4 写 macOS 时无从复用，两个平台的行为必然分叉。

## Global Constraints

- MSRV **1.85**。**每个任务自己跑完四项门禁再交**。
- `#![forbid(unsafe_code)]`。**`arca-core` 一行不改**。
- 只在 `main` 分支。提交信息、文档、注释一律中文。
- **策略引擎不做 IO**——它是纯函数，输入是「文件的元数据 + 访问意图 + 配置」，
  输出是「该不该驻留 / 该取多少字节」。这让它能被穷举测试，也让两个 OS 实现
  共用同一段判断（与 `arca-core` sans-io 同一条理由）。
- **client 角色在 M3c 仍是全量物化**——本切片交付的是**策略与调度**，
  不是占位符。任何「现在就能省磁盘了」的说法都是过度承诺。

---

### Task 1: 访问意图与驻留决策（纯函数）

**Files:** `crates/arca-agentd/src/hydration.rs`

- [ ] `Intent`：`Metadata`（stat/列目录）、`Head { bytes }`（读文件头，缩略图/类型嗅探）、
      `Full`（真的要整个文件）。**这个枚举是整条防线的入口**——
      §4.8 要求 2 的全部内容就是「别把前两种当成第三种」。
- [ ] `Policy { resident_max_bytes: u64（默认 8 MiB）, hot_days: u32（默认 14）, ... }`。
- [ ] `decide(file: &FileFacts, intent: Intent, policy: &Policy) -> Decision`，
      `Decision` ∈ `AlreadyLocal` / `NoFetch`（元数据够了）/ `FetchRange { .. }` / `FetchFull`。
- [ ] **穷举测试**：`Intent × 体积档位 × pin 状态` 的每一格都要有断言，
      不留 `_ =>` 兜底（与 `arca-core` 的 18 格决策表同一手法）。

### Task 2: 「全库索引水化字节数 = 0」（§6.3 第 7 条，必过）

**Files:** `crates/arca-agentd/src/hydration.rs` 或 `tests/`

- [ ] 模拟一次全库遍历：对**每一个**受管文件依次发 `Intent::Metadata`，
      断言**累计 `FetchFull` 字节数为 0**。
- [ ] 再模拟缩略图服务：对每个文件发 `Intent::Head { bytes: 4096 }`，
      断言产生的是 `FetchRange` 而**不是** `FetchFull`——
      「读了 4 KB 就把 2 GB 视频拉下来」正是要挡的那个故障模式。
- [ ] **反面断言**：把策略故意改坏（让 `Metadata` 也返回 `FetchFull`），
      这条测试必须失败。不验反面的必过测试还是假绿（本项目已被咬四次）。

### Task 3: 水化队列——并发上限与批量合并

**Files:** `crates/arca-agentd/src/hydration.rs`

- [ ] 同一路径的重复请求**合并成一个**（30 张图的笔记不该发 30 次重复拉取）。
- [ ] 并发上限可配，默认小（建议 4）——目标是 NAS 上行不被打满（spec §1.1）。
- [ ] 队列有界；满了**拒绝并说明**，不是无限堆积（M2b/M2c 的内存上限教训）。
- [ ] 测试：50 个并发请求同一路径 → 只产生 1 次拉取；不同路径 → 并发数不超过上限。

### Task 4: `Provider` trait 与全量物化实现

**Files:** `crates/arca-agentd/src/projection.rs` 或新建

- [ ] `Provider` trait：`ensure_local(path, intent)`、`evict(path)`、`capabilities()`。
- [ ] `FullMaterialization`：**spec §3.1 强制的降级路径**——不支持占位符的平台
      （Linux、以及注册失败的 Windows/macOS）走它，行为是「所有文件本来就在本地，
      `ensure_local` 是 no-op，`evict` **拒绝**」。
- [ ] `evict` 在全量物化下拒绝这一条要写清楚理由：全量物化承诺的就是
      「本地永远有完整数据」，一个会驱逐的全量物化是自相矛盾的。
- [ ] 测试：`capabilities()` 如实报告不支持占位符；`evict` 返回明确的拒绝而不是静默成功。

---

## Self-Review

**范围**：真正的占位符注册（CfAPI / File Provider）是 M3d/M4，本切片不碰。
交付的是「策略 + 调度 + 抽象边界」，以及那条必过测试的可执行形态。

**最容易过度承诺的地方**：写完这一切之后，很容易在归档里说「分级驻留已实现」。
实际实现的是**策略**；没有占位符层，client 角色仍然全量物化，磁盘一个字节都没省。
归档必须把这条说清楚，否则下一个人会以为 M3d 只剩「接个 API」。

**最容易写错的地方**：把 `Intent` 当成建议而不是约束。如果 `FetchRange` 的实现
偷偷读了整个文件再切一段，§6.3 第 7 条那条测试仍然通过（它数的是决策，不是字节），
而真实故障模式原封不动。所以 Task 4 的 `Provider` 实现里，**读多少字节要能被
测试观测到**，不能只测决策。
