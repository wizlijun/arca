# M1d · CLI 与 file:// 同步闭环

**完成于 2026-08-05** · 结束时 434 个测试全绿 · 全仓库 19921 行 Rust · 104 个提交

M1 的最后一块，也是**第一次有用户能直接用的东西**。前三块（M1a 存储根 IO、
M1b 调和决策、M1c git 接缝）都是地基，这一块把它们接成闭环。

---

## M1 的验收标准跑通了

spec §12.3 的 M1 行要求：在一个真实的笔记库上 `arca adopt`，**文件原地不动、
`git status` 干净、清单进 git、后续提交不再增长**。实测：

```
$ arca init
$ arca register assets --hub home --hub-url file:///tmp/root
$ arca adopt assets
upload	京都/鸭川.png
upload	街景.mp4
注意：adopt 只阻止未来的提交继续膨胀。已经 commit 过的二进制仍留在 git 历史里……

$ git add -A && git commit -m adopt
git status 是否干净：是
清单进 git：1
受管二进制进 git：0
文件仍在原地：是
```

存储根的 `files/` 是普通文件树，内容逐字节一致——**I1 逃生舱在真实数据路径上成立**。

**双设备同步也跑通了**：设备 A `adopt`，设备 B 拿到 `.gitarca` 与 `dataset.toml` 后
`arca sync`，文件被拉下来、内容一致。全程**没有任何守护进程**——这正是 spec §3.1
「手动模式是基线，必须完整可用」的兑现。

---

## 交付了什么

| 模块 | 内容 |
| --- | --- |
| `scan.rs` | 遍历数据集、流式算 BLAKE3、不合规路径进 `rejected` 并发 `path.reject` |
| `baseline.rs` | 客户端投影（I9 可抛弃）；损坏/缺失吸收为空基线 + `ResetReason` 信号 |
| `hub.rs` | 从存储根读远端状态 |
| `adopt.rs` | 就地纳管——I6 文件原地不动 |
| `sync.rs` | 闭环：scan → `arca_core::decide` → 执行 → 更新基线 |
| `commands/` | `init` / `register` / `adopt` / `sync` / `status` / `verify` / `doctor` / `fsck` + plumbing |
| `trace_sink.rs` | trace 失败落盘到全机唯一的 `<state>/trace/` |
| `arca_store::atomic::Batch` | 批量提交：目录 fsync 延迟去重 |

**决策全部来自 `arca_core::decide`**——CLI 里没有第二套判断逻辑。这是客户端与 hub
共用同一段代码的根基，也是本切片最重要的纪律。

---

## 执行中的三次拍板

### tombstone 属 M2，不是 M1（实现者提出，我确认）

实现者在写 `hub.rs` 时发现计划里的一句话不可实现：「当前版本是 tombstone 记录时产出
`RemoteState::Tombstoned`」。他查证后指出——`items/` 的版本链**结构上只放 upsert
形状的记录**，FORMAT.md §7.2 明文规定 tombstone 只活在 `journal/` 里，而 spec §12.3
把 journal 与 tombstone 划进 M2。

**是我写计划时把 M2 的能力写进了 M1 的数据源。** 采用他的方案 1：`read_remote` 只产出
`Present`，`RemoteState::Tombstoned` 保留类型支持但 M1 构造不出来。

连带后果必须一起处理：本地删除会得到 `TombstoneRemote` 决策，而 M1 无处落盘。
**绝不能静默当 no-op**——按 I5，能力缺失要说出来。最终形态是
`SyncReport::tombstone_pending`，CLI 打印「删除传播属 M2，本轮未执行」，退出码 1。

### 存储根的创建权归 `arca-store`，不归 CLI

实现者问「全新存储根的创建放哪」，倾向放 CLI 内部助手。我改判放 `arca-store`：
读写存储根的消费者有两个（M2 的 arcad、M1 的 CLI），**创建是同一类工作**；
放 CLI 意味着 M2 要么重写要么反向依赖 CLI crate。还有一条更实际的理由——
骨架的布局常量与 `format.json` 结构就住在 `arca-store`/`arca-format`，
**创建逻辑挨着校验逻辑放，两者才不会漂移**。

### trace 落盘位置：规范对，我的 brief 错

我在 brief 里写「落到 `<dataset>/.arca/client/trace/`」，实现者按字面做了，
但同时标记出这与 FORMAT.md §10.6 / spec §3.3 定义的**全机唯一** `<state>/trace/`
不符，并写明了连带缺口。

