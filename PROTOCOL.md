# arca 线上协议规范（PROTOCOL.md）

> 状态：**骨架，未定稿**。与代码同仓、同评审（I10）。
> 设计依据：docs/2026-08-03-arca-spec.md §3.1、§5、§8。

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

- Rule of Silence：成功时安静；数据走 stdout，进度与诊断走 stderr。
- TODO：各命令的 JSON schema 与退出码表。

### 5.1 trace 读侧（M1）

事件格式见 `FORMAT.md` §10；本节只定契约的命令行一侧。

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

`code` 是稳定的短字符串，出现在 trace 的 `error` 事件（`FORMAT.md` §10.4）、
HTTP 错误响应体与 `--json` 输出中，三处同源。`class` 决定调用方（尤其是 agent）的处置：
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
| `path.rejected` | `needs_human` | 路径不合规，细类见 `FORMAT.md` §2 |
| `format.unsupported_version` | `needs_human` | 格式版本高于本实现（`FORMAT.md` §0） |
| `format.malformed` | `needs_human` | 结构损坏，绝不尽力解析（I5） |
| `lock.busy` | `retryable` | `arca.lock` 被占用 |
| `commit.stale_parent` | `protocol` | CAS 412，父版本过期（I4） |
| `journal.reset_required` | `protocol` | 游标早于保留区间，走全量对账兜底 |
| `internal.invariant_violated` | `bug` | 内部不变量被破坏 |

TODO：退出码与 `code` 的映射表（M1）；HTTP 状态码与 `code` 的映射表（M2）。
