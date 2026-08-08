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
- **`arca.lock` 落地于 M2b 切片评审 I3**：`<storage-root>/.arca/locks/arca.lock` 上的
  OS 级整文件独占锁（`arca_store::lock`，经 `fs4` crate 的 `flock`/`LockFileEx`，
  阻塞式获取，持有进程崩溃/被杀时由内核自动释放，不会遗留死锁）。
  [`LocalTransport::commit`](../crates/arca-cli/src/transport/local.rs)/
  `commit_streamed`/`tombstone`——`arcad` 的 HTTP 写入端点与 `arca-cli` 的
  `file://` 直连同步全部经这三者落盘——在各自的"读当前状态 → CAS 校验 →
  写入"临界区外层持有它，是这三者跨进程真正互斥的唯一机制；`Dataset::write_lock`
  （`crates/arcad/src/storage.rs`）只序列化单个 `arcad` 进程内部的并发请求，
  两把锁的职责不重叠（进程内 vs. 跨进程）。纯读操作（`GET`/`arca status`）
  不获取它。
- TODO：崩溃恢复（`.txn` 前滚/回滚）。

### 1.2 HTTP API（M2，arcad）

- 条件请求：If-Match / If-None-Match / 412 Precondition Failed。
- 断点续传：Range / 206 + If-Match 版本钉住。
- 变更流：longpoll（30–90 秒挂起）+ SSE（agent 场景）；2 秒短轮询为降级路径——**这部分留到 M2c**（游标/longpoll），本节只定 M2b 交付的五个端点。
- 挂载缺失即离线：数据集离线 → 503，绝不呈现为空库（I11）。

本节定稿于 M2b（`docs/superpowers/plans/2026-08-08-m2b-arcad-cas.md` Task 2）：I10 要求协议先于实现，`arcad` 的 HTTP 表面（Task 3–5）照本节实现。

#### 通用约定

适用于本节全部端点：

- Base path：`/v1/datasets/{dataset_id}` —— `dataset_id` 为 32 位小写十六进制
  （`format.json` 的 `dataset_id`，与 `item_id` 同一编码纪律，`FORMAT.md` §1）。
- `{path}` 是数据集内的逻辑路径（`FORMAT.md` §2 路径规则），按标准 URL 路径段
  编码：`/` 是路径分隔符本身、不编码，其余字符（含非 ASCII）按 RFC 3986
  百分号编码。**服务端必须先过 `arca_format::path_rules::check` 再落到任何
  文件系统操作**——HTTP 是不可信输入的入口，一条 `../../etc/passwd` 必须在
  进文件系统之前被拒（M2b Task 4 的验收点，`code=path.rejected`）。
- 请求体/响应体为 JSON 时 `Content-Type: application/json`；`PUT` 的内容体为
  `Content-Type: application/octet-stream`（原始字节，不做任何编码转换，与
  `arca cat` plumbing 的纪律一致，见 §5.0b）。
- `Arca-Session: <sid>`：客户端把自己的 trace `sid`（`FORMAT.md` §10.2）放进
  这个请求头（本节把 §5.2 已经点名的头正式钉进端点表）——可选但强烈建议
  携带；arcad 把它记进对应 journal 事件的 `actor.session`
  （`FORMAT.md` §3），构成 I8 的审计闭环：一次改动能从客户端 trace 一路
  追到服务端 journal。**缺失记空、非法拒绝**（M2c Task 4，两者分开处理的
  理由）：
  - **缺失**（未携带这个头，或携带了空字符串）→ `actor.session` 记一个
    空串，**不拒绝请求**——trace 是诊断产物，协议层不应该因为它缺失就
    中止一次合法的写入；这也是老客户端（不发这个头）的正常情形，不能
    因为协议新增了一个头就让旧客户端全部写入失败。
  - **携带了、但不是合法 sid**（`arca_format::trace::Sid::parse` 校验，
    与客户端 trace 落盘同一份格式纪律：`<紧凑时间戳>-<16 位小写十六进制>`，
    可选以 `/` 分隔的层次化子段，段数上限 8）→ `400
    request.session_invalid`，**拒绝这次写入**，不是"尽力塞进去"。这个头
    此刻是**不可信输入**（任何人都能在 HTTP 请求上手写这个头）：一个格式
    不合法的取值要么是客户端实现有 bug，要么是有意构造——两种情形都不该
    被静默记入 journal 的 `actor.session`：那是在伪造归因记录，与 I8「每个
    事件可归因」的本意相反（一个查不出真实来源、格式还不合法的字符串，
    "记录"下来除了污染审计线索没有别的作用），也与 I5「绝不猜测」相悖——
    "缺失"与"提供了但读不懂"是两种不同的输入形状，不能用同一个"记空"的
    默认值兜底。只有 `PUT`/`DELETE`/`PUT .../batch`/`POST .../rename` 四个
    会落盘 journal 事件的端点执行这条校验并可能因此拒绝（M2c Task 5 新增
    `POST .../rename` 时随之补入这份名单）；纯读端点（`GET .../files` 等）
    不落盘 `actor.session`，携带的 `Arca-Session` 值即便格式不合法也不影响
    读取结果，不在这条校验范围内。
