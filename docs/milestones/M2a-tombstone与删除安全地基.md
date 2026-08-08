# M2a · tombstone 与删除安全地基

**完成于 2026-08-08** · 12 个提交 · 16 个文件、5692 行新增 · 结束时 512 个测试全绿
（M2a 新增 66 条）

M2 的第一块，也是**项目第一次真的删东西**。此前每个里程碑都刻意做到「没有任何销毁路径」；
这一块引入了生产代码里唯一的一处 `fs::remove_file`，挡在四道闸门之后。

---

## 为什么这块必须排在 M2 最前面

M1d 的切片评审留了一条明确的前置条件：

> `write_local_atomic` 完全不 fsync，而基线在其后保存。崩溃可能留下「基线持久、
> 下载的内容丢失」。下次 sync 看到 `(base=present, local=absent, remote=present)`
> → `TombstoneRemote`。M1 里只报告所以无害，**但 M2 真的执行 tombstone 之后，
> 这就变成崩溃引发的 hub 副本销毁**。

也就是说：**删除被接通之前，这个洞必须先补**，否则 M2 的第一个动作就是推翻 M1 建立的
「绝不丢数据」信誉。spec §12.3 的排序原则写得很清楚——先建立信誉，再兑现体验。

---

## 交付了什么

| 文件 | 内容 |
| --- | --- |
| `atomic::write_local` | 工作区写入的 fsync 纪律（关掉 M1d 的隐患） |
| `journal.rs` | hub 侧 journal 的读写：append-only、epoch 指针、`AppendBatch` 批量提交 |
| `trash.rs` | `.arca/trash/` 的写入、枚举、恢复；保留期判断 |
| `gates.rs` | **删除传播的四道闸门** |
| `arca restore` | 保留期内一条命令找回 |
| `FORMAT.md` §7.3 | trash 记录格式（原文是「M2 定义」的占位，按 I10 补上） |

**tombstone 不是删除**：它是一条 `op="tombstone"` 的 journal 事件 + 内容被 **rename 移进**
`.arca/trash/`（绝不 copy+unlink——那有丢数据的窗口）。物理销毁只经显式 `arca gc`，
属后续切片。

### 四道闸门

决策表说「可以删」，闸门问的是「**现在真的安全吗**」。这两件事必须分开——
决策发生在调和时刻，执行发生在之后，中间有窗口。

| 闸门 | 检查什么 | 为什么 |
| --- | --- | --- |
| 1 read_roots 范围 | 路径在本次调和**实际扫描过**的范围内 | 拿一份不完整的观察去销毁数据 |
| 2 单点确认 | 远端**明确给出**了 tombstone，不是「查不到记录」 | `remote_vanished_without_tombstone` 已在决策层挡过一次，这是第二道 |
| 3 基线一致性 | **重新读一次实际字节**再比哈希 | 调和与执行之间文件可能被改了——这道闸门存在的全部理由就是那个窗口 |
| 4 保留期存在 | 回收站里的内容**三方哈希一致**且未过保留期 | 本地副本移除后，权威副本必须仍可取回，否则这是销毁不是删除 |

任一不过 → **不删**，报告，退出码 1（I5）。`GateFailure` 逐条可区分——
运维要知道是第几道拦下的。

---

## `arca-core` 全程一行未改

M1b 写决策表时，`DeleteLocal` / `TombstoneRemote` / `both_deleted` /
`tombstone_for_unknown_item` 四个格子就已经写好并被 5 万条 proptest 覆盖了，
只是当时 `read_remote` 只看 index+items，所以 `RemoteState::Tombstoned` 不可达。

M2a 只是把它接通。**决策逻辑一个字没动。**

这是当初坚持「core 是 sans-io 的纯决策、执行在别处」换来的直接收益。
如果当初把判断混进 CLI，现在接通删除就得重新推一遍全部十八格。

---

## 评审抓到了什么

切片评审**实机攻击了二进制**——建两个设备共享一个 `file://` hub，逐条尝试击穿闸门。
四道里 1/2/3 无法绕过，第 4 道被攻破两次。

### C1：`arca restore` 覆盖 hub 当前内容且不留副本

触发场景正是 spec §4.1 明文预期的「删除后同名重建」：

1. `photo.png = OLD` → 删除 → tombstone，OLD 进 trash（item `757a…`）
2. `photo.png = NEW` 重建 → sync → 新身份 item `979b…`
3. `arca restore photo.png` → exit 0，**NEW 的字节从 hub 上整个消失，且没进 trash**

第二跳更糟：因为本地相对基线未改动，下次 `sync` 判定 `download` 而**不是 `Conflict`**，
把本地那份 NEW 也覆盖成 OLD。两条正常命令、两个 exit 0，一张从未被删过的照片没了。

修法：`restore` 写回前检查该路径当前的 index 记录，若属于另一个 item 或内容不同，
**先把现有内容 `move_to_trash` 再写**。恢复不该比删除拥有更大的销毁权。

### C2：第 4 道闸门查的是 inode 存在，不是内容可取回

`data_exists` 是 `symlink_metadata().is_ok()`——**0 字节文件、悬空符号链接、目录都能通过**。
实机复现：把 trash 的 `.data` 截成 0 字节 → 闸门放行 → 本地副本被移除 → 双端皆无，exit 0。
换成悬空符号链接同理，而且紧随其后的 `arca restore` 直接报 `No such file or directory`——
系统先宣布「可取回」，再证明自己撒了谎。

