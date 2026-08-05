# M1a · 存储根 IO 地基

**完成于 2026-08-05** · 10 个提交（`72dd670`..`1df3f0b`）· 11 个文件、2355 行新增  
· 结束时 207 个测试全绿（M1a 新增 39 条）

M1 的第一块。M0 的最终评审把它列为 M1 开工前必须闭合的项：`arca-store` 当时只有 `fsck`，  
`atomic` 与存储根身份校验都还是 `TODO(M1)`——也就是说 **I11 的挂载检查至今没有实现**，  
而 trace 的 `mount.check` 事件与 `PROTOCOL.md` 的错误码已经定义好了，golden trace 里那条  
`mount.check` 一直是空头支票。

---

## 为什么这块要先做

I11 要防的事故形态很具体：外置盘被拔掉、NFS 掉线、挂载点漂移之后，  
**未挂载的卷与空库在字节上难以区分，语义上却天差地别**。把前者当后者，  
同步引擎会认为「远端把文件全删了」，于是触发删除对账，把用户本地的数据清掉。

M0 的逃生舱脚本已经在浅水区踩过这个坑（空 `files/` 被报成干净恢复，见 M0 文档）。  
M1a 是把它在真正的数据路径上堵死。

---

## 交付了什么

**`StorageRoot::open`** —— 触碰存储根的唯一入口。它把五种失败变成**彼此可区分**的结果：

| 变体 | 含义 | 运维该做什么 |
| --- | --- | --- |
| `Absent` | 根不存在，或存在但没有 `.arca/format.json` | 检查挂载、检查路径——**绝不能当成「库是空的」** |
| `IdentityMismatch` | 身份标记存在但与期望不符 | 挂到了别的数据集上 |
| `Malformed` | 身份标记存在但读不出来 | 与「没有身份」是不同的故障 |
| `Io` | 读取失败（权限、挂载点损坏） | 挂着但读不了，与「不存在」要采取不同行动 |
| `BadExpectedId` | 调用方传入的期望 id 格式非法 | 调用方参数错误，不是卷的问题 |

`open` 是**纯只读的**——无论成功失败都不创建任何文件或目录。理由：探测一个可能未挂载的  
路径时留下副作用，会在挂载点上制造出「本地空壳目录」，正好是它要检测的那种故障形态。

**`atomic::write`** —— 唯一的写入路径：写内容 → fsync 文件 → rename → 逐层 fsync 到根。  
崩溃后要么看到旧内容、要么看到新内容，绝不看到半截。

**`sweep_tmp`** —— 崩溃残留清理，纪律继承自 lazync：`.arca/tmp/` 下的孤儿**普通文件**  
可以安全删除；出现**符号链接或目录**则拒绝并报告，**绝不递归删除**。  
理由是 I3 ∩ I5——tmp 里本不该有目录，出现了就说明状态超出预期，此时递归删除是把  
「我不理解这个状态」变成「我删掉了不理解的东西」。

**trace 发射** —— `mount.check` 在六条返回路径上各发一条（失败路径同样发，它们的线索  
比成功路径更有价值），失败路径另发一条 `error` 事件带 `code` 与 `class`。

**`fsck` 收口** —— 改经 `StorageRoot` 打开，不再自己读 `format.json`。  
「这不是一个存储根」（`Err`）与「这个存储根里有问题」（`Ok(报告)`）成为两种不同的答案，  
而不是都塞进 `FsckReport` 当成一条 `Problem`。两个从此永不产生的 `Problem` 变体已删除。

---

## 执行中做的决定

### `join` 从 `PathBuf` 改为 `Result<PathBuf, RootEscape>`（`fc52496`）

计划里原本写的是 `join(&self, relative: &str) -> PathBuf`，直接转发 `Path::join`。  
评审指出这是个无防护的逃逸口：**`Path::join` 遇到绝对路径会把 `self.path` 整个丢掉**——  
`root.join("/etc/passwd")` 返回的就是 `/etc/passwd`。唯一防线是一句文档注释。

`StorageRoot` 这个类型存在的全部意义就是「持有它就不必在每个调用点重新推导根的安全性」，  
所以拒绝绝对路径、`..` 组件与盘符前缀是**它的职责**。判断用 `Path::components()` 匹配  
`Component::ParentDir` / `Component::Prefix`，不用字符串 `contains("..")`——那会误伤  
名为 `a..b` 的合法文件名。

### `mount.check` 的字段语义写进 FORMAT.md §10.3（`68889d9`）

原来 §10.3 只列了字段名 `dataset_id · expect · found · ok`，没定义各自含义。  
实现者按「expect 否则 found 否则空串」处理 `dataset_id`，但这只活在 Rust 的 doc comment 里——  
第二个实现根本看不到，同一个物理场景会导出不同字节，违反 I10 的第三方可复现要求。

定稿语义：`dataset_id` 是「这条事件关于哪个数据集」的**关联键**（agent 靠它跨事件族分组），  
取 expect 优先、否则 found、否则空串；`expect` 无期望时**整个字段不出现**；  
`found` 读不到时是**空字符串**。「字段缺失」与「字段为空」是两个不同的信号，  
agent 做精确匹配时要能区分——这一点两侧都有测试盯着。

### `PROTOCOL.md` §7 补两条错误码（`1df3f0b`）

`MountError::Io` 与 `BadExpectedId` 在原码表里没有对应的 code。补了  
`mount.io_error`（`needs_human`）与 `mount.bad_expected_id`（`bug`）。

注意 `mount.io_error` 归为 `needs_human` 而不是 `retryable`：NFS 抖动值得重试，  
但那是**上层策略**，不该在这里判定。

---

## 评审抓到了什么

### 计划自身的两个错误