- `Authorization`：设备令牌/agent 令牌（spec §9 第四形态）。握手流程是 §4 的
  TODO，本节只约定这个头出现在这里，不展开认证细节。

#### `If-Match` 认的是版本号，不是 ETag——两个验证器各管一件事

`ETag = BLAKE3 内容哈希`（spec §8 已定），但**CAS（写路径的 `If-Match`）必须
认版本号，不是 ETag**——这不是随手选的，是 M1b 踩过的真实教训
（`arca-core/src/state.rs` 顶部 doc comment 有完整推导，这里复述结论）：
`version_id` 一旦提交即不可变、由**客户端**在提交时刻生成
（`<紧凑时间戳>-<32 位随机十六进制>`），**不由内容派生**；同一份内容被重新
上传会产生一个新 `version_id`，但哈希不变。如果 CAS 认的是内容哈希，"同内容
重新上传"这个完全正常的操作会让 `theirs.hash == 客户端本地已知的 hash`，
用哈希判定"没有冲突"从而放行覆盖——这恰好掩盖了期间可能发生的、内容恰好
相同的并发写入；反过来，如果调用方拿着内容哈希当 `If-Match` 提交，一旦
远端内容真的变了、哈希也跟着变了，命中永远为假，`412` 会在**内容没有真正
冲突**的场景（例如远端只是把同一份内容重新提交了一次）里错误触发，
制造死循环——两种方向的错误都源于用错误的验证器做 CAS。

**两个验证器因此分开，各管一件事，不能混用**：

| 用途 | 出现在 | 携带的值 | 目的 |
| --- | --- | --- | --- |
| 内容缓存/去重 | `GET` 响应的 `ETag`、`GET` 请求的 `If-None-Match` | BLAKE3 内容哈希（`"blake3:<hex>"`，加引号，RFC 9110 的 opaque 验证器） | "给我内容，除非我手上这份字节已经一样"——同一份内容换了个 `version_id` 也该命中缓存 |
| CAS / 版本钉住 | `PUT`/`DELETE` 请求的 `If-Match`；`GET` Range 续传请求的 `If-Match` | `version_id`（`FORMAT.md` 编码，不加引号的裸串，与 `Arca-Version-Id` 响应头同一形式） | "仅当远端仍是我认识的这个版本才生效"——推进/终结/续传都钉在同一个版本号上，不受"内容碰巧相同"影响 |
| 仅创建（RFC 9110 §13.1.2 标准写法） | `PUT` 请求的 `If-None-Match: *` | 字面量 `*` | "仅当这个路径此刻完全不存在时创建"——`arca_core::reconcile::Action::Upload{parent:None}` 的线上形态，复用 HTTP 标准的"创建幂等"惯用法（S3/CalDAV 等同一惯例），不是 arca 自造 |

`GET` 响应因此同时携带两个头：`ETag`（内容哈希，供缓存/去重）与
`Arca-Version-Id`（版本号，供后续 `If-Match` 钉住）——两者服务不同目的，
不能相互替代。**这条规则贯穿本节全部端点：出现 `If-Match`/`Arca-Version-Id`
的地方，值永远是 `version_id`；出现 `ETag`/`If-None-Match`（`GET .../files`
一处，值不是 `*` 时）的地方，值永远是内容哈希。**

#### 端点表

