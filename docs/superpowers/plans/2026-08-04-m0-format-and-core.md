# M0 格式与核心 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付 arca 的磁盘格式规范与内容寻址地基——`FORMAT.md` v1 定稿、`arca-format` 解析器、`arca-chunk` 原语、`arcad fsck` 巡检，并用 golden vectors、fuzz、逃生舱恢复演示把「格式先于代码」与「绝不销毁数据」变成可执行断言。

**Architecture:** 三层依赖：`arca-chunk`（BLAKE3 / FastCDC / zstd，无上游依赖）→ `arca-format`（纯数据结构与解析/序列化，依赖 arca-chunk 的哈希类型）→ `arcad`（唯一做 IO 的 M0 消费者，提供 `arcad fsck` 子命令）。全部解析器遵循「损坏输入 → 明确错误，绝不 panic」（I5），由 cargo-fuzz 守护。

**Tech Stack:** Rust 2021 / MSRV 1.75 · blake3 · fastcdc · zstd · serde + serde_json（JSON Lines）· toml · clap（仅 arcad）· proptest · cargo-fuzz

## Global Constraints

以下约束适用于**每一个任务**，不再逐任务重复。

- **MSRV 1.75，edition 2021**；`version`/`edition`/`rust-version`/`homepage` 一律 `.workspace = true` 继承。
- **许可证**：`crates/arcad/` 为 `AGPL-3.0-only`，其余 crate 一律 `MIT`（spec §12.2）；官网 `https://gitarca.com`。
- **只在 `main` 分支工作**，不开特性分支；每个任务结束提交一次。
- **核心 crate 保持 `#![forbid(unsafe_code)]`**（`arca-format`、`arca-core`、`arca-chunk`、`arca-publish`、`arca-git`、`arca-conformance`）。
- **文档与注释用中文**，与既有骨架一致；每个模块 doc comment 保留其 spec 章节引用。
- **`arca-format` 与 `arca-chunk` 零重依赖、可嵌入**：不得引入 tokio 或任何异步运行时。
- **解析器绝不 panic**（I5）：所有 `parse` 返回 `Result`，不得使用 `unwrap`/`expect`/索引越界/整数溢出 panic。
- **格式先于代码**（I10）：任何磁盘格式的改动必须先落到 `FORMAT.md`，再改实现。
- **spec 是唯一真相源**：`docs/2026-08-03-arca-spec.md`，本计划的每个决定都标注了出处章节。
- **不变量编号**（I1–I11）见 spec §2，代码注释以编号引用。

## 从 lazync 继承的既定值

`/Users/bruce/git/lazync` 是前身项目（Free Pascal）。以下值**照搬，不重新设计**，出处为 `shared/src/nc_path_rules.pas` 与 `docs/LIMITS.md`：

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
| 单文件大小上限 | 2,000,000,000,000 字节 | `LIMITS.md`（对齐 Dropbox 桌面端published 值） |
| 索引键 | 小写规范化路径的哈希（arca 改用 BLAKE3；大小写冲突拒绝，绝不静默合并） | `STORAGE.md` §File Identity Index |

---

## File Structure

**新建：**

| 文件 | 职责 |
| --- | --- |
| `crates/arca-chunk/src/hash.rs` | `ContentHash`（BLAKE3）+ 流式计算 + `blake3:` 文本表示 + SHA-256 互操作 |
| `crates/arca-chunk/src/cdc.rs` | FastCDC 切块参数与迭代器 |
| `crates/arca-chunk/src/compress.rs` | zstd 压缩/解压 |
| `crates/arca-chunk/src/store.rs` | 内容寻址块存储的**路径计算**（纯函数，无 IO） |
| `crates/arca-format/src/path_rules.rs` | 路径规范化与校验（继承 lazync） |
| `crates/arca-format/src/model.rs` | `ItemId` / `VersionId` / `Actor` / `Version` |
| `crates/arca-format/src/manifest.rs` | 行式清单解析/序列化（确定性） |
| `crates/arca-format/src/gitarca.rs` | `.gitarca` 注册表 TOML |
| `crates/arca-format/src/dataset.rs` | `dataset.toml` |
| `crates/arca-format/src/hub_layout.rs` | 存储根路径常量 + `format.json` |
| `crates/arca-format/src/items.rs` | `items/*.jsonl` 版本链记录（JSON Lines） |
| `crates/arca-format/src/journal.rs` | `journal/*.jsonl` 事件记录 + `epoch:seq` 游标 |
| `crates/arca-format/src/index.rs` | `index/*.json` 路径→item_id 映射 |
| `crates/arca-format/src/error.rs` | `FormatError`：所有解析错误的统一类型 |
| `crates/arcad/src/fsck.rs` | 存储根完整性巡检（M0 唯一的 IO 消费者） |
| `crates/arca-conformance/tests/escape-hatch/recover.sh` | 逃生舱恢复演示（不含任何 arca 代码） |
| `fuzz/` | cargo-fuzz 工程，每个解析器一个 target |
| `.github/workflows/ci.yml` | check / test / clippy / 逃生舱演示 |

**修改：** 各 `Cargo.toml`（加依赖）、`FORMAT.md`（Task 1 定稿）、`crates/arcad/src/main.rs`（加 `fsck` 子命令）、`crates/arcad/src/gc.rs`（注释指向 `fsck.rs`）。

**说明：** `arca-format/src/journal.rs` 只定义**磁盘记录格式**；`arca-core/src/journal.rs` 定义**两端共用的事件语义**，M2 再实现，届时后者引用前者。两者不重复。

---

### Task 1: FORMAT.md v1 定稿

**Files:**
- Modify: `FORMAT.md`（整体重写，从骨架变为可实现的规范）

**Interfaces:**
- Consumes: 无
- Produces: 后续所有任务的字节级契约。Task 2–9 的实现必须与本文件逐字一致。

本任务只写文档，不写代码——I10 要求格式先于代码。以下内容必须全部落到 `FORMAT.md`。

- [ ] **Step 1: 写入「0. 版本与兼容性承诺」**

规定：所有格式带版本号；`format.json` 的 `format` 字段是存储根的格式版本；文本格式以首行魔法注释声明版本；只向前迁移；遇到高于已知版本的格式 → 拒绝并明确报错（I5），绝不尽力解析。

- [ ] **Step 2: 写入「1. 通用编码约定」**

```
- 字符编码：一律 UTF-8，不带 BOM。
- 换行：一律 LF（0x0A）。写入永不产生 CRLF；解析遇到 CR 结尾时容忍并剥除。
- 时间戳：RFC 3339，UTC，秒级精度，形如 2026-08-04T10:23:02Z。
- 哈希文本表示：blake3:<64 位小写十六进制>。
- 标识符：item_id 与 dataset_id 为 128-bit 随机值，表示为 32 位小写十六进制。
- JSON Lines（.jsonl）：一行一个 JSON 对象，行内不得含裸换行；
  文件以 LF 结尾；追加写入必须整行原子追加（先构造完整字节串再单次 write）。
- 所有 JSON 对象含 "v" 字段（记录格式版本，整数）作为第一个键。
```

- [ ] **Step 3: 写入「2. 路径规则」**

把上文「从 lazync 继承的既定值」表格完整抄入，并补充 arca 特有的两条：

```
- Tab（0x09）与换行（0x0A/0x0D）已被「控制字符」规则排除，
  因此行式 manifest 的 Tab 分隔无歧义（spec §4.4.1）。
- 索引键 = BLAKE3(小写规范化路径的 UTF-8 字节)。
  大小写不同但规范化后相同的两个路径视为冲突：拒绝，绝不静默合并
  （继承 lazync STORAGE.md §File Identity Index）。
- Unicode 规范化：v1 不做 NFC/NFD 转换，按字节原样保存与比较。
  macOS 的 NFD 与其他平台的 NFC 会被视为不同路径——
  这是已知边界，记录在 §9 已知限制，v2 议题。
```

- [ ] **Step 4: 写入「3. 三层模型的磁盘表示」**

```
item_id：128-bit 随机，32 位小写十六进制，创建时分配，永不复用。
version_id：<RFC3339 紧凑形式><32 位十六进制随机>，
            例 20260804T102302Z-0123456789abcdef0123456789abcdef
            前缀使版本 ID 的字典序即时间序（继承 lazync STORAGE.md §Historical Versions）。
actor：{"account": "<字符串>", "device": "<字符串>", "session": "<字符串>"}
       三者皆可为空字符串，表示未知；journal 每条事件必须携带（I8）。
```

- [ ] **Step 5: 写入「4. hub 存储根布局」**

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

所有目录必须位于同一文件系统，rename 提交才是原子的
（继承 lazync STORAGE.md）。两级十六进制分片避免单目录条目数过大。
```

- [ ] **Step 6: 写入「5. format.json」**

```json
{"v":1,"format":1,"dataset_id":"9c41000000000000000000000000abcd","hash_algo":"blake3","created_at":"2026-08-04T10:00:00Z"}
```

规定：`dataset_id` 即卷身份标记（I11）——hub 配置、客户端绑定请求与本文件三方必须一致，不符则数据集离线，**绝不呈现为空库、绝不触发删除对账**。`hash_algo` v1 恒为 `"blake3"`，其他值 → 拒绝。

- [ ] **Step 7: 写入「6. index 记录」**

```json
{"v":1,"item_id":"3f2a000000000000000000000000beef","path":"京都/鸭川.png"}
```

文件名 = `BLAKE3(小写规范化路径)` 的 64 位十六进制 + `.json`，置于其前 2 位命名的子目录下。整体原子替换（tmp → fsync → rename），不追加。`path` 存**规范化后的显示路径**（保留原始大小写）。

- [ ] **Step 8: 写入「7.1 items 版本链记录」**

`items/<xx>/<item_id>.jsonl`，append-only，一行一个版本，按提交顺序追加：

```json
{"v":1,"version_id":"20260804T102302Z-0123456789abcdef0123456789abcdef","item_id":"3f2a000000000000000000000000beef","parent":null,"hash":"blake3:9f2c…","size":2411008,"mtime":"2026-08-04T10:22:31Z","actor":{"account":"bruce","device":"mac-studio","session":"s1"},"committed_at":"2026-08-04T10:23:05Z"}
```

规定：`parent` 为上一版的 `version_id`，首版为 `null`；`item_id` 在每行重复（冗余但使单行自描述，截断的文件仍可诊断）；hub 上版本链**线性**，不存在分叉（CAS 失败以冲突副本落地为新身份，不进链）；**末行不完整时截断到最后一个完整行边界**，中间行损坏则失败而非跳过（继承 lazync STORAGE.md §Incremental Change Journal 的处置纪律）。

- [ ] **Step 9: 写入「7.2 journal 事件记录」**

`journal/<epoch>.jsonl`，append-only：

```json
{"v":1,"seq":42,"op":"upsert","item_id":"3f2a…","version_id":"20260804T102302Z-…","path":"京都/鸭川.png","actor":{"account":"bruce","device":"mac-studio","session":"s1"},"at":"2026-08-04T10:23:05Z"}
```

`op` ∈ `upsert` / `tombstone` / `rename`（`rename` 额外含 `"from"` 字段）。游标为 `<epoch>:<seq>`；`seq` 在一个 epoch 内单调递增、无空洞。客户端游标早于保留区间 → 返回 `reset_required`，走全量对账兜底。压缩规则 M2 定义。

- [ ] **Step 10: 写入「8. chunks 块存储」**

```
chunks/<xx>/<64 位十六进制 BLAKE3>.zst
```

规定：块内容以 zstd 压缩落盘，文件名的哈希是**未压缩内容**的 BLAKE3；块仅服务历史版本与增量传输，`files/` 的当前版本永远平放（I1，不可谈判）。切块用 FastCDC，参数见 §8.1：min 16 KiB / avg 64 KiB / max 256 KiB——出处：FastCDC 论文（USENIX ATC'16）的推荐区间，avg 64 KiB 在去重率与元数据开销间取平衡；zstd 级别 3（默认，压缩比与 ARM NAS 的 CPU 成本平衡，spec §1.1 目标 9）。引用计数与 GC 属 M2，v1 格式为其预留 `chunks/refs/` 目录名，M0 不写入。

- [ ] **Step 11: 写入「9. vault 侧文件」**

把骨架里已有的 `.gitarca` / `dataset.toml` / `manifest` / `.gitignore` 块小节补全字段表：

```
.gitarca（TOML）: schema=1; [hub.<名>] instance_id, url; [[dataset]] path, hub
dataset.toml（TOML）: schema=1, dataset_id, hub_instance_id,
                      public_base_url（可选）, url_style（可选，"path"|"hash"）