**他是对的。** 一个位置依赖「成功解析出数据集」的 trace，恰恰记录不了
「解析数据集失败」这件事——而那正是最需要线索的时刻（注册表损坏、数据集嵌套、
hub 身份不符）。已改回规范定义的位置。

实现者还顺带纠正了我的一处转述错误：macOS 路径他按 FORMAT.md 原文的
`~/Library/Logs/arca` 而非我括号里随手写的 `~/Library/Application Support`——
依 I10「唯一真相源」以规范文档为准。这个判断是对的。

---

## 1 万文件基准：160 秒，未达 120 秒预算

spec §12.3 要求「1 万张照片 2 分钟内去重归档 + 全量校验」。实测：

| | 初版 | 加批量提交后 |
| --- | --- | --- |
| 归档（哈希 + 写入） | 308.4 s | **159.86 s** |
| 全量校验 | 0.49 s | 0.54 s |
| 合计 | 308.9 s | **160.4 s** |

批量提交（目录 fsync 延迟去重）带来 1.93× 提速，但**仍超预算 33%**。

**没有为了凑指标改动实现或标准**，`#[ignore]` 的基准测试如实保留失败状态。

**下一个瓶颈已定位**：每次 Upload 仍有 3 次独立的文件级 fsync
（`files/` / `items/` / `index/` 各一次，1 万文件即 3 万次）。

**为什么这轮到此为止**：再往下要动的是「批内是否可以推迟内容 fsync」——
那是对持久性保证的**实质修改**（推迟后崩溃可能让部分文件回退到旧内容），
值得单独一轮设计与评审，而不是在长会话末尾赶工。差 33% 而不是差 3 倍，
诚实记下比凑指标有价值。**记为 M2 的待办。**

批量提交的持久性论证：`Batch::write` 内的 tmp→fsync→rename 逐次立即完成，
内容持久性与逐文件 `write` **完全一致**；只有目录项的落盘被推迟到 `commit()`。
`commit()` 失败则不保存基线、整个 `sync` 按失败返回（I3）。

---

## `doctor` 背着两条别处欠下的债

**债一：`git clean -xdf` 风险的唯一缓解措施。** M1c 实测确认 `git clean -xdf`
（以及 `-Xdf`）真的会删掉受管二进制。项目决定接受这个风险不绕过，
缓解措施就是 `doctor` 检出「本地存在但 hub 尚无副本」的文件并**显著告警**——
告警前后各留空行、以「！！！」开头，用户在跑 `git clean` 前扫一眼就该看见。

**债二：`Issue::CheckIncomplete` 必须显式呈现。** M1c 的评审两次发现静默降级，
修的就是这个。`doctor` 把「这项检查没跑成功」当成「检查通过」，等于把整条修复白做。

**还有 `check_ignore_no_index`**：`git check-ignore` 默认查 index，
已被追踪的路径一律报「未忽略」——于是「元数据已入库、反选块后来被改坏」
这个最需要检出的场景会**假通过**。doctor 断言反选块正确性时用的是
M1c 专门为它加的 `check_ignore_no_index`。

---

## 有意不做的（明确写出，不是悄悄少做）

`history` / `restore` / `gc` / `bundle` ——spec §12.3 的 M1 行里有，但它们都依赖
`.arca/trash/` 与 journal 的完整实现，而那两块的格式在 `FORMAT.md` 里标着「M2 定义」。
在计划里就写明了，不是执行时才发现。

---

## 留给 M2 的

- **1 万文件基准的剩余 33%**：需要重新设计写入形状（每次 Upload 的 3 次文件级 fsync）
- **删除传播**：tombstone + journal，`RemoteState::Tombstoned` 与决策表的对应分支
  已就绪，只等落盘能力
- **`https://` transport**：M1 只认 `file://` 与裸路径，遇到 `https://` 明确报
  「该 transport 属 M2」而非静默失败
- M1b 留下的：属性测试的 I3 断言仍是变更探测器而非不变量检查；
  错误码表 `arca-store` 用字面量、`arca-core` 用带类型的 `code()`，该统一到
  `arca-format` 的共享注册表

---

## M1 完成

| 切片 | 内容 | 状态 |
| --- | --- | --- |
| M1a | 存储根 IO 地基 | ✅ |
| M1b | 调和状态机 | ✅ |
| M1c | arca-git（git 接缝） | ✅ |
| **M1d** | CLI 与 file:// 同步闭环 | ✅ |

---