触发不需要恶意：hub 常放在外置盘/网盘同步目录/备份还原出来的副本上（I1 的整个卖点
就是 hub 可被普通工具处理），ENOSPC 下的部分拷贝、rsync 出来的悬空链接、位腐都会造出这个状态。

**根因在格式层**：`.meta` 只有 `{v, path, item_id, deleted_at}`，**没有 `hash`/`size`**，
所以闸门、`restore`、未来的 `gc`/`fsck` 都**没有任何手段**判断回收站里那份字节是不是原来那份。

修法按 I10「格式先于代码、只向前迁移」：`FORMAT.md` §7.3 加 `hash` 与 `size`，
闸门改成**三方哈希核验**（基线期望值 = `.meta` 记录值 = 现场重算值）。
这同时堵死了另一个表现——同一 item 有多条历史 trash 记录时，
一条**陈旧**记录足以为一条**缺失**记录背书。

### 评审给的两条方法论修正

**销毁面不等于 `remove_file` 面。** 原本的审计口径是「生产代码只有一处 `remove_file`，
被四道闸门包着」——前提是真的，但 `atomic::write` 落到一个**已有内容**的 `files/` 路径上
同样是销毁，而那条路径一道闸门都没有。C1 正是从那里出来的。

**删除的写入顺序对偶不是「最后发布指针」，而是「最先撤回指针」。**
上传时先写字节后写指针，失败留下无人指向的孤儿字节（无害）；
删除时先搬字节后写指针，失败留下**指向空洞的悬空指针**——实测会让整个 hub
对所有设备停摆，并把一次半途而废的 tombstone 误诊成「存储根损坏」。

### 四条 Important

- **未完成 tombstone 被误诊为存储根损坏** → 新增 `HubError::PendingTombstone`，
  可诊断、可自愈（`arca restore` 能解卡）
- **tombstone 状态只活在 journal 里**，index 记录从不清理 → 于是 **journal 在 arca 里
  当时是不可截断的**：清空 journal 会让每个已删除路径退化成 hub 级 `MissingContent`，
  全设备停摆。改为 `execute_tombstone` 同时清理 index 记录，让「没有 index 记录」
  本身成为证据（M2b 的 epoch 轮转与 `gc` 的历史压缩都要靠这个解锁）
- **第 4 道闸门 O(n·m)**：每删一条就全量扫一遍回收站，`journal::append` 每条事件
  重读+重写整段 journal。实测 300 次 append 3.06s → 批量化后 15.8ms（约 193 倍）
- **保留期从没被判断过**：`deleted_at` 记下来却没人读，而闸门名与计划都写着
  「且未过保留期」。补上 180 天默认判断，并把 `restore --list` 的文案与实现对齐

---

## 验证证据

512 个测试全绿 · clippy `-D warnings` 零告警 ·
`cargo +1.85 check --workspace --locked --all-targets` 通过 · `cargo fmt --check` 干净 ·
`arca-core` 未改动一行。

闸门 4 的三条实机复现测试各自构造精确状态并断言拦住：0 字节截断、悬空符号链接、
陈旧记录为缺失记录背书。修复前后各跑一次，确认修复前确实会放行。

端到端：两个工作区共享一个 `file://` 存储根，走完
`init → register → adopt → 本地删除 → sync（提交 tombstone）→ 另一端 sync（闸门放行、
移除本地副本）→ restore 找回 → 两端重新拿到内容 → trash 记录全程未被清理`。

> **一处记录在案的自我更正**：我用 shell 重跑 C2 攻击时一度得到「本地副本被删」，
> 以为没修好。实际是那个两设备脚本接线错了——设备二的搭建方式粗糙，很可能从未
> 拿到过文件，所以「被删」不是证据。真正的证据是三条构造精确状态的单元测试。
> 记下来是因为**攻击脚本本身也会说谎**，这是验证工作里容易犯的错。

---

## 留给后续的

**给 M2b 的一条建议**（评审提出）：把第 4 道闸门的接口抽成一个
「这个 item 的内容此刻是否可取回（附哈希）」的 trait。HTTP CAS 下它要变成远端查询，
`DeleteCheck` 拿 `&StorageRoot` 的签名会挡路；而 O(n·m) 在网络往返下会从「慢」变成
「不可用」。现在抽比那时再动便宜得多。

**给 `arca gc` 的**：闸门通过与 gc 物理销毁之间存在 TOCTOU 窗口——
闸门确认「内容在」，gc 随即删掉，`fs::remove_file` 才执行，正是 C2 的场景。
gc 上线时两者之间需要明确的互斥/租约约定。

**已知的弱项**：一条损坏的 `.meta` 会让整个数据集的删除与 `restore --list` 失效
（方向对——回收站是删除安全性的权威依据，读错一条比停下更危险——但没有修复通路，
`arca doctor` 至少应能点名是哪个 `trash_id` 坏了）。

---

## M2 的其余切片

| 切片 | 内容 | 状态 |
| --- | --- | --- |
| **M2a** | tombstone 与删除安全地基 | ✅ |
| M2b | arcad 本体 + HTTP API + CAS（If-Match / 412） | 待做 |
| M2c | journal + longpoll 变更流 + `epoch:seq` 游标 + `sid` 进协议头闭环 | 待做 |
| M2d | 多卷映射 + server/client 角色 + 多 hub 独立故障域 + 拔盘演练 | 待做 |
| M2e | `https://` 传输的手动 pull/push/status + `arca bugreport` | 待做 |