manifest（行式）: 首行 #%arca-manifest v1
                 其后每行：<路径>\t<blake3:hash>\t<字节数>\t<mtime RFC3339>
                 按路径的 UTF-8 字节序升序排序；同内容必产生同字节（确定性）
```

- [ ] **Step 12: 写入「10. 已知限制」**

诚实记录三条边界：Unicode 规范化不做转换（见 §2）；引用计数与 trash/uploads/locks 格式待 M2；逃生舱恢复演示依赖 `b3sum`（BLAKE3 官方 CLI），严格意义上不属于 coreutils——I1 的承诺是「不需要任何 arca 代码」，而非「只用 coreutils」，此处按前者执行并明示。

- [ ] **Step 13: 提交**

```bash
git add FORMAT.md
git commit -m "FORMAT.md v1 定稿：字节级磁盘格式规范（I10 格式先于代码）"
```

---

### Task 2: arca-chunk 哈希原语

**Files:**
- Modify: `crates/arca-chunk/Cargo.toml`, `crates/arca-chunk/src/hash.rs`, `crates/arca-chunk/src/lib.rs`
- Test: `crates/arca-chunk/src/hash.rs`（`#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: 无
- Produces: `arca_chunk::hash::ContentHash`（`Copy`, `Eq`, `Ord`, `Hash`），方法 `ContentHash::from_bytes(&[u8]) -> Self`、`ContentHash::hasher() -> Hasher`、`Hasher::update(&mut self, &[u8])`、`Hasher::finish(self) -> ContentHash`、`ContentHash::to_text(&self) -> String`（`blake3:<hex>`）、`ContentHash::parse(&str) -> Result<Self, HashParseError>`、`ContentHash::to_hex(&self) -> String`（无前缀，供文件名用）。Task 3–9 全部依赖这些签名。

- [ ] **Step 1: 加依赖**

```bash
cargo add --package arca-chunk blake3 sha2
```

- [ ] **Step 2: 写失败的测试**

写入 `crates/arca-chunk/src/hash.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // BLAKE3 官方测试向量：空输入
    const EMPTY_HEX: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn 空输入的哈希匹配官方向量() {
        let h = ContentHash::from_bytes(b"");
        assert_eq!(h.to_hex(), EMPTY_HEX);
    }

    #[test]
    fn 流式与一次性计算结果相同() {
        let data = b"git manages text, arca manages binaries";
        let once = ContentHash::from_bytes(data);
        let mut hasher = ContentHash::hasher();
        hasher.update(&data[..10]);
        hasher.update(&data[10..]);
        assert_eq!(once, hasher.finish());
    }

    #[test]
    fn 文本表示往返一致() {
        let h = ContentHash::from_bytes(b"round trip");
        let text = h.to_text();
        assert!(text.starts_with("blake3:"));
        assert_eq!(ContentHash::parse(&text).unwrap(), h);
    }

    #[test]
    fn 拒绝错误前缀而不是_panic() {
        assert!(ContentHash::parse("sha256:00").is_err());
        assert!(ContentHash::parse("blake3:xyz").is_err());
        assert!(ContentHash::parse("blake3:").is_err());
        assert!(ContentHash::parse("").is_err());
        // 大写十六进制不接受：文本表示必须确定性（同内容必同字节）
        assert!(ContentHash::parse(&format!("blake3:{}", EMPTY_HEX.to_uppercase())).is_err());
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test -p arca-chunk`
Expected: 编译失败，`cannot find type ContentHash`

- [ ] **Step 4: 实现**

在 `crates/arca-chunk/src/hash.rs` 的 doc comment 之后写入（保留既有 doc comment）：

```rust
use std::fmt;

/// BLAKE3 内容哈希——arca 的原生内容地址（I2：blob 不可变）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; 32]);

/// 流式哈希计算器：大文件与 Range 校验用。
pub struct Hasher(blake3::Hasher);

#[derive(Debug, PartialEq, Eq)]
pub enum HashParseError {
    /// 缺少 `blake3:` 前缀
    MissingPrefix,
    /// 十六进制部分长度不是 64
    BadLength(usize),
    /// 含非小写十六进制字符
    BadDigit(char),
}

impl ContentHash {
    pub fn from_bytes(data: &[u8]) -> Self {
        ContentHash(*blake3::hash(data).as_bytes())
    }

    pub fn hasher() -> Hasher {
        Hasher(blake3::Hasher::new())
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
            out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
        }
        out
    }

    pub fn to_text(&self) -> String {
        format!("blake3:{}", self.to_hex())
    }

    pub fn parse(text: &str) -> Result<Self, HashParseError> {
        let hex = text.strip_prefix("blake3:").ok_or(HashParseError::MissingPrefix)?;
        if hex.len() != 64 {
            return Err(HashParseError::BadLength(hex.len()));
        }
        let mut bytes = [0u8; 32];
        let raw = hex.as_bytes();
        for (i, slot) in bytes.iter_mut().enumerate() {
            let hi = lower_hex_value(raw[i * 2] as char)?;
            let lo = lower_hex_value(raw[i * 2 + 1] as char)?;
            *slot = (hi << 4) | lo;
        }
        Ok(ContentHash(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn lower_hex_value(c: char) -> Result<u8, HashParseError> {
    match c {
        '0'..='9' => Ok(c as u8 - b'0'),
        'a'..='f' => Ok(c as u8 - b'a' + 10),
        _ => Err(HashParseError::BadDigit(c)),
    }
}

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> ContentHash {
        ContentHash(*self.0.finalize().as_bytes())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_text())
    }
}

impl fmt::Display for HashParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HashParseError::MissingPrefix => write!(f, "缺少 blake3: 前缀"),
            HashParseError::BadLength(n) => write!(f, "十六进制长度为 {n}，应为 64"),
            HashParseError::BadDigit(c) => write!(f, "非小写十六进制字符：{c:?}"),
        }
    }
}

impl std::error::Error for HashParseError {}

/// SHA-256 懒计算——仅为互操作（Git LFS oid、Dropbox 导入校验，spec §8）。
/// 不是 arca 的内容地址，绝不用于寻址。
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p arca-chunk`
Expected: 5 个测试全部 PASS

- [ ] **Step 6: 提交**

```bash
git add crates/arca-chunk
git commit -m "arca-chunk: BLAKE3 内容哈希与 blake3: 文本表示"
```

---

### Task 3: arca-format 路径规则

**Files:**
- Modify: `crates/arca-format/Cargo.toml`, `crates/arca-format/src/path_rules.rs`, `crates/arca-format/src/lib.rs`
- Test: `crates/arca-format/src/path_rules.rs`（内联 tests 模块）

**Interfaces:**
- Consumes: `arca_chunk::hash::ContentHash`（用于索引键）
- Produces: `arca_format::path_rules::{normalize, check, PathStatus, MAX_RELATIVE_PATH_BYTES, MAX_PATH_DEPTH, MAX_SEGMENT_BYTES, index_key}`。签名：`normalize(&str) -> String`；`check(&str) -> Result<String, PathStatus>`（Ok 返回规范化路径）；`index_key(&str) -> ContentHash`（对小写规范化路径取 BLAKE3）。Task 5、7、9 依赖。

- [ ] **Step 1: 加依赖**

```bash
cargo add --package arca-format --path crates/arca-chunk
```

（即在 `crates/arca-format/Cargo.toml` 的 `[dependencies]` 加 `arca-chunk = { path = "../arca-chunk" }`）

