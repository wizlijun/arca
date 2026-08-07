# M1b · 调和状态机

**完成于 2026-08-05** · 14 个提交 · 15 个文件、3278 行新增 · 结束时 242 个测试全绿
（M1b 新增 35 条）· `arca-core` 共 2959 行

M1 的第二块。这是整个项目的**智力核心**：给定「基线 × 本地 × 远端」三态，
决定该做什么。18 格决策表，纯函数，无任何 IO。

---

## 为什么这块最要紧

spec §3 的第一条架构约束是「arca-core 是同一份对账状态机，两端共用」。
客户端与 hub 对同一个文件必须跑同一段代码得出同一个结论——否则会出现
**「双方各自认为自己对」**的错误，而这类错误在同步系统里最难查也最伤。

这也是不变量 I3「同步路径无销毁权」唯一能被**可执行地**断言的地方：
决策表是纯函数，输入空间可穷举，于是「没有任何一条决策路径会销毁数据」
可以是一条机器每次提交都检查的断言。在别处它只能是一句承诺。

---

## 交付了什么

| 文件 | 内容 |
| --- | --- |
| `state.rs` | 三态词汇：`BaseState` / `LocalState` / `RemoteState` 及其分类 |
| `reconcile.rs` | `decide(base, local, remote) -> Decision`——18 格穷举决策表 + `reconcile.decide` trace 发射 |
| `error.rs` | `CoreError` 映射到 `arca_format::trace::ErrorClass`，不另起一套分类 |
| `tests/decision_table.rs` | 23 条穷举用例，逐格断言 action 与其**携带字段** |
| `tests/convergence.rs` | 四条 proptest 性质 |
| `tests/simulation.rs` | 种子可复现的确定性模拟 + 两种崩溃注入 |
| `tests/common/mod.rs` | 共享的 `World` / `apply_decision` 模型 |

**决策表里两条贯穿全表的纪律**：

1. **没有任何一格的动作是销毁数据。** `DeleteLocal` 移除的是本地副本，而权威副本在
   hub 的 trash 保留期内；`TombstoneRemote` 记的是墓碑不是销毁。物理销毁只经显式 `arca gc`。
2. **模糊必停。** `remote_vanished_without_tombstone` 三格——基线说这个 item 存在过，
   远端却既无记录也无 tombstone——宁可 `NeedsHuman` 停下要人介入，也绝不推断成
   「远端删了」。推断会导致本地数据被清掉，这是最贵的一类事故。

---

## 执行中做的决定

### 分类维度：远端按版本号，本地按哈希（评审后修订）

**这是本切片最重要的一次修正，起因是评审发现了一个 412 死循环。**

原设计里远端也按哈希分类。但 `VersionId` 是「时间戳 + 32 位随机 hex」，
**不由内容派生**。于是「同一份内容被重新上传一次」会产生
`remote.hash == base.hash` 但 `remote.version_id != base.version_id`：

- 分类为 `Unchanged`（哈希没变）
- 发出 `Upload{parent: Some(base_version)}`
- hub 的 CAS 以 parent 过期拒绝
- 客户端重新拉取，分类**仍然**是 `Unchanged`，再发同一个过期 parent
- **表里没有任何一格能推进基线版本，循环无出口**

修法三条：

1. `RemoteClass::Unchanged` 改为「与基线**同版本**」。版本号才是 CAS 的权威标识。
   `LocalClass` 仍按哈希——本地文件系统没有版本号，这个不对称是有意的。
2. `Upload` / `TombstoneRemote` 的 `parent` 取**远端当前版本**而非基线版本。
   CAS 的 If-Match 要匹配 hub 上当前是什么。CAS 仍有意义——它保护的是
   「调和之后、提交之前」那段窗口。
3. `remote=modified` 的格子加哈希子分支：版本变了不代表内容变了。
   `present|unchanged|modified` 且哈希与基线相同 → `AdoptBaseline` /
   `remote_version_advanced`——**这一格就是死循环的出口**。

`present|modified|modified` 因此成了**三分支，且顺序不可换**：
先问「远端到底变没变内容」（`remote_hash == base_hash` → `Upload`），
再问「两端是否撞成一样」（`local_hash == remote_hash` → `AdoptBaseline`），
否则才是 `Conflict`。反过来先比 local 与 remote，会把「远端没变、本地变了」
误判进冲突——这一格是第二轮复审才发现的。

### `into_result` 改名 `into_outcome`，冲突不再是 `Err`

原本 `Conflict` 会被转成 `Err`，而 `PROTOCOL.md` §7 对 `class=protocol` 的定义原文是
「走结构化冲突流程，**不作为错误处理**」。今天没有调用方所以不是缺陷，
但 M1d 的 CLI 若写成 `decide(..).into_result()?`，**一个冲突文件就会中止整轮 sweep**——
恰恰是相反的行为。改成只有 `NeedsHuman` 是 `Err`，`Conflict` 走显式出口。

形状本身就该引导正确用法，而不是靠注释提醒。

---

## 评审抓到了什么

