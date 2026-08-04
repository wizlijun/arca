# arca 磁盘格式规范（FORMAT.md）

> 状态：**v1 定稿**。本文件与代码同仓、同评审、先行合并（I10：格式先于代码）。
> 格式变更需要 RFC 流程：先改本文件、经评审，再改实现；`crates/arca-format` 以 golden vectors 回归验证一致性。
> 设计依据：`docs/2026-08-03-arca-spec.md` §4（数据模型、存储与拓扑）。本文件是其**字节级实现契约**，
> 不复述 spec 的设计论证——需要"为什么这样设计"查 spec，需要"精确长什么样"以本文件为准。

## 0. 版本与兼容性承诺

- 所有磁盘格式带版本号。`format.json` 的 `format` 字段是**存储根**的格式版本；
  单条 JSON 记录内的 `"v"` 字段是**该记录格式**的版本；二者独立演进，互不覆盖。
- 文本格式（`manifest`）以首行魔法注释声明版本，如 `#%arca-manifest v1`。
- 只向前迁移：新版本的解析器可以读旧版本数据（并按需升级），永不静默改格式。
- 遇到高于自己已知版本号的格式 → **拒绝并给出明确报错**（I5：绝不猜测），绝不"尽力解析"。
  错误信息须包含：文件路径、期望的最高已知版本、实际读到的版本。

## 1. 通用编码约定

以下约定适用于本文件涉及的**所有**格式，不再逐节重复：

- 字符编码：一律 UTF-8，不带 BOM。
- 换行：一律 LF（0x0A）。写入永不产生 CRLF；解析遇到 CR 结尾时容忍并剥除。
- 时间戳：RFC 3339，UTC，秒级精度，形如 `2026-08-04T10:23:02Z`。
- 哈希文本表示：`blake3:<64 位小写十六进制>`。
- 标识符：`item_id` 与 `dataset_id` 为 128-bit 随机值，表示为 32 位小写十六进制。
- JSON Lines（`.jsonl`）：一行一个 JSON 对象，行内不得含裸换行；
  文件以 LF 结尾；追加写入必须整行原子追加（先构造完整字节串再单次 write）。
- 所有 JSON 对象含 `"v"` 字段（记录格式版本，整数）作为第一个键。

## 2. 路径规则

以下限值**照搬自 lazync**（前身项目，Free Pascal，`/Users/bruce/git/lazync`），不重新设计，
出处为 `shared/src/nc_path_rules.pas` 与 `docs/LIMITS.md`：

| 项 | 值 | 出处 |
| --- | --- | --- |
| 相对路径最大字节 | 2048 | `nc_max_relative_path_bytes` |
| 目录最大深度 | 64 段 | `nc_max_path_depth` |
| 单段最大字节 | 240 | `nc_max_path_segment_bytes` |
| 物理路径最大字节 | 3800 | `nc_max_physical_path_bytes` |
| 非法字符 | `< 0x20` 控制字符、`< > : " \| ? *` | `nc_has_invalid_char` |
| 非法段结尾 | 空格、句点 | 同上 |
| Windows 保留名 | CON PRN AUX NUL COM1–9 LPT1–9（按首个 `.` 前的部分比较，大小写不敏感） | `nc_is_windows_reserved_name` |
| 规范化 | `\`→`/`、折叠重复 `/`、丢弃空段与 `.` 段 | `nc_normalize_relative_path` |
| 单文件大小上限 | 2,000,000,000,000 字节 | `LIMITS.md`（对齐 Dropbox 桌面端 published 值） |
| 索引键 | 小写规范化路径的哈希（arca 改用 BLAKE3；大小写冲突拒绝，绝不静默合并） | `STORAGE.md` §File Identity Index |

arca 特有的三条补充：

- Tab（0x09）与换行（0x0A/0x0D）已被「控制字符」规则排除，
  因此行式 manifest 的 Tab 分隔无歧义（spec §4.4.1）。
- 索引键 = BLAKE3(小写规范化路径的 UTF-8 字节)。
  大小写不同但规范化后相同的两个路径视为冲突：拒绝，绝不静默合并
  （继承 lazync STORAGE.md §File Identity Index）。
- Unicode 规范化：v1 不做 NFC/NFD 转换，按字节原样保存与比较。
  macOS 的 NFD 与其他平台的 NFC 会被视为不同路径——
  这是已知边界，记录在 §10 已知限制，v2 议题。

## 3. 三层模型的磁盘表示

设计论证见 spec §4.1；本节只给磁盘/线上表示。

- **item_id**：128-bit 随机，32 位小写十六进制，创建时分配，永不复用。
- **version_id**：`<RFC3339 紧凑形式><32 位十六进制随机>`，
  例 `20260804T102302Z-0123456789abcdef0123456789abcdef`；
  前缀使版本 ID 的字典序即时间序（继承 lazync STORAGE.md §Historical Versions）。
- **actor**：`{"account": "<字符串>", "device": "<字符串>", "session": "<字符串>"}`；
  三者皆可为空字符串，表示未知；三个键也都允许在 JSON 中整体缺失，
  缺失与空字符串同义（实现侧用 serde 的 `#[serde(default)]`）；journal 每条事件必须携带（I8）。