| 端点 | 请求头 | 请求体 | 成功响应 | 失败响应 |
| --- | --- | --- | --- | --- |
| `GET /v1/datasets/{id}/files/{path}` | `If-None-Match: "<hash>"`（可选，缓存校验）；`Range: bytes=...`（可选，续传）；`If-Match: <version_id>`（Range 续传时应携带，钉住版本）；`Arca-Session` | 无 | `200`：全量字节，响应头 `ETag`/`Arca-Version-Id`/`Content-Length`；`206`：区间字节，另加 `Content-Range`；`304`：`If-None-Match` 命中，空体 | `404`：路径此刻没有可下载的内容（`Absent` 与 `Tombstoned` 统一折叠成 404，见下）；`412`：Range 续传时 `If-Match` 与当前版本不符（内容在续传期间被改写）；`503`：数据集离线 |
| `PUT /v1/datasets/{id}/files/{path}` | **`If-Match: <version_id>` 或 `If-None-Match: *` 二选一必需**（I4：一切写入走 CAS，不允许无条件写）；`Arca-Item-Id: <item_id>` 必需（客户端生成，创建与推进都要带——`item_id` 的分配权在客户端，见 `arca-cli::ids::new_item_id`）；`Arca-Version-Id: <version_id>` 必需（本次要落地的新版本号，客户端生成）；`Arca-Mtime: <rfc3339>` 必需；`Arca-Session` | 原始字节（`application/octet-stream`） | `201`（`If-None-Match: *` 创建）/`200`（`If-Match` 推进）：响应头 `ETag`/`Arca-Version-Id`，体可为空或 `{item_id,version_id,hash,size}` | `400`：两个条件头都未提供（`code=request.if_match_required`），或 `Arca-Item-Id`/`Arca-Version-Id`/`Arca-Mtime` 缺失（`code=request.metadata_missing`）；`409`：`Arca-Item-Id` 与目标路径 / 该 item 自身版本链实际归属的身份不符，或该 item_id 已被 tombstone 终结（`code=request.item_id_mismatch`，评审 C1）；`412`：CAS 冲突，结构化响应体见下；`503`：数据集离线 |
| `DELETE /v1/datasets/{id}/files/{path}` | `If-Match: <version_id>` 必需（I4，删除同样是 CAS 提交，没有"仅创建"这种豁免）；`Arca-Item-Id: <item_id>` 必需（单点确认的线上对应：明确对哪个 item 提交 tombstone，闸门第 2 道同一条纪律的服务端版本）；`Arca-Session` | 无 | `204`：tombstone 已提交（`files/<path>` 移入 `.arca/trash/`，服务端同样不得物理销毁，见 M2b Task 5） | `400`：缺 `If-Match`（`code=request.if_match_required`）；`404`：路径此刻不存在，无事可删；`409`：`Arca-Item-Id` 与该路径实际归属不符（`code=request.item_id_mismatch`，评审 C1——挡住伪造 item_id 把 tombstone 记到错误身份名下）；`412`：CAS 冲突；`503`：数据集离线 |
| `GET /v1/datasets/{id}/state` | `Arca-Session` | 无 | `200`：JSON 数组，每条 `{"path","item_id","version_id","hash","size","state"}`（`state` 为 `"present"` 或 `"tombstoned"`；`tombstoned` 条目没有 `hash`/`size`）——字段命名与 `arca ls --json`（§5.0a）同源，`state` 是新增字段（`ls` 的 M1 输出不含 tombstone，见其模块文档）；按路径 UTF-8 字节序排序，供客户端直接构造 `RemoteState` 集合 | `503`：数据集离线，**绝不返回 `200` 加空数组**（I11） |
| `GET /v1/datasets/{id}/trash/{item_id}?hash=<blake3-hex>` | `Arca-Session` | 无 | `200`：`{"recoverable":true,"hash":"blake3:...","size":1234}`——`hash`/`size` 是现场重算的结果（三方核验：查询参数带的期望哈希 = `.meta` 记录 = 现场重算，与 `Transport::recoverable(item_id, expected_hash)` 同一签名，见 M2b Task 1） | `400`：`hash` 缺失或格式不合法（`code=request.hash_missing`）；`404`：`{"recoverable":false}`（没有匹配的可取回记录——用 JSON 体而不是空 404，闸门第 4 道的调用方不需要额外分支判断"是不是解析失败"）；`503`：数据集离线 |
| `GET /v1/datasets/{id}/blobs/{hash}` | `Arca-Session` | 无 | `200`：全量字节，响应头 `ETag`（内容哈希，与请求路径的 `{hash}` 相同）、`Content-Length`——按内容哈希直接寻址，不经过路径/CAS（`arca cat <hash>` 的传输层对应，M2c Task 1；`Transport::read_by_hash` 同一签名）；多个路径共享同一份内容时结果确定（按路径 UTF-8 字节序取第一个命中，与 `arca cat` plumbing 现有算法一致，见 `commands/plumbing.rs::cat_cmd`） | `400`：`{hash}` 不是合法的 `blake3:<hex>` 形式（`code=request.hash_invalid`）；`404`：hub 侧当前没有任何路径的内容匹配这个哈希（`Absent`/`Tombstoned`/从未出现过统一折叠，与 `GET .../files/{path}` 的 404 同一纪律）；`503`：数据集离线 |
| `PUT /v1/datasets/{id}/batch` | `Content-Type: application/json`；`Arca-Session` | JSON 数组，每条与单个 `PUT .../files/{path}` 的元数据同构：`{"path","item_id","version_id","parent","mtime","content_base64"}`（`parent` 为 `null` 表示仅创建，语义等价单文件的 `If-None-Match: *`；`content_base64` 是这次要落地的原始字节的标准 Base64——批量端点用 JSON 信封而不是多段 `multipart`，字节内联编码是这个信封形状能表达任意内容最简单的方式，M2c 尚不为批量端点做流式优化，见 Task 1 brief「批量提交」一节） | `200`：JSON 数组，与请求顺序一一对应，每条 `{"item_id","version_id","hash","size"}`（全部成功，`Transport::BatchOutcome::Committed` 的线上形状）——**要么整批成功要么整批不生效**（I5：不做"部分成功"，调用方无法据此判断该从哪里重试） | `400`：请求体不是合法 JSON、或某一条缺少必需字段/`content_base64` 不是合法 Base64（`code=request.batch_malformed`，响应体附 `index` 指出哪一条）；`409`：某一条的 `item_id` 与目标路径/该 item 自身版本链归属不符，或已被 tombstone 终结（`code=request.item_id_mismatch`，响应体附 `index`，整批不生效）；`412`：某一条 CAS 冲突（`code=commit.stale_parent`，结构化冲突体见下，附 `index` 指出哪一条，整批不生效——**CAS 仍逐条校验**，见 Task 1 brief）；`503`：数据集离线 |
| `GET /v1/datasets/{id}/changes?since=<epoch:seq>&wait=<秒>` | `Arca-Session` | 无 | `200`：`{"events":[...],"cursor":"<epoch:seq>"或null}`——`events` 是该游标之后的 journal 事件（`FORMAT.md` §7.2 字段形状，`from`/`hash`/`size` 视 `op` 而定），`cursor` 是可用于下一次 `since` 的新游标（没有任何历史事件时为 `null`）；省略 `since` 等价于"从头开始"；`wait`（秒，可选，默认 0）大于 0 时挂起等待新事件（longpoll，spec §5.2，M2c Task 3），超时仍返回 `200` 与空 `events`+原游标，**不是错误**；服务端把 `wait` 钳制到 `[0, 90]`，超过上限不报错、静默取 90（资源耗尽面，见下） | `400`：`since` 不是合法的 `<epoch:seq>` 语法（`code=request.cursor_invalid`，**不当作"从头开始"处理**，I5：别猜）；`410`：`since` 携带的 `epoch` 与数据集当前 epoch 不符——本切片没有 journal 压缩，`epoch` 只会在（未来的）压缩后轮转，任何不匹配当前 epoch 的游标都视为"早于保留区间"（spec §5.2 `reset_required`），响应体 `{"code":"journal.reset_required","message":"...","cursor":"<当前有效游标>"}`，客户端应据此做一次全量对账，之后从响应体的 `cursor` 继续增量拉取；`503`：数据集离线（**含挂起期间掉线**——longpoll 等待过程中每次重新探测都会重新校验挂载，掉线立即返回 503，不等到 `wait` 超时才发现，I11） |
| `POST /v1/datasets/{id}/rename` | `Content-Type: application/json`；`Arca-Session` | JSON 对象 `{"from","to","item_id","parent"}`（`from`/`to` 是改名前后的逻辑路径，`item_id` 是这次改名声称的身份，`parent` 是 `from` 此刻的版本——CAS 的 If-Match 对象，随请求体传而不是请求头，见下「为什么是 POST body 不是 If-Match 头」） | `200`：`{"item_id","version_id"}`——**`version_id` 与请求体的 `parent` 相同**（改名不产生新版本，`Transport::rename` 的文档「为什么需要这第三个写入原语」） | `400`：请求体不是合法 JSON、或 `from`/`to`/`item_id`/`parent` 缺失或格式不合法（`code=request.rename_malformed`）；`400`：`to` 未通过 `path_rules::check`（`code=path.rejected`，与其它端点同一纪律）；`409`：`from` 的实际归属与声称的 `item_id` 不符，或 `to` 此刻已被另一个 item_id 占用（`code=request.item_id_mismatch`，与 `PUT`/`DELETE` 同一个码，响应体额外带 `path` 指出是 `from` 还是 `to` 触发）；`412`：CAS 冲突（`code=commit.stale_parent`，结构化响应体同 `PUT`，`base`/`yours` 退化为只带 `item_id`/`version_id`——改名没有"这次要落地的新内容"，没有独立的 `yours.hash`/`yours.size` 概念，与 `base`/`yours` 描述 Range 续传冲突时的退化写法同一条纪律）；`503`：数据集离线 |