- [ ] **Step 2: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 规范化折叠分隔符与点段() {
        assert_eq!(normalize("a\\b//c/./d"), "a/b/c/d");
        assert_eq!(normalize("./x"), "x");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn 接受合法路径并返回规范化形式() {
        assert_eq!(check("京都/鸭川.png").unwrap(), "京都/鸭川.png");
        assert_eq!(check("a\\b.txt").unwrap(), "a/b.txt");
    }

    #[test]
    fn 拒绝绝对路径与父引用() {
        assert_eq!(check("/etc/passwd"), Err(PathStatus::Absolute));
        assert_eq!(check("C:/x"), Err(PathStatus::Absolute));
        assert_eq!(check("\\\\server\\share"), Err(PathStatus::Absolute));
        assert_eq!(check("a/../b"), Err(PathStatus::ParentRef));
    }

    #[test]
    fn 拒绝空路径() {
        assert_eq!(check(""), Err(PathStatus::Empty));
        assert_eq!(check("./."), Err(PathStatus::Empty));
    }

    #[test]
    fn 拒绝控制字符包括_tab_与换行() {
        // manifest 的 Tab 分隔依赖这一条（spec §4.4.1）
        assert_eq!(check("a\tb"), Err(PathStatus::InvalidChar));
        assert_eq!(check("a\nb"), Err(PathStatus::InvalidChar));
        assert_eq!(check("a<b"), Err(PathStatus::InvalidChar));
        assert_eq!(check("a?b"), Err(PathStatus::InvalidChar));
    }

    #[test]
    fn 拒绝段以空格或句点结尾() {
        assert_eq!(check("a /b"), Err(PathStatus::InvalidChar));
        assert_eq!(check("a./b"), Err(PathStatus::InvalidChar));
    }

    #[test]
    fn 拒绝_windows_保留名() {
        assert_eq!(check("CON"), Err(PathStatus::ReservedName));
        assert_eq!(check("dir/nul.txt"), Err(PathStatus::ReservedName));
        assert_eq!(check("com9.dat"), Err(PathStatus::ReservedName));
        // 但 "console.txt" 不是保留名
        assert!(check("console.txt").is_ok());
    }

    #[test]
    fn 拒绝超限路径() {
        let long_segment = "a".repeat(MAX_SEGMENT_BYTES + 1);
        assert_eq!(check(&long_segment), Err(PathStatus::SegmentTooLong));

        let deep = vec!["d"; MAX_PATH_DEPTH + 1].join("/");
        assert_eq!(check(&deep), Err(PathStatus::TooDeep));

        let long = format!("{}/x", "a".repeat(MAX_SEGMENT_BYTES))
            .repeat(MAX_RELATIVE_PATH_BYTES / 8);
        assert_eq!(check(&long).unwrap_err(), PathStatus::TooLong);
    }

    #[test]
    fn 索引键对大小写不敏感但路径本身保留大小写() {
        assert_eq!(index_key("A/B.png"), index_key("a/b.png"));
        assert_ne!(index_key("a/b.png"), index_key("a/c.png"));
        assert_eq!(check("A/B.png").unwrap(), "A/B.png");
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test -p arca-format`
Expected: 编译失败，`cannot find function normalize`

- [ ] **Step 4: 实现**

在 `crates/arca-format/src/path_rules.rs` 的 doc comment 之后写入：

```rust
use arca_chunk::hash::ContentHash;

/// 相对路径最大字节数（继承 lazync `nc_max_relative_path_bytes`）。
pub const MAX_RELATIVE_PATH_BYTES: usize = 2048;
/// 目录最大深度，单位为段（继承 lazync `nc_max_path_depth`）。
pub const MAX_PATH_DEPTH: usize = 64;
/// 单段最大字节数（继承 lazync `nc_max_path_segment_bytes`）。
pub const MAX_SEGMENT_BYTES: usize = 240;
/// 解析后物理路径最大字节数（继承 lazync `nc_max_physical_path_bytes`）。
pub const MAX_PHYSICAL_PATH_BYTES: usize = 3800;

/// 路径校验结果。拒绝理由必须可诊断——绝不猜测、绝不截断修复（I5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStatus {
    Empty,
    Absolute,
    ParentRef,
    TooLong,
    TooDeep,
    SegmentTooLong,
    InvalidChar,
    ReservedName,
}

const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 规范化：`\` → `/`、折叠重复分隔符、丢弃空段与 `.` 段。
/// 不做 Unicode NFC/NFD 转换（FORMAT.md §2 已知限制）。
pub fn normalize(raw: &str) -> String {
    raw.split(|c| c == '/' || c == '\\')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// 校验并返回规范化路径。
pub fn check(raw: &str) -> Result<String, PathStatus> {
    if raw.is_empty() {
        return Err(PathStatus::Empty);
    }
    if is_absolute(raw) {
        return Err(PathStatus::Absolute);
    }

    let normalized = normalize(raw);
    if normalized.is_empty() {
        return Err(PathStatus::Empty);
    }
    if normalized.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(PathStatus::TooLong);
    }

    let segments: Vec<&str> = normalized.split('/').collect();
    if segments.len() > MAX_PATH_DEPTH {
        return Err(PathStatus::TooDeep);
    }

    for segment in &segments {
        if *segment == ".." {
            return Err(PathStatus::ParentRef);
        }
        if segment.len() > MAX_SEGMENT_BYTES {
            return Err(PathStatus::SegmentTooLong);
        }
        if has_invalid_char(segment) {
            return Err(PathStatus::InvalidChar);
        }
        if is_reserved(segment) {
            return Err(PathStatus::ReservedName);
        }
    }

    Ok(normalized)
}

fn is_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    match bytes.first() {
        Some(b'/') | Some(b'\\') => true,
        // 盘符形式 C:\ 或 C:/
        _ => bytes.len() >= 2 && bytes[1] == b':',
    }
}

fn has_invalid_char(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    for c in segment.chars() {
        // 控制字符含 Tab(0x09) 与换行(0x0A/0x0D)——manifest 分隔依赖此排除
        if (c as u32) < 0x20 || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            return true;
        }
    }
    matches!(segment.chars().next_back(), Some(' ') | Some('.'))
}

fn is_reserved(segment: &str) -> bool {
    let base = segment.split('.').next().unwrap_or(segment).to_ascii_uppercase();
    WINDOWS_RESERVED.contains(&base.as_str())
}

/// 索引键：小写规范化路径的 BLAKE3。
/// 大小写不同但小写后相同的路径会得到同一个键——调用方据此检出冲突并拒绝，
/// 绝不静默合并（继承 lazync STORAGE.md §File Identity Index）。
pub fn index_key(raw: &str) -> ContentHash {
    ContentHash::from_bytes(normalize(raw).to_lowercase().as_bytes())
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p arca-format`
Expected: 9 个测试全部 PASS

- [ ] **Step 6: 补一条属性测试防 panic**

```bash
cargo add --package arca-format --dev proptest
```

在 tests 模块内追加：

```rust
    use proptest::prelude::*;

    proptest! {
        /// I5：任意输入都不得 panic，只能返回明确结果。
        #[test]
        fn 任意输入都不_panic(raw in ".*") {
            let _ = normalize(&raw);
            let _ = check(&raw);
            let _ = index_key(&raw);
        }

        /// 规范化是幂等的——同内容必产生同字节。
        #[test]
        fn 规范化幂等(raw in ".*") {
            let once = normalize(&raw);
            prop_assert_eq!(normalize(&once), once.clone());
        }
    }
```

- [ ] **Step 7: 运行并提交**

Run: `cargo test -p arca-format`
Expected: 全部 PASS

```bash
git add crates/arca-format crates/arca-chunk
git commit -m "arca-format: 路径规范化与校验（继承 lazync nc_path_rules 的限值与规则）"
```

---

### Task 4: arca-format 核心类型与错误

**Files:**
- Create: `crates/arca-format/src/error.rs`
- Modify: `crates/arca-format/src/model.rs`, `crates/arca-format/src/lib.rs`, `crates/arca-format/Cargo.toml`

**Interfaces:**
- Consumes: `arca_chunk::hash::ContentHash`，`arca_format::path_rules::PathStatus`
- Produces:
  - `arca_format::error::FormatError`（枚举，实现 `std::error::Error`），变体：`UnsupportedVersion { found: u32, max: u32 }`、`Malformed { line: usize, reason: String }`、`BadPath(PathStatus)`、`BadHash(String)`、`Io(String)`。
  - `arca_format::model::{ItemId, VersionId, Actor, Version}`。签名：`ItemId::parse(&str) -> Result<Self, FormatError>`、`ItemId::to_hex(&self) -> String`、`ItemId::from_bytes([u8;16]) -> Self`；`VersionId::new(timestamp: &str, random_hex: &str) -> Result<Self, FormatError>`、`VersionId::as_str(&self) -> &str`；`Actor { account: String, device: String, session: String }`；`Version { version_id, item_id, parent: Option<VersionId>, hash: ContentHash, size: u64, mtime: String, actor: Actor, committed_at: String }`。Task 5、6、7、9 依赖。

- [ ] **Step 1: 加依赖**

```bash
cargo add --package arca-format serde --features derive
cargo add --package arca-format serde_json toml
```

- [ ] **Step 2: 写失败的测试**

写入 `crates/arca-format/src/model.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_id_往返一致() {
        let id = ItemId::from_bytes([0x3f, 0x2a, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xbe, 0xef]);
        assert_eq!(id.to_hex(), "3f2a000000000000000000000000beef");
        assert_eq!(ItemId::parse(&id.to_hex()).unwrap(), id);
    }

    #[test]
    fn item_id_拒绝非法输入而不是_panic() {
        assert!(ItemId::parse("").is_err());
        assert!(ItemId::parse("3f2a").is_err());                      // 太短
        assert!(ItemId::parse(&"a".repeat(33)).is_err());             // 太长
        assert!(ItemId::parse(&"3F2A".repeat(8)).is_err());           // 大写不接受
        assert!(ItemId::parse("zz2a000000000000000000000000beef").is_err());
    }

    #[test]
    fn version_id_的字典序即时间序() {
        let early = VersionId::new("20260804T102302Z", &"0".repeat(32)).unwrap();
        let late = VersionId::new("20260804T102303Z", &"0".repeat(32)).unwrap();
        assert!(early.as_str() < late.as_str());
    }

    #[test]
    fn version_id_拒绝错误形状() {
        assert!(VersionId::new("2026-08-04T10:23:02Z", &"0".repeat(32)).is_err()); // 非紧凑形式
        assert!(VersionId::new("20260804T102302Z", "abc").is_err());               // 随机段长度不对
    }
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cargo test -p arca-format model`
Expected: 编译失败，`cannot find type ItemId`

- [ ] **Step 4: 实现 error.rs**

```rust
//! 统一错误类型：损坏输入 → 明确错误，绝不 panic、绝不猜测（I5）。

use crate::path_rules::PathStatus;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// 格式版本高于本实现已知的最高版本 → 拒绝，不尽力解析（I10）。
    UnsupportedVersion { found: u32, max: u32 },
    /// 结构损坏。`line` 为 1 起的行号，0 表示非行式格式。
    Malformed { line: usize, reason: String },
    BadPath(PathStatus),
    BadHash(String),
    Io(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::UnsupportedVersion { found, max } => {
                write!(f, "格式版本 {found} 高于本实现支持的 {max}；请升级 arca")
            }
            FormatError::Malformed { line, reason } => {
                if *line == 0 {
                    write!(f, "格式损坏：{reason}")
                } else {
                    write!(f, "第 {line} 行格式损坏：{reason}")
                }
            }
            FormatError::BadPath(status) => write!(f, "路径不合规：{status:?}"),
            FormatError::BadHash(text) => write!(f, "哈希不合规：{text}"),
            FormatError::Io(msg) => write!(f, "IO 错误：{msg}"),
        }
    }
}

impl std::error::Error for FormatError {}

impl From<PathStatus> for FormatError {
    fn from(status: PathStatus) -> Self {
        FormatError::BadPath(status)
    }
}
```

- [ ] **Step 5: 实现 model.rs**

在既有 doc comment 之后写入：

```rust
use crate::error::FormatError;
use arca_chunk::hash::ContentHash;
use serde::{Deserialize, Serialize};

/// 身份：128-bit 随机，创建时分配，永不复用；跨改名稳定（I7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId([u8; 16]);

impl ItemId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        ItemId(bytes)
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn parse(text: &str) -> Result<Self, FormatError> {
        if text.len() != 32 {
            return Err(FormatError::Malformed {
                line: 0,
                reason: format!("item_id 长度为 {}，应为 32", text.len()),
            });
        }
        let mut bytes = [0u8; 16];
        let raw = text.as_bytes();
        for (i, slot) in bytes.iter_mut().enumerate() {
            let hi = lower_hex(raw[i * 2])?;
            let lo = lower_hex(raw[i * 2 + 1])?;
            *slot = (hi << 4) | lo;
        }
        Ok(ItemId(bytes))
    }

    /// 前 2 位十六进制——存储分片目录名（FORMAT.md §4）。
    pub fn shard(&self) -> String {
        format!("{:02x}", self.0[0])
    }
}

fn lower_hex(byte: u8) -> Result<u8, FormatError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(FormatError::Malformed {
            line: 0,
            reason: format!("非小写十六进制字节：{byte:#04x}"),
        }),
    }
}

/// 版本标识：`<紧凑时间戳>-<32 位十六进制随机>`，字典序即时间序。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VersionId(String);

impl VersionId {
    /// `timestamp` 形如 `20260804T102302Z`；`random_hex` 为 32 位小写十六进制。
    pub fn new(timestamp: &str, random_hex: &str) -> Result<Self, FormatError> {
        let valid_ts = timestamp.len() == 16
            && timestamp.as_bytes()[8] == b'T'
            && timestamp.ends_with('Z')
            && timestamp[..8].bytes().all(|b| b.is_ascii_digit())
            && timestamp[9..15].bytes().all(|b| b.is_ascii_digit());
        if !valid_ts {
            return Err(FormatError::Malformed {
                line: 0,
                reason: format!("时间戳 {timestamp:?} 不是 YYYYMMDDTHHMMSSZ 形式"),
            });
        }
        if random_hex.len() != 32 || !random_hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            return Err(FormatError::Malformed {
                line: 0,
                reason: "随机段应为 32 位小写十六进制".to_string(),
            });
        }
        Ok(VersionId(format!("{timestamp}-{random_hex}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 事件归因（I8）：账号 + 设备/agent + 会话。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub session: String,
}

/// 一个版本。hub 上的版本链是线性的（spec §4.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub version_id: VersionId,
    pub item_id: ItemId,
    pub parent: Option<VersionId>,
    pub hash: ContentHash,
    pub size: u64,
    pub mtime: String,
    pub actor: Actor,
    pub committed_at: String,
}
```

- [ ] **Step 6: 在 lib.rs 注册 error 模块**

在 `crates/arca-format/src/lib.rs` 的模块列表中加入 `pub mod error;`（保持字母序：`dataset, error, gitarca, hub_layout, manifest, model, path_rules`）。

- [ ] **Step 7: 运行测试确认通过**

Run: `cargo test -p arca-format`
Expected: 全部 PASS

- [ ] **Step 8: 提交**

```bash
git add crates/arca-format
git commit -m "arca-format: 身份/版本/actor 核心类型与统一错误类型"
```

---

### Task 5: manifest 行式解析与确定性序列化

**Files:**
- Modify: `crates/arca-format/src/manifest.rs`
- Test: 内联 tests 模块 + `crates/arca-format/tests/golden/manifest/`

**Interfaces:**
- Consumes: `ContentHash`、`FormatError`、`path_rules::check`
- Produces: `arca_format::manifest::{Manifest, ManifestEntry}`。签名：`Manifest::parse(&str) -> Result<Manifest, FormatError>`、`Manifest::to_string(&self) -> String`（确定性）、`Manifest::from_entries(Vec<ManifestEntry>) -> Manifest`（内部排序去重）、`ManifestEntry { path: String, hash: ContentHash, size: u64, mtime: String }`。Task 9 依赖。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use arca_chunk::hash::ContentHash;

    fn 样例哈希(seed: &[u8]) -> ContentHash {
        ContentHash::from_bytes(seed)
    }

    #[test]
    fn 解析合法清单() {
        let text = format!(
            "#%arca-manifest v1\n京都/鸭川.png\t{}\t2411008\t2026-08-04T10:22:31Z\n",
            样例哈希(b"a").to_text()
        );
        let manifest = Manifest::parse(&text).unwrap();
        assert_eq!(manifest.entries().len(), 1);
        assert_eq!(manifest.entries()[0].path, "京都/鸭川.png");
        assert_eq!(manifest.entries()[0].size, 2411008);
    }

    #[test]
    fn 序列化按路径字节序排序且往返一致() {
        let entries = vec![
            ManifestEntry { path: "z.png".into(), hash: 样例哈希(b"z"), size: 1, mtime: "2026-08-04T10:00:00Z".into() },
            ManifestEntry { path: "a.png".into(), hash: 样例哈希(b"a"), size: 2, mtime: "2026-08-04T10:00:00Z".into() },
        ];
        let manifest = Manifest::from_entries(entries);
        let text = manifest.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "#%arca-manifest v1");
        assert!(lines[1].starts_with("a.png"));
        assert!(lines[2].starts_with("z.png"));
        assert_eq!(Manifest::parse(&text).unwrap(), manifest);
    }

    #[test]
    fn 同内容必产生同字节() {
        let mk = |order: [&str; 2]| {
            Manifest::from_entries(
                order.iter().map(|p| ManifestEntry {
                    path: (*p).into(), hash: 样例哈希(p.as_bytes()), size: 1,
                    mtime: "2026-08-04T10:00:00Z".into(),
                }).collect()
            ).to_string()
        };
        assert_eq!(mk(["a.png", "b.png"]), mk(["b.png", "a.png"]));
    }

    #[test]
    fn 拒绝缺失或错误的头部() {
        assert!(Manifest::parse("").is_err());
        assert!(Manifest::parse("京都/鸭川.png\tblake3:00\t1\tt\n").is_err());
        assert!(Manifest::parse("#%arca-manifest v99\n").is_err());
    }

    #[test]
    fn 拒绝字段数错误的行并报出行号() {
        let text = "#%arca-manifest v1\na.png\tblake3:00\n";
        match Manifest::parse(text) {
            Err(crate::error::FormatError::Malformed { line, .. }) => assert_eq!(line, 2),
            other => panic!("应报第 2 行损坏，实得 {other:?}"),
        }
    }

    #[test]
    fn 拒绝不合规路径() {
        let text = format!("#%arca-manifest v1\n../逃逸.png\t{}\t1\t2026-08-04T10:00:00Z\n", 样例哈希(b"x").to_text());
        assert!(Manifest::parse(&text).is_err());
    }

    #[test]
    fn 空清单只有头部() {
        let manifest = Manifest::from_entries(vec![]);
        assert_eq!(manifest.to_string(), "#%arca-manifest v1\n");
        assert_eq!(Manifest::parse("#%arca-manifest v1\n").unwrap(), manifest);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p arca-format manifest`
Expected: 编译失败，`cannot find type Manifest`

- [ ] **Step 3: 实现**

```rust
use crate::error::FormatError;
use crate::path_rules;
use arca_chunk::hash::ContentHash;

const HEADER: &str = "#%arca-manifest v1";
const MAX_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: String,
    pub hash: ContentHash,
    pub size: u64,
    pub mtime: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    entries: Vec<ManifestEntry>,
}

