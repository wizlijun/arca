# arca 使用指南

**文本留在 git，二进制在 arca，都在原来的相对路径上。**

你的 `![图](assets/鸭川.png)` 一直是这么写的，git 里只有笔记与一份清单，
图片本体在你自己的硬盘或 NAS 上。**受管文件原地不动**——不改名、不移动、
不换成指针或符号链接（这是 arca 与 Git LFS / git-annex 的根本分野）。

---

## 目录

按**主次**排列。1–3 是核心，跑完就已经能用；4–6 是多机与安全；
7–9 是自动化与生态；10 是排障。

| # | 能力 | 命令 |
| --- | --- | --- |
| 1 | [纳管一个已有笔记库](#1-纳管一个已有笔记库核心) | `init` / `register` / `adopt` |
| 2 | [日常同步](#2-日常同步核心) | `sync` / `status` |
| 3 | [换一台机器](#3-换一台机器核心) | `git clone` + `setup` |
| 4 | [删除与找回](#4-删除与找回安全) | `restore` / `gc` |
| 5 | [网络 hub](#5-网络-hub把硬盘变成服务) | `arcad` |
| 6 | [这台机器要不要永久保留副本](#6-副本角色) | `role` |
| 7 | [自动同步](#7-自动同步) | `arca-agentd` |
| 8 | [发布到网站](#8-发布到网站) | `publish-map` |
| 9 | [从 Git LFS 迁入](#9-从-git-lfs-迁入) | `import lfs` |
| 10 | [排障](#10-排障) | `doctor` / `verify` / `fsck` / `bugreport` |

**先构建**（暂无安装包，见文末「现状」）：

```bash
cargo build --release -p arca-cli -p arcad -p arca-agentd
export PATH="$PWD/target/release:$PATH"
```

---

## 1. 纳管一个已有笔记库（核心）

假设你有一个 Obsidian 笔记库，附件在 `assets/`：

```bash
mkdir -p 笔记库/assets && cd 笔记库
printf '# 京都\n\n![鸭川](assets/duck.png)\n' > note.md
printf 'FAKE-PNG-BYTES' > assets/duck.png
printf 'FAKE-VIDEO' > assets/video.mp4
git init -q . && git config user.email you@example.com && git config user.name you
```

### `arca init` —— 建立 vault

```bash
arca init .
```

安静（成功时无输出，退出码 0）。它做两件事：建 `.gitarca`、装 pre-push 钩子。

### `arca register` —— 把一个目录登记为数据集

```bash
arca register assets --hub home --hub-url "file:///Volumes/外置盘/arca-store"
```

```
assets dataset_id=c6089ab0f8014d0eaa4c991a694e2aa2 hub=home hub_instance_id=da4947...
```

它写了两个文件。`.gitarca`（进 git，协作者据此知道去哪儿取内容）：

```toml
schema = 1

[hub.home]
instance_id = "da4947324800b0ef919637c657835c1a"
url = "file:///Volumes/外置盘/arca-store"

[[dataset]]
path = "assets"
hub = "home"
```

`.gitignore` 里的**反选块**——这是全设计最要紧的几行，让 git 忽略二进制
但**不**忽略清单：

```gitignore
# >>> arca managed (do not edit inside) >>>
/assets/*
!/assets/.arca/
/assets/.arca/client/
# <<< arca managed <<<
```

### `arca adopt` —— 就地纳管

```bash
arca adopt assets
```

```
upload	duck.png
upload	video.mp4
注意：adopt 只阻止未来的提交继续膨胀。已经 commit 过的二进制仍留在 git 历史里……
```

现在：

- 硬盘上出现了 `files/duck.png`、`files/video.mp4`——**平铺的普通文件树**，
  不是什么私有格式（这是「逃生舱」承诺：没有 arca 你也能用 `cp` 拿回数据）
- 你的 `assets/` 里文件**一个都没动**

提交一下，看看 git 里到底有什么：

```bash
git add -A && git commit -q -m "纳管附件"
git ls-files
```

```
.gitarca
.gitignore
assets/.arca/dataset.toml
assets/.arca/manifest
note.md
```

**二进制不在里面，清单在。** 这就是全部的魔法。

---

## 2. 日常同步（核心）

### `arca status` —— 看看有没有待办

全同步时**安静**、退出码 0（Rule of Silence，学 git）：

```bash
arca status assets      # 无输出
```

改一个文件再看：

```bash
printf 'EDITED-PNG' > assets/duck.png
arca status assets
```

```
待上传：duck.png
```

退出码 **1**（有待办）。退出码语义：`0` 全同步 · `1` 有待办 · `2` 数据集离线。

> 你现在还会看到一条「已知的 server 副本数为 1，低于阈值 2」的提示。
> 它不影响退出码，见 [§6](#6-副本角色)。

### `arca sync` —— 跑一轮调和

```bash
arca sync assets
```

```
upload	duck.png
```

`sync` 是**双向**的：本地新增/修改 → 上传；远端新增 → 下载；
两边都改了 → 报冲突并**两份内容都不动**，把决定权交回给你。

---

## 3. 换一台机器（核心）

这是 arca 的核心体验。第二台机器上：

```bash
git clone <你的仓库> 第二台 && cd 第二台
ls assets/        # 空的——二进制不在 git 里
```

### `arca setup` —— 克隆之后的第一条命令

```bash
arca setup
```

```
已安装 pre-push 钩子（`git clone` 不会带上 .git/hooks/，所以每台新设备都要装一次）。
基线已重建（此前缺失或损坏）——本轮是一次全量对账
download	duck.png
download	video.mp4
引导完成：1 个数据集的内容都已就位。
```

`assets/` 里的文件回来了，Obsidian 打开笔记图片正常渲染。

> **为什么必须跑 `setup` 而不只是 `sync`**：`git clone` **不会**带上
> `.git/hooks/`。没有 pre-push 钩子，你可以推送一个「二进制还没上传完」的
> 提交，协作者拉下来就是一堆悬空引用。`setup` 把这个静默缺口补上。

---

## 4. 删除与找回（安全）

**arca 里没有任何一条代码路径会在你不知情时销毁数据。** 删除只是 tombstone。

```bash
rm assets/video.mp4
arca sync assets
```

```
tombstone	video.mp4
```

hub 的 `files/` 里它没了，但内容**移进了回收站**而不是被销毁：

```bash
ls /Volumes/外置盘/arca-store/.arca/trash/
# 0f469fb1....data  0f469fb1....meta
```

### `arca restore` —— 一条命令找回

```bash
arca restore assets video.mp4
```

```
restore	video.mp4	20260810T053001Z-c6f47a09aa0fa1bd825fd5758276a523
```

### `arca gc` —— 唯一一条会真的删字节的命令

**默认是 dry-run**：只出清单，什么都不销毁。

```bash
arca gc assets
```

```
hub 回收站（.arca/trash/） 里的 1 条记录全部仍在保留期内（180 天），没有可清理的条目——本次什么都没做。
另有 1 条仍在保留期内（180 天），**即使加 `--yes` 也不会被销毁**。
```

要真的销毁得显式 `--yes`，而且**保留期（默认 180 天）内的一律不动**。
它也**绝不会被任何东西自动触发**——写进 cron 是你自己的决定。

---

## 5. 网络 hub：把硬盘变成服务

前面用的是 `file://`（本机路径或挂载的外置盘）。要让别的机器通过网络访问，
在放着存储根的那台机器上跑 `arcad`。

`hub.toml`：

```toml
instance_id = "cccccccccccccccccccccccccccccccc"

[[dataset]]
id = "c6089ab0f8014d0eaa4c991a694e2aa2"    # 就是 register 那次输出的 dataset_id
path = "/Volumes/外置盘/arca-store"
```

```bash
arcad --config hub.toml --bind 0.0.0.0:18800
```

另一台机器改用 `http://` 登记，之后**一切命令用法完全一样**：

```bash
arca register assets --hub home \
  --hub-url "http://nas.local:18800" \
  --dataset-id c6089ab0f8014d0eaa4c991a694e2aa2
arca sync assets
```

```
download	duck.png
download	video.mp4
```

### TLS

`https://` 走系统信任库。自签名证书**必须先 pin 指纹**——arca **绝不 TOFU**：

```bash
arca hub trust home                       # 只打印指纹，不写入；请带外核对
arca hub trust home --fingerprint sha256:…   # 核对无误后写进 .gitarca
```

指纹变更即拒连并报出两个值。

> ⚠️ **arcad 目前没有认证。** 一旦端口可达，任何人都能读写你的全部数据。
> 现在只适合完全可信的局域网，**不要暴露到公网**。见文末「现状」。

---

## 6. 副本角色

一句话：**这台机器是「永久保留一份完整副本」，还是「本地只是可再生缓存」？**

```bash
arca role assets            # client（默认）
arca role assets --set server
```

```
assets 已设为 server 角色：本设备承诺为这个数据集永久保留一份完整副本——
远端删除到达、过闸门之后，本地副本只会移入本地回收站（.arca/client/trash/），不释放空间。
```

默认是 `client`：远端删除传播过来时，本地副本会被移除。
把你那台「当备份用」的机器设成 `server`，它就永远不会因为云侧语义而缩水。

角色是**设备本地决策**，存在 `<数据集>/.arca/client/role.toml`，**不进 git**。

---

## 7. 自动同步

前面都是手动命令——**手动模式是基线，永远完整可用，不需要任何 daemon**。
`arca-agentd` 是可选的增强。

### 跑一轮就退出（脚本 / cron 用）

```bash
arca-agentd --once
```

```
arca-agentd 已启动：1 个数据集，间隔 30 秒，单实例锁 …/.arca/agentd.lock（--once：跑一轮即退出）
assets：上传 1 · 下载 1 · 改名 0 · 本地删除 0 · 冲突 0
```

### 长驻

```bash
arca-agentd --interval 300
```

它同时盯三路：**本地文件变动**（实时事件）、**远端变更**（longpoll）、
**周期兜底**。实测：间隔设成 300 秒，本地改一个文件后 **0.7 秒**就同步完成——
靠的是 watcher 唤醒，不是等到点。

`arca status` 能看见它：

```
agentd：运行中（pid 41556，心跳 2026-08-10T05:34:44Z）
  assets：实时监听，上次成功 2026-08-10T05:34:29Z
```

（心跳每 15 秒写一次，所以 agentd 刚起来的头十几秒里只有第一行，
数据集明细要等第一次心跳落盘。）

> **agentd 崩了，手动命令照常工作。** 这条有自动化演练守着
> （`kill -9` 之后 `sync`/`status`/`verify` 全部正常）。

---

## 8. 发布到网站

把笔记发布成网页时，`assets/duck.png` 这样的相对路径需要变成公网 URL。
arca **只产出映射，绝不改写你的 md**。

先在 `<数据集>/.arca/dataset.toml` 里配好：

```toml
public_base_url = "https://cdn.example.com/assets"
# url_style = "hash"   # 可选：不可变 URL，适合 CDN 永久缓存
```

```bash
arca publish-map
```

```json
{
  "schema": 1,
  "datasets": {
    "assets": { "prefix": "assets/", "base_url": "https://cdn.example.com/assets", "style": "path" }
  },
  "items": {
    "assets/duck.png": { "hash": "blake3:d30b8b3d…", "size": 10 }
  }
}
```

```
已按 --referenced-only（默认）产出映射：1 个数据集、1 个被引用的资源。
未被任何 md 引用的文件**不在其中**——要全量公开请显式 `--all`。
```

两个要点：

- **默认只发布被 md 引用到的资源。** 直接公开整个数据集会暴露没被任何笔记
  引用的文件——那是隐私事故的常见来源，且挂上 CDN 后不可撤回。
- **生成映射时一个 blob 都不读。** 映射完全由清单构造，所以 **CI 可以在不下载
  任何二进制的前提下构建出图片可访问的静态站**。100 GB 的图库，一个字节都不用拉。

---

## 9. 从 Git LFS 迁入

```bash
arca import lfs          # 默认 dry-run
```

```
lfs-ready	assets/好的.png	10
lfs-skipped	assets/没拉过.png
assets/没拉过.png：`.git/lfs/objects/` 下没有 8af1d328… 这个对象——多半是这个克隆还没跑过 `git lfs pull`。

.gitattributes:1：*.png filter=lfs diff=lfs merge=lfs -text
**注意**：以上 1 条 `.gitattributes` 规则仍然把文件交给 LFS filter。不处理它们的话，
下一次 `git add` 会让 git 把这些文件重新变回指针——迁入会被静默撤销。
```

```bash
arca import lfs --yes
```

它把指针换回真实内容，并把 `.gitattributes` 里的 LFS 规则**注释掉**
（不是删掉——那是你的配置）：

```gitignore
# 已由 arca import lfs 注释：这条规则会让 git 在下次 add 时把文件变回 LFS 指针
# *.png filter=lfs diff=lfs merge=lfs -text
```

**校验通过之前一个字节都不写**：LFS 的 `oid` 就是内容的 SHA-256，
对不上就跳过并说明，**指针原封不动**。不需要装 `git-lfs`。

迁完之后 `arca adopt <数据集>` 接管。

---

## 10. 排障

```bash
arca doctor           # 一致性巡检：.gitarca、反选块实测结果、清单漂移、回收站
arca verify assets    # fixity：逐文件重算 BLAKE3 与版本链比对
arca fsck /Volumes/外置盘/arca-store    # 直接巡检存储根（只读，绝不修改）
```

三者干净时都安静、退出码 0。

### `arca checkout` —— `git checkout` 到旧提交之后

清单在 git 里、会跟着切换，而受管二进制不在 git 里、不会跟着变。
`arca status` 会报出这道缝，`arca checkout` 弥合它（默认 dry-run）。

> ⚠️ 目前**还原不了历史版本**——见文末「现状」。

### `arca bugreport` —— 一条命令收齐现场

```bash
arca bugreport
```

输出版本、平台、各数据集的角色与健康度、trace 落盘列表、
`.gitignore` 反选块的**实测**结果、hub 可达性。

**绝不读取任何受管文件的内容**（有测试守着：往文件里塞魔法串，断言输出里搜不到）。
只打到 stdout，不落盘、不上传——你按回车之前就能把收了什么看个遍。

### 给脚本 / agent 用的 plumbing

```bash
arca ls assets                  # hub 侧当前清单（JSON）
arca resolve assets duck.png    # 路径 → 身份/版本
arca cat assets blake3:d30b8b3d…   # 按哈希取字节，原样写 stdout
arca state dump assets          # 本地基线投影
```

---

## 现状：还不能做什么

诚实清单，免得你踩空：

| 事项 | 状态 |
| --- | --- |
| **arcad 没有认证** | 端口可达 = 任何人可读写。只适合可信局域网 |
| **历史版本不可恢复** | 版本链只记元数据，旧版本的**字节从未被保留**；`arca checkout` 因此还原不了旧版本 |
| **大库首次 `adopt` 慢** | 一万文件约 234 秒。瓶颈是 macOS 上每文件多次 `F_FULLFSYNC`，已定位，修法待定 |
| **占位符层没有** | Windows CfAPI / macOS File Provider 都还没实现——`client` 角色目前仍是全量物化，磁盘没省 |
| **数据集搬迁走不通** | 把数据集整个移到另一个仓库后 `register` 会因 `hub_instance_id` 不符而拒绝 |
| **没有安装包** | 只能 `cargo build`；没有 release 工作流 |

---

## 一句话速查

```bash
arca init .                                    # 建 vault
arca register <目录> --hub <名> --hub-url <地址>  # 登记数据集
arca adopt <目录>                               # 就地纳管（首次）
arca sync <目录>                                # 日常同步
arca status <目录>                              # 看待办（0 干净 / 1 有待办 / 2 离线）
arca setup                                     # 克隆之后的第一条命令
arca restore <目录> <文件>                       # 找回删除的文件
arca gc <目录>                                  # 清理过期回收站（默认 dry-run）
arca doctor                                    # 出问题时先跑它
arca bugreport                                 # 报障时附上它
```