`GET .../files/{path}` 对 `RemoteState::Tombstoned` 与从未存在过的
`RemoteState::Absent` 统一报 `404`——都是"这个路径此刻没有可下载的内容"，
这个端点回答的是"给我内容"，不需要区分两者（要问"是不是被删的、还能不能
找回"，用 `GET .../trash/{item_id}` 或 `.../state`，那里的 `RemoteState`
区分是完整的）。

#### `412` 的响应体：结构化冲突，不是一句错误文本

```json
{
  "code": "commit.stale_parent",
  "base":   {"item_id": "8b...", "version_id": "20260805T093012Z-0123456789abcdef", "hash": "blake3:...", "size": 42},
  "theirs": {"item_id": "8b...", "version_id": "20260805T094500Z-fedcba9876543210", "hash": "blake3:...", "size": 51},
  "yours":  {"item_id": "8b...", "version_id": "20260805T094501Z-1111222233334444", "hash": "blake3:...", "size": 60}
}
```

- `base`：客户端提交时声明的 `If-Match`（它认为的"当前版本"）——`item_id`/
  `version_id` 取自请求头；`hash`/`size` 是 arcad 记录中这个版本当时的值
  （这个版本本身已经找不到时——理论上不该发生——只留 `item_id`/`version_id`）。
- `theirs`：arcad 此刻对这个路径的真实认知，与
  `crate::transport::CommitOutcome::Conflict` 的 `actual: RemoteState` 同一
  形状；路径此刻是 tombstone 时换成
  `{"tombstoned": true, "item_id": "...", "version_id": "..."}`；路径此刻
  完全不存在时为 `null`。