## 4. hub 存储根布局

```
dataset_root/
├── files/                          ← I1 逃生舱：普通文件树，当前版本完整平放
└── .arca/
    ├── format.json                 ← 见 §5
    ├── index/<xx>/<hash>.json      ← 见 §6；<xx> 为 hash 前 2 位十六进制
    ├── items/<xx>/<item_id>.jsonl  ← 见 §7；<xx> 为 item_id 前 2 位
    ├── chunks/<xx>/<hash>.zst      ← 见 §8
    ├── journal/epoch               ← 单行文本：当前 epoch 标识（32 位十六进制）
    ├── journal/<epoch>.jsonl       ← 见 §7.2
    ├── trash/                      ← M2 定义
    ├── uploads/                    ← M2 定义
    ├── tmp/                        ← 写入暂存；孤儿普通文件可安全清除，
    │                                  出现符号链接或目录则启动失败（绝不递归删除）
    └── locks/                      ← arca.lock + <id>.txn（M2 定义）
```

所有目录必须位于同一文件系统，rename 提交才是原子的
（继承 lazync STORAGE.md）。两级十六进制分片避免单目录条目数过大。

`journal/epoch` 指针文件：单行文本，内容为当前 epoch 的 32 位小写十六进制标识
（以 LF 结尾，见 §1）；它是唯一告诉读者 `journal/<epoch>.jsonl`（§7.2）里哪个才是当前 epoch
的文件，重要性类比 `format.json` 之于数据集身份。三种情况的处置：

- **缺失**：全新未初始化的存储根的合法状态，代表"尚无 journal"，不是错误。
  首次写入 journal 前必须先原子创建该文件（tmp → fsync → rename，同 §6 index 记录的原子替换手法）。
- **内容不是合法的 32 位小写十六进制** → 拒绝并给出明确报错（I5：绝不猜测应该用哪个 epoch）。
- 切换 epoch（M2 压缩流程的一部分）同样走 tmp → fsync → rename 原子替换，绝不原地覆盖。

## 5. format.json

```json
{"v":1,"format":1,"dataset_id":"9c41000000000000000000000000abcd","hash_algo":"blake3","created_at":"2026-08-04T10:00:00Z"}
```

`dataset_id` 即卷身份标记（I11）——hub 配置、客户端绑定请求与本文件三方必须一致，
不符则数据集离线，**绝不呈现为空库、绝不触发删除对账**。
`hash_algo` v1 恒为 `"blake3"`，其他值 → 拒绝。

## 6. index 记录

```json
{"v":1,"item_id":"3f2a000000000000000000000000beef","path":"京都/鸭川.png"}
```

文件名 = `BLAKE3(小写规范化路径)` 的 64 位十六进制 + `.json`，置于其前 2 位命名的子目录下。
整体原子替换（tmp → fsync → rename），不追加。`path` 存**规范化后的显示路径**（保留原始大小写）。

## 7. items 与 journal 记录

hub 侧两条 append-only 事件流，均为 JSON Lines（§1）。

### 7.1 items 版本链记录

`items/<xx>/<item_id>.jsonl`，append-only，一行一个版本，按提交顺序追加：

