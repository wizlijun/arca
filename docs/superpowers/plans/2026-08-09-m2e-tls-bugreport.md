# M2e TLS、bugreport 与 M2 收尾 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 收掉 M2 的最后一块——把本地 trash 从「只写」变成可管理、让健康检查命令支持 `http(s)://`、加上 TLS 与 `arca bugreport`。

**Architecture:** 四件事互相独立，但都指向同一个主题：**M2 建起来的东西要能被运维**。前四个切片交付了能力，这一块让人能看见、能恢复、能报障。

**Tech Stack:** Rust 2021 / MSRV 1.85 · rustls（TLS）· 已有的 `arca-cli` / `arcad`

---

## 为什么这四件事在一起

M2d 的切片评审列了三条 carry-forward，加上 spec §12.3 点名的两项，构成本切片：

1. **本地 trash 是只写的**（评审称「最大的一条 carry-forward」）——spec §4.7 说
   server 角色的 tombstone「移入本地 trash **保留期**，物理销毁只经显式 GC」，
   但现在没有保留期概念、没有 GC、没有 `restore` 通路、没有列表、`doctor` 看不见。
   **今天恢复要手读 `.data`/`.meta`，而 server 设备的 trash 会无界增长。**
   这是留给一个存储产品跨里程碑的尴尬状态。
2. **`status` / `doctor` / `verify` 仍是 `file://` only**——而 arcad 是 M2 的主线，
   主健康检查命令对主 hub 类型不工作。
3. TLS（spec §12.3 的 `https://`）。
4. `arca bugreport`（spec §3.3、§12.3）。

## Global Constraints

- MSRV **1.85**，edition 2021。依赖要求高于 1.85 **报告而非降级钉版**。
- 各 crate 保持 `#![forbid(unsafe_code)]`；**`arca-core` 一行不改**。
- **tokio 只允许在 `arcad`**；`arca-cli` 无异步运行时（spec §3.1）。
  TLS 的客户端侧要选**不强制异步**的方案。
- 只在 `main` 分支工作。提交信息用中文。文档与注释一律中文；已有 doc comment 保留。
- 四项门禁：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo fmt --all -- --check`、`cargo +1.85 check --workspace --locked --all-targets`。
- **I3**：`arca gc` 是本项目**第一个被授权物理销毁数据的命令**。它必须
  **只能显式触发**、默认安装绝不销毁任何东西、`--dry-run` 先出清单。
  这一条是 README 第一屏那句承诺的边界，写代码前先读 spec §7。

## 已交付、可直接使用的接口

```rust
arca_cli::local_trash          // <ds>/.arca/client/trash/ 的写入（M2d，目前只写）
arca_cli::trash                // hub 侧 .arca/trash/ 的写入/列表/恢复（M2a）
arca_cli::role::{Role, read, write}
arca_cli::transport::{Transport, LocalTransport, HttpTransport}
arca_cli::doctor / gates / sync
arca_format::trace             // Sid / RingSink / 落盘（M1d 已有 trace 失败落盘）
```

---

### Task 1: 本地 trash 的保留期、列表与恢复

**Files:** `crates/arca-cli/src/local_trash.rs`、`commands/porcelain.rs`、`doctor.rs`、`FORMAT.md`

- [ ] **先写 `FORMAT.md`**：本地 trash 的 `.meta` 是否已含 `deleted_at`？
      若无，按 hub 侧 trash 的形状补齐（`hash`/`size`/`deleted_at` ——
      M2a 的评审教训：没有 `hash` 就没法验证「回收站里那份是不是原来那份」）。
- [ ] `local_trash::list()` / `restore()`：与 hub 侧 `trash` 的形状保持一致，
      **别发明第二套语义**。恢复时同样要三方哈希核验（M2a 的 C2 教训）。
- [ ] `arca restore --local`（或等价形式）：从**本地** trash 恢复。
      现有的 `arca restore` 是从 hub 侧恢复，两者的区别要在帮助文本里说清楚。
- [ ] `arca doctor` 报告本地 trash 的占用与最老条目——**让它可见**。
- [ ] 测试：写入 → 列表 → 恢复 → 内容逐字节一致；损坏的 `.meta` 能被点名。

### Task 2: `arca gc`——第一个被授权销毁的命令

**Files:** `crates/arca-cli/src/gc.rs`（新建）、`commands/porcelain.rs`

**动手前先读 spec §7 全节。** 这是 README 第一屏那句
「arca 里没有任何一条代码路径能在你不知情时销毁数据」的边界。

- [ ] `arca gc --dry-run`：**默认行为**，只出清单不销毁。
- [ ] `arca gc --yes`（或等价的显式确认）：真的销毁**超过保留期**的 tombstone 与失引用块。
- [ ] **绝不自动触发**：没有任何定时器、没有任何「顺手清理」。
      cron 里写 `arca gc` 是用户的主动决策。
- [ ] gc 与 fsck 共享引用计数校验；**发现悬空/多余引用 → 停下报告，不销毁**（I5）。
- [ ] 保留期未过的条目**一律不动**，即使用户加了 `--yes`。
      要销毁未过期的需要另一个更显式的开关，并在帮助文本里写明后果。
- [ ] 测试：`--dry-run` 不改变文件系统（断言前后哈希一致）；
      未过保留期的条目在 `--yes` 下仍然存活；过期条目被销毁且**报告列出了销毁清单**；
      悬空引用导致停下而不是继续。

### Task 3: `status` / `doctor` / `verify` 支持 `http(s)://`