- `yours`：客户端这次提交试图落地的版本——`item_id`/`version_id` 取自请求头，
  `hash`/`size` 由请求体现场算出。

三者合在一起，正是 `arca_core::reconcile::decide` 的三态输入形状（`base` /
`local`≈`yours` / `remote`≈`theirs`）——agent 收到 `412` 后可以在本地**原样
跑一遍同一份决策表**，决定"重新下载再试"还是"报冲突"，不需要 arcad 替它做
这个判断，决策权仍然全部留在 sans-io 的 `arca-core`，HTTP 只是把它需要的
三个输入原样递过去。**`class=protocol`**（§7）：这是正常的并发信号，客户端
**不应该**把 `412` 当错误处理、弹出异常中止整轮同步——`Decision::into_outcome`
已经在这个形状上踩过一次教训（一个冲突文件不该中止整轮 sweep，见
`arca-core/src/reconcile.rs` 文档），协议层不能重犯。

#### `503`：数据集离线

存储根未挂载、或挂载了但卷身份与 `hub.toml`（`arcad` 的存储根配置，见
`docs/superpowers/plans/2026-08-08-m2b-arcad-cas.md` Task 3）记录的不符
（I11）时，这个数据集上的**任何**请求都返回 `503`，响应体二选一：

```json
{"code": "mount.absent", "message": "..."}
```

```json
{"code": "mount.identity_mismatch", "message": "..."}
```

两个码已在 §7 定义。**绝不返回 `200` 加一个空的 `.../state` 数组、或对
`.../files/{path}` 返回看似合理的 `404`**——两者都会被客户端误读成"这个
数据集是空的/这个文件不存在"，进而可能触发不该发生的删除对账（I11 明文
禁止）。`arcad` 的其它数据集（不同存储根，独立故障域，spec §4.3.2）不受
影响，照常 `200`。

#### `GET .../changes`：游标失效与 longpoll 的资源上限（M2c Task 2/3）

`since` 缺省即"从头开始"；`since` 提供时先经 `Cursor::parse`（`FORMAT.md` §4：
`<epoch>:<seq>`，`epoch` 必须是合法的 32 位小写十六进制）——**语法不合法直接
`400 request.cursor_invalid`，绝不当作"从头开始"处理**（I5：一个解析不出来
的游标可能是客户端的 bug，也可能是被篡改的输入，"从头开始"是一个静默但影响
巨大的猜测，不能替客户端做这个决定）。

语法合法之后才谈得上"游标是否还在保留区间内"：本切片没有实现 journal
压缩（`crates/arcad/src/journal_store.rs` 仍是 M2 之前的骨架），`epoch`
因此只在数据集的 journal 从未初始化过时不存在，一旦存在就不会变——**任何
`since.epoch` 与数据集当前 epoch 不一致的游标，都视为"早于保留区间"**
（`journal.reset_required`，§7 已注册，`class=protocol`），选用 HTTP
`410 Gone`（RFC 9110 §15.5.11：目标资源不再可用，且这个状态应视为永久）
——游标指向的正是一个不再可服务的历史位置，语义上比 `409 Conflict`
更精确，也与 `412`（CAS 的"版本"维度）区分开，不共用同一个状态码。响应体
带 `cursor` 字段给出数据集当前实际游标，客户端据此先做一次全量对账
（`GET .../state`），再从这个 `cursor` 继续增量拉取——不是简单地把 `since`
清空重来，那样会在有历史的数据集上把 `epoch` 都读错。

`wait`：longpoll 挂起的秒数（spec §5.2「客户端挂起 30–90 秒」）。**服务端把
它钳制到 `[0, 90]`，超过上限不报错、静默取 90**——这是 M2b 评审在 C2
（单请求 600MB PUT 让 RSS 涨到 1.86GB、无并发上限）之后留下的教训在一个新
维度上的复现：挂起的连接本身不占大量内存，但**占用连接与等待时长**，一个
声称 `wait=999999` 的客户端不该让 `arcad` 真的挂一个请求处理线程/任务
将近 12 天。上限选 90 秒，与 spec 明文的挂起区间上界一致，不是随意选定的
数字。

挂起期间**每次重新探测新事件都会重新打开、重新校验存储根身份**（与非
longpoll 端点的"每请求重新打开"是同一条纪律，见 `storage.rs` 模块文档）：
数据集在挂起期间掉线，下一次探测立即发现并返回 `503`，不会等到 `wait`
超时才返回——挂到超时才返回空增量，在客户端看来与"这本来就是个没有变化的
空库"无法区分，等价于 I11 明确禁止的"呈现为空库"。

服务端**另设一个独立于全局并发请求上限（`MAX_CONCURRENT_REQUESTS`）的
longpoll 并发上限**：一次挂起可能占用一个请求处理槽位长达 90 秒，若不单独
限制，一批恶意或行为不当的 longpoll 客户端能把全局并发配额全部耗尽，
连累所有 `PUT`/`GET`/`DELETE`——那正是新引入的资源维度（brief 原文：
"挂起的连接"）。超过 longpoll 专属上限的请求不排队等待（排队本身也占用
全局配额，无助于隔离），而是**降级为立即返回当前增量**（可能为空），
等价于 spec 提到的"2 秒短轮询"这条降级路径的极限形式——不算错误，客户端
按正常的空增量处理，下一次重试即可。