- `join` 的无防护转发（见上）——是我写进计划的代码
- `open_不发任何事件` 这条测试**永远不可能失败**：它构造了 `VecSink` 但 `open` 根本不接  
  sink 参数，断言恒真。已改为「`open` 与 `open_traced` + `NullSink` 对同一输入返回相同结果」，  
  这才是「`open` 是薄壳」的可执行断言

### 切片级最终评审（opus）——四条 Important，全部实测发现

评审自己构造场景跑出来的，不是读代码猜的：

1. **`sweep_tmp` 检查了每个条目，却没检查 `.arca/tmp` 目录本身。**  
   它若是符号链接，`read_dir` 会跟过去，把被指向目录里的每个普通文件当孤儿删掉——  
   把本 crate 唯一被授权的删除变成了批量删除，直接违反 I3。  
   而且不需要恶意场景：管理员用 `ln -s` 把 tmp 挪到别的卷是常规操作，  
   rsync/Dropbox 同步来的数据集根也可能带进符号链接
2. **`atomic::write` 只 fsync 直接父目录。** `create_dir_all` 新建的上层目录从未 fsync，  
   于是掉电后 `Ok()` 报告已提交的文件可能整个不可达。代码注释自己论证过  
   「rename 让新内容可见但目录项可能还没落盘」——同一条论证对上一层的 `mkdir` 同样成立，  
   却没照做
3. **`mount.check` 无法区分 Absent / Io / Malformed。** 三条失败路径的载荷逐字节相同，  
   而模块文档正说这三者必须彼此可区分。Rust 的类型区分了它们，可观察的记录没有
4. **`AtomicError` 无法区分 rename 前失败与 rename 后失败。** 后者目标**已被原子替换**，  
   只是那条目录项的持久性未确认，调用方却收到与「磁盘满、目标完全没动」相同的变体。  
   M1b 的调和状态机需要这个区分来决定重试还是回滚

另修两条 Minor：`join` 原本放行 `""` 与 `"."`（于是 `target.parent()` 是存储根的**上一级**）；  
临时文件原用 `File::create`（会跟随符号链接并静默截断，且 pid 被系统回收后会撞名），  
改用 `create_new`。

---

## 验证证据

`cargo test --workspace` 207 通过 / 0 失败 · clippy `-D warnings` 零告警 ·  
`cargo +1.85 check --workspace --locked --all-targets` 通过 · `cargo fmt --all -- --check` 干净。

CLI 三个退出码实测：不存在的根 → **2** 并打印中文 I11 提示；健康的根 → **0** 且  
stdout/stderr **零字节**输出（Rule of Silence）；有问题的根 → **1**。

---

## 留给后续的

**M1d 必须处理的**

- **目前没有任何 arca 二进制真的发出过 `mount.check`**：`fsck::check_path` 调的是 `open`  
  （走 `NullSink`），CLI 完全没有 trace 接线。这是 M1a 正确的范围划分，但意味着 trace  
  这一侧目前只被测试覆盖。M1d 要接真实 sink、改调 `open_traced`，并补  
  `PROTOCOL.md` 自己的「TODO：退出码与 code 的映射表（M1）」
- **`atomic::write(&[u8])` 无法用于多 GB 视频**——需要流式变体。是**加法**不是重设计，  
  且句柄形式（`begin`/`commit`）更好：M1d 可以在同一遍扫描里算内容哈希而不必重读文件
- **每文件一次 `create_dir_all` + 一次目录 fsync**：首次同步上千个小文件时是每文件一次  
  fsync，M1d 会想要延迟/批量的父目录 sync

**要写进 M1b 调和契约的**

`atomic::write` 本身是**破坏性原语**——`fs::rename` 会静默覆盖目标，唯一防线是 `join` 的  
词法检查。I3 的「同步路径无销毁权」在本 crate 内只对 `.arca/tmp/` 强制；对 `files/`  
要靠调用方保证「绝不把当前内容尚未记入 `items/` 的路径交给 `write`」。  
这条要明写进 M1b 的契约，不能当默认假设。

**已知限制**

- `join` 是**词法**检查：根内部的符号链接祖先目录仍能把写入重定向到根外（评审实测确认）。  
  当前威胁模型（本地存储根，非敌对多用户目录）下可接受，但 `RootEscape` 的文档把 `join`  
  描述成「根安全性确立之处」，该补一句说明它是词法的
- `sweep_tmp` 不验证「孤儿」：它删除 tmp 下每个普通文件，包括并发运行的另一个 arca 进程  
  的活跃临时文件。后果有界（Unix 上 unlink 已打开的 fd 是安全的，受害方的 rename 会  
  `ENOENT` 失败，无损坏），但真正的「孤儿」判定需要锁纪律（`.arca/locks/`），那还不存在
- 非 Unix 平台跳过目录 fsync（`File::open(dir)` 在 Windows 上会失败）。这是诚实记录的  
  平台局限，Windows 的等价保证属 M3 范围

---

## M1 的其余切片

| 切片 | 内容 | 依赖 |
| --- | --- | --- |
| **M1a** ✅ | 存储根 IO 地基 | 无 |
| M1b | `arca-core` 调和状态机（sans-io 三态对账）+ `reconcile.decide` trace 发射 + 确定性模拟测试与 proptest 收敛性 | M1a |
| M1c | `arca-git`：`.gitignore` 反选块（全设计最易出错处）+ 清单同步 + pre-push 钩子 + 追踪冲突检测 | 无，可与 M1b 并行 |
| M1d | CLI porcelain/plumbing + `file://` 直连同步闭环 + trace 失败落盘；跑通 spec §12.3 的 M1 验收演示 | M1a + M1b + M1c |

每块独立可用、独立可演示，避免 Perkeep 式「大教堂建到一半」。