```json
{"v":1,"version_id":"20260804T102302Z-0123456789abcdef0123456789abcdef","item_id":"3f2a000000000000000000000000beef","parent":null,"hash":"blake3:9f2c…","size":2411008,"mtime":"2026-08-04T10:22:31Z","actor":{"account":"bruce","device":"mac-studio","session":"s1"},"committed_at":"2026-08-04T10:23:05Z"}
```

规定：`parent` 为上一版的 `version_id`，首版为 `null`；`item_id` 在每行重复
（冗余但使单行自描述，截断的文件仍可诊断）；hub 上版本链**线性**，不存在分叉
（CAS 失败以冲突副本落地为新身份，不进链）；**末行不完整时截断到最后一个完整行边界**，
中间行损坏则失败而非跳过（继承 lazync STORAGE.md §Incremental Change Journal 的处置纪律）。

### 7.2 journal 事件记录

`journal/<epoch>.jsonl`，append-only：

```json
{"v":1,"seq":42,"op":"upsert","item_id":"3f2a…","version_id":"20260804T102302Z-…","path":"京都/鸭川.png","actor":{"account":"bruce","device":"mac-studio","session":"s1"},"at":"2026-08-04T10:23:05Z"}
```

`op` ∈ `upsert` / `tombstone` / `rename`。字段随 `op` 变化，语义依据 spec §5.3
（删除 = tombstone；改名/移动 = 身份不动、映射搬家）：`tombstone` 与 `rename` 都不改变内容，
因此不在 items 版本链（§7.1）产生新版本，`version_id` 沿用该 item 最后一个存活版本的 id。

| 字段 | `upsert` | `tombstone` | `rename` |
| --- | --- | --- | --- |
| `version_id` | 新写入版本的 id | 删除前最后一个存活版本的 id | 改名前最后一个存活版本的 id（内容未变） |
| `path` | 当前路径 | 被删除前的路径 | 改名后的新路径 |
| `from` | 不出现 | 不出现 | 必填，改名前的路径 |

```json
{"v":1,"seq":43,"op":"tombstone","item_id":"3f2a…","version_id":"20260804T102302Z-…","path":"京都/鸭川.png","actor":{"account":"bruce","device":"mac-studio","session":"s1"},"at":"2026-08-04T11:00:00Z"}
```

`rename` 示例用另一个 item（`8b1d…`）演示，避免与上面的 `tombstone` 示例连成一条
"删除后又改名"的隐含时间线——本文件未定义 tombstone 之后能否 rename，示例不应替读者做这个判断：

```json
{"v":1,"seq":44,"op":"upsert","item_id":"8b1d…","version_id":"20260804T110400Z-…","path":"书法/兰亭序.jpg","actor":{"account":"bruce","device":"mac-studio","session":"s1"},"at":"2026-08-04T11:04:00Z"}
```

```json
{"v":1,"seq":45,"op":"rename","item_id":"8b1d…","version_id":"20260804T110400Z-…","path":"书法/兰亭序-扫描.jpg","from":"书法/兰亭序.jpg","actor":{"account":"bruce","device":"mac-studio","session":"s1"},"at":"2026-08-04T11:05:00Z"}
```

游标为 `<epoch>:<seq>`；`seq` 在一个 epoch 内单调递增、无空洞。
客户端游标早于保留区间 → 返回 `reset_required`，走全量对账兜底。压缩规则 M2 定义。

损坏处置：**末行不完整时截断到最后一个完整行边界，中间行损坏则失败而非跳过**——
与 §7.1 items 版本链相同的处置纪律，直接继承自 lazync STORAGE.md §Incremental Change Journal
（该节描述的正是 lazync 侧 `journal.bin` 的截断行为，是这条纪律真正的出处；
arca 把它同时用于 journal 与 items 版本链）。

## 8. chunks 块存储

```
chunks/<xx>/<64 位十六进制 BLAKE3>.zst
```

块内容以 zstd 压缩落盘，文件名的哈希是**未压缩内容**的 BLAKE3；
块仅服务历史版本与增量传输，`files/` 的当前版本永远平放（I1，不可谈判）。
切块用 FastCDC，参数见 §8.1。

引用计数与 GC 属 M2，v1 格式为其预留 `chunks/refs/` 目录名，M0 不写入。

### 8.1 切块与压缩参数

- FastCDC：min 16 KiB / avg 64 KiB / max 256 KiB——
  出处：FastCDC 论文（USENIX ATC'16）的推荐区间，avg 64 KiB 在去重率与元数据开销间取平衡。
