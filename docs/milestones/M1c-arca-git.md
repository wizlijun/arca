# M1c · arca-git（git 接缝）

**完成于 2026-08-05** · 6 个提交 · 结束时 286 个测试全绿（1 个 `#[ignore]`，见下）

M1 的第三块。arca 与 git **并行**工作，不做 clean/smudge filter——寄生 git 管道正是
Git LFS 的失败根源（spec §1.2）。接缝只有三样：`.gitignore` 里的一个标记块、清单、
以及一个 pre-push 钩子。

---

## 为什么这块的风险最高

CLAUDE.md 把 `.gitignore` 反选块列为**全设计最易出错处**。git 的规则是：
**父目录被排除后，其内容无法再被反选**。所以不能写 `/assets/`，必须

```gitignore
/assets/*
!/assets/.arca/
/assets/.arca/client/
```

写错一个字符，灾难有两个方向：反选没生效 → 协作者克隆后看到一个空目录和零线索；
排除没生效 → 整个数据集进 git 历史，瘦身要重写历史。

因此本切片有一条不可谈判的纪律：**断言 `git check-ignore` 的实际结果，而不是文本**。
文本比对能通过而行为是错的——那正是最危险的失败形态。

---

## 交付了什么

| 文件 | 内容 |
| --- | --- |
| `repo.rs` | 调真 git 的薄封装：`check_ignore` / `ls_files` / `git_path`，退出码三态不折叠 |
| `ignore_block.rs` | 反选块的生成、幂等更新、移除；损坏输入报错而非吞内容 |
| `tracking.rs` | vault 一致性检查，覆盖 spec §4.3.2 的全部处置表条目 |
| `hooks.rs` | pre-push 钩子的安装/卸载；拒绝覆盖用户已有的钩子 |
| `tests/ignore_block.rs` | 真建 git 仓库、真跑 `git check-ignore` / `git add` |
| `tests/nightmare.rs` | spec §6.3 第 9 条的三条噩梦路径 |

**反选块经实证核对的路径**（评审在临时仓库里逐条跑 `git check-ignore` 验证）：

| 路径 | 期望 | 实测 |
| --- | --- | --- |
| `assets/京都/鸭川.png`（受管二进制） | 被忽略 | ✅ |
| `assets/.arca/dataset.toml` | 能进 git | ✅ |
| `assets/.arca/manifest` | 能进 git | ✅ |
| `assets/.arca/client/state.db` | 被忽略 | ✅ |
| `assets/.arca`（目录自身，含带/不带尾斜杠） | 能进 git | ✅ |
| `assets/.arca/catalog/albums/x.toml`（更深一层） | 能进 git | ✅ |
| `photos/x.png`（数据集名是 `photo` 的前缀） | 按 `photos` 自己的规则 | ✅ |
| `spaced name/pic.png`（路径含空格） | 被忽略 | ✅ |

**九条额外路径全部正确——反选块的模式设计本身没有发现灾难级问题。** 这是本切片
最重要的一条结论。

---

## `git clean -xdf` 会删掉受管二进制——已确认，且决定不绕过

噩梦路径测试实测：`git clean -xdf` **真的会删掉受管二进制**，连同其父目录，
以及本地投影目录 `.arca/client/`。不进回收站、不留 tombstone、找不回来。

**决定：接受这个风险，不绕过。** 理由是 `-x` 的清理判据就是「被 `.gitignore` 忽略」，
而受管二进制正因为反选块**生效**才被忽略——同一个信号。想把它从 `git clean` 的范围里
摘出来，就得破坏反选块本身的语义，后果是整个数据集被误提交进 git，**比丢一个文件更糟**。

这与 spec §13 风险表里那一行（「git 操作误伤」）完全吻合。落地的处置：

- 那条测试**保留**并标记 `#[ignore]`，忽略消息与 doc comment 里写清了完整实测记录——
  它是可执行的证据，不是被删掉的不方便事实
- `README.md` 在「承诺」那段紧邻处加了警示：讲清 `git clean -xdf` 会删掉尚未推送的
  受管文件、已推送的可以拉回、以及为什么不能绕过