#### `POST .../rename`：为什么是独立端点，为什么请求体带 `parent` 不是 `If-Match` 头（M2c Task 5）

两机端到端落地时发现：`PUT`/`DELETE` 现有的身份校验（评审 C1）刻意让
"一个 item_id 只能同时归属一个路径"与"被 tombstone 的 item_id 永不可复用"
两条规则**不可绕过**——这是防住伪造身份接管的正确设计，但也意味着
"`DELETE from` 再 `PUT to`（同一 item_id）"这种用现有两个端点拼出改名的
组合，会被第二步的身份校验拒绝（`from` 的 tombstone 已经永久终结这个
item_id，见 §7 `request.item_id_mismatch` 的注册说明）。改名必须是独立的
第三个写入原语：**不产生新版本**——内容没有变化，`items/<item_id>.jsonl`
版本链不动，只搬 `files/<path>` 物理文件与 `index/` 的路径→item_id 映射，
journal 追加一条 `op=rename` 事件（`FORMAT.md` §7.2 早已定义这个操作码与
`from` 字段，此前只有读侧实现，从未被任何写入端触发——与 M2c Task 1「`commit`
从未写 `Op::Upsert`」是同一类型的落地缺口）。客户端侧对应
`Transport::rename`（`crates/arca-cli/src/transport/mod.rs`），完整论证见
其文档「为什么需要这第三个写入原语」。

请求体带 `parent` 而不是复用 `If-Match` 请求头：这次 CAS 校验的对象是
`from` 路径的当前版本，但 URL 只有一个路径段（`{id}`），`from`/`to` 两个
路径都必须出现在某处——放进 JSON 请求体比"其中一个进 URL、另一个进自定义
头"更不容易在实现/客户端两端读错谁是谁；`parent` 随请求体传递、不占用
`If-Match` 头，是这个决定的直接推论（并非不遵守"CAS 认版本号"的既有纪律，
只是这次传递的载体是请求体字段而不是请求头）。

`arca_core::reconcile::decide` 本身不产生"改名"这个动作——三态调和仍然是
逐路径独立判断（spec 设计如此，`arca-core` 本切片未改一行）；改名的
**检测**（同一次 `arca sync` 里，一个路径消失、另一个路径以相同内容出现）
在客户端 `arca-cli::sync` 里用内容哈希匹配完成，检测到之后才调用这个端点，
不经过 `decide()` 的逐路径决策表——这条检测规则不是协议的一部分（纯客户端
启发式，不同客户端实现可以有不同的检测策略），协议只定义"如何提交一次
已经确定的改名"。

#### HTTP 状态码 ↔ `code`（§7 表格的 M2 部分，随端点增补，只增不改语义）

| HTTP 状态码 | `code` | 出现在 |
| --- | --- | --- |
| `400` | `request.if_match_required` | `PUT`/`DELETE` 缺少必需的条件头，或 `If-Match` 语法解析不出合法 `version_id` |
| `400` | `request.metadata_missing` | `PUT` 缺少 `Arca-Item-Id`/`Arca-Version-Id`/`Arca-Mtime` |
| `400` | `path.rejected` | 路径未通过 `path_rules::check`（含 `.arca` 保留段，`FORMAT.md` §2） |
| `400` | `request.hash_missing` | `GET .../trash/{item_id}` 缺少或格式不合法的 `hash` 查询参数（含 `hash` 参数重复出现，评审 Minor 项：与"没提供"共用同一诊断） |
| `400` | `request.item_id_invalid` | `GET .../trash/{item_id}` 的 `item_id` 不是合法的 32 位小写十六进制 |
| `400` | `request.header_ambiguous` | `Range` 续传的 `If-Match` 重复出现且取值有歧义（评审 Minor 项：`PUT`/`DELETE` 的同类歧义复用 `request.if_match_required`，语义已经是"没有提供有效的单一条件"） |
| `400` | `request.hash_invalid` | `GET .../blobs/{hash}` 的 `{hash}` 不是合法的 `blake3:<hex>` 形式（M2c Task 1） |
| `400` | `request.batch_malformed` | `PUT .../batch` 请求体不是合法 JSON，或某一条的路径/`item_id`/`version_id`/`parent`/`mtime` 不合规、或 `content_base64` 不是合法 Base64（M2c Task 1，响应体附 `index`；批量端点用一个码覆盖全部条目级结构问题，不像单文件端点那样为路径单独区分 `path.rejected`——`index` 已经足够定位，不需要额外的码区分维度） |
| `400` | `request.cursor_invalid` | `GET .../changes` 的 `since` 不是合法的 `<epoch>:<seq>` 语法（M2c Task 2，I5：不当作"从头开始"处理） |
| `400` | `request.session_invalid` | `PUT`/`DELETE`/`PUT .../batch` 携带的 `Arca-Session` 不是合法 sid（M2c Task 4，缺失记空、非法拒绝，见上「通用约定」一节） |
| `400` | `request.rename_malformed` | `POST .../rename` 请求体不是合法 JSON，或 `from`/`to`/`item_id`/`parent` 缺失/格式不合法（M2c Task 5） |
| `404` | （无 `code`，标准 HTTP 语义已自解释） | 路径/记录此刻不存在 |
| `409` | `request.item_id_mismatch` | `Arca-Item-Id` 与目标路径/该 item 自身版本链实际归属的身份不符，或该 item_id 已被 tombstone 终结（评审 C1，见 §7 总表）；`PUT .../batch` 某一条命中同一判定时同样用这个 `code`，响应体附 `index` |
| `410` | `journal.reset_required` | `GET .../changes` 的 `since.epoch` 与数据集当前 epoch 不符——游标早于保留区间（M2c Task 2，spec §5.2） |
| `412` | `commit.stale_parent` | CAS 冲突，结构化响应体见上；`Range` 续传的 `If-Match` 与当前版本不符时同样用这个 `code`（响应体退化为只带 `theirs`，没有 `base`/`yours`——续传不是一次写入提交，没有这两者的概念）；`PUT .../batch` 某一条 CAS 冲突时同样用这个 `code`，响应体附 `index`，整批不生效 |
| `413` | `request.body_too_large` | `PUT` 请求体超过体积上限（评审 C2：流式接收，累计超限即中止，不等请求体收完） |
| `500` | `store.corrupt` | `arcad` 已通过挂载检查、请求本身也合法，但从存储层拿到失败——链断裂、内容缺失、索引/journal 解析失败等（评审 I2，见 §7 总表） |
| `503` | `mount.absent` / `mount.identity_mismatch` | 数据集离线（I11），含 `GET .../changes` 挂起期间掉线（立即返回，不等 `wait` 超时） |
| `504` | （无 `code`，标准 HTTP 语义已自解释） | 单次请求处理超时（评审 C2，传输层兜底，不是本节定义的业务失败） |

