# arca 线上协议规范（PROTOCOL.md）

> 状态：**骨架，未定稿**。与代码同仓、同评审（I10）。
> 设计依据：docs/2026-08-03-arca-spec.md §3.1、§3.3、§5、§8。

## 0. 原则

- 能复用公开规范的绝不发明私有协议：条件请求 / ETag / Range 全部对齐 RFC 9110。
- ETag = BLAKE3 内容哈希；一切写入走 CAS（If-Match，I4），过期即 412。
- 传输与对象模型分离：v1 支持 `https://`（经 arcad）与 `file://` 直连；`ssh://` 列 v2。

## 1. 传输

### 1.1 file:// 直连（M1）

- dataset_root 所在卷本地挂载时，无 daemon 完成同步；排他由 `arca.lock` 保证。
- TODO：锁协议、崩溃恢复（`.txn` 前滚/回滚）。

### 1.2 HTTP API（M2，arcad）

- 条件请求：If-Match / If-None-Match / 412 Precondition Failed。
- 断点续传：Range / 206 + If-Match 版本钉住。
- 变更流：longpoll（30–90 秒挂起）+ SSE（agent 场景）；2 秒短轮询为降级路径。
- 挂载缺失即离线：数据集离线 → 503，绝不呈现为空库（I11）。
- TODO：端点表、请求/响应格式、错误码表。

## 2. 上传协议

- 分块断点续传、幂等五元组、丢失 commit 的 no-op 恢复（继承 Lazync §7）。
- 修改过的文件只传变化的 CDC 块（FastCDC + BLAKE3 索引，hub 已有块跳过）。
- 上传前两轮稳定性签名防抖。
- TODO：会话状态机定义。

## 3. journal 与游标

- append-only、`epoch:seq` 游标、压缩后 `reset_required` 全量对账兜底。
- 每个事件带 actor（账号 + 设备/agent + 会话，I8）。
- TODO：事件类型表与序列化。

## 4. 认证与令牌

- 密码 → 设备令牌（服务端只存哈希）→ 内存会话；agent 令牌为第四形态
  `{scope, caps, ttl, actor_label}`（spec §9）。
- TODO：握手流程。

## 5. plumbing 输出契约（spec §3.2）

`arca ls --json` / `arca cat <hash>` / `arca resolve <path>` / `arca state dump --json`
的输出格式与退出码属于本规范，受兼容性承诺约束。

- Rule of Silence 在 plumbing 这一层的具体含义：**数据永远走 stdout**——
  plumbing 存在的意义就是产出可脚本消费的输出，即便结果是空清单也要打印 `[]`，
  这不是"安静"；安静只留给"没有这回事"的诊断信息（走 stderr）。
- 退出码延续 `arca fsck` 定下的三态（spec §3.2）：**0** = 成功；**1** = 命令本身
  的失败（数据集未登记、查无此哈希/路径等）；**2** = 存储根身份不明
  （I11：未挂载或卷身份不符，绝不可呈现为"这个路径没有记录"）。

四个命令都以 `arca <cmd> <dataset-path> ...` 的形状取一个数据集相对 vault 根
的路径作为第一个定位参数（`--root` 可覆盖从 `.gitarca` 解析出的存储根路径，
语义与 `arca sync --root` 一致）。`ls`/`cat`/`resolve` 读的是 **hub 侧**当前
状态（`.arca/index/` + `.arca/items/` 的当前版本，M1 尚不产出 tombstone，
理由见 `crates/arca-cli/src/hub.rs` 模块文档）；`state dump` 读的是**客户端
本地投影**（`.arca/client/baseline.jsonl`，I9：可抛弃投影）。

### 5.0a `arca ls <path> --json`

hub 侧当前清单，一个 JSON 数组，按路径 UTF-8 字节序排序：

```json
[
  {"path":"京都/鸭川.png","item_id":"3f...","version_id":"20260805T093012Z-0123456789abcdef","hash":"blake3:...","size":1234},
  {"path":"notes/a.md","item_id":"8b...","version_id":"20260805T093013Z-fedcba9876543210","hash":"blake3:...","size":42}
]
```

`item_id` 为 32 位小写十六进制（`ItemId::to_hex`）；`hash` 为 `blake3:<hex>`
形式（`ContentHash::to_text`）；空数据集输出 `[]`，退出码仍是 0。

### 5.0b `arca cat <path> <hash>`

按内容哈希取字节，**原样写 stdout**（不追加换行、不做任何编码转换——输出
可能是任意二进制，供管道给别的工具用）。`<hash>` 取 `blake3:<hex>` 形式；
多个路径共享同一份内容时（去重命中）按路径排序取第一个命中，结果确定。
查无此哈希、或 `<hash>` 本身格式不合规，退出码 1，诊断信息走 stderr。

### 5.0c `arca resolve <path> <file>`

单个路径 → hub 侧身份/版本，一个 JSON 对象，字段与 `ls` 的单条记录相同：

```json
{"path":"notes/a.md","item_id":"8b...","version_id":"20260805T093013Z-fedcba9876543210","hash":"blake3:...","size":42}
```

`<file>` 在 hub 侧没有记录（从未同步过，或已被删除——M1 尚无法区分两者，
见 `hub.rs` 模块文档）时退出码 1。

### 5.0d `arca state dump <path> --json`

