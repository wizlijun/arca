# M2d 副本角色、多 hub 故障域与拔盘演练 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 闭合归档产品的信任模型——引入 server/client 副本角色、让多 hub 的故障域真正独立、并把「拔盘」从一句承诺变成自动化演练。

**Architecture:** 角色是**设备本地决策**（存 `<dataset>/.arca/client/`，不进 git），它只改变**执行侧**对删除的处置，**不改变 `arca-core` 的决策**——决策表说「可以删」，server 角色的执行侧仍然只把数据移进本地 trash。

**Tech Stack:** Rust 2021 / MSRV 1.85 · 已有的 `arca-cli` / `arca-store` / `arcad`

---

## 为什么这块是 M2 的收口

spec §4.7 用一句话概括了它要闭合的东西：

> **server 承诺「本地永远有完整数据，任何云侧语义都不会缩减它」；client 把本地视为
> 可再生缓存。** 归档产品的信任模型由此闭合：**只要任一 server 副本存活，
> 数据就在自己手里。**

M2a 已经建好了删除的四道闸门，但**所有绑定目前都被当成 client 角色对待**——
过了闸门就移除本地副本。一个把外置大盘当作离线备份的用户，会发现他的备份
跟着云侧语义一起缩水。这正是归档产品最不能接受的。

而 spec §12.3 的 M2 验收里点名了「**拔盘演练：卷离线呈现为数据集离线而非空库**」——
M1a/M2b 已经把 I11 的机制建好并各自攻击过，本切片要把它做成**自动化演练**，
像 M0 的逃生舱恢复演示一样进 CI，而不是靠人记得去试。

## Global Constraints

- MSRV **1.85**，edition 2021。依赖要求高于 1.85 **报告而非降级钉版**。
- 各 crate 保持 `#![forbid(unsafe_code)]`；**`arca-core` 一行不改**——
  角色只影响执行侧，决策表不认识角色。若你认为需要改它，**先停下报告**。
