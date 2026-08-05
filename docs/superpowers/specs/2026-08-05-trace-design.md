# arca trace：agent 友好的诊断轨迹

> 日期：2026-08-05 · 状态：设计定稿，M0 落地 schema
> 依据：spec §2（I1–I11）、§3.2（plumbing/porcelain）、§4.2（hub 布局）、§11.2（正确性基础设施）
> 命名与结构对齐 git 的 `trace2`（`GIT_TRACE2_EVENT`）与 `git bugreport`。

## 1. 问题

arca 的失败大多不是「一句报错」能说清的：一次 `arca push` 失败，真正要回答的是
「调和引擎对这 3000 个文件分别决定了什么、在第几个文件上停的、当时本地/远端/基线三方各是什么状态」。
传统的按级别打日志（info/warn/debug + 自由文本）无法回答这类问题——
agent 拿到的是一堆需要正则去捞的字符串，而不是可查询的结构。

同时 arca 有一组硬约束，任何日志方案必须同时满足：

- **arca-core 是 sans-io**（spec §11.3）：无 IO、无 tokio、无全局状态。它不能写日志。
- **客户端零常驻**（spec §3.1，git 形态）：每条命令是一次性进程，没有常驻收集器可依赖。
- **Rule of Silence**（spec §3.2）：成功时安静。
- **I5 绝不猜测**：状态模糊要停下并**可诊断**——「可诊断」这三个字正是本设计要兑现的。

## 2. 概念定位：trace 不是 journal

三个概念必须严格分开，否则会有人往 journal 里塞调试信息，journal 就废了。

| | 位置 | 记什么 | 生命周期 | 损坏时的纪律 | 对应 git |
| --- | --- | --- | --- | --- | --- |
| **journal** | hub `.arca/journal/` | 数据事实 + actor（I8） | 永久，是真相 | 中间行损坏 → **失败**（FORMAT.md §7.2） | `reflog`（但 arca 的是权威的） |
| **`.txn`** | hub `.arca/locks/` | 不可逆点的前滚/回滚意图 | 事务完成即清 | 前滚或回滚，绝不猜测（I5） | ref transaction 的 lock 文件 |
| **trace** | 客户端 `<state>/trace/` | 一次会话的决策轨迹与失败上下文 | 最近 N 次失败 | 坏行**跳过并计数**（§6） | `trace2` |

**两条相反的损坏纪律是刻意的**：journal 是真相，读错一行等于伪造历史，必须失败；
trace 是事故现场的线索，为了一行坏数据丢掉其余 4095 条线索是荒谬的。
知道何时该严格、何时该宽容，是这份设计里唯一需要反直觉记住的一条。

**trace 与 `.txn` 的关系**：`.txn` 保持自己的格式与 fsync 保证，是权威的；
每次 `.txn` 写入**同时**向 trace 发一条 `txn.*` 事件，是可丢的镜像。
这样 agent 读 trace 能看到完整故事，而崩溃恢复的正确性不依赖 trace。

**「集中」的范围**：单机集中——同一台机器上所有 arca 进程（cli / agentd）写同一个 state 目录；
hub 侧 arcad 自有一份。两侧通过 `sid` 关联（§7），但**不自动回传**：
同步失败往往就是网络坏了，自动回传在最需要它的时刻恰好不可用。
跨机器合并走显式的 `arca bugreport`。

## 3. 命名（对齐 git）

| 概念 | arca | git 出处 |
| --- | --- | --- |
| 系统 | trace | `GIT_TRACE` / `GIT_TRACE2` |
| 会话标识 | `sid`，**层次化** | trace2 的 session id |
| 事件类型键 | `event` | trace2 event format |
| 相对时间 | `t_abs`（微秒，自 `start` 起） | trace2 的 `t_abs` |
| 嵌套作用域 | `region_enter` / `region_leave` | trace2 原语 |
| 环境变量 | `ARCA_TRACE_EVENT=<path>\|1\|2` | `GIT_TRACE2_EVENT` |
| plumbing 读侧 | `arca trace list` / `arca trace show <sid>` | — |
| porcelain 打包 | `arca bugreport` | `git bugreport` |