impl Manifest {
    /// 内部按路径 UTF-8 字节序排序，保证确定性序列化。
    pub fn from_entries(mut entries: Vec<ManifestEntry>) -> Self {
        entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Manifest { entries }
    }

    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let mut lines = text.lines().enumerate();
        let (_, header) = lines.next().ok_or(FormatError::Malformed {
            line: 0,
            reason: "清单为空，缺少头部".to_string(),
        })?;
        parse_header(header.trim_end_matches('\r'))?;

        let mut entries = Vec::new();
        for (zero_based, raw) in lines {
            let line_no = zero_based + 1;
            let line = raw.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            entries.push(parse_entry(line, line_no)?);
        }
        Ok(Manifest::from_entries(entries))
    }

    pub fn to_string(&self) -> String {
        let mut out = String::from(HEADER);
        out.push('\n');
        for entry in &self.entries {
            out.push_str(&entry.path);
            out.push('\t');
            out.push_str(&entry.hash.to_text());
            out.push('\t');
            out.push_str(&entry.size.to_string());
            out.push('\t');
            out.push_str(&entry.mtime);
            out.push('\n');
        }
        out
    }
}

fn parse_header(header: &str) -> Result<(), FormatError> {
    let version = header.strip_prefix("#%arca-manifest v").ok_or(FormatError::Malformed {
        line: 1,
        reason: format!("头部应为 {HEADER:?}，实得 {header:?}"),
    })?;
    let found: u32 = version.parse().map_err(|_| FormatError::Malformed {
        line: 1,
        reason: format!("版本号 {version:?} 不是整数"),
    })?;
    if found > MAX_VERSION {
        return Err(FormatError::UnsupportedVersion { found, max: MAX_VERSION });
    }
    Ok(())
}

fn parse_entry(line: &str, line_no: usize) -> Result<ManifestEntry, FormatError> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() != 4 {
        return Err(FormatError::Malformed {
            line: line_no,
            reason: format!("应有 4 个 Tab 分隔字段，实得 {}", fields.len()),
        });
    }
    let path = path_rules::check(fields[0]).map_err(|status| FormatError::Malformed {
        line: line_no,
        reason: format!("路径不合规：{status:?}"),
    })?;
    let hash = ContentHash::parse(fields[1]).map_err(|e| FormatError::Malformed {
        line: line_no,
        reason: format!("哈希不合规：{e}"),
    })?;
    let size: u64 = fields[2].parse().map_err(|_| FormatError::Malformed {
        line: line_no,
        reason: format!("大小 {:?} 不是无符号整数", fields[2]),
    })?;
    Ok(ManifestEntry { path, hash, size, mtime: fields[3].to_string() })
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p arca-format manifest`
Expected: 7 个测试全部 PASS

- [ ] **Step 5: 加 golden vector**

创建 `crates/arca-format/tests/golden/manifest/basic.manifest`（注意用真实 Tab 字符）：

```
#%arca-manifest v1
京都/街景.mp4	blake3:c71a000000000000000000000000000000000000000000000000000000000000	1884301776	2026-08-04T10:23:02Z
京都/鸭川.png	blake3:9f2c000000000000000000000000000000000000000000000000000000000000	2411008	2026-08-04T10:22:31Z
```

创建 `crates/arca-format/tests/golden_manifest.rs`：

```rust
//! golden vectors 回归：格式变更必须通过全部既有样例（spec §11.2）。

use arca_format::manifest::Manifest;

#[test]
fn basic_样例往返字节一致() {
    let text = include_str!("golden/manifest/basic.manifest");
    let manifest = Manifest::parse(text).expect("样例应可解析");
    assert_eq!(manifest.to_string(), text, "往返后字节必须完全一致");
}
```

- [ ] **Step 6: 运行并提交**

Run: `cargo test -p arca-format`
Expected: 全部 PASS

```bash
git add crates/arca-format
git commit -m "arca-format: 行式 manifest 解析与确定性序列化 + golden vector"
```

---

### Task 6: vault 侧 TOML（.gitarca 与 dataset.toml）

**Files:**
- Modify: `crates/arca-format/src/gitarca.rs`, `crates/arca-format/src/dataset.rs`

**Interfaces:**
- Consumes: `FormatError`
- Produces:
  - `arca_format::gitarca::{Registry, HubEntry, DatasetEntry}`：`Registry::parse(&str) -> Result<Registry, FormatError>`、`Registry::to_toml(&self) -> String`、`Registry::hub(&self, name: &str) -> Option<&HubEntry>`、`Registry::validate(&self) -> Result<(), FormatError>`。
  - `arca_format::dataset::DatasetConfig`：`DatasetConfig::parse(&str) -> Result<Self, FormatError>`、`to_toml(&self) -> String`；字段 `schema: u32, dataset_id: String, hub_instance_id: String, public_base_url: Option<String>, url_style: Option<UrlStyle>`；`UrlStyle` ∈ `Path | Hash`。
- Task 9 依赖 `Registry::validate`。

- [ ] **Step 1: 写失败的测试（gitarca.rs）**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const 样例: &str = r#"
schema = 1

[hub.home]
instance_id = "3f2a000000000000000000000000beef"
url = "https://nas.example.com:8443"

[[dataset]]
path = "assets"
hub  = "home"
"#;

    #[test]
    fn 解析注册表() {
        let reg = Registry::parse(样例).unwrap();
        assert_eq!(reg.hub("home").unwrap().url, "https://nas.example.com:8443");
        assert_eq!(reg.datasets().len(), 1);
        assert_eq!(reg.datasets()[0].path, "assets");
    }

    #[test]
    fn 拒绝未知_schema_版本() {
        assert!(Registry::parse("schema = 99\n").is_err());
    }

    #[test]
    fn 拒绝引用了不存在的_hub() {
        let text = "schema = 1\n[[dataset]]\npath = \"assets\"\nhub = \"ghost\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(reg.validate().is_err(), "引用不存在的 hub 必须拒绝（spec §4.3.2）");
    }

    #[test]
    fn 拒绝同一路径登记两次() {
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"assets\"\nhub = \"h\"\n\
                    [[dataset]]\npath = \"assets\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(reg.validate().is_err(), "重复路径必须拒绝（spec §4.3.2）");
    }

    #[test]
    fn 拒绝嵌套数据集() {
        let text = "schema = 1\n[hub.h]\ninstance_id = \"a\"\nurl = \"u\"\n\
                    [[dataset]]\npath = \"assets\"\nhub = \"h\"\n\
                    [[dataset]]\npath = \"assets/inner\"\nhub = \"h\"\n";
        let reg = Registry::parse(text).unwrap();
        assert!(reg.validate().is_err(), "归属必须唯一，嵌套拒绝（spec §4.3.2）");
    }

    #[test]
    fn 拒绝损坏的_toml_而不是_panic() {
        assert!(Registry::parse("[[[").is_err());
        assert!(Registry::parse("").is_err());
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p arca-format gitarca`
Expected: 编译失败