## 切片评审的补充修复（2026-08-08）

M1d 的 10 个提交起初**没有经过切片级评审**。补做时评审**驱动了真实的二进制**，
找到两个 Critical 数据丢失路径——都是这个项目存在的意义要挡住的那类：

### C1：指针先于字节发布（`6c29b05`）

上传顺序原本是 `items/` → `index/` → `files/`，**让版本可见的指针，在它指向的字节之前发布**。
而 `read_remote` 只看 `index/` + `items/`，从不检查内容是否真的存在。

于是上传中途失败（ENOSPC、拔盘）会留下「hub 宣称有这个版本，字节却从未落地」，
而 `status` / `sync` / `doctor` **全部报告干净**，只有 `verify` 能抓到。更糟的是基线丢失后，
零传输认领路径会把这个谎言**写进基线**——用户看到全清后删掉本地副本，内容两边都没了。

修法两条缺一不可：写入顺序改为 `files/` → `items/` → **`index/` 最后**（指针最后发布，
中途失败留下的是「有字节没指针」这种无害状态）；`read_remote` 要求内容实际存在，
索引有记录而内容缺失是**损坏**，按 I5 如实报告而不是静默当成「文件不存在」
（后者看似自愈，实则掩盖存储根损坏）。

### C2：`adopt` 在未挂载的路径上凭空造存储根（`1e0b46f`）

`MountError::Absent` 被直接映射到 `StorageRoot::create`，于是**「外置盘被拔了」与
「全新的根」不可区分**。基线也丢失时（按 I9 那是**正常的可抛弃状态**），
`arca adopt` 会静默把整个数据集复制到幽灵挂载点上，退出码 0。
而且它盖的是正确的 `dataset_id`，之后 `StorageRoot::open` 会欣然接受，
把真正的 hub 呈现为「什么都没了」——I11 明令禁止的「呈现为空库」的镜像形态。

修法的关键是**不用基线兜底**——基线是唯一保证会被丢弃的输入，拿它当安全网方向就是反的。
数据集自己带着信号：`<dataset>/.arca/manifest` 是进 git 的。manifest 存在即说明
此前被纳管过，此时根缺失就是真的不见了，按 I11 拒绝；确实要新建时用显式的 `--create-root`。

### 四条 Important

- **`doctor` 从不断言 `.gitignore` 反选块**——CLAUDE.md 点名的全设计最危险处，
  而 doctor 对它完全失明。M1c 专门为此加的 `check_ignore_no_index` 全仓库唯一调用者
  是一个测试。现在用探针路径实测三件事：受管二进制被忽略、`.arca/` 元数据不被忽略、
  `.arca/client/` 被忽略
- **`sync` 从不告知基线被重置**——字段算出来了却没人读，而 `status` 读了。
  混合场景下最要命：基线重置正是一次普通编辑变成冲突的原因，用户却被蒙在鼓里
- **`doctor` 静默丢弃解析不了的数据集然后报告 vault 干净**——M2 一上 `https://` hub，
  每个用户都会撞上
- **`sync` 从不重新生成清单**——清单是协作者从 git 拿到的东西，它在日常命令上静默漂移，
  用户会提交并推送一份漏掉受管文件的清单

另修：清单 mtime 改取文件自身 mtime（原本用 adopt 的墙上时钟，导致每次重跑都弄脏清单，
**侵蚀了「adopt 后 git status 干净」这条验收性质**）；`VaultError` 的退出码统一。

### 实机验证

补修后我逐条驱动二进制确认：「有索引无内容」的存储根让 status/sync/doctor/verify
全部退出 1 并给出点名 item_id 的诊断，而健康状态下 status/doctor 仍是**退出 0、零字节输出**；
manifest 存在 + 根被移走时 adopt 退出 2 且**未创建任何目录**；
清空 `.gitignore` 与「只有排除行没有反选行」两种破坏都被 doctor 抓出并退出 1。

446 个测试全绿，四项门禁通过。

### 留给 M2 的一条隐患（本轮未改）

`write_local_atomic` 完全不 fsync，而基线在其后保存。崩溃可能留下
「基线持久、下载的内容丢失」。下次 sync 看到 `(base=present, local=absent, remote=present)`
→ `TombstoneRemote`。M1 里只报告所以无害，**但 M2 真的执行 tombstone 之后，
这就变成崩溃引发的 hub 副本销毁**。M2 开工前要定策略：下载内容先 fsync 再存基线，
或给删除传播加确认闸门。