- tokio/axum 只允许在 `arcad`；`arca-cli` 无异步运行时。
- 只在 `main` 分支工作。提交信息用中文。文档与注释一律中文；已有 doc comment 保留。
- 四项门禁：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo fmt --all -- --check`、`cargo +1.85 check --workspace --locked --all-targets`。
- **I3**：本切片不得新增任何销毁路径。server 角色的处置是**更保守**，不是更激进。
- **I11**：卷离线一律呈现为数据集离线，绝不空库。
- **角色不进 git**（spec §4.3 的表格：「角色 · 驻留策略 · 基线」都在 `<ds>/.arca/client/`，
  设备差异不进共享配置）。

## 已交付、可直接使用的接口

```rust
arca_cli::baseline                       // <dataset>/.arca/client/baseline.jsonl 的读写
arca_cli::gates::{check_delete_transport, GateFailure}
arca_cli::trash::move_to_trash
arca_cli::transport::{Transport, LocalTransport, HttpTransport}
arca_cli::dataset::resolve               // .gitarca → 存储根 / transport
arca_store::root::{StorageRoot, MountError}
```

---

## File Structure

| 文件 | 职责 |
| --- | --- |
| `crates/arca-cli/src/role.rs`（新建） | 角色的读写与语义（`<ds>/.arca/client/role.toml`） |
| `crates/arca-cli/src/sync.rs`（改） | `DeleteLocal` 的执行按角色分流 |
| `crates/arca-cli/src/commands/porcelain.rs`（改） | `arca role`；`arca status` 的健康度与副本数报告 |
| `crates/arca-conformance/tests/drills/`（新建） | 拔盘演练（自动化，进 CI） |
| `FORMAT.md` | `role.toml` 的格式（先写） |

---

### Task 1: 角色的存储与语义

**Files:** `crates/arca-cli/src/role.rs`（新建）、`FORMAT.md`

- [ ] **先写 `FORMAT.md`**（I10）：`<dataset>/.arca/client/role.toml` 的字段。
      至少 `schema` 与 `role ∈ server | client`。默认值要写明——
      **未声明时按哪个算？** 我倾向 `client`（保守：不承诺永久保留），
      但你判断并写进文档与理由。
- [ ] `role::read(dataset_root) -> Result<Role, RoleError>`、`role::write(...)`。
      文件缺失 = 默认角色，**不是错误**（老仓库的正常情形）；
      文件存在但内容非法 = **错误**（I5：别猜）。
- [ ] `arca role <path>` 显示当前角色；`arca role <path> --set server|client` 设置。
      设置为 server 时要提示这意味着什么（永不释放空间）。
- [ ] 测试：缺失走默认；非法报错；往返一致；`.arca/client/` 确实不被 git 追踪
      （用 `arca-git` 的 `check_ignore_no_index` 实测，别只看文本）。

---

### Task 2: 角色改变删除的执行侧

**Files:** `crates/arca-cli/src/sync.rs`

spec §4.7 的表格里，两种角色对「远端 tombstone」的处置不同：

| | server 角色 | client 角色 |
| --- | --- | --- |
| 远端 tombstone 到达 | **数据移入本地 trash 保留期**，物理销毁只经显式 GC | 四道闸门通过后移除本地副本 |

**关键**：`arca-core` 的决策表**不认识角色**——它仍然给出 `DeleteLocal`。
角色只在**执行侧**分流。这个分工要在代码注释里写明，否则下一个人会想把角色塞进决策表。

- [ ] client 角色：维持现状（四道闸门 → `fs::remove_file`）。
- [ ] **server 角色：过闸门之后不 unlink，而是把本地副本移进本地的 trash**
      （复用 M2a 的 `move_to_trash` 形态，但落在**工作区侧**而不是存储根侧——
      需要一个本地 trash 位置，建议 `<ds>/.arca/client/trash/`，**先写进 `FORMAT.md`**）。
- [ ] 报告文案要区分：client 是「已移除本地副本」，server 是
      「已移入本地回收站（server 角色永不释放空间）」。
- [ ] 测试：同一个远端 tombstone，两种角色的**本地文件系统终态不同**；
      server 角色下原文件仍可从本地 trash 找回；**两种角色下 hub 侧都不受影响**。

---

### Task 3: 多 hub 独立故障域（客户端侧）

**Files:** `crates/arca-cli/src/commands/porcelain.rs`、`dataset.rs`

spec §4.3.2：「daemon 为每个数据集维护独立的绑定、独立的 journal 游标、独立的传输队列
与退避状态。一个 hub 不可达时，**只有它承载的数据集进入离线态（I11），
其余数据集完全不受影响**——`arca status` 按数据集分别报告健康度。」

arcad 侧的独立故障域 M2b 已验证（14/14）。**本切片做客户端侧**：
一个 vault 里两个数据集分属两个 hub，断开其一。

- [ ] `arca sync`（不带路径 = 全部数据集）：一个 hub 不可达时，
      **其余数据集照常同步完成**，退出码反映「部分失败」而不是整体失败。
- [ ] `arca status`：按数据集分别报告，离线的明确标为离线**并说明是哪个 hub**。
- [ ] **绝不因为一个 hub 离线就跳过其余数据集**——那是最容易写出的错误形态
      （一个 `?` 就中止整个循环）。M1b 在 `into_result` 上踩过同构的问题。
- [ ] 测试：两数据集分属两 hub，断开其一 → 另一个的同步**实际完成了**
      （断言文件真的传过去了，不只是退出码）。

---

### Task 4: server 副本数告警

**Files:** `crates/arca-cli/src/commands/porcelain.rs`

spec §4.5：「`arca status` 报告每个数据集的 server 副本数，低于阈值（默认 2）即告警——
致敬 git-annex 的 numcopies。」

- [ ] hub 自己的存储根**即隐式 server 角色**（spec §4.7），所以基础副本数是 1。
- [ ] 本设备若是 server 角色则 +1。
- [ ] **诚实的边界**：本切片**无法知道其它设备的角色**——那需要 hub 侧登记绑定，
      属 M2e 或更后。所以告警文案必须写明「已知的 server 副本数」而不是绝对值，
      **别让用户以为这是全局真相**。这条要在报告里说明你怎么措辞的。

---

### Task 5: 拔盘演练（自动化，进 CI）

**Files:** `crates/arca-conformance/tests/drills/`（新建）、`.github/workflows/ci.yml`

spec §12.3 的 M2 验收：「**拔盘演练：卷离线呈现为数据集离线而非空库（I11）**」。

像 M0 的逃生舱恢复演示一样做成脚本 + CI 作业。**必须同时断言正反两面**——
只跑正例的演练是假绿（M0 时踩过这个）。

- [ ] 演练脚本：建 vault + 存储根 → adopt → sync → **把存储根移走（模拟拔盘）** →
      断言 `arca status` / `sync` / `verify` **全部报离线且退出码非 0**、
      **本地一个文件都没被删**；再把盘挂回来 → 断言恢复正常同步。
- [ ] 反面断言：如果某个命令在盘不在时**返回成功或空库**，演练必须失败。
- [ ] 同时演练 arcad 侧：起 arcad → 移走某个数据集的根 →
      断言该数据集 503 而**其余数据集照常 200**。
- [ ] 进 CI（可以和逃生舱演示同一个作业，或并列一个新作业）。

---

## Self-Review

**范围**：占位符层（OS 云文件、按需 hydration、pin/LRU）属 **M3**，本切片不碰——
client 角色在 M2 仍是「全量物化」，只是**语义上**声明了它把本地视为可再生缓存。
这个区别要在 `role.rs` 的 doc comment 里写明，否则会有人以为 M2 就有占位符了。

**`arca-core` 不改**：角色只影响执行侧。决策表给 `DeleteLocal`，角色决定
「移除」还是「移进本地 trash」。

**最容易写错的地方**：Task 3 的「一个 hub 挂了不影响其余」——用 `?` 中止整个循环是
最自然也最错的写法。测试要断言**其余数据集的文件真的传过去了**，不能只看退出码。