`request.if_match_required`/`request.metadata_missing`/`request.hash_missing`/
`request.item_id_invalid`/`request.header_ambiguous` 是本节新增的五个码，
`class=needs_human`（调用方的客户端实现有 bug，需要人修，不是可以退避重试
的瞬时故障，也不是协议层的正常冲突）——按 §7 的既有登记纪律补进那张总表；
`request.item_id_mismatch`/`store.corrupt`/`request.body_too_large`/
`request.body_read_failed` 随 M2b 切片评审的 C1/C2/I2 修复补入总表，同样
`class=needs_human`。`request.hash_invalid`/`request.batch_malformed`/
`request.cursor_invalid` 随 M2c Task 1/2 补入，`request.session_invalid`
随 M2c Task 4 补入，`request.rename_malformed` 随 M2c Task 5 补入，同样
`class=needs_human`；
`journal.reset_required` 早已在 §7 登记（`class=protocol`），本节只是把它
接到具体的 HTTP 状态码上。longpoll/SSE（`Content-Type: text/event-stream`
的 agent 场景）与更多端点落地时继续增补，不改动已登记条目的语义（I10）。

**实现落地时对本节文本未明确覆盖的两处分支做了最小一致延伸**
（`crates/arcad/src/api.rs`，M2b Task 5）：

- `PUT` 用 `If-None-Match: *`（仅创建）提交、但路径此刻已存在而冲突时，
  412 响应体的 `base` 为 `null`——本节「412 的响应体」一节的示例只覆盖了
  「客户端声明了一个具体 `version_id`」的情形；`If-None-Match: *` 场景下
  客户端没有声明任何版本，`null` 与 `theirs`/`yours` 在"这里没有这个东西"
  上用同一个记号，不是遗漏。
- `base.hash`/`base.size` 需要反查 `items/<item_id>.jsonl` 版本链才能得到；
  找不到对应版本时（原文「理论上不该发生」的情形）响应体只留
  `base.item_id`/`base.version_id`，不让这个诊断性丰富信息的缺失连累整个
  412 响应失败。

## 2. 上传协议

- 分块断点续传、幂等五元组、丢失 commit 的 no-op 恢复（继承 Lazync §7）。
- 修改过的文件只传变化的 CDC 块（FastCDC + BLAKE3 索引，hub 已有块跳过）。
- 上传前两轮稳定性签名防抖。
- TODO：会话状态机定义。

## 3. journal 与游标