- [ ] **Step 3: 实现 gitarca.rs**

```rust
use crate::error::FormatError;
use serde::{Deserialize, Serialize};

const MAX_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubEntry {
    pub instance_id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetEntry {
    pub path: String,
    pub hub: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub schema: u32,
    #[serde(default)]
    hub: std::collections::BTreeMap<String, HubEntry>,
    #[serde(default)]
    dataset: Vec<DatasetEntry>,
}

impl Registry {
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let registry: Registry = toml::from_str(text).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("TOML 解析失败：{e}"),
        })?;
        if registry.schema > MAX_SCHEMA {
            return Err(FormatError::UnsupportedVersion { found: registry.schema, max: MAX_SCHEMA });
        }
        Ok(registry)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    pub fn hub(&self, name: &str) -> Option<&HubEntry> {
        self.hub.get(name)
    }

    pub fn datasets(&self) -> &[DatasetEntry] {
        &self.dataset
    }

    /// spec §4.3.2 的一致性规则：引用存在、路径唯一、不得嵌套。
    /// 违反即拒绝，绝不静默激活（I5）。
    pub fn validate(&self) -> Result<(), FormatError> {
        let mut seen: Vec<String> = Vec::new();
        for entry in &self.dataset {
            if !self.hub.contains_key(&entry.hub) {
                return Err(FormatError::Malformed {
                    line: 0,
                    reason: format!("数据集 {:?} 引用了未登记的 hub {:?}", entry.path, entry.hub),
                });
            }
            let normalized = crate::path_rules::normalize(&entry.path);
            for existing in &seen {
                if existing.as_str() == normalized {
                    return Err(FormatError::Malformed {
                        line: 0,
                        reason: format!("路径 {normalized:?} 被登记了两次"),
                    });
                }
                if normalized.starts_with(&format!("{existing}/"))
                    || existing.starts_with(&format!("{normalized}/"))
                {
                    return Err(FormatError::Malformed {
                        line: 0,
                        reason: format!("数据集 {normalized:?} 与 {existing:?} 嵌套；归属必须唯一"),
                    });
                }
            }
            seen.push(normalized);
        }
        Ok(())
    }
}
```

- [ ] **Step 4: 写 dataset.rs 的测试与实现**

测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析数据集配置() {
        let text = r#"
schema = 1
dataset_id = "9c41000000000000000000000000abcd"
hub_instance_id = "3f2a000000000000000000000000beef"
public_base_url = "https://cdn.example.com/assets"
url_style = "path"
"#;
        let cfg = DatasetConfig::parse(text).unwrap();
        assert_eq!(cfg.dataset_id, "9c41000000000000000000000000abcd");
        assert_eq!(cfg.url_style, Some(UrlStyle::Path));
    }

    #[test]
    fn 发布配置可缺省() {
        let text = "schema = 1\ndataset_id = \"9c41000000000000000000000000abcd\"\n\
                    hub_instance_id = \"3f2a000000000000000000000000beef\"\n";
        let cfg = DatasetConfig::parse(text).unwrap();
        assert!(cfg.public_base_url.is_none());
    }

    #[test]
    fn 拒绝未知_url_style_而不是猜测() {
        let text = "schema = 1\ndataset_id = \"a\"\nhub_instance_id = \"b\"\nurl_style = \"magic\"\n";
        assert!(DatasetConfig::parse(text).is_err());
    }

    #[test]
    fn 拒绝缺失必填字段() {
        assert!(DatasetConfig::parse("schema = 1\n").is_err());
    }
}
```

实现：

```rust
use crate::error::FormatError;
use serde::{Deserialize, Serialize};