- 缓解措施是 `arca doctor` 检出「本地存在但 hub 尚无副本」的文件并显著告警——
  **这是 M1d 的义务**

另外两条噩梦路径实测通过：`git checkout` 切分支（双向）与 `git stash` / `stash pop`
都不影响受管二进制。

---

## 评审抓到了什么

反选块本身零问题，但周边有三条 Important，其中一条是**实测复现的数据丢失**：

**1. `upsert` / `remove` 在块尾标记被删时会吞掉用户内容。**
评审构造了这个场景：用户手滑删掉了结束标记。第一次 `upsert()` 找不到配对的结束标记，
判定「没有块」，在文件末尾追加一个新块——此时用户内容还在。**再跑一次**（这正是
文档承诺的幂等操作，也是 `arca setup` / `arca register` 的正常调用模式）：这次从头
找到孤立的旧起始标记，往后找到的第一个结束标记是**新追加块**的，于是把两者之间的
全部内容当成「块」整体替换——用户的行消失了。

这与 `find_block` 自己 doc comment 里「绝不吞用户内容」的承诺直接矛盾。
修法：损坏输入按 I5 报错而非猜测——`upsert` / `remove` 改返回 `Result`，
新增 `UnterminatedBlock{line}` 与 `MultipleHeaders{lines}`。

**2. `check_vault` 的静默降级。** `ls_files()` 失败 → `unwrap_or_default()` →
`AlreadyTracked` 检测完全不跑，而函数照常返回其余检查的结果。调用方拿到的信号
与「库真的干净」**没有任何区别**——正是 I5 禁止的「把没查成功折叠成查了没问题」。

实现者原本把这归因于「`Vec<Issue>` 签名固定，没法上报」。评审的反驳成立：
`Issue` 是可扩展枚举，完全可以在不改签名的前提下新增变体。修法是加
`Issue::CheckIncomplete { check, reason }`，每个降级点 push 一条。

`dataset.toml` 读取失败走同一条 `continue`，会连带跳过 `HubIdMismatch` 检查——
那是 spec §11 的防误绑安全检查，被静默跳过的后果更值得警惕。

**3. `Repo::open` 把「路径不存在」误报成「git 未安装」。** 根因是
`Command::current_dir` 的 `ENOENT` 与「PATH 里找不到 git」的 `ENOENT` 在
`ErrorKind` 层面无法区分。用户会被指向错误的修复方向。修法：spawn 前先探测路径。

另修两条 Minor：`check_vault` 的扫描顺序不确定（`render()` 那边有排序，这边缺了，
不对称）；`render` 的去重发生在裁剪 `/` 之前，于是 `["assets/", "assets"]` 会产出
重复的块。

---

## 验证证据

286 个测试通过、1 个 `#[ignore]`（记录 `git clean` 风险的证据测试）· clippy `-D warnings`
零告警 · `cargo +1.85 check --workspace --locked --all-targets` 通过 ·
`cargo fmt --check` 干净。

`git check-ignore` 的核对是在真实临时仓库里跑真实 git 命令完成的，不是文本比对。

---

## 留给 M1d 的

- **`arca doctor` 必须检出「本地存在但 hub 尚无副本」的文件并显著告警**——
  这是 `git clean -xdf` 风险的唯一缓解措施
- pre-push 钩子里调的检查命令属 M1d。M1c 生成的脚本在该命令不存在时**优雅降级**
  （打印提示并放行，不阻塞 `git push`），M1d 实现后要验证阻止路径真的工作
- `check_vault` 返回的 `Issue::CheckIncomplete` 必须被 `arca doctor` 显式呈现——
  「巡检不完整」不能被当成「干净」

---

## M1 的其余切片

| 切片 | 内容 | 状态 |
| --- | --- | --- |
| M1a | 存储根 IO 地基 | ✅ |
| M1b | 调和状态机 | ✅ |
| **M1c** | arca-git（git 接缝） | ✅ |
| M1d | CLI porcelain/plumbing + `file://` 直连同步闭环 + trace 失败落盘；跑通 spec §12.3 的 M1 验收演示 | 待做 |