- append-only、`epoch:seq` 游标、压缩后 `reset_required` 全量对账兜底。
- 每个事件带 actor（账号 + 设备/agent + 会话，I8）。
- 事件类型表、序列化、线上端点（`GET .../changes`）、游标失效与 longpoll
  的完整定义见 §1.2「`GET .../changes`：游标失效与 longpoll 的资源上限」
  （M2c Task 2/3）——不在本节重复，本节只记录一条本切片顺带补齐的落地前提：

  **`upsert` 事件现在由 `commit`/`commit_streamed`/`commit_batch` 落盘时一并
  写入**（`crates/arca-cli/src/transport/local.rs`，M2c Task 1）。M2a 只让
  `tombstone` 写 journal（删除传播闸门当时唯一的消费者），`commit` 落地一个
  新版本从未写过 `Op::Upsert` 事件——这在 M2a/M2b 语境下无害（`hub::read_remote`
  从 `items/`/`index/` 直接推导 `Present` 状态，不依赖 journal），但本切片
  新增的"变更流"端点如果只回放 tombstone/rename，客户端能看到删除却看不到
  新增/修改，长轮询就失去了存在的意义。这是 Task 1「补 trait」范围之外、
  但支撑 Task 2/3 成立的必要前提，随 Task 1 一并补上，不新增磁盘格式
  （`Op::Upsert` 早已在 `FORMAT.md` §7.2 定义，只是此前从未被写入端触发）。

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
- HTTP 请求携带 `Arca-Session: <sid>`（§1.2）：`arca-cli` 的 `http::HttpTransport`
  把本次调用解析出的 sid（同一份 `resolve_sid()`/`ARCA_TRACE_SID` 继承逻辑，
  `trace_sink.rs`）原样放进这个头，随每个请求发送；arcad 校验格式后记入
  journal 事件的 `actor.session`（`FORMAT.md` §3，**缺失记空、非法拒绝**，
  §1.2「通用约定」一节与 `request.session_invalid` 已定），构成 I8 的审计
  闭环——一次改动能从客户端落盘的 trace 会话文件一路追到服务端 journal 里
  同一个 sid 的事件（M2c Task 4）。

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
| `request.if_match_required` | `needs_human` | HTTP `PUT`/`DELETE` 缺少必需的条件头（`If-Match` 或 `If-None-Match: *`）——I4 不允许无条件写，§1.2 |
| `request.item_id_mismatch` | `needs_human` | HTTP `PUT`/`DELETE` 的 `Arca-Item-Id` 与目标路径 / 该 item 自身版本链实际归属的身份不符，或该 item_id 已被 tombstone 终结、不允许任何后续提交复用（M2b 切片评审 C1，§1.2）——`409`，不是 CAS `412`：换 `parent` 重试也不会成功，必须先修正客户端对 `item_id` 的认知 |
| `request.metadata_missing` | `needs_human` | HTTP `PUT` 缺少 `Arca-Item-Id`/`Arca-Version-Id`/`Arca-Mtime` 中的一个或多个，§1.2 |
| `request.hash_missing` | `needs_human` | HTTP `GET .../trash/{item_id}` 缺少或格式不合法的 `hash` 查询参数，§1.2 |
| `request.item_id_invalid` | `needs_human` | HTTP `GET .../trash/{item_id}` 的 `item_id` 不是合法的 32 位小写十六进制，§1.2 |
| `internal.invariant_violated` | `bug` | 内部不变量被破坏 |
| `store.corrupt` | `needs_human` | HTTP 端点在已经通过挂载检查、请求本身也合法之后，仍从 `Transport`（`arca-cli::transport`）拿到失败——链断裂、指针指向的内容缺失、索引/journal 解析失败、EACCES 等（M2b 切片评审 I2）：这些是存储根本身或其可访问性出了问题，不是 `arcad` 代码逻辑错误，绝不能报成 `class=bug` 那种"提 issue"的处置，运维应先跑 `arca fsck`/`arca doctor` 诊断 |
| `request.body_too_large` | `needs_human` | HTTP `PUT` 请求体超过 `MAX_BODY_BYTES`（`crates/arcad/src/api.rs`）——评审 C2：流式接收，累计超限即中止清理，不等请求体收完才拒绝，`413` |
| `request.body_read_failed` | `needs_human` | HTTP `PUT` 流式读取请求体本身失败（客户端提前断开、传输层错误），`400`——评审 C2 |
| `request.header_ambiguous` | `needs_human` | HTTP `Range` 续传的 `If-Match` 重复出现且取值有歧义（评审 M2b 切片评审 Minor 项）——`400`；`PUT`/`DELETE` 的同类歧义复用 `request.if_match_required`，语义已经是"没有提供有效的单一条件" |
| `request.session_invalid` | `needs_human` | HTTP `PUT`/`DELETE`/`PUT .../batch` 携带的 `Arca-Session` 头不是合法 sid（`arca_format::trace::Sid::parse` 校验失败）——`400`（M2c Task 4，I8/I5：缺失记空是正常情形，格式不合法则是不可信输入，不能静默塞进 journal 的 `actor.session` 伪造归因） |
| `request.rename_malformed` | `needs_human` | HTTP `POST .../rename` 请求体不是合法 JSON，或 `from`/`to`/`item_id`/`parent` 缺失/格式不合法——`400`（M2c Task 5） |

TODO：退出码与 `code` 的映射表（M1）。HTTP 状态码与 `code` 的映射表——§1.2
「HTTP 状态码 ↔ `code`」已覆盖 M2b 交付的五个端点；longpoll/SSE/游标（M2c）
与后续端点落地时继续在那张表里增补。
