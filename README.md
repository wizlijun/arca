# arca

**git 管可变的文本，arca 管不可变的二进制——相对路径原样工作。**

官网：**[gitarca.com](https://gitarca.com)**

arca 是 git 仓库的二进制附件层：自托管、按相对路径原样可用。
笔记库（Obsidian 等）与团队仓库里的图片、视频、音频、电子书、设计稿、数据集：
文本继续由 git 管理，二进制交给 arca，但仍待在它们该在的相对路径上。

> **承诺**：arca 里没有任何一条代码路径能在你不知情时销毁数据。
> （删除 = tombstone；物理销毁只经显式 `arca gc`。这句话被测试守护。）

> **这句承诺管不到 git 自己的销毁路径**：`git clean -xdf` 会删掉尚未推送到 hub 的
> 受管文件，且是真删——不进回收站、不留 tombstone、找不回来。原因是 `-x` 清理的判据是
> "被 `.gitignore` 忽略"，而受管二进制正因为 arca 的反选块**生效**才被忽略；把它从
> `git clean` 的清理范围里摘出来，就得破坏反选块本身的语义，后果是整个数据集被误提交
> 进 git（仓库爆炸），比丢一个文件更糟。已经推送到 hub 的文件可以重新拉回；尚未推送的
> 没有第二份副本。缓解措施：`arca doctor` 会检出"本地存在但 hub 尚无副本"的文件并
> 显著告警——在跑 `git clean` 之类的清场命令前，养成先看一眼它的输出的习惯。

## 状态

**M0（格式与核心）已完成**，M1 尚未开始。设计规格见
[docs/2026-08-03-arca-spec.md](docs/2026-08-03-arca-spec.md)（v1.2 定稿）。

已实现：`FORMAT.md` 字节级格式契约定稿；`arca-format`（路径规则、身份/版本模型、行式清单、
`.gitarca` 与 `dataset.toml`、hub 侧 JSON Lines 记录、trace 事件 schema）；`arca-chunk`
（BLAKE3、FastCDC、zstd）；`arca-store` 的 fsck 存储根巡检；`arca fsck` 命令。
配套 168 个测试、六种格式的 golden vectors、11 个 fuzz target，以及每晚在 CI 里跑的
**逃生舱恢复演示**——用不含任何 arca 代码的 shell 脚本从存储根取回数据并逐个校验哈希，
把「删掉 arca 数据照样可用」（I1）变成持续验证的承诺而不是一句话。

其余 crate 仍是骨架（只有 doc comment 与 `TODO(Mn)` 标记）。

## 仓库布局

```
arca/
├── FORMAT.md                 ← 磁盘格式规范（I10，与代码同仓同评审）
├── PROTOCOL.md               ← 线上协议规范
├── docs/                     ← 设计规格与 ADR
└── crates/
    ├── arca-format           ← 格式的纯数据结构 + 解析/序列化 + golden vectors
    ├── arca-core             ← 对账/提交状态机（sans-io，两端共用）
    ├── arca-chunk            ← FastCDC + BLAKE3 + zstd
    ├── arca-store            ← hub 存储根 IO：布局读写 · 原子提交 · fsck 巡检
    ├── arca-git              ← git 集成：注册表 · .gitignore 反选块 · 清单 · 钩子
    ├── arca-publish          ← 发布映射：publish-map · referenced-only · 静态导出
    ├── arcad                 ← 服务端 daemon（全系统唯一常驻进程）
    ├── arca-agentd           ← 可选客户端 daemon（自动同步 · 占位符投影）
    ├── arca-catalog          ← 目录卡工具（独立可执行，核心不依赖）
    ├── arca-cli              ← arca / git arca 命令行
    ├── arca-winfs            ← Windows CfAPI 适配
    ├── arca-macfs            ← macOS File Provider（Swift 工程，不在 cargo workspace）
    ├── arca-mcp              ← MCP server（agent 接口）
    └── arca-conformance      ← 一致性测试套件
```

## 里程碑

| | 交付 |
| --- | --- |
| **M0 ✅** | 格式与核心：FORMAT.md v1 · arca-format · arca-chunk · arca-store fsck · trace schema · fuzz 与 CI |
| M1 | 单机纳管 + file:// 同步：CLI 基线闭环（无任何 daemon）。**M1a 存储根 IO 地基 ✅**，M1b/M1c/M1d 待做 |
| M2 | arcad 与手动同步：CAS · tombstone · journal · 多 hub 独立故障域 |
| M3 | agentd + Windows 占位符（★ 核心演示） |
| M4 | macOS 占位符 |
| M5 | 生态与迁移：catalog · Git LFS 桥 · 发布映射 · Obsidian 插件 |
| M6 | agent 接口：arca-mcp · agent 令牌 · checkpoint |

验收标准见 spec §12.3。已完成阶段的总结（交付内容、偏离原规格的决定及理由、
评审抓到的问题、留给后续的）归档在 [docs/milestones/](docs/milestones/)。

## 许可证

分许可证：客户端与库最大化嵌入，服务端保留商业留白。

| 组件 | 许可证 |
| --- | --- |
| 客户端、CLI、库、占位符层、conformance（除 `crates/arcad/` 外的全部 crate） | **MIT**，见 [LICENSE](LICENSE) |
| 服务端 daemon `crates/arcad/` | **AGPL-3.0-only**，见 [LICENSE-AGPL-3.0](LICENSE-AGPL-3.0) |

MIT 的库可被 AGPL 的 arcad 使用；反向不成立——**任何代码进 arcad 即受 AGPL 约束**，
因此两端共用的逻辑必须落在 arca-core / arca-format 等 MIT crate 里（这与 §3.1 的分层约束同向）。
理由见 spec §12.2。

贡献采用 DCO。