const MAX_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UrlStyle {
    Path,
    Hash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetConfig {
    pub schema: u32,
    pub dataset_id: String,
    pub hub_instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_style: Option<UrlStyle>,
}

impl DatasetConfig {
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let cfg: DatasetConfig = toml::from_str(text).map_err(|e| FormatError::Malformed {
            line: 0,
            reason: format!("TOML 解析失败：{e}"),
        })?;
        if cfg.schema > MAX_SCHEMA {
            return Err(FormatError::UnsupportedVersion { found: cfg.schema, max: MAX_SCHEMA });
        }
        Ok(cfg)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p arca-format`
Expected: 全部 PASS

- [ ] **Step 6: 提交**

```bash
git add crates/arca-format
git commit -m "arca-format: .gitarca 注册表与 dataset.toml，含 §4.3.2 一致性规则"
```

---

### Task 7: hub 侧 JSON Lines 记录（format.json / index / items / journal）

**Files:**
- Create: `crates/arca-format/src/items.rs`, `crates/arca-format/src/journal.rs`, `crates/arca-format/src/index.rs`
- Modify: `crates/arca-format/src/hub_layout.rs`, `crates/arca-format/src/lib.rs`

**Interfaces:**
- Consumes: `ItemId`、`VersionId`、`Actor`、`ContentHash`、`FormatError`
- Produces:
  - `hub_layout::{FormatJson, layout}`：`FormatJson::parse(&str) -> Result<Self, FormatError>`、`FormatJson::to_json(&self) -> String`；`layout::{FILES_DIR, ARCA_DIR, FORMAT_JSON, INDEX_DIR, ITEMS_DIR, CHUNKS_DIR, JOURNAL_DIR, TRASH_DIR, UPLOADS_DIR, TMP_DIR, LOCKS_DIR}` 常量；`layout::item_path(&ItemId) -> String`、`layout::index_path(&ContentHash) -> String`、`layout::chunk_path(&ContentHash) -> String`。
  - `items::VersionRecord`：`parse_line(&str, line_no: usize) -> Result<Version, FormatError>`、`to_line(&Version) -> String`、`parse_chain(&str) -> Result<Vec<Version>, FormatError>`（校验 parent 链线性且首版 parent 为 None）。
  - `journal::{JournalEvent, Op, Cursor}`：`JournalEvent::parse_line`、`to_line`、`Cursor { epoch: String, seq: u64 }`、`Cursor::parse("<epoch>:<seq>")`、`Cursor::to_string`。
  - `index::IndexRecord`：`parse(&str)`、`to_json(&self)`；字段 `item_id: ItemId, path: String`。
- Task 9 全部依赖。

- [ ] **Step 1: 写失败的测试（items.rs）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, ItemId, VersionId};
    use arca_chunk::hash::ContentHash;

    fn 样例版本(parent: Option<VersionId>) -> crate::model::Version {
        crate::model::Version {
            version_id: VersionId::new("20260804T102302Z", &"0".repeat(32)).unwrap(),
            item_id: ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            parent,
            hash: ContentHash::from_bytes(b"content"),
            size: 2411008,
            mtime: "2026-08-04T10:22:31Z".into(),
            actor: Actor { account: "bruce".into(), device: "mac".into(), session: "s1".into() },
            committed_at: "2026-08-04T10:23:05Z".into(),
        }
    }

    #[test]
    fn 版本记录往返一致() {
        let version = 样例版本(None);
        let line = to_line(&version);
        assert!(!line.contains('\n'), "记录内不得含裸换行");
        assert_eq!(parse_line(&line, 1).unwrap(), version);
    }

    #[test]
    fn 首版的_parent_为_null() {
        let line = to_line(&样例版本(None));
        assert!(line.contains("\"parent\":null"));
    }

    #[test]
    fn 解析版本链并校验线性() {
        let v1 = 样例版本(None);
        let v2 = crate::model::Version {
            version_id: VersionId::new("20260804T102400Z", &"1".repeat(32)).unwrap(),
            parent: Some(v1.version_id.clone()),
            ..样例版本(None)
        };
        let text = format!("{}\n{}\n", to_line(&v1), to_line(&v2));
        let chain = parse_chain(&text).unwrap();
        assert_eq!(chain.len(), 2);
    }

    #[test]
    fn 拒绝断裂的版本链() {
        let v1 = 样例版本(None);
        let 孤儿 = crate::model::Version {
            version_id: VersionId::new("20260804T102400Z", &"1".repeat(32)).unwrap(),
            parent: Some(VersionId::new("20260804T999999Z", &"9".repeat(32)).unwrap_or_else(|_| v1.version_id.clone())),
            ..样例版本(None)
        };
        let text = format!("{}\n{}\n", to_line(&v1), to_line(&孤儿));
        // parent 不指向上一行 → 链断裂，必须报错而非跳过
        assert!(parse_chain(&text).is_err());
    }

    #[test]
    fn 末行不完整时截断而非报错() {
        let v1 = 样例版本(None);
        let text = format!("{}\n{{\"v\":1,\"version_", to_line(&v1));
        let chain = parse_chain(&text).expect("末行不完整应截断到最后完整行");
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn 中间行损坏则失败() {
        let v1 = 样例版本(None);
        let text = format!("{}\n损坏的行\n{}\n", to_line(&v1), to_line(&v1));
        assert!(parse_chain(&text).is_err(), "中间行损坏必须失败，不得跳过");
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p arca-format items`
Expected: 编译失败，模块不存在

- [ ] **Step 3: 实现 items.rs**

```rust
//! `items/<xx>/<item_id>.jsonl`：append-only 版本链（FORMAT.md §7.1）。
//!
//! 一行一个版本记录，按提交顺序追加。hub 上的链是线性的——
//! CAS 失败产生的分叉以冲突副本（新身份）落地，不进链（spec §4.1）。

use crate::error::FormatError;
use crate::model::{Actor, ItemId, Version, VersionId};
use arca_chunk::hash::ContentHash;
use serde::{Deserialize, Serialize};

const RECORD_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u32,
    version_id: String,
    item_id: String,
    parent: Option<String>,
    hash: String,
    size: u64,
    mtime: String,
    actor: Actor,
    committed_at: String,
}

pub fn to_line(version: &Version) -> String {
    let wire = Wire {
        v: RECORD_VERSION,
        version_id: version.version_id.as_str().to_string(),
        item_id: version.item_id.to_hex(),
        parent: version.parent.as_ref().map(|p| p.as_str().to_string()),
        hash: version.hash.to_text(),
        size: version.size,
        mtime: version.mtime.clone(),
        actor: version.actor.clone(),
        committed_at: version.committed_at.clone(),
    };
    serde_json::to_string(&wire).unwrap_or_default()
}

pub fn parse_line(line: &str, line_no: usize) -> Result<Version, FormatError> {
    let wire: Wire = serde_json::from_str(line).map_err(|e| FormatError::Malformed {
        line: line_no,
        reason: format!("JSON 解析失败：{e}"),
    })?;
    if wire.v > RECORD_VERSION {
        return Err(FormatError::UnsupportedVersion { found: wire.v, max: RECORD_VERSION });
    }
    let bad = |reason: String| FormatError::Malformed { line: line_no, reason };
    Ok(Version {
        version_id: parse_version_id(&wire.version_id).map_err(|e| bad(format!("{e}")))?,
        item_id: ItemId::parse(&wire.item_id).map_err(|e| bad(format!("{e}")))?,
        parent: match wire.parent {
            Some(ref p) => Some(parse_version_id(p).map_err(|e| bad(format!("{e}")))?),
            None => None,
        },
        hash: ContentHash::parse(&wire.hash).map_err(|e| bad(format!("{e}")))?,
        size: wire.size,
        mtime: wire.mtime,
        actor: wire.actor,
        committed_at: wire.committed_at,
    })
}

fn parse_version_id(text: &str) -> Result<VersionId, FormatError> {
    let (timestamp, random) = text.split_once('-').ok_or(FormatError::Malformed {
        line: 0,
        reason: format!("version_id {text:?} 缺少分隔符"),
    })?;
    VersionId::new(timestamp, random)
}

/// 解析整条版本链。
///
/// 处置纪律（继承 lazync STORAGE.md）：**末行不完整 → 截断到最后一个完整行**
/// （崩溃时的正常残留）；**中间行损坏 → 失败**（真损坏，绝不跳过、绝不猜测，I5）。
pub fn parse_chain(text: &str) -> Result<Vec<Version>, FormatError> {
    let mut versions = Vec::new();
    let complete_upto = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let complete = &text[..complete_upto];

    for (zero_based, raw) in complete.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let version = parse_line(line, zero_based + 1)?;
        match (&version.parent, versions.last()) {
            (None, None) => {}
            (Some(parent), Some(prev)) if parent == &(prev as &Version).version_id => {}
            _ => {
                return Err(FormatError::Malformed {
                    line: zero_based + 1,
                    reason: "版本链断裂：parent 不指向上一条记录".to_string(),
                })
            }
        }
        versions.push(version);
    }
    Ok(versions)
}
```

- [ ] **Step 4: 运行 items 测试确认通过**

Run: `cargo test -p arca-format items`
Expected: 6 个测试 PASS

- [ ] **Step 5: 实现 journal.rs（含测试）**

测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 游标往返一致() {
        let cursor = Cursor { epoch: "abc123".into(), seq: 42 };
        assert_eq!(cursor.to_string(), "abc123:42");
        assert_eq!(Cursor::parse("abc123:42").unwrap(), cursor);
    }

    #[test]
    fn 拒绝畸形游标() {
        assert!(Cursor::parse("no-colon").is_err());
        assert!(Cursor::parse("abc:notanumber").is_err());
        assert!(Cursor::parse("").is_err());
    }

    #[test]
    fn 事件往返一致() {
        let event = JournalEvent {
            seq: 42,
            op: Op::Upsert,
            item_id: crate::model::ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            version_id: Some(crate::model::VersionId::new("20260804T102302Z", &"0".repeat(32)).unwrap()),
            path: "京都/鸭川.png".into(),
            from: None,
            actor: crate::model::Actor::default(),
            at: "2026-08-04T10:23:05Z".into(),
        };
        let line = event.to_line();
        assert!(!line.contains('\n'));
        assert_eq!(JournalEvent::parse_line(&line, 1).unwrap(), event);
    }

    #[test]
    fn rename_事件携带来源路径() {
        let line = r#"{"v":1,"seq":1,"op":"rename","item_id":"3f2a000000000000000000000000beef","version_id":null,"path":"新.png","from":"旧.png","actor":{"account":"","device":"","session":""},"at":"2026-08-04T10:00:00Z"}"#;
        let event = JournalEvent::parse_line(line, 1).unwrap();
        assert_eq!(event.op, Op::Rename);
        assert_eq!(event.from.as_deref(), Some("旧.png"));
    }

    #[test]
    fn 拒绝未知操作码而不是忽略() {
        let line = r#"{"v":1,"seq":1,"op":"魔法","item_id":"3f2a000000000000000000000000beef","version_id":null,"path":"a.png","from":null,"actor":{"account":"","device":"","session":""},"at":"t"}"#;
        assert!(JournalEvent::parse_line(line, 1).is_err());
    }
}
```

实现要点：`Op` 用 `#[serde(rename_all = "lowercase")]` 的枚举 `Upsert | Tombstone | Rename`；`JournalEvent` 结构体字段与 FORMAT.md §7.2 逐字对应；`Cursor::parse` 用 `split_once(':')` 并对 `seq` 做 `u64::from_str`，失败返回 `FormatError::Malformed`。序列化与 items 同风格（`serde_json::to_string`）。

- [ ] **Step 6: 实现 index.rs 与 hub_layout.rs（含测试）**

`index.rs` 测试：往返一致；拒绝畸形 JSON；拒绝不合规路径。
`hub_layout.rs` 测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemId;

    #[test]
    fn format_json_往返一致() {
        let text = r#"{"v":1,"format":1,"dataset_id":"9c41000000000000000000000000abcd","hash_algo":"blake3","created_at":"2026-08-04T10:00:00Z"}"#;
        let parsed = FormatJson::parse(text).unwrap();
        assert_eq!(parsed.dataset_id, "9c41000000000000000000000000abcd");
        assert_eq!(FormatJson::parse(&parsed.to_json()).unwrap(), parsed);
    }

    #[test]
    fn 拒绝未来的格式版本() {
        let text = r#"{"v":1,"format":99,"dataset_id":"a","hash_algo":"blake3","created_at":"t"}"#;
        assert!(FormatJson::parse(text).is_err(), "高于已知版本必须拒绝（I10）");
    }

    #[test]
    fn 拒绝未知哈希算法() {
        let text = r#"{"v":1,"format":1,"dataset_id":"a","hash_algo":"md5","created_at":"t"}"#;
        assert!(FormatJson::parse(text).is_err());
    }

    #[test]
    fn 分片路径按前两位十六进制() {
        let id = ItemId::parse("3f2a000000000000000000000000beef").unwrap();
        assert_eq!(layout::item_path(&id), ".arca/items/3f/3f2a000000000000000000000000beef.jsonl");
    }
}
```

- [ ] **Step 7: 在 lib.rs 注册新模块**

模块列表变为：`dataset, error, gitarca, hub_layout, index, items, journal, manifest, model, path_rules`。

- [ ] **Step 8: 运行全部测试并提交**

Run: `cargo test -p arca-format && cargo clippy -p arca-format --all-targets`
Expected: 测试全 PASS，clippy 无警告

```bash
git add crates/arca-format
git commit -m "arca-format: hub 侧 JSON Lines 记录（format.json / index / items / journal）"
```

---

### Task 8: arca-chunk 切块、压缩与块存储路径

**Files:**
- Modify: `crates/arca-chunk/Cargo.toml`, `crates/arca-chunk/src/cdc.rs`, `crates/arca-chunk/src/compress.rs`, `crates/arca-chunk/src/lib.rs`
- Create: `crates/arca-chunk/src/store.rs`

**Interfaces:**
- Consumes: `ContentHash`
- Produces:
  - `cdc::{MIN_CHUNK, AVG_CHUNK, MAX_CHUNK, split(&[u8]) -> Vec<Chunk>}`，`Chunk { offset: usize, len: usize, hash: ContentHash }`。
  - `compress::{compress(&[u8]) -> Vec<u8>, decompress(&[u8]) -> Result<Vec<u8>, CompressError>, LEVEL}`。
  - `store::chunk_relative_path(&ContentHash) -> String`（返回 `chunks/<xx>/<hex>.zst`）。
- Task 9 依赖 `compress::decompress` 与 `store::chunk_relative_path`。

- [ ] **Step 1: 加依赖**

```bash
cargo add --package arca-chunk fastcdc zstd
```

- [ ] **Step 2: 写失败的测试**

`cdc.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn 可压缩数据(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 7 + i / 251) % 256) as u8).collect()
    }

    #[test]
    fn 切块覆盖全部字节且不重叠() {
        let data = 可压缩数据(1_000_000);
        let chunks = split(&data);
        assert!(!chunks.is_empty());
        let mut cursor = 0;
        for chunk in &chunks {
            assert_eq!(chunk.offset, cursor, "块必须首尾相接");
            cursor += chunk.len;
        }
        assert_eq!(cursor, data.len(), "块必须覆盖全部字节");
    }

    #[test]
    fn 块大小落在参数区间内() {
        let data = 可压缩数据(1_000_000);
        let chunks = split(&data);
        for chunk in chunks.iter().take(chunks.len() - 1) {
            assert!(chunk.len >= MIN_CHUNK, "非末块不得小于 min");
            assert!(chunk.len <= MAX_CHUNK, "块不得大于 max");
        }
    }

    #[test]
    fn 切块是确定性的() {
        let data = 可压缩数据(500_000);
        assert_eq!(split(&data), split(&data));
    }

    #[test]
    fn 中间插入只影响局部块() {
        // CDC 的核心价值：插入不应导致后续所有块边界移位
        let base = 可压缩数据(500_000);
        let mut modified = base.clone();
        modified.splice(250_000..250_000, [0xffu8; 64]);

        let a = split(&base);
        let b = split(&modified);
        let 共享 = a.iter().filter(|c| b.iter().any(|d| d.hash == c.hash)).count();
        assert!(共享 * 2 > a.len(), "多数块应保持不变，实得 {共享}/{}", a.len());
    }

    #[test]
    fn 空输入产生零个块() {
        assert!(split(b"").is_empty());
    }
}
```

`compress.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 压缩解压往返一致() {
        let data: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
        let packed = compress(&data);
        assert!(packed.len() < data.len(), "可压缩数据应变小");
        assert_eq!(decompress(&packed).unwrap(), data);
    }

    #[test]
    fn 空输入往返一致() {
        assert_eq!(decompress(&compress(b"")).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn 损坏输入返回错误而不是_panic() {
        assert!(decompress(b"not zstd at all").is_err());
        assert!(decompress(&[]).is_err());
    }
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cargo test -p arca-chunk`
Expected: 编译失败

- [ ] **Step 4: 实现 cdc.rs**

```rust
use crate::hash::ContentHash;

/// FastCDC 参数（FORMAT.md §8.1）。出处：FastCDC 论文（USENIX ATC'16）推荐区间；
/// avg 64 KiB 在去重率与块元数据开销之间取平衡。
pub const MIN_CHUNK: usize = 16 * 1024;
pub const AVG_CHUNK: usize = 64 * 1024;
pub const MAX_CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: usize,
    pub len: usize,
    pub hash: ContentHash,
}

/// 按 FastCDC 切块。块首尾相接、覆盖全部字节，结果对同一输入确定。
pub fn split(data: &[u8]) -> Vec<Chunk> {
    if data.is_empty() {
        return Vec::new();
    }
    fastcdc::v2020::FastCDC::new(data, MIN_CHUNK as u32, AVG_CHUNK as u32, MAX_CHUNK as u32)
        .map(|entry| Chunk {
            offset: entry.offset,
            len: entry.length,
            hash: ContentHash::from_bytes(&data[entry.offset..entry.offset + entry.length]),
        })
        .collect()
}
```

- [ ] **Step 5: 实现 compress.rs 与 store.rs**

`compress.rs`：

```rust
/// zstd 压缩级别。3 是 zstd 默认值——压缩比与 ARM NAS 的 CPU 成本平衡
/// （spec §1.1 目标 9：弱硬件友好）。
pub const LEVEL: i32 = 3;

#[derive(Debug)]
pub struct CompressError(String);

impl std::fmt::Display for CompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "zstd 解压失败：{}", self.0)
    }
}

impl std::error::Error for CompressError {}

pub fn compress(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, LEVEL).unwrap_or_else(|_| data.to_vec())
}

pub fn decompress(packed: &[u8]) -> Result<Vec<u8>, CompressError> {
    zstd::decode_all(packed).map_err(|e| CompressError(e.to_string()))
}
```

`store.rs`：

```rust
//! 内容寻址块存储的**路径计算**——纯函数，不做 IO（core 可嵌入纪律）。

