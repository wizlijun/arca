# M2c · journal 变更流、longpoll 与 sid 闭环

**完成于 2026-08-09** · 9 个提交 · 23 个文件、7350 行新增 · 结束时 666 个测试全绿
（M2c 新增 85 条）

M2 的第三块。**同步第一次真的跑在网络上**——spec §12.3 要求的「两机改名/删除/冲突
全场景，纯手动命令完成」在这一块兑现。

---

## 交付了什么

| 部分 | 内容 |
| --- | --- |
| `Transport` 补齐四条缺口 | 流式读、Range/续传、按哈希寻址、批量提交 |
| `GET /changes` | `epoch:seq` 游标 + `reset_required` + `limit` 分页 |
| longpoll | 挂起 30–90 秒；专属并发配额；掉线立即 503 |
| `sid` 闭环 | 客户端 trace sid → `Arca-Session` 头 → journal 的 `actor.session` |
| `HttpTransport` | `Transport` 的第二个实现（阻塞式 `ureq`） |
| `Transport::rename` | 改名传播（I7 身份跨改名稳定） |

### 为什么第一个任务是补 trait 而不是写 HTTP

M2b 的评审列了四条缺口，**都是 trait 形状的问题**。先补 trait、让 `local.rs` 跟上，
`http.rs` 才有正确的靶子；反过来先写 HTTP，等于把缺口固化进**两个**实现。
其中一条（`read_content -> Vec<u8>` 强制整文件缓冲）是服务端 C2 的客户端镜像。

判据是「接口扩展不是行为变更」：581 条既有测试**一条不改**全过。做到了。

### HTTP 客户端选 `ureq` 而不是 `reqwest`

`reqwest` 的 blocking 模式内部仍会起一个隐藏的 tokio 运行时。spec §3.1
「客户端零常驻」是形态约束——CLI 是一次性进程，为发几个请求启动运行时违背它。
`ureq` 基于 `std::net::TcpStream`，真正同步。实测 `cargo tree -p arca-cli` 里
**一个 tokio 节点都没有**。

### 两机端到端（M2 的明文验收）

| 场景 | 结果 |
| --- | --- |
| adopt → sync | 设备 B 经真实 HTTP 下载到内容 |
| **改名** | hub 上 **item_id 不变**；设备 B 零传输本地 rename |
| **删除** | tombstone → 设备 B 四道闸门放行后移除本地副本 → `restore` 找回 |
| **冲突** | 先提交者赢，后者退出 1；**逐字节验证双版本并存**，两侧都没被覆盖 |

---

## 评审抓到了什么

切片评审做了 **30 次实机攻击**。

### 扛住的部分

游标状态机 400/410/200 三态干净，**没有一个畸形游标被当成「从头开始」**（那会静默
重下全库，是 I5 违规）；sid 校验注入面为零——往 sid 里塞 `","op":"tombstone` 与换行
全部 400，**journal 无法被注入**；longpoll 配额真实有效（24 并发恰好 16 挂起、8 立即返回），
写不被饿死（16 挂起时 PUT 35ms 完成并同时唤醒全部挂起），掉线 3 秒内 503，
客户端断开不泄漏许可；两个 Transport 在 CAS/身份/离线/重试分类上**逐字段一致**。

### C1：`GET /changes` 每次轮询重读+重解析整段 journal

longpoll 循环体里调 `read_all`——每个挂起连接**每秒一次全量读盘+全量解析**，
与「这次有没有新事件」无关。全量拉取更糟：`to_line()` → `String` → `from_str` →
`Value` → collect → `Vec<Value>` → 整体序列化，**一份 journal 同时存在 5 份内存副本**。

| 场景 | 修复前 | 修复后 |
| --- | --- | --- |
| 50 MB journal，4 并发 `/changes` | RSS **3.25 GB**（≈810 MB/请求） | ≈462 MB 峰值（≈94 MB/请求） |
| 13.6 MB journal，16 个**空闲** longpoll | **90–440% CPU**、520 MB 常驻 | **0–4.9% CPU**，等待窗口内 RSS 不增长 |
| 同上，无关的 `GET /state` | 13 ms → **1.5 s** | 稳定 **22–26 ms** |

**这是无请求体的 GET**——攻击者能发出的最廉价请求。全局并发 64 ⇒ 天花板约 52 GB。

M2b 的 C2 教训（单请求 1.86 GB）被用在了写路径与客户端，**唯独没用在这条新的读路径上**。

### C2：journal 不是完整的变更流

`arca adopt` 与 `file://` 的 `arca sync` 走的 `execute_upload` **从不写 `Op::Upsert`**——
只有 `commit`/`commit_streamed`/`commit_batch` 写。而 `PROTOCOL.md` §3 本切片新增的段落
明文宣称 upsert 由这三个写入，并把它当作变更流成立的前提——
**偏偏「数据集第一次被填满」的那条路径不在这三个之内**。