三轮评审（两轮任务级 + 一轮切片级），逐格核对了 18 格——**决策表本身零偏差**。
问题全在**守护契约的那一层**：

| 发现 | 为什么要紧 |
| --- | --- |
| **CAS 死循环**（见上） | 计划本身的设计空洞，不是实现偏差 |
| `Action` 携带的字段一个都没被断言 | 把 `Upload{parent: Some(..)}` 误写成 `None` 意味着绕过 CAS 无条件创建——正是 I4 禁止的静默覆盖，而 20 条用例全绿。更糟的是有行注释谎称「字段值另有专门断言」 |
| I10 词汇的断言是**自证的** | 测试两边调的是同一个表达式，只证明 `decide_traced` 调了 `classify`，不证明它吐出的词对不对。把 `"added"` 改成 `"new"` 整套测试仍全绿，而 agent 的精确匹配已全部失效 |
| `LocalState::as_str()` 能吐出非法的 `local:"present"` | 与 `LocalClass::as_str()` 同名同签名、零生产调用者——纯粹是给后续任务摆的陷阱。`RemoteState` 那个更隐蔽：它的 `"present"` 恰好合法，误用后**错误看起来完全正常** |
| **`apply_decision` 忽略 CAS parent** | 切片级评审的头号发现：把本切片修掉的死循环 bug 重新引入，`convergence.rs` 与 `simulation.rs` **依然全绿**。属性测试在最贵的那一维上没有表达能力 |
| 崩溃注入是「跳过一步」 | `decide` 是纯函数，跳过一步后必然推出同一决策。没有制造 I9 真正在意的那种崩溃：**持久侧落了、可抛弃投影没落** |
| `remote_vanished_without_tombstone` 三格在模拟里从未走到 | 全表最重要的 I5 格子，模拟测试一次都没覆盖 |
| 模拟的版本计数器 `u8` 余量只剩 3 | release 下静默回绕成已用过的版本号——正好制造「版本推进被误判成没变」 |

**修复后的自检值得一提**：要确认新加的 CAS 断言真的有牙齿，实现者临时把修复改回
bug 状态。第一次注入没能复现失败——排查发现选错了格子（那一格两个版本号恰好相等，
改哪个都一样）。换到真正的死循环位置后属性测试立即变红。
「找对注入点」比「加了断言」更容易被忽略。

---

## 验证证据

242 个测试全绿 · clippy `-D warnings` 零告警 · `cargo +1.85 check --workspace --locked
--all-targets` 通过 · `cargo fmt --check` 干净 · sans-io 自检
（`grep std::fs|std::net|tokio|SystemTime::now|Instant::now`）对 `src/` 与 `tests/` 均无命中。

压力跑：**5 万条 proptest 案例 + 3 千个模拟种子**（各 80 步 churn + 24 步 settle，
每次调和有 1/3 概率崩溃），未发现决策表的新问题。

---

## 留给后续的

**M1d 的交接义务**（这三条目前只写在注释里，要进 M1d 的 brief）：

- `Action::Upload` / `AdoptBaseline` 不带 `item_id` 与 `size`，执行侧要自己从
  `RemoteState` / `LocalState` 取
- `local_new_over_tombstone` 那格按 spec §4.1 必须**铸一个新 ItemId**，
  而不是复活远端的旧身份
- `delete_vs_modify` 与 `remote_modified` 在 `Action` 层**完全同形**，
  「你的删除被推翻了」这件事只由 `reason` 承载，CLI 必须据此报告

**已知的弱项**（评审如实分诊，非阻塞）：

- 性质 1（全域性）与性质 2（I3 无销毁）都偏弱。性质 2 的 `NON_DESTRUCTIVE` 白名单
  列的正好是当前全部 8 个 `Action` 变体，今天恒为真——它是**变更探测器**而不是
  不变量检查。更强的形式是在 `World` 层断言「内容可达性保全」：每一步之前可达的
  每个哈希，之后仍可达。那样 `DeleteLocal` 就必须自己证明安全
- 性质 4（收敛）的 8 步上限从未被接近，实质是性质 3 的复述。要让它有独立价值，
  需要模型里有一个会因 parent 过期而拒绝的 hub
- 错误码表现在有两套机制：`arca-store` 直接写字面量，`arca-core` 用带类型的
  `code()`。M2 加 HTTP 映射前该统一到 `arca-format` 的共享注册表 + 全局唯一性测试

---

## M1 的其余切片

| 切片 | 内容 | 状态 |
| --- | --- | --- |
| M1a | 存储根 IO 地基 | ✅ |
| **M1b** | 调和状态机 | ✅ |
| M1c | `arca-git`：`.gitignore` 反选块（全设计最易出错处）+ 清单同步 + pre-push 钩子 + 追踪冲突检测 | 待做，可与 M1b 并行（无依赖） |
| M1d | CLI porcelain/plumbing + `file://` 直连同步闭环 + trace 失败落盘；跑通 spec §12.3 的 M1 验收演示 | 待做，依赖 M1a+M1b+M1c |