use crate::hash::ContentHash;

/// 返回块相对于 `.arca/` 的路径：`chunks/<前两位十六进制>/<64 位十六进制>.zst`。
/// 两级分片避免单目录条目数过大（FORMAT.md §4）。
pub fn chunk_relative_path(hash: &ContentHash) -> String {
    let hex = hash.to_hex();
    format!("chunks/{}/{}.zst", &hex[..2], hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 块路径按前两位分片() {
        let hash = ContentHash::from_bytes(b"x");
        let path = chunk_relative_path(&hash);
        let hex = hash.to_hex();
        assert_eq!(path, format!("chunks/{}/{}.zst", &hex[..2], hex));
    }
}
```

在 `lib.rs` 模块列表加入 `pub mod store;`。

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo test -p arca-chunk`
Expected: 全部 PASS

- [ ] **Step 7: 提交**

```bash
git add crates/arca-chunk
git commit -m "arca-chunk: FastCDC 切块、zstd 压缩与块存储路径计算"
```

---

### Task 9: arcad fsck 存储根巡检

**Files:**
- Create: `crates/arcad/src/fsck.rs`
- Modify: `crates/arcad/src/main.rs`, `crates/arcad/src/gc.rs`（注释指向 fsck）, `crates/arcad/Cargo.toml`
- Test: `crates/arcad/tests/fsck.rs`

**Interfaces:**
- Consumes: `arca_format::{hub_layout, items, index, manifest, model}`、`arca_chunk::{hash, compress, store}`
- Produces: `arcad::fsck::{check_root(root: &Path) -> FsckReport, FsckReport { problems: Vec<Problem>, checked_files: usize, checked_chunks: usize }, Problem}`。`Problem` 变体：`MissingFormatJson`、`BadFormatJson(String)`、`MissingFile { path: String }`、`HashMismatch { path: String, expected: String, actual: String }`、`SizeMismatch { path: String, expected: u64, actual: u64 }`、`OrphanIndex { key: String }`、`BrokenChain { item: String, reason: String }`、`CorruptChunk { hash: String }`。

- [ ] **Step 1: 加依赖**

```bash
cargo add --package arcad clap --features derive
cargo add --package arcad --dev tempfile
```

- [ ] **Step 2: 写失败的测试**

创建 `crates/arcad/tests/fsck.rs`：

```rust
//! fsck 巡检的集成测试。构造真实的存储根目录，注入损坏，断言可诊断。

use arca_chunk::hash::ContentHash;
use arcad::fsck::{check_root, Problem};
use std::fs;
use std::path::Path;

/// 造一个最小但合法的存储根：一个文件、一条版本记录、一条索引记录。
fn 造一个健康的存储根(root: &Path) -> ContentHash {
    let content = b"hello arca";
    let hash = ContentHash::from_bytes(content);

    fs::create_dir_all(root.join("files")).unwrap();
    fs::write(root.join("files/note.txt"), content).unwrap();

    fs::create_dir_all(root.join(".arca/items/3f")).unwrap();
    fs::create_dir_all(root.join(".arca/index")).unwrap();
    fs::write(
        root.join(".arca/format.json"),
        r#"{"v":1,"format":1,"dataset_id":"9c41000000000000000000000000abcd","hash_algo":"blake3","created_at":"2026-08-04T10:00:00Z"}"#,
    ).unwrap();

    let item_line = format!(
        r#"{{"v":1,"version_id":"20260804T102302Z-{}","item_id":"3f2a000000000000000000000000beef","parent":null,"hash":"{}","size":{},"mtime":"2026-08-04T10:00:00Z","actor":{{"account":"","device":"","session":""}},"committed_at":"2026-08-04T10:00:00Z"}}"#,
        "0".repeat(32), hash.to_text(), content.len()
    );
    fs::write(root.join(".arca/items/3f/3f2a000000000000000000000000beef.jsonl"), format!("{item_line}\n")).unwrap();

    let key = arca_format::path_rules::index_key("note.txt");
    let shard = root.join(".arca/index").join(&key.to_hex()[..2]);
    fs::create_dir_all(&shard).unwrap();
    fs::write(
        shard.join(format!("{}.json", key.to_hex())),
        r#"{"v":1,"item_id":"3f2a000000000000000000000000beef","path":"note.txt"}"#,
    ).unwrap();

    hash
}

#[test]
fn 健康的存储根零问题() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    let report = check_root(dir.path());
    assert!(report.problems.is_empty(), "不应有问题，实得 {:?}", report.problems);
    assert_eq!(report.checked_files, 1);
}

#[test]
fn 检出内容被篡改() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    fs::write(dir.path().join("files/note.txt"), b"tampered!!").unwrap();

    let report = check_root(dir.path());
    assert!(
        report.problems.iter().any(|p| matches!(p, Problem::HashMismatch { .. })),
        "应检出哈希不匹配，实得 {:?}", report.problems
    );
}

#[test]
fn 检出当前版本文件缺失() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    fs::remove_file(dir.path().join("files/note.txt")).unwrap();

    let report = check_root(dir.path());
    assert!(report.problems.iter().any(|p| matches!(p, Problem::MissingFile { .. })));
}

#[test]
fn 缺少_format_json_时报告而不是崩溃() {
    let dir = tempfile::tempdir().unwrap();
    let report = check_root(dir.path());
    assert!(report.problems.iter().any(|p| matches!(p, Problem::MissingFormatJson)));
}

#[test]
fn fsck_绝不修改任何文件() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    fs::write(dir.path().join("files/note.txt"), b"tampered!!").unwrap();

    let 前 = fs::read(dir.path().join("files/note.txt")).unwrap();
    let _ = check_root(dir.path());
    let 后 = fs::read(dir.path().join("files/note.txt")).unwrap();
    assert_eq!(前, 后, "fsck 是只读诊断，绝无销毁权（I3）");
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cargo test -p arcad`
Expected: 编译失败，`arcad` 不是库 crate

- [ ] **Step 4: 把 arcad 改造成 lib + bin**

在 `crates/arcad/Cargo.toml` 加入：

```toml
[lib]
name = "arcad"
path = "src/lib.rs"

[[bin]]
name = "arcad"
path = "src/main.rs"
```

创建 `crates/arcad/src/lib.rs`：

```rust
//! arcad 的库形态——供 `arcad` 可执行文件与集成测试共用。
//! 服务端实现从 M2 开始；M0 只交付 fsck（spec §12.3）。

pub mod fsck;
```

（`main.rs` 保留自己的 `mod` 声明用于 M2 的模块；M0 阶段 `main.rs` 只 `use arcad::fsck`。）

- [ ] **Step 5: 实现 fsck.rs**

```rust
//! 存储根完整性巡检（spec §7、§4.5）。
//!
//! **只读**：fsck 报告问题，从不修复、从不删除（I3：同步路径无销毁权；
//! 修复动作属于显式命令）。发现悬空引用 → 停下报告，绝不猜测（I5）。

use arca_chunk::hash::ContentHash;
use arca_format::hub_layout::FormatJson;
use arca_format::{items, path_rules};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    MissingFormatJson,
    BadFormatJson(String),
    MissingFile { path: String },
    HashMismatch { path: String, expected: String, actual: String },
    SizeMismatch { path: String, expected: u64, actual: u64 },
    OrphanIndex { key: String },
    BrokenChain { item: String, reason: String },
    CorruptChunk { hash: String },
}

#[derive(Debug, Default)]
pub struct FsckReport {
    pub problems: Vec<Problem>,
    pub checked_files: usize,
    pub checked_chunks: usize,
}

pub fn check_root(root: &Path) -> FsckReport {
    let mut report = FsckReport::default();

    // 1. format.json 必须存在且可解析——这是卷身份标记（I11）
    let format_path = root.join(".arca/format.json");
    match fs::read_to_string(&format_path) {
        Err(_) => {
            report.problems.push(Problem::MissingFormatJson);
            return report; // 身份不明 → 停下，不做任何进一步推断（I5）
        }
        Ok(text) => {
            if let Err(e) = FormatJson::parse(&text) {
                report.problems.push(Problem::BadFormatJson(e.to_string()));
                return report;
            }
        }
    }

    // 2. 逐条 item：当前版本必须在 files/ 存在，且哈希与大小一致
    let items_dir = root.join(".arca/items");
    for shard in read_dir_sorted(&items_dir) {
        for item_file in read_dir_sorted(&shard) {
            let text = match fs::read_to_string(&item_file) {
                Ok(t) => t,
                Err(e) => {
                    report.problems.push(Problem::BrokenChain {
                        item: item_file.display().to_string(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let chain = match items::parse_chain(&text) {
                Ok(c) => c,
                Err(e) => {
                    report.problems.push(Problem::BrokenChain {
                        item: item_file.display().to_string(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let Some(current) = chain.last() else { continue };
            let logical = lookup_path(root, current.item_id.to_hex().as_str());
            let Some(logical) = logical else {
                report.problems.push(Problem::OrphanIndex { key: current.item_id.to_hex() });
                continue;
            };
            let physical = root.join("files").join(&logical);
            report.checked_files += 1;
            match fs::read(&physical) {
                Err(_) => report.problems.push(Problem::MissingFile { path: logical }),
                Ok(bytes) => {
                    if bytes.len() as u64 != current.size {
                        report.problems.push(Problem::SizeMismatch {
                            path: logical.clone(),
                            expected: current.size,
                            actual: bytes.len() as u64,
                        });
                    }
                    let actual = ContentHash::from_bytes(&bytes);
                    if actual != current.hash {
                        report.problems.push(Problem::HashMismatch {
                            path: logical,
                            expected: current.hash.to_text(),
                            actual: actual.to_text(),
                        });
                    }
                }
            }
        }
    }

    // 3. 块存储：每个块解压后哈希必须与文件名一致
    let chunks_dir = root.join(".arca/chunks");
    for shard in read_dir_sorted(&chunks_dir) {
        for chunk_file in read_dir_sorted(&shard) {
            report.checked_chunks += 1;
            let name = chunk_file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let ok = fs::read(&chunk_file)
                .ok()
                .and_then(|packed| arca_chunk::compress::decompress(&packed).ok())
                .map(|raw| ContentHash::from_bytes(&raw).to_hex() == name)
                .unwrap_or(false);
            if !ok {
                report.problems.push(Problem::CorruptChunk { hash: name });
            }
        }
    }

    report
}

/// 反查 item_id 对应的逻辑路径：遍历 index/ 记录。
/// M0 用线性扫描（存储根规模有限）；M2 随 storage.rs 换成内存索引。
fn lookup_path(root: &Path, item_id_hex: &str) -> Option<String> {
    for shard in read_dir_sorted(&root.join(".arca/index")) {
        for record in read_dir_sorted(&shard) {
            let Ok(text) = fs::read_to_string(&record) else { continue };
            let Ok(parsed) = arca_format::index::IndexRecord::parse(&text) else { continue };
            if parsed.item_id.to_hex() == item_id_hex {
                // 路径必须合规，否则视为损坏记录而非可用映射
                return path_rules::check(&parsed.path).ok();
            }
        }
    }
    None
}

/// 排序读目录：使 fsck 的输出确定（同一状态必产生同一报告）。
fn read_dir_sorted(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    paths
}
```

- [ ] **Step 6: 接上 main.rs 的 fsck 子命令**

替换 `crates/arcad/src/main.rs` 的 `fn main()`（保留文件顶部 doc comment 与 M2 的 `mod` 声明，但把它们标注为 M2 未启用）：

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "arcad", about = "arca 服务端 daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 巡检一个存储根的完整性（只读，绝不修改任何文件）
    Fsck {
        /// 存储根路径（含 files/ 与 .arca/）
        root: std::path::PathBuf,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Fsck { root } => {
            let report = arcad::fsck::check_root(&root);
            // Rule of Silence：数据走 stdout，诊断走 stderr（spec §3.2）
            for problem in &report.problems {
                eprintln!("{problem:?}");
            }
            println!(
                "检查 {} 个文件、{} 个块，发现 {} 个问题",
                report.checked_files, report.checked_chunks, report.problems.len()
            );
            if report.problems.is_empty() {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::from(1)
            }
        }
    }
}
```

- [ ] **Step 7: 运行测试确认通过**

Run: `cargo test -p arcad`
Expected: 5 个测试 PASS

- [ ] **Step 8: 提交**

```bash
git add crates/arcad
git commit -m "arcad: fsck 存储根巡检（只读诊断，M0 唯一的 IO 消费者）"
```

---

### Task 10: 逃生舱恢复演示

**Files:**
- Create: `crates/arca-conformance/tests/escape-hatch/recover.sh`, `crates/arca-conformance/tests/escape-hatch/make-fixture.sh`
- Modify: `crates/arca-conformance/tests/escape-hatch/README.md`

**Interfaces:**
- Consumes: `arcad fsck`（仅用于造夹具的对照，不参与恢复）
- Produces: 可在 CI 中执行的 `recover.sh <dataset_root> <dest>`，退出码 0 表示恢复且校验通过。

I1 的承诺是**删掉 arca，数据零成本可用**。本任务把它变成每晚跑的可执行断言。

- [ ] **Step 1: 写 recover.sh**

```bash
#!/bin/sh
# 逃生舱恢复演示（I1）——不含任何 arca 代码。
#
# 用法：recover.sh <dataset_root> <dest>
#
# 依赖：POSIX shell + coreutils + b3sum。
# b3sum 是 BLAKE3 的官方 CLI，不属于 coreutils——I1 的承诺是
# 「不需要任何 arca 代码」，而非「只用 coreutils」（FORMAT.md §10 已明示）。
set -eu