- zstd 级别 3（默认，压缩比与 ARM NAS 的 CPU 成本平衡，spec §1.1 目标 9）。

## 9. vault 侧文件

vault 侧的目录结构与职责划分见 spec §4.3；本节给出各文件的字段表。

### 9.1 `.gitarca`（根注册表，TOML）

`schema = 1`。

| 表 | 字段 | 说明 |
| --- | --- | --- |
| `[hub.<名>]` | `instance_id` | hub 的稳定身份（与 URL 无关，见 spec §11） |
| `[hub.<名>]` | `url` | 当前如何连接该 hub |
| `[[dataset]]` | `path` | 数据集相对 vault 根的路径 |
| `[[dataset]]` | `hub` | 引用的 `[hub.<名>]` 键名 |

示例（改编自 spec §4.3，删去了其中的第二个 hub 与行内注释）：

```toml
schema = 1

[hub.home]
instance_id = "3f2a…"
url = "https://nas.example.com:8443"

[[dataset]]
path = "assets"
hub  = "home"
```

### 9.2 `dataset.toml`（数据集自描述，TOML）

`schema = 1`。

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `dataset_id` | 是 | 全局唯一，不可变，即 hub 侧 `format.json` 的 `dataset_id`（§5、I11） |
| `hub_instance_id` | 是 | 认的是 hub 身份，不是 URL |
| `public_base_url` | 否 | 发布用（spec §4.9） |
| `url_style` | 否 | `"path"` \| `"hash"`（spec §4.9） |

### 9.3 `manifest`（行式清单）

首行魔法注释：`#%arca-manifest v1`。

其后每行一条记录，Tab（0x09）分隔四个字段：

```
<路径>\t<blake3:hash>\t<字节数>\t<mtime RFC3339>
```

| 字段 | 说明 |
| --- | --- |
| 路径 | 相对数据集根，规则见 §2 |
| `blake3:hash` | 内容哈希，文本表示见 §1 |
| 字节数 | 十进制 ASCII |
| mtime | RFC 3339，见 §1 |

规则：按路径的 UTF-8 字节序升序排序；同内容必产生同字节（确定性序列化）。
Tab 与换行已被路径规则的控制字符规则排除（§2），分隔无歧义。

示例（原样摘自 spec §4.4.1）：

```
#%arca-manifest v1
京都/街景.mp4	blake3:c71a…	1884301776	2026-08-04T10:23:02Z
京都/鸭川.png	blake3:9f2c…	2411008	2026-08-04T10:22:31Z
```

### 9.4 `.gitignore` 标记块

由 arca 生成与维护、幂等、可人工审阅：父目录整体忽略后无法再反选其内部路径，
因此必须先排除内容、再显式反选 `.arca/`：

```gitignore
# >>> arca managed (do not edit inside) >>>
/<dataset>/*
!/<dataset>/.arca/
/<dataset>/.arca/client/
# <<< arca managed <<<
```

每个受管数据集各占三行；`<dataset>/.arca/client/` 是本地投影（gitignored，I9），不进 git。

### 9.5 范围之外

`<dataset>/.arca/catalog/`（目录卡）与 `<dataset>/.arca/client/`（本地投影）不在本文件 v1 范围内：
catalog 的格式由独立工具 `arca-catalog` 定义（spec §4.4）；`client/` 是纯本地、可丢弃的状态，
不构成跨设备/跨实现的字节级契约。

## 10. 已知限制

- **Unicode 规范化不做转换**（见 §2）：v1 按字节原样保存与比较路径，
  macOS 的 NFD 与其他平台的 NFC 会被视为不同路径。已知边界，v2 议题。
- **引用计数与 `trash/` / `uploads/` / `locks/` 格式待 M2**：本文件只固定了它们的目录名与用途，
  未固定字节级格式。
- **逃生舱恢复演示依赖 `b3sum`**（BLAKE3 官方 CLI），严格意义上不属于 coreutils——
  I1 的承诺是"不需要任何 arca 代码"，而非"只用 coreutils"，此处按前者执行并明示。

## 11. 不变量对照

实现不得违反 spec §2 的 I1–I11；本规范每一节标注其约束来源。