**Files:** `crates/arca-cli/src/commands/porcelain.rs`、`doctor.rs`

M2d 的评审：「arcad 是 M2 的主线，而主健康检查命令对主 hub 类型不工作」。

- [ ] 这三个命令目前在 `local_root()` 处 bail。改为经 `Transport` 工作——
      M2b/M2c 已经把需要的方法都补齐了（`read_remote` / `list` / `read_content` /
      `read_by_hash` / `recoverable`）。
- [ ] **`verify` 的 fixity 巡检在网络上代价很高**（要拉全部内容重算哈希）。
      给一个明确的取舍：默认只校验元数据一致性，`--deep` 才拉内容。
      **帮助文本要说清楚默认模式验的是什么、不验什么**——
      一个自称 verify 却只对了对元数据的命令，比没有更危险。
- [ ] 测试：三个命令对 http hub 都能工作；离线时仍然是 I11 的处置（退出 2、绝不空库）。

### Task 4: TLS（`https://`）

**Files:** `crates/arcad/`（服务端）、`crates/arca-cli/src/transport/http.rs`（客户端）

spec §9：系统根证书静默通过；自签名走**指纹人工确认 + pin**，指纹变更即拒连。

- [ ] 服务端：`hub.toml` 可选配置证书与私钥路径；未配置则仍是明文 `http://`
      （本机/内网场景合法）。
- [ ] 客户端：`https://` 走系统信任库；**自签名证书要求先 pin 指纹**，
      `.gitarca` 或 hub 配置里记录 pin。**指纹变更即拒连并明确报错**（I5），
      绝不「首次使用即信任」后静默接受变更。
- [ ] **不要给 `arca-cli` 引入异步运行时**。若选的 TLS 库强制异步，**停下报告**。
- [ ] 测试：自签名 + 正确 pin → 连通；pin 不符 → 拒连且错误可诊断；
      无 pin 的自签名 → 拒连并提示如何 pin。

### Task 5: `arca bugreport`

**Files:** `crates/arca-cli/src/commands/porcelain.rs`

spec §3.3 借 git 的 `git bugreport`。目的：**一条命令收齐诊断现场**，让用户/agent
不必被追问二十个问题。

- [ ] 收集：版本、平台、各数据集的角色与健康度、最近的 trace 落盘文件列表、
      本地 trash 占用、`.gitignore` 反选块的**实测**结果、hub 可达性。
- [ ] **绝不收集文件内容或路径以外的用户数据**；输出前要能让用户看到收了什么。
      隐私边界写进帮助文本。
- [ ] M2d 评审建议：`role.toml` 与本地 trash 清单应当进 bugreport——
      角色正是那种「解释为什么这台设备行为和那台不同」的设备本地状态，
      而目前所有诊断命令都看不见它。
- [ ] 测试：在一个已知状态的 vault 上跑，断言输出含关键字段；
      断言**不含**文件内容。

---

## Self-Review

**范围**：占位符层与 hydration 属 M3。hub 侧登记绑定（让副本计数看到其它设备）
需要认证与设备注册，属 M2 之后。

**Task 2 是本切片风险最高的**——`arca gc` 是项目第一个被授权销毁的命令。
前面四个切片的评审一共找到 6 个 Critical，其中 4 个是「本该保住的数据没保住」。
写它的时候把那些教训当作检查表：默认不销毁、`--dry-run` 先行、
未过期一律不动、悬空引用停下报告、销毁前列清单。