客户端本地投影（基线）检视——SQLite 是二进制没关系，git 的 index 也是，
前提是有 dump 命令（spec §3.2）。一个 JSON 对象：

```json
{
  "was_reset": false,
  "reset_reason": null,
  "entries": [
    {"path":"notes/a.md","item_id":"8b...","version_id":"20260805T093013Z-fedcba9876543210","hash":"blake3:...","size":42}
  ]
}
```

`was_reset` 为 `true` 表示本次读取时基线缺失或损坏、已重置为空（I9：`arca
status`/`arca sync` 据此判断本轮是否会做全量对账）；`reset_reason` 是重置
原因的人类可读描述，`was_reset` 为 `false` 时恒为 `null`。基线本身不需要
打开存储根，因此这个命令不会因存储根身份不明而返回退出码 2——它只可能因
数据集本身未登记而返回退出码 1。

### 5.1 trace 读侧（M1）

设计依据 spec §3.3，事件格式见 `FORMAT.md` §10；本节只定契约的命令行一侧。

| 命令 | 层 | 输出 |
| --- | --- | --- |
| `arca trace list --json` | plumbing | 每行一个留存的会话：`sid` · `argv` · `exit_code` · `at` · `events` · `dropped` |
| `arca trace show <sid> --json` | plumbing | 该会话的事件流，原样透传 `FORMAT.md` §10 的 JSONL；`--children` 连同子 sid 按 `seq` 归并 |
| `arca trace last --json` | plumbing | 最近一次失败会话的事件流，等价于对最新 sid 执行 `show` |
| `arca doctor --json` | porcelain | 当前健康度（对应 `git fsck`），附最近失败的 sid |
| `arca bugreport` | porcelain | 打包 trace + doctor + 版本环境（对应 `git bugreport`） |

`show` / `last` 读不出任何行时以退出码 0 输出空——「没有失败记录」不是错误。
坏行按 `FORMAT.md` §10.5 跳过，跳过计数必须写入 stderr，绝不静默。

### 5.2 sid 的跨进程传播（M1）

- 子进程从环境变量 `ARCA_TRACE_SID` 读父 sid，追加自己的一段（`FORMAT.md` §10.2）；
  变量缺失或不合法则以自身为根，**不报错**——trace 是诊断产物，绝不能因它而使命令失败。
- HTTP 请求携带 `Arca-Session: <sid>`（§1.2），arcad 记入 journal 事件的 `actor.session`
  （`FORMAT.md` §3），构成 I8 的审计闭环。

## 6. Git LFS 桥（M5）

- 实现 LFS Batch API 与指针格式（oid 为 SHA-256，懒计算缓存）。
- TODO：映射规则。

## 7. 错误码表

设计依据 spec §3.3「让 agent 好用的两个决定」。`code` 是稳定的短字符串，
出现在 trace 的 `error` 事件（`FORMAT.md` §10.4）、HTTP 错误响应体与 `--json` 输出中，
三处同源。`class` 决定调用方（尤其是 agent）的处置：
**只看 `class` 就能决定重试 / 停下 / 走冲突流程 / 报 bug，无需理解 `code` 的语义。**

| `class` | 处置 |
| --- | --- |
| `retryable` | 退避重试 |
| `needs_human` | 停下（I5），报告给人 |
| `protocol` | 走结构化冲突流程，不作为错误处理 |
| `bug` | 提 issue |

已定的码（随里程碑增补，只增不改语义——I10）：

| `code` | `class` | 含义 |
| --- | --- | --- |
| `mount.identity_mismatch` | `needs_human` | `format.json` 的 `dataset_id` 与绑定不符（I11） |
| `mount.absent` | `needs_human` | 存储根未挂载——数据集离线，绝不呈现为空库（I11） |
| `mount.io_error` | `needs_human` | 读取存储根身份标记时遇到非「不存在」的 IO 故障（权限、挂载点损坏等）；NFS 抖动一类值得重试的场景属于上层策略，不在这里判定为 `retryable` |
| `mount.bad_expected_id` | `bug` | 调用方传入的期望 `dataset_id` 不是合法的 32 位小写十六进制——调用方参数错误，不是卷的问题 |
| `path.rejected` | `needs_human` | 路径不合规，细类见 `FORMAT.md` §2 |
| `format.unsupported_version` | `needs_human` | 格式版本高于本实现（`FORMAT.md` §0） |
| `format.malformed` | `needs_human` | 结构损坏，绝不尽力解析（I5） |
| `lock.busy` | `retryable` | `arca.lock` 被占用 |
| `commit.stale_parent` | `protocol` | CAS 412，父版本过期（I4） |
| `journal.reset_required` | `protocol` | 游标早于保留区间，走全量对账兜底 |
| `reconcile.needs_human` | `needs_human` | 三态调和判定为模糊终态（`reason=remote_vanished_without_tombstone`）——基线说某个 item 存在过，远端却既无记录也无 tombstone，按 I5 停下，绝不推断成「远端删了」 |
| `reconcile.conflict` | `protocol` | 三态调和判定为结构化冲突（`reason` 为 `both_new_divergent`/`three_way_divergent`/`modify_vs_delete` 之一）——走 M2 冲突落地流程，不作为错误处理 |
| `internal.invariant_violated` | `bug` | 内部不变量被破坏 |

TODO：退出码与 `code` 的映射表（M1）；HTTP 状态码与 `code` 的映射表（M2）。
