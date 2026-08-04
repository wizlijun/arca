# arca

**git 管可变的文本，arca 管不可变的二进制——相对路径原样工作。**

官网：**[gitarca.com](https://gitarca.com)**

arca 是 git 仓库的二进制附件层：自托管、按相对路径原样可用。
笔记库（Obsidian 等）与团队仓库里的图片、视频、音频、电子书、设计稿、数据集：
文本继续由 git 管理，二进制交给 arca，但仍待在它们该在的相对路径上。

> **承诺**：arca 里没有任何一条代码路径能在你不知情时销毁数据。
> （删除 = tombstone；物理销毁只经显式 `arca gc`。这句话被测试守护。）

## 状态

**早期骨架阶段**——项目结构已就位，实现尚未开始。
设计规格见 [docs/2026-08-03-arca-spec.md](docs/2026-08-03-arca-spec.md)（v1.1 定稿）。

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
| M0 | 格式与核心：FORMAT.md v1 · arca-format · arca-chunk · fsck |
| M1 | 单机纳管 + file:// 同步：CLI 基线闭环（无任何 daemon） |
| M2 | arcad 与手动同步：CAS · tombstone · journal · 多 hub 独立故障域 |
| M3 | agentd + Windows 占位符（★ 核心演示） |
| M4 | macOS 占位符 |
| M5 | 生态与迁移：catalog · Git LFS 桥 · 发布映射 · Obsidian 插件 |
| M6 | agent 接口：arca-mcp · agent 令牌 · checkpoint |

验收标准见 spec §12.3。

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