**层次化 sid 是白捡的收益。**git 的 sid 是 `<父>/<子>`，子进程继承父 sid 并追加自己的。
arca 的 porcelain/plumbing 分层（spec §3.2）正好需要它：`arca sync` 内部走 `fetch` + `push`，
层次化 sid 让三个进程的 trace 天然串成一棵树，agent 拿到根 sid 就能捞出整棵。

**与 git 的两处刻意偏离**：

1. **不发 `version` 事件。**git trace2 首行是 `{"event":"version","evt":3,...}`；
   arca 的 FORMAT.md §1 已经要求每个 JSON 对象以 `"v"` 为第一个键，逐行自描述，
   再发一个 version 事件是冗余。arca 版本号放进 `start` 事件的 `exe` 字段。
2. **不记 `thread`。**arca-core 是单线程状态机；边缘的并发（上传池）以 `region` 表达。
   等真有多线程 trace 需求再加字段，不预留（YAGNI）。

## 4. 事件模型

### 4.1 信封

每行 JSON 对象固定携带四个信封字段，其后是该事件类型的载荷字段：

```json
{"v":1,"sid":"20260805T093012Z-0123456789abcdef","seq":17,"t_abs":48211,"event":"reconcile.decide","action":"conflict","base":"blake3:9f2c…","local":"modified","path":"京都/鸭川.png","reason":"three_way_divergent","remote":"modified"}
```

- `v`：记录格式版本（FORMAT.md §1 通用约定）。
- `sid`：层次化会话标识，形如 `<紧凑时间戳>-<16 位十六进制>`，子会话以 `/` 追加。
  时间戳前缀使**字典序即时间序**，与 `version_id`（FORMAT.md §3）同构。
- `seq`：该 sid 内单调递增、无空洞——与 journal 的 `seq` 同纪律，让「中间丢了事件」可检测。
- `t_abs`：自本会话 `start` 起的**单调**微秒数。**不是墙钟。**
  墙钟只在 `start` 事件里出现一次（`at` 字段，RFC 3339）。
  理由：确定性——模拟测试（spec §11.2）注入模拟时钟即可逐字节复现 trace；
  且单调时钟不受 NTP 跳变影响，区间测量才有意义。

### 4.2 载荷字段是平坦的标量

字段值只允许 `string` / `u64` / `i64` / `bool` / `null`，**不允许嵌套对象或数组**。

三个理由：Rust 侧零额外依赖即可序列化；agent 用 `jq` 处理无需展开嵌套
（`jq 'select(.event=="error") | .code'` 就是全部）；
以及最重要的——防止有人图省事把整个结构体塞进 trace，让 schema 失控。
需要表达集合时用多条事件，不用数组。

载荷字段**按键名字节序升序**输出（同 manifest 的确定性序列化纪律，FORMAT.md §9.3）。
起初我按插入顺序输出，`path` 在前更好读；但 `arca trace show --children` 要归并
多个会话再重新序列化，插入顺序会让输出不可复现，arca-conformance 就无法逐字节比对第三方实现。
可读性输给确定性。

### 4.3 事件族（封闭枚举）

**`event` 的取值是封闭枚举，进 FORMAT.md，受 I10 约束。**
这是「agent 友好」的第一个秘密：agent 对 `event` 做精确匹配，而不是正则捞字符串。

会话骨架（借 git trace2）：`start` · `exit` · `region_enter` · `region_leave` · `error` · `panic`

arca 领域事件（M0 定义 schema，各里程碑填充发射点）：

| 事件 | 载荷 | 里程碑 |
| --- | --- | --- |
| `mount.check` | `dataset_id` `expect` `found` `ok` | M1（I11） |
| `lock.acquire` / `lock.wait` / `lock.release` | `holder` `waited_us` | M1 |
| `path.reject` | `path` `status` | M0 |
| `scan.summary` | `files` `bytes` `rejected` | M1 |
| **`reconcile.decide`** | `path` `item_id` `local` `remote` `base` `action` `reason` | M1 |
| `commit.attempt` / `commit.result` | `item_id` `if_match` `hash` `size` / `outcome` `version_id` | M1 |
| `conflict.copy` | `path` `copy_path` `item_id` `other_version_id` | M1 |
| `txn.begin` / `txn.commit` / `txn.rollback` | `txn_id` `kind` | M2 |
| `transfer.summary` | `chunks_sent` `chunks_skipped` `bytes` | M2 |

