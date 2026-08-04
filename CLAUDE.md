# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目状态

**骨架阶段**：12 个 crate 的目录与模块划分已就位，但**没有任何实现代码**——
每个模块只有 doc comment（说明职责、对应 spec 章节、所属里程碑），以 `TODO(Mn)` 标记待实现处。
外部依赖一律未引入，各 `Cargo.toml` 以注释记录实现时的计划依赖（blake3 / axum / rusqlite 等）。

## 常用命令

```bash
cargo check --workspace              # 骨架阶段的主要验证手段（零依赖，秒级）
cargo build --workspace
cargo test --workspace
cargo test -p arca-core <测试名>     # 跑单个测试
cargo clippy --workspace --all-targets
cargo fmt                            # 需先 rustup component add rustfmt（当前未安装）
```

MSRV 1.85，edition 2021。`arca-macfs` 是 Swift 工程，不在 cargo workspace 内，
不参与上述命令。

## 唯一真相源：spec

`docs/2026-08-03-arca-spec.md`（v1.1 定稿，中文）是本项目的设计规格。
**每个模块的 doc comment 都标注了它对应的 spec 章节号**——动任何模块前先读该章节。
本仓库的文档与注释以中文书写，保持一致。

`FORMAT.md`（磁盘格式）与 `PROTOCOL.md`（线上协议）是从 spec 提炼出的规范骨架，
受不变量 I10 约束：**格式先于代码**——改磁盘格式或线上协议时，先改规范文档再改实现，
只向前迁移，永不静默改格式。

算法实现参考前身项目 **lazync**（`/Users/bruce/git/lazync`，Free Pascal）：
模块注释里点名了对应的 Pascal 单元，例如 `arca-core/src/reconcile.rs` ← `client/src/nc_sync_engine.pas`、
`arcad/src/journal_store.rs` ← `server/src/nc_change_journal.pas`。

## 架构约束（最容易被无意破坏的几条）

**arca-core 是 sans-io 的、两端共用的**。它是纯状态机：无 IO、无 tokio、无运行时依赖。
客户端与 hub 必须对路径规则、哈希、过滤器、调和决策跑**同一段代码**。
把 IO 或异步运行时加进 core 会摧毁这个设计——IO 属于 arcad / arca-agentd / arca-cli。

**分层降级关系，上层永远是下层的增强，不是依赖**（spec §3.1）：

```
手动 CLI（基线，必须完整可用，无需任何 daemon）
  └─ arca-agentd（可选：自动同步）
       └─ 占位符层 arca-winfs / arca-macfs（可选：按需水化）
```

agentd 崩了，手动命令必须照常工作；占位符注册失败，必须退回全量物化。
Linux / CI 只用手动模式，是一等用户而非降级路径。

**服务端 arcad 是全系统唯一的常驻进程**（形态参考 git：客户端零常驻，一次性进程）。

**里程碑决定实现顺序**（spec §12.3）：M0 格式与核心 → M1 单机纳管 + `file://` 同步 →
M2 arcad 与手动同步 → M3 agentd + Windows 占位符 → M4 macOS → M5 生态 → M6 agent 接口。
排序原则是**先建立"绝不丢数据"的信誉，再兑现体验承诺**。`TODO(Mn)` 标记与此对应。

## 不变量（协议级承诺，任何实现不得违反）

spec §2 定义了 I1–I11，全文以编号引用。实现时最常触及的几条：

- **I1 逃生舱**：hub 的 `files/` 永远是普通文件树，当前版本完整平放，无翻译层。
  CDC 分块只出现在 `.arca/chunks/`，只服务历史版本与增量传输。
- **I3 同步路径无销毁权**：删除 = tombstone；物理销毁只经显式 `arca gc`，绝不自动触发。
  这是可执行断言，不是文档承诺——收敛性测试必须断言"无任何路径销毁数据"。
- **I4 一切写入走 CAS**：提交必须携带父版本哈希（If-Match），过期即拒绝，绝不静默覆盖。
- **I5 绝不猜测**：状态模糊 → 停下并可诊断，而不是尽力恢复。
- **I6 不污染用户目录**：受管文件**原地不动**——不改名、不移动、不换成指针或符号链接。
- **I9 客户端可重建**：OS 占位符层与本地 SQLite 投影都是可抛弃投影，随时可从 hub 重建；
  真相在 hub 的 journal 与库。「删掉重建」是一等公民操作，不是灾难恢复脚注。
- **I11 挂载缺失即离线**：存储根未挂载或卷身份不符 → 数据集离线并明确报错，
  绝不呈现为"空库"，绝不因此触发删除对账。

## 已知的高危处

`crates/arca-git/src/ignore_block.rs` 的 `.gitignore` 反选块是**全设计最易出错处**
（spec §4.3、§6.3 第 9 条）。写错一个字符，要么 `.arca/` 元数据没进 git（协作者拿不到清单），
要么整个数据集被误提交进 git（仓库爆炸）。要求：生成器只此一处 + golden 样例；
`arca doctor` 断言的是 `git check-ignore` 的**实际结果**而非文本。

`crates/arca-agentd/src/hydration.rs` 的分级驻留策略挡的是 hydration 风暴
（全库索引/备份/杀毒扫描触发全量水化）——"全库索引水化字节数 = 0"是必过测试。

## 正确性基础设施（spec §11.2，要求第一天就建）

确定性模拟测试（模拟时钟/网络/文件系统 + 崩溃注入 + 种子可复现）、收敛性属性测试（proptest）、
格式 fuzz（损坏输入 → 明确错误，绝不 panic）、golden vectors、§6.3 噩梦路径集成测试。
逃生舱恢复演示（纯 shell + coreutils，不含任何 arca 代码）进 CI 每晚验证。

核心 crate 均已 `#![forbid(unsafe_code)]`；`arca-winfs` 实现 CfAPI FFI 时是唯一的 unsafe 边界。

## 约定

- 只在 `main` 分支工作，不开特性分支。
- 许可证分层（spec §12.2）：**除 `crates/arcad/` 外全部 MIT**，服务端 `arcad` 为 **AGPL-3.0-only**。
  依赖方向单向：MIT 库可进 arcad，反之不行——**两端共用的逻辑必须留在 arca-core / arca-format**，
  否则会被 AGPL 污染而无法在客户端复用。新建 crate 时 `license` 字段照此填。
- 官网 gitarca.com。
- CLI 遵循 git 的 plumbing / porcelain 分层与 Rule of Silence：成功时安静，
  数据走 stdout、诊断走 stderr，处处可加 `--json`；与 git 同名的动词语义必须一致。