root=${1:?用法: recover.sh <dataset_root> <dest>}
dest=${2:?用法: recover.sh <dataset_root> <dest>}

test -d "$root/files" || { echo "缺少 $root/files" >&2; exit 2; }

# 1. 恢复就是一次普通拷贝——这正是 I1 的全部含义
mkdir -p "$dest"
cp -R "$root/files/." "$dest/"

# 2. 用 items 记录校验：每条当前版本的哈希与大小
#    （items 是 JSON Lines，用 sed 取字段，不引入 jq 依赖）
问题=0
文件数=0
for item in "$root"/.arca/items/*/*.jsonl; do
    test -e "$item" || continue
    # 取最后一个完整行 = 当前版本
    行=$(grep '^{' "$item" | tail -n 1) || continue
    test -n "$行" || continue

    哈希=$(printf '%s' "$行" | sed -n 's/.*"hash":"blake3:\([0-9a-f]\{64\}\)".*/\1/p')
    大小=$(printf '%s' "$行" | sed -n 's/.*"size":\([0-9]*\).*/\1/p')
    条目=$(printf '%s' "$行" | sed -n 's/.*"item_id":"\([0-9a-f]\{32\}\)".*/\1/p')

    # 从 index 反查逻辑路径
    路径=$(grep -l "\"item_id\":\"$条目\"" "$root"/.arca/index/*/*.json 2>/dev/null | head -n 1)
    test -n "$路径" || { echo "item $条目 无索引记录" >&2; 问题=$((问题+1)); continue; }
    逻辑=$(sed -n 's/.*"path":"\([^"]*\)".*/\1/p' "$路径")

    目标="$dest/$逻辑"
    test -f "$目标" || { echo "缺少文件: $逻辑" >&2; 问题=$((问题+1)); continue; }

    实际大小=$(wc -c < "$目标" | tr -d ' ')
    test "$实际大小" = "$大小" || { echo "大小不符: $逻辑 ($实际大小 != $大小)" >&2; 问题=$((问题+1)); continue; }

    实际哈希=$(b3sum --no-names "$目标")
    test "$实际哈希" = "$哈希" || { echo "哈希不符: $逻辑" >&2; 问题=$((问题+1)); continue; }

    文件数=$((文件数+1))
done

echo "恢复并校验 $文件数 个文件，$问题 个问题"
test "$问题" -eq 0
```

- [ ] **Step 2: 写 make-fixture.sh（造测试夹具）**

用 shell 造一个与 Task 9 集成测试同构的最小存储根：`files/note.txt` + `format.json` + 一条 items 记录 + 一条 index 记录。哈希用 `b3sum --no-names` 现算，索引键用 `printf '%s' "note.txt" | b3sum --no-names` 计算（小写规范化路径的 BLAKE3，与 `path_rules::index_key` 一致）。

- [ ] **Step 3: 本地跑通**

```bash
chmod +x crates/arca-conformance/tests/escape-hatch/*.sh
./crates/arca-conformance/tests/escape-hatch/make-fixture.sh /tmp/arca-fixture
./crates/arca-conformance/tests/escape-hatch/recover.sh /tmp/arca-fixture /tmp/arca-recovered
```

Expected: `恢复并校验 1 个文件，0 个问题`，退出码 0

- [ ] **Step 4: 注入损坏，确认演示会失败**

```bash
echo tampered > /tmp/arca-recovered/note.txt
./crates/arca-conformance/tests/escape-hatch/recover.sh /tmp/arca-fixture /tmp/arca-recovered2
# 改为直接篡改夹具后重跑，断言退出码非 0
```

Expected: 篡改后退出码为 1，stderr 打印「哈希不符」

- [ ] **Step 5: 更新 README.md**

把 TODO 换成实际用法、依赖说明（含 b3sum 的诚实注解）与 CI 中的调用方式。

- [ ] **Step 6: 提交**

```bash
git add crates/arca-conformance
git commit -m "conformance: 逃生舱恢复演示（不含任何 arca 代码，I1 变为可执行断言）"
```

---

### Task 11: fuzz 目标与 CI

**Files:**
- Create: `fuzz/Cargo.toml`, `fuzz/fuzz_targets/manifest.rs`, `fuzz/fuzz_targets/items.rs`, `fuzz/fuzz_targets/registry.rs`, `fuzz/fuzz_targets/path_rules.rs`, `.github/workflows/ci.yml`
- Modify: `.gitignore`（加 `fuzz/target`、`fuzz/corpus`、`fuzz/artifacts`）

**Interfaces:**
- Consumes: `arca_format` 的全部 `parse` 入口
- Produces: CI 绿灯是后续所有任务的前提

- [ ] **Step 1: 初始化 fuzz 工程**

```bash
cargo install cargo-fuzz   # 若未安装
cargo fuzz init            # 在仓库根执行；会创建 fuzz/ 且不加入 workspace
```

- [ ] **Step 2: 写四个 fuzz target**

`fuzz/fuzz_targets/manifest.rs`：

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;

// I5：任意字节输入 → 明确错误，绝不 panic。
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arca_format::manifest::Manifest::parse(text);
    }
});
```

`items.rs` 对 `arca_format::items::parse_chain`、`registry.rs` 对 `arca_format::gitarca::Registry::parse`、`path_rules.rs` 对 `arca_format::path_rules::check` 同构。

- [ ] **Step 3: 各跑 60 秒确认无 panic**

```bash
cargo fuzz run manifest -- -max_total_time=60
cargo fuzz run items -- -max_total_time=60
cargo fuzz run registry -- -max_total_time=60
cargo fuzz run path_rules -- -max_total_time=60
```

Expected: 四个都跑满 60 秒无崩溃；有崩溃则**先修解析器再继续**（这正是 fuzz 的价值）

- [ ] **Step 4: 写 CI**

`.github/workflows/ci.yml`：

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:
  schedule:
    - cron: "0 18 * * *"   # 每晚跑逃生舱演示（UTC 18:00）

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace

  escape-hatch:
    name: 逃生舱恢复演示（I1）
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: 安装 b3sum
        run: cargo install b3sum
      - name: 造夹具并恢复
        run: |
          chmod +x crates/arca-conformance/tests/escape-hatch/*.sh
          ./crates/arca-conformance/tests/escape-hatch/make-fixture.sh /tmp/fixture
          ./crates/arca-conformance/tests/escape-hatch/recover.sh /tmp/fixture /tmp/recovered
      - name: 断言篡改会被检出
        run: |
          printf 'tampered' > /tmp/fixture/files/note.txt
          if ./crates/arca-conformance/tests/escape-hatch/recover.sh /tmp/fixture /tmp/recovered2; then
            echo "篡改未被检出——逃生舱校验失效" >&2
            exit 1
          fi
```

- [ ] **Step 5: 本地验证 CI 的每条命令都能过**

```bash
rustup component add rustfmt
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: 全部通过。`cargo fmt --check` 若报格式差异，先运行 `cargo fmt --all` 再提交。

- [ ] **Step 6: 提交并推送**

```bash
git add fuzz .github .gitignore
git commit -m "fuzz 目标与 CI：解析器 fuzz、clippy 零警告、每晚逃生舱演示"
git push origin main
```

---

## Self-Review

**1. Spec 覆盖检查（对 M0 验收标准，spec §12.3）：**

| M0 要求 | 对应任务 |
| --- | --- |
| FORMAT.md v1 | Task 1 |
| arca-format | Task 3–7 |
| arca-chunk | Task 2、8 |
| fsck | Task 9 |
| coreutils 恢复演示进 CI | Task 10、11 |
| fuzz 无 panic | Task 11（60 秒门禁；72 小时长跑另行排期，见下） |
| golden vectors 就绪 | Task 5（manifest）；items/registry 的 golden 随 Task 7 的往返测试覆盖 |

**已知缺口（有意留给后续里程碑，不在本计划内）：**
- **72 小时 fuzz** 需要长驻 runner 或 OSS-Fuzz 接入，CI 里先跑 60 秒作为回归门禁；长跑排到 M1 期间以定时任务补上。
- `trash/`、`uploads/`、`locks/` 的格式与 chunks 引用计数属 M2（FORMAT.md §10 已明示）。
- `arca-core` 的对账状态机属 M1–M2，本计划不触碰。

**2. Placeholder 扫描：** 已消除，无 TBD / TODO / 「稍后填写」。

**2b. 执行前预检（2026-08-04）：** 修正了两处会让实现者浪费一轮的缺陷——
18 个测试函数名含空格（Rust 标识符非法，已改为下划线；CJK 标识符本身经实测可通过
`clippy -D warnings`），以及 `Registry::validate` 中的 `Box::leak` 内存泄漏写法（已改为 `Vec<String>`）。

**3. 类型一致性检查：**
- `ContentHash` 的方法名在 Task 2 定义（`from_bytes`/`hasher`/`to_hex`/`to_text`/`parse`/`as_bytes`），Task 3、5、7、8、9 的引用一致。
- `FormatError::Malformed { line, reason }` 的字段名在 Task 4 定义，Task 5、6、7 一致使用。
- `ItemId::parse`/`to_hex`/`shard` 在 Task 4 定义，Task 7、9 一致。
- `items::parse_chain` 在 Task 7 定义，Task 9 使用；`compress::decompress` 在 Task 8 定义，Task 9 使用。
- `path_rules::index_key` 在 Task 3 定义，Task 9 与 Task 10 的 shell 脚本使用同一算法（小写规范化路径的 BLAKE3）。