`reconcile.decide` 是全表最重要的一条——它把「调和引擎为什么这么决定」变成可查询的结构。
注意它记的是**汇总后的决策**而非逐次比较：扫描类的高频事实走 `scan.summary`，
只有决策与拒绝逐条记录，否则 trace 体积会淹没信号。

### 4.4 错误分类：`error` 事件的 `class`

这是「agent 友好」的第二个秘密，也是 `arca-core/src/error.rs` 那个 TODO 的答案：

| `class` | 含义 | agent 该做什么 |
| --- | --- | --- |
| `retryable` | 网络抖动、锁竞争 | 退避重试 |
| `needs_human` | 卷身份不符、孤儿数据集、一致性冲突 | **停下**（I5），报告给人 |
| `protocol` | CAS 412 等 | 走结构化冲突流程，不是错误处理 |
| `bug` | 内部不变量被破坏 | 提 issue，附 `arca bugreport` |

agent 只看 `class` 就知道该重试、该停、还是该报告，**不需要理解 `code` 的语义**。
`code` 是稳定的短字符串（`mount.identity_mismatch` / `path.too_deep` / `commit.stale_parent`），
错误码表进 PROTOCOL.md，受兼容性承诺约束。

## 5. sans-io：core 怎么产出 trace

**core 只产出载荷（`TraceRecord`），信封的 `sid`/`seq` 由 sink 补齐。**
`t_abs` 由 core 从调用方注入的时钟取值——这不破坏 sans-io，
因为 spec §11.2 要求的确定性模拟测试本来就要向 core 注入模拟时钟。

```rust
pub trait TraceSink {
    fn record(&mut self, rec: TraceRecord);
}
```

三个实现随 `arca-format` 出厂：

| sink | 用途 |
| --- | --- |
| `NullSink` | 零成本丢弃 |
| `VecSink` | 测试：全量留存，供模拟测试**断言决策序列** |
| `RingSink` | 生产：固定容量环形缓冲，记录被挤掉时累计 `dropped` |

**`VecSink` 是白送的巨大收益**：spec §11.2 的确定性模拟测试与 proptest 至今只能断言
「最终三态收敛」这个结果；有了 `VecSink`，可以直接断言状态机的**推理路径**——
proptest 缩小出反例时，你看到的不是「结果不对」，而是引擎在第几步选错了哪个动作。
I3（无任何路径销毁数据）也从此可以断言为「trace 中不出现任何 destructive action」。

`RingSink` 的 `dropped` 计数必须落进 flush 出的文件——沉默地截断线索，
读的人会以为「前面什么都没发生」，这与 I5 同源。**插入位置是留存事件序列的最前面，
不是"`start` 之后"**（本文档早先的说法有误，已按 FORMAT.md §10.1/§10.6 的定稿修正）：
环一旦溢出，`start` 作为这次会话最早产生的事件，往往正是最先被挤出去的那条，
"在 `start` 之后插入"在那种情况下根本无法实现，实现（`RingSink::drain`）从一开始
就是插在最前面。合成的 `trace.dropped` 的 `seq` 恒为 `2^64-1`（不参与 §10.1 那条
"单调递增、无空洞"的序列，用哨兵值避免与任何真实 `seq` 冲突），此后真实事件保留
各自原本的 `seq`，相邻记录之间因此可能出现空洞——那正是丢弃发生过的证据，不是
文件损坏。

## 6. 落盘策略：成功即丢弃

```
正常退出（code 0）  → 不写任何文件                    ← Rule of Silence
非零退出 / panic    → flush 整个环到 <state>/trace/<sid>.jsonl
ARCA_TRACE_EVENT=…  → 强制实时写（路径 / 1=stdout / 2=stderr，同 git 约定）
```

**这个策略把日志轮转这个老大难消掉了**：平时不产生文件，自然不需要轮转。
保留由下一次进程启动时顺手 GC（零常驻，没有别人能做这事）：
`<state>/trace/` 超过 50 个文件或 14 天的删除，默认值可配。