实测：adopt 两个文件 + sync 一个之后 journal **0 字节**；`GET /changes`（从头开始）
只返回 1 条，而 `/state` 返回 3 条。按 I9「真相在 hub 的 journal」，
这是一份说「nara.png 从未发生过任何事」的 journal。

今日影响潜伏（还没有 `/changes` 的客户端消费者），但**这正是 M3 agentd longpoll
要建在上面的地基**，必须在有消费者之前关掉。

修法是让 Upload 改走 `commit_batch`（顺带解决了「批量接口无生产调用者」那条 Important），
**并因此暴露修掉了两个真实 bug**：tombstone 后用新 item_id 重建被误判、
两个 `AppendBatch` 交错提交互相践踏。

### 七条 Important 里最该记的三条

- **`ureq` 的 10 MB 默认 body 上限没被显式覆盖**：`/state` 超过 10 MB 后 http 同步
  **整条命令失效**。每条 state 条目均 249 B ⇒ **约 42,000 文件即触顶**，
  对「个人照片库」完全在范围内。而且被分类成 `class=Bug`（「去看代码」），
  实际原因是「库太大了」。**同一个数据集 file:// 能同步、http:// 不能**——
  两个实现之间最硬的分叉。提到 256 MB 并修正分类
- **`read_range` 在 http 上静默短读、在 file:// 上报错**：服务端按 RFC 把 `end` 钳到
  文件末尾并回 206（服务端正确），**客户端不校验返回长度**。
  续传场景下一次静默短读就是一个被截断的文件
- **全部阻塞工作直接跑在 tokio worker 上，没有 `spawn_blocking`**：12 并发 batch 时
  一个**零 IO 的纯路由 404** 首次耗时 4.45 s。本机 12 核尚可掩盖；
  部署目标是 2–4 核 ARM NAS，届时几个大 batch 就能把 worker 占满，
  **连 I11 的 503 都发不出去**

另外 journal 的写侧也是 O(n²)（每次追加重写整个文件，而本切片把事件量从
O(删除数) 变成 O(全部写入数)）——50 MB journal 上一个 **7 字节 PUT 耗时 5.08 s**。
新增 `atomic::append` 真增量写原语，`AppendBatch` 区分「干净尾巴走快路径 / 撕裂尾巴走治愈慢路径」。

---

## 一处诚实的取舍

**批量提交在磁盘上不是全有全无**（校验阶段是，写入阶段分三段）。实测 3000 条 batch
中途 `kill -9` → **1245 个内容文件落盘、指针全无**，而 `fsck`/`doctor`/`sync`
**全部看不见它们**（fsck exit 0）——I1 逃生舱下的人用 coreutils 会恢复出一个
arca 认为不存在的文件。

实现者选了**「让 fsck 能看见孤儿」而不是「做真原子」**，理由是真原子需要跨文件
事务机制、超出本轮范围。这个取舍是对的：现在 `fsck` 会报 `OrphanFile` 并 exit 1，
而真原子留给 `.txn` 事务日志（spec §4.2 已规划）。

---

## 验证证据

666 个测试全绿 · clippy `-D warnings` 零告警 ·
`cargo +1.85 check --workspace --locked --all-targets` 通过 · `cargo fmt --check` 干净 ·
`arca-core` 未改动一行 · `arca-cli` 依赖树 **tokio 节点数为 0**。

C2 的独立复验：`adopt`(2 文件) + `sync`(1 文件) 之后 **`files/` 3 个文件、journal 3 条事件**
（修复前 journal 是 0 字节）。

---

## 留给 M2d / M2e 的

评审点名「必须先关掉再进 M2d」的五项（C1、C2、I1、I5、I6）**本轮全部关掉了**。

剩下的：

- `sync_transport`（http 路径）的**批量化未完成**——`Action::Upload` 在 http 下仍是
  每文件一次往返。`file://` 路径已经走 `commit_batch`
- `Action::Download` 在调用点仍物化一个 `Vec<u8>`，虽然 `read_content_into` 本身是真流式。
  真正的落盘流式下载需要客户端临时文件机制
- `journal_store.rs` 仍是 8 行骨架——**journal 压缩尚未实现**。M2a 已让 tombstone 在
  index 侧留痕，所以理论上可截断，但压缩落地前要先确认 C2 修复覆盖了所有写路径

---

## M2 的其余切片

| 切片 | 内容 | 状态 |
| --- | --- | --- |
| M2a | tombstone 与删除安全地基 | ✅ |
| M2b | arcad 与 HTTP CAS | ✅ |
| **M2c** | journal 变更流、longpoll、sid 闭环 | ✅ |
| M2d | 多卷映射 + server/client 角色 + 多 hub 独立故障域 + 拔盘演练 | 待做 |
| M2e | `https://`（TLS）+ `arca bugreport` | 待做 |