state 目录：Linux `$XDG_STATE_HOME/arca`（缺省 `~/.local/state/arca`）·
macOS `~/Library/Logs/arca` · Windows `%LOCALAPPDATA%\arca`。

**panic 也要落盘**：装 panic hook，记一条 `panic` 事件（`payload` `location`）再 flush。
否则最需要现场的那一类失败恰好没有现场。

### 崩溃安全：明确不给 trace 上 fsync

环形缓冲的弱点是 SIGKILL / 断电时缓冲丢失。**这个取舍是刻意的。**

对数据可靠性而言，真正不能丢的是 `.txn` 事务日志与 journal，它们各自已有 fsync 保证。
trace 丢了，事实仍然完整地存在于 `.txn` 与 hub journal 中——trace 只是让重建过程省事。
给每条 trace 事件上 fsync 会让一次万文件同步多出几万次 fsync，
为一个**次要的**东西付出一个数量级的性能代价，并且会诱使人们把 trace 当作真相来依赖。

## 7. 与 journal 的关联（I8 闭环）

客户端提交时把 `sid` 放进请求头（PROTOCOL.md），arcad 记进 journal 事件的 `actor.session`。
于是：从客户端 trace → 能知道它产生了哪些 journal 事件；
从 journal 的一条事件 → 能知道是哪台机器哪次会话干的（trace 本身在那台机器上，按需 `bugreport`）。

`actor.session` 这个字段 FORMAT.md §3 已经存在，本设计只是给它一个确定的取值来源。

## 8. 查询面

agent 不该去 grep 文件，该有 plumbing：

| 命令 | 层 | 用途 |
| --- | --- | --- |
| `arca trace list --json` | plumbing | 列出留存的 trace（sid / 命令 / 退出码 / 时间 / 事件数） |
| `arca trace show <sid> --json` | plumbing | 吐出该会话的完整事件流；`--children` 连同子 sid |
| `arca trace last --json` | plumbing | 最近一次失败——**agent 90% 的情况只需要这个** |
| `arca doctor --json` | porcelain | 检查当前健康度（对应 `git fsck`），附最近失败的 sid |
| `arca bugreport` | porcelain | 打包 trace + doctor + 版本环境（对应 `git bugreport`） |

M6 的 MCP 侧加一个 `vault_diagnose` 工具，直接返回最近失败的结构化 trace，
与 spec §10.2 的工具表并列。

## 9. 里程碑落位

| 里程碑 | 内容 |
| --- | --- |
| **M0** | FORMAT.md §10 schema · `arca-format::trace`（`TraceEvent` / `TraceRecord` / `EventKind` / `ErrorClass` / `Sid` / 三个 sink） · golden vectors |
| **M1** | `RingSink` 接入 cli · 失败落盘 + panic hook · `arca trace list/show/last` · core 发射 `reconcile.decide` 等 |
| **M2** | `sid` 进协议头 · arcad 侧 trace · `txn.*` 事件 · `arca bugreport` |
| **M6** | MCP `vault_diagnose` |

## 10. 测试

- **golden vectors**：trace JSONL 样例进 `tests/golden/`，跨版本回归（I10）。
- **坏行容错**：截断行 / 非法 JSON / 未知 `event` / 更高的 `v` → 跳过并计数，绝不 panic，绝不丢其余行。
- **Rule of Silence 可执行化**：断言成功的 run 不产生任何文件。
- **决策序列断言**：`VecSink` 接入现有模拟测试，把 I3 断言为「trace 中无 destructive action」。
- **fuzz**：`read_lines` 进 cargo-fuzz 目标集，与其他解析器同等对待。

## 11. 已知限制

- **多线程 trace 未定义**：`thread` 字段不预留，等真需要时按 I10 走 RFC 加字段。
- **trace 内含明文路径**。单机不回传时这不构成新的暴露面（读它的 agent 本来就能读文件系统），
  但 `arca bugreport` 是**显式外发**，M2 落地时必须提供路径脱敏选项并默认提示。
- **环形缓冲容量是一刀切的**（默认 4096 条）。超大数据集的一次全量对账可能挤掉早期事件；
  `dropped` 计数会如实反映，但选择性保留（如「只保留 error 前后 N 条」）留作后续议题。
