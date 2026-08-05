# M1a 存储根 IO 地基 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 `arca-store` 补上打开存储根、校验卷身份（I11）与原子提交（tmp→fsync→rename）两块地基，让 M1 后续的 `file://` 同步、`adopt`、`verify` 都有一个「身份明确、写入不会撕裂」的底座。

**Architecture:** `StorageRoot::open` 是所有存储根访问的唯一入口——它把「根不存在 / 身份不符 / 正常」三态显式化，并发一条 `mount.check` trace 事件；`atomic` 提供唯一的写入路径。既有的 `fsck` 改为经由 `StorageRoot` 打开，不再自己读 `format.json`。

**Tech Stack:** Rust 2021 / MSRV 1.85 · arca-format（格式解析 + trace）· arca-chunk · tempfile（dev）

---

## 为什么这是 M1 的第一块

M0 的最终评审把它列为 M1 开工前必须闭合的项：`arca-store` 目前只有 `fsck`，
`atomic` 与存储根身份校验都还是 `TODO(M1)`。也就是说 **I11 的挂载检查至今没有实现**，
而 trace 的 `mount.check` 事件与 `PROTOCOL.md` 的 `mount.identity_mismatch` / `mount.absent`
错误码已经定义好了——golden trace 里那条 `mount.check` 现在还是空头支票。

I11 要防的事故形态很具体：外置盘被拔掉、NFS 掉线、挂载点漂移之后，
**未挂载的卷与空库在字节上难以区分，语义上却天差地别**。把前者当后者，
同步引擎会认为「远端把文件全删了」，于是触发删除对账，把用户本地的数据清掉。
M0 的逃生舱脚本已经踩过这个坑的浅水区（空 `files/` 被报成干净恢复），
这次是把它在真正的数据路径上堵死。

## Global Constraints

- MSRV **1.85**，edition 2021。`Cargo.lock` 已入库；加依赖用 `cargo add`，
  若某依赖要求高于 1.85 **报告而非降级钉版**。
- `arca-store` 保持 `#![forbid(unsafe_code)]`，不得引入 tokio 或任何异步运行时。
- 只在 `main` 分支工作。提交信息用中文。
- 文档与注释一律中文；各文件顶部已有的中文 doc comment 必须保留。
- 四项门禁每个任务结束都要绿：`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo +1.85 check --workspace --locked --all-targets`、`cargo fmt --all -- --check`。
- **绝不猜测（I5）**：状态模糊必须停下并可诊断，不得尽力恢复。
- **同步路径无销毁权（I3）**：本 crate 不得出现任何删除用户数据的代码路径。
  唯一允许的删除是 `.arca/tmp/` 下的孤儿**普通文件**（见 Task 3），且规则严格。
- 格式契约是 `FORMAT.md`（§4 布局、§5 format.json、§10 trace）。**不修改它**。

## 已交付、可直接使用的接口

```rust
// arca-format
arca_format::hub_layout::FormatJson { format: u32, dataset_id: String,
                                      hash_algo: String, created_at: String }
FormatJson::parse(&str) -> Result<FormatJson, FormatError>
arca_format::hub_layout::layout::{FILES_DIR, ARCA_DIR, FORMAT_JSON, INDEX_DIR, ITEMS_DIR,
                                  CHUNKS_DIR, JOURNAL_DIR, EPOCH_FILE, TRASH_DIR,
                                  UPLOADS_DIR, TMP_DIR, LOCKS_DIR}
arca_format::error::FormatError    // UnsupportedVersion / Malformed{line,reason} / BadPath / BadHash / Io

// arca-format::trace
trace::{TraceSink, TraceRecord, EventKind, Sid, NullSink, VecSink, RingSink}
EventKind::MountCheck                       // 本任务要发的事件
TraceRecord::new(EventKind, t_abs_us: u64) -> TraceRecord
TraceRecord::with(self, key: &'static str, value: impl Into<FieldValue>) -> TraceRecord
TraceSink                                   // trait；有 &mut T 的 blanket impl
VecSink::new() / .records() / .kinds()      // 测试里断言决策序列
```

`arca-store` 已依赖 `arca-format` 与 `arca-chunk`，dev 依赖有 `tempfile`。

---

## File Structure

| 文件 | 职责 |
| --- | --- |
| `crates/arca-store/src/root.rs`（新建） | `StorageRoot` 与 `MountStatus`：打开、身份校验、`mount.check` 发射 |
| `crates/arca-store/src/atomic.rs`（新建） | tmp → fsync → rename 原子写入；目录 fsync |
| `crates/arca-store/src/lib.rs`（改） | 注册两个新模块，删掉对应 TODO |
| `crates/arca-store/src/fsck.rs`（改） | 改为经 `StorageRoot` 打开，不再自己读 `format.json` |
| `crates/arca-store/tests/mount.rs`（新建） | I11 场景矩阵集成测试 |
| `crates/arca-store/tests/atomic.rs`（新建） | 原子写入与崩溃残留的集成测试 |

---

### Task 1: StorageRoot 打开与卷身份校验（I11）

**Files:**
- Create: `crates/arca-store/src/root.rs`
- Modify: `crates/arca-store/src/lib.rs`
- Test: `crates/arca-store/tests/mount.rs`

**Interfaces:**
- Consumes: `FormatJson`、`layout::*`、`FormatError`
- Produces:
  - `arca_store::root::MountError`，变体：`Absent { path: String }`、
    `IdentityMismatch { expected: String, found: String }`、`Malformed(FormatError)`、
    `Io { path: String, reason: String }`。实现 `Display`（中文）+ `std::error::Error`。
  - `arca_store::root::StorageRoot`：`open(root: &Path, expected_dataset_id: Option<&str>) -> Result<StorageRoot, MountError>`；
    `path(&self) -> &Path`；`format(&self) -> &FormatJson`；`dataset_id(&self) -> &str`；
    `join(&self, relative: &str) -> Result<PathBuf, RootEscape>`。

> **`join` 为什么返回 `Result`**（评审后修订）：`Path::join` 遇到绝对路径会把根整个丢掉——
> `root.join("/etc/passwd")` 返回的就是 `/etc/passwd`。`StorageRoot` 存在的意义正是
> 「持有它就不必在每个调用点重新推导根的安全性」，所以拒绝绝对路径、`..` 组件与盘符前缀
> 是这个类型的职责，不是调用方的。Task 3 的写入路径建在它之上。
>
> `MountError` 相应新增 `BadExpectedId`，`Malformed` 改为 `{ path, source }` 带上路径。
- Task 2、3、4 依赖 `StorageRoot::open` 与 `join`。

- [ ] **Step 1: 写失败的测试**

创建 `crates/arca-store/tests/mount.rs`：

```rust
//! I11 场景矩阵：未挂载的卷绝不能被当成空库。
//!
//! 这些不是形式测试——把「根不存在」当成「库是空的」，同步引擎会认为远端删光了文件，
//! 于是触发删除对账清掉用户本地数据。每一条都对应一种真实的挂载故障。

use arca_store::root::{MountError, StorageRoot};
use std::fs;
use std::path::Path;

const 样例_ID: &str = "9c41000000000000000000000000abcd";

fn 造存储根(root: &Path, dataset_id: &str) {
    fs::create_dir_all(root.join(".arca")).unwrap();
    fs::create_dir_all(root.join("files")).unwrap();
    fs::write(
        root.join(".arca/format.json"),
        format!(
            r#"{{"v":1,"format":1,"dataset_id":"{dataset_id}","hash_algo":"blake3","created_at":"2026-08-05T10:00:00Z"}}"#
        ),
    )
    .unwrap();
}

#[test]
fn 健康的存储根可以打开() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let root = StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    assert_eq!(root.dataset_id(), 样例_ID);
    assert_eq!(root.join("files").file_name().unwrap(), "files");
}

#[test]
fn 不指定期望身份时也能打开() {
    // fsck 这类只读巡检不一定知道期望的 dataset_id
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    assert!(StorageRoot::open(dir.path(), None).is_ok());
}

#[test]
fn 根目录整个不存在时报_absent_而不是空库() {
    let dir = tempfile::tempdir().unwrap();
    let 不存在 = dir.path().join("从未挂载");
    match StorageRoot::open(&不存在, Some(样例_ID)) {
        Err(MountError::Absent { .. }) => {}
        other => panic!("必须报 Absent，实得 {other:?}"),
    }
}

#[test]
fn 根存在但_format_json_缺失时报_absent() {
    // 这正是「挂载点下面有个本地建的空壳目录」的形态
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("files")).unwrap();
    match StorageRoot::open(dir.path(), Some(样例_ID)) {
        Err(MountError::Absent { .. }) => {}
        other => panic!("必须报 Absent，实得 {other:?}"),
    }
}

#[test]
fn 身份不符时报_identity_mismatch_并带上两侧的值() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), "1111111111111111111111111111aaaa");
    match StorageRoot::open(dir.path(), Some(样例_ID)) {
        Err(MountError::IdentityMismatch { expected, found }) => {
            assert_eq!(expected, 样例_ID);
            assert_eq!(found, "1111111111111111111111111111aaaa");
        }
        other => panic!("必须报 IdentityMismatch，实得 {other:?}"),
    }
}

#[test]
fn format_json_损坏时报_malformed_而不是_absent() {
    // 「读不出身份」与「没有身份」是不同的故障，不可混为一谈（I5）
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".arca")).unwrap();
    fs::write(dir.path().join(".arca/format.json"), "{ 这不是 JSON").unwrap();
    match StorageRoot::open(dir.path(), Some(样例_ID)) {
        Err(MountError::Malformed(_)) => {}
        other => panic!("必须报 Malformed，实得 {other:?}"),
    }
}

#[test]
fn 打开是只读的绝不创建任何东西() {
    // I3：本 crate 无销毁权，也不该在探测时留下副作用
    let dir = tempfile::tempdir().unwrap();
    let 空 = dir.path().join("空目录");
    fs::create_dir(&空).unwrap();
    let _ = StorageRoot::open(&空, Some(样例_ID));
    let 条目数 = fs::read_dir(&空).unwrap().count();
    assert_eq!(条目数, 0, "打开失败的探测不得创建任何文件或目录");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p arca-store --test mount`
Expected: 编译失败，`arca_store::root` 模块不存在

- [ ] **Step 3: 实现 root.rs**

```rust
//! 存储根的打开与卷身份校验（I11）。
//!
//! **为什么这是一个显式的三态而不是 `Option`**：未挂载的卷与空库在字节上难以区分，
//! 语义上却天差地别。把前者当后者，同步引擎会认为远端删光了文件，
//! 于是触发删除对账，清掉用户本地的数据（spec §4.6、I11）。
//! 所以「根不存在」「身份不符」「身份读不出来」必须是三种彼此可区分的失败，
//! 而不是统一折叠成「没有数据」。
//!
//! 格式契约见 `FORMAT.md` §4（布局）与 §5（format.json）。

use arca_format::error::FormatError;
use arca_format::hub_layout::{layout, FormatJson};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// 打开存储根时的失败。四种原因必须彼此可区分（I5：如实报告失败的性质）。
#[derive(Debug)]
pub enum MountError {
    /// 根目录不存在，或存在但没有 `.arca/format.json`——卷未挂载、路径写错、
    /// 或挂载点下面是个本地建的空壳目录。**绝不能当成「库是空的」**。
    Absent { path: String },
    /// 身份标记存在但与期望不符——挂到了别的数据集上（spec §4.6 的防误绑）。
    IdentityMismatch { expected: String, found: String },
    /// 身份标记存在但读不出来。与 `Absent` 是不同的故障。
    Malformed(FormatError),
    /// 读取失败（权限、IO 错误）。与「不存在」是不同的故障。
    Io { path: String, reason: String },
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MountError::Absent { path } => write!(
                f,
                "存储根 {path} 缺少 {}——卷未挂载、路径错误，或这不是一个 arca 存储根（绝不视为空库，I11）",
                layout::FORMAT_JSON
            ),
            MountError::IdentityMismatch { expected, found } => write!(
                f,
                "卷身份不符：期望 dataset_id {expected}，实际是 {found}——挂到了别的数据集上"
            ),
            MountError::Malformed(e) => write!(f, "存储根身份标记无法解析：{e}"),
            MountError::Io { path, reason } => write!(f, "读取 {path} 失败：{reason}"),
        }
    }
}

impl std::error::Error for MountError {}

/// 一个已打开、身份已确认的存储根。
///
/// 持有它即代表「这个根存在、是 arca 存储根、且身份与期望一致」——
/// 后续的读写不必再重复这些判断。
#[derive(Debug)]
pub struct StorageRoot {
    path: PathBuf,
    format: FormatJson,
}

impl StorageRoot {
    /// 打开存储根并校验身份。
    ///
    /// `expected_dataset_id` 为 `None` 时跳过身份比对——`fsck` 这类只读巡检
    /// 不一定知道期望值。为 `Some` 时不符即失败（I11）。
    ///
    /// **只读**：无论成功失败都不创建任何文件或目录。
    pub fn open(root: &Path, expected_dataset_id: Option<&str>) -> Result<Self, MountError> {
        let format_path = root.join(layout::FORMAT_JSON);
        let text = match fs::read_to_string(&format_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MountError::Absent {
                    path: root.display().to_string(),
                })
            }
            Err(e) => {
                return Err(MountError::Io {
                    path: format_path.display().to_string(),
                    reason: e.to_string(),
                })
            }
        };

        let format = FormatJson::parse(&text).map_err(MountError::Malformed)?;

        if let Some(expected) = expected_dataset_id {
            if expected != format.dataset_id {
                return Err(MountError::IdentityMismatch {
                    expected: expected.to_string(),
                    found: format.dataset_id.clone(),
                });
            }
        }

        Ok(StorageRoot {
            path: root.to_path_buf(),
            format,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> &FormatJson {
        &self.format
    }

    pub fn dataset_id(&self) -> &str {
        &self.format.dataset_id
    }

    /// 拼接存储根内的相对路径。传 `layout::` 里的常量，不要手写字面量。
    pub fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}
```

在 `lib.rs` 加 `pub mod root;`（保持字母序：`atomic`（Task 2 加）, `fsck`, `root`），
并删掉 `// TODO(M1)：pub mod layout;` 那一行（本模块就是它，名字改为 `root` 以免与
`arca_format::hub_layout::layout` 混淆——在提交信息里说明这个改名）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p arca-store --test mount`
Expected: 7 个测试全部 PASS

- [ ] **Step 5: 四项门禁 + 提交**

```bash
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check && cargo +1.85 check --workspace --locked --all-targets
git add crates/arca-store && git commit -m "arca-store: StorageRoot 打开与卷身份校验（I11 三态可区分）"
```

---

### Task 2: mount.check trace 事件

**Files:**
- Modify: `crates/arca-store/src/root.rs`
- Test: `crates/arca-store/tests/mount.rs`（追加）

**Interfaces:**
- Consumes: `trace::{TraceSink, TraceRecord, EventKind, VecSink}`
- Produces: `StorageRoot::open_traced(root: &Path, expected_dataset_id: Option<&str>, t_abs_us: u64, sink: &mut dyn TraceSink) -> Result<StorageRoot, MountError>`。
  `open` 保留为 `open_traced(..., &mut NullSink)` 的薄壳。
- Task 4 用它把 fsck 的挂载检查接进 trace。

> **为什么 `t_abs_us` 由调用方注入**：`arca-store` 做 IO，但时钟仍然注入——
> spec §11.2 的确定性模拟测试要能重放，而挂载检查是崩溃注入测试要覆盖的场景之一。
> 这与 §3.3 里「core 的 `t_abs` 取自调用方注入的时钟」是同一条纪律。

- [ ] **Step 1: 写失败的测试**

追加到 `crates/arca-store/tests/mount.rs`：

```rust
use arca_format::trace::{EventKind, VecSink};

#[test]
fn 成功打开会发一条_mount_check_且_ok_为真() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let mut sink = VecSink::new();
    StorageRoot::open_traced(dir.path(), Some(样例_ID), 1_000, &mut sink).unwrap();

    let 记录 = sink.records();
    assert_eq!(记录.len(), 1, "应恰好发一条事件");
    assert_eq!(记录[0].event, EventKind::MountCheck);
    assert_eq!(
        记录[0].field("ok").map(|v| v.to_string()),
        Some("true".to_string())
    );
    assert_eq!(
        记录[0].field("found").map(|v| v.to_string()),
        Some(样例_ID.to_string())
    );
}

#[test]
fn 身份不符也会发_mount_check_且带上两侧的值() {
    // 失败路径的 trace 比成功路径更重要——它是事故现场的线索
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), "1111111111111111111111111111aaaa");
    let mut sink = VecSink::new();
    let 结果 = StorageRoot::open_traced(dir.path(), Some(样例_ID), 2_000, &mut sink);
    assert!(结果.is_err());

    let 记录 = sink.records();
    assert_eq!(记录[0].event, EventKind::MountCheck);
    assert_eq!(
        记录[0].field("ok").map(|v| v.to_string()),
        Some("false".to_string())
    );
    assert_eq!(
        记录[0].field("expect").map(|v| v.to_string()),
        Some(样例_ID.to_string())
    );
    assert_eq!(
        记录[0].field("found").map(|v| v.to_string()),
        Some("1111111111111111111111111111aaaa".to_string())
    );
}

#[test]
fn 根缺失时的_mount_check_的_found_为空() {
    let dir = tempfile::tempdir().unwrap();
    let mut sink = VecSink::new();
    let _ = StorageRoot::open_traced(&dir.path().join("从未挂载"), Some(样例_ID), 3_000, &mut sink);

    let 记录 = sink.records();
    assert_eq!(记录[0].event, EventKind::MountCheck);
    assert_eq!(
        记录[0].field("ok").map(|v| v.to_string()),
        Some("false".to_string())
    );
    assert_eq!(记录[0].field("found").map(|v| v.to_string()), Some(String::new()));
}

#[test]
fn open_不发任何事件() {
    // Rule of Silence 的对应物：不注入 sink 就不该有开销
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let mut sink = VecSink::new();
    StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    assert!(sink.records().is_empty());
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p arca-store --test mount`
Expected: 编译失败，`open_traced` 未定义

- [ ] **Step 3: 实现**

把 `open` 的实现体移进 `open_traced`，在**每一条返回路径之前**发一条 `mount.check`。
字段按 trace 设计文档 §4.3 的事件表：`dataset_id`、`expect`、`found`、`ok`。
`found` 在根缺失或解析失败时为空字符串（不是省略——agent 对字段做精确匹配，
缺字段与空值是两回事）。`open` 改为：

```rust
    pub fn open(root: &Path, expected_dataset_id: Option<&str>) -> Result<Self, MountError> {
        let mut sink = arca_format::trace::NullSink;
        Self::open_traced(root, expected_dataset_id, 0, &mut sink)
    }
```

**注意**：`TraceRecord::with` 接 `&'static str` 的键与 `impl Into<FieldValue>` 的值。
先确认 `FieldValue` 是否有 `bool` 与 `&str` 的 `From` 实现——没有的话按实际可用的类型写
（例如 `ok` 用字符串 `"true"`/`"false"`，或先给 `FieldValue` 补 `From<bool>`；
**若要改 `arca-format`，先停下告诉控制方**，那属于另一个 crate 的改动范围）。

- [ ] **Step 4: 运行测试确认通过 + 门禁 + 提交**

Run: `cargo test -p arca-store` 全绿，四项门禁全绿

```bash
git add crates/arca-store && git commit -m "arca-store: 挂载检查发射 mount.check trace 事件（失败路径同样发）"
```

---

### Task 3: 原子写入（tmp → fsync → rename）

**Files:**
- Create: `crates/arca-store/src/atomic.rs`
- Modify: `crates/arca-store/src/lib.rs`
- Test: `crates/arca-store/tests/atomic.rs`

**Interfaces:**
- Consumes: `StorageRoot`、`layout::TMP_DIR`
- Produces:
  - `arca_store::atomic::AtomicError`：`Io { path: String, reason: String }`、
    `CrossDevice { tmp: String, target: String }`。实现 `Display`（中文）+ `Error`。
  - `atomic::write(root: &StorageRoot, relative_target: &str, bytes: &[u8]) -> Result<(), AtomicError>`
  - `atomic::sync_dir(dir: &Path) -> Result<(), AtomicError>`
- Task 4 依赖 `write`。

> **为什么必须 fsync 父目录**：`rename` 让新内容在目录项里可见，但目录项本身在
> 崩溃后可能还没落盘。只 fsync 文件不 fsync 目录，崩溃后可能出现「文件内容是新的、
> 但目录项还指向旧的」或「目录项指向一个尚不存在的 inode」。这是 lazync
> STORAGE.md「所有目录必须位于同一文件系统，rename 才是原子的」那条纪律的另一半。

- [ ] **Step 1: 写失败的测试**

创建 `crates/arca-store/tests/atomic.rs`：

```rust
//! 原子写入：崩溃后要么看到旧内容、要么看到新内容，绝不看到半截。

use arca_store::atomic;
use arca_store::root::StorageRoot;
use std::fs;
use std::path::Path;

const 样例_ID: &str = "9c41000000000000000000000000abcd";

fn 造存储根(root: &Path) {
    fs::create_dir_all(root.join(".arca/tmp")).unwrap();
    fs::create_dir_all(root.join("files")).unwrap();
    fs::write(
        root.join(".arca/format.json"),
        format!(
            r#"{{"v":1,"format":1,"dataset_id":"{样例_ID}","hash_algo":"blake3","created_at":"2026-08-05T10:00:00Z"}}"#
        ),
    )
    .unwrap();
}

#[test]
fn 写入新文件() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/note.txt", b"hello arca").unwrap();
    assert_eq!(fs::read(dir.path().join("files/note.txt")).unwrap(), b"hello arca");
}

#[test]
fn 覆盖既有文件是原子替换() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/note.txt", b"old").unwrap();
    atomic::write(&root, "files/note.txt", b"new content").unwrap();
    assert_eq!(fs::read(dir.path().join("files/note.txt")).unwrap(), b"new content");
}

#[test]
fn 写入后_tmp_目录不残留() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/note.txt", b"x").unwrap();
    let 残留 = fs::read_dir(dir.path().join(".arca/tmp")).unwrap().count();
    assert_eq!(残留, 0, "成功写入后不得在 tmp 留下临时文件");
}

#[test]
fn 自动创建目标的父目录() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/京都/鸭川.png", b"png bytes").unwrap();
    assert!(dir.path().join("files/京都/鸭川.png").exists());
}

#[test]
fn 空内容也能写() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/empty", b"").unwrap();
    assert_eq!(fs::read(dir.path().join("files/empty")).unwrap(), Vec::<u8>::new());
}

#[test]
fn 并发写同一路径最终得到其中一个完整版本() {
    // 不测「哪一个赢」——测的是绝不会出现半截内容
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());

    std::thread::scope(|s| {
        for i in 0..8 {
            let 根路径 = dir.path().to_path_buf();
            s.spawn(move || {
                let root = StorageRoot::open(&根路径, None).unwrap();
                let 内容 = format!("版本-{i:03}");
                atomic::write(&root, "files/race.txt", 内容.as_bytes()).unwrap();
            });
        }
    });

    let 最终 = fs::read_to_string(dir.path().join("files/race.txt")).unwrap();
    assert!(
        最终.starts_with("版本-") && 最终.len() == "版本-000".len(),
        "必须是某一次写入的完整内容，实得 {最终:?}"
    );
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p arca-store --test atomic`
Expected: 编译失败，`arca_store::atomic` 不存在

- [ ] **Step 3: 实现 atomic.rs**

要点（不给完整代码，按这些约束自己写，写完在报告里说明每条怎么落实的）：

1. 临时文件建在 `<root>/.arca/tmp/` 下——**必须与目标同一文件系统**，否则 `rename`
   不是原子的（FORMAT.md §4：所有目录必须位于同一文件系统）。文件名要唯一，
   但**不能用 `Math.random` 式的全局随机**：用进程 id + 单调递增计数器 + 目标路径的哈希，
   保证同进程内不撞、跨进程也极难撞。在注释里说明命名方案。
2. 顺序严格是：写入内容 → `File::sync_all()`（fsync 文件）→ `fs::rename` → fsync 目标的父目录。
   少任何一步都在崩溃窗口里留下漏洞，在代码注释里逐条说明为什么。
3. 目标的父目录若不存在则创建（`create_dir_all`）。
4. **失败时清理自己的临时文件**，但清理失败不掩盖原始错误——原始错误优先返回。
5. `rename` 返回 `CrossesDevices`（或对应的 errno）时映射为 `AtomicError::CrossDevice`，
   错误信息要说清「tmp 与目标不在同一文件系统，rename 不是原子的」——这是配置错误，
   不是可重试的 IO 抖动，属于 `needs_human`。
6. 不得使用 `unsafe`。fsync 目录在 Unix 上用 `File::open(dir)?.sync_all()`；
   Windows 上打开目录会失败，用 `#[cfg(unix)]` 分流并在非 Unix 上跳过目录 fsync，
   **在注释里诚实写明这是平台局限而不是遗漏**（Windows 的等价保证属 M3 的范围）。

- [ ] **Step 4: 运行测试确认通过 + 门禁 + 提交**

```bash
cargo test -p arca-store --test atomic
git add crates/arca-store && git commit -m "arca-store: 原子写入（tmp → fsync → rename → fsync 父目录）"
```

---

### Task 4: tmp 残留清理 + fsck 改经 StorageRoot

**Files:**
- Modify: `crates/arca-store/src/atomic.rs`, `crates/arca-store/src/fsck.rs`
- Test: `crates/arca-store/tests/atomic.rs`（追加）, `crates/arca-store/tests/fsck.rs`（既有，需适配）

**Interfaces:**
- Produces: `atomic::sweep_tmp(root: &StorageRoot) -> Result<SweepReport, AtomicError>`；
  `SweepReport { removed: usize, refused: Vec<String> }`。
- `fsck::check_root` 签名改为接 `&StorageRoot` 而不是 `&Path`；
  新增 `fsck::check_path(root: &Path) -> Result<FsckReport, MountError>` 作为 CLI 用的便捷壳。

> **清理纪律直接继承 lazync**（STORAGE.md §Move And Delete Recovery 末段）：
> `tmp/` 下的孤儿**普通文件**可以安全删除；出现**符号链接或目录**则拒绝并报告，
> **绝不递归删除**。理由是 I3 与 I5 的交叉——tmp 里不该有目录，出现了就说明状态超出预期，
> 此时递归删除是把「我不理解这个状态」变成「我删掉了不理解的东西」。

- [ ] **Step 1: 写失败的测试**

追加到 `crates/arca-store/tests/atomic.rs`：

```rust
#[test]
fn 清理孤儿临时文件() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    fs::write(dir.path().join(".arca/tmp/orphan-1"), b"crash residue").unwrap();
    fs::write(dir.path().join(".arca/tmp/orphan-2"), b"more residue").unwrap();

    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 报告 = atomic::sweep_tmp(&root).unwrap();
    assert_eq!(报告.removed, 2);
    assert!(报告.refused.is_empty());
    assert_eq!(fs::read_dir(dir.path().join(".arca/tmp")).unwrap().count(), 0);
}

#[test]
fn tmp_下出现目录时拒绝而不是递归删除() {
    // I5：不理解的状态要停下报告，不能变成「我删掉了不理解的东西」（I3）
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    fs::create_dir(dir.path().join(".arca/tmp/意外目录")).unwrap();
    fs::write(dir.path().join(".arca/tmp/意外目录/内含文件"), b"x").unwrap();

    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 报告 = atomic::sweep_tmp(&root).unwrap();
    assert_eq!(报告.removed, 0);
    assert_eq!(报告.refused.len(), 1, "应报告拒绝处理的条目");
    assert!(
        dir.path().join(".arca/tmp/意外目录/内含文件").exists(),
        "绝不递归删除"
    );
}

#[cfg(unix)]
#[test]
fn tmp_下出现符号链接时拒绝() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let 目标 = dir.path().join("files/重要文件");
    fs::write(&目标, b"绝不能被顺着链接删掉").unwrap();
    std::os::unix::fs::symlink(&目标, dir.path().join(".arca/tmp/link")).unwrap();

    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 报告 = atomic::sweep_tmp(&root).unwrap();
    assert_eq!(报告.removed, 0);
    assert_eq!(报告.refused.len(), 1);
    assert!(目标.exists(), "符号链接指向的文件必须完好");
}

#[test]
fn tmp_目录不存在时清理是无操作() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".arca")).unwrap();
    fs::write(
        dir.path().join(".arca/format.json"),
        format!(
            r#"{{"v":1,"format":1,"dataset_id":"{样例_ID}","hash_algo":"blake3","created_at":"2026-08-05T10:00:00Z"}}"#
        ),
    )
    .unwrap();
    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 报告 = atomic::sweep_tmp(&root).unwrap();
    assert_eq!(报告.removed, 0);
}
```

- [ ] **Step 2: 运行确认失败，实现 sweep_tmp**

按上面的纪律实现：遍历 `tmp/`，用 `symlink_metadata`（**不是 `metadata`**，
后者会跟随符号链接）判断类型；普通文件删除并计数；其余一律计入 `refused` 并保留。

- [ ] **Step 3: 把 fsck 改经 StorageRoot 打开**

`fsck::check_root` 现在自己读 `format.json` 并在缺失/损坏时推 `Problem::MissingFormatJson` /
`BadFormatJson` 后立即返回。改为：

- `check_root(root: &StorageRoot) -> FsckReport`：不再做身份检查（调用方已经做了），
  删掉那两个 `Problem` 变体的产生逻辑与提前返回；
- 新增 `check_path(root: &Path) -> Result<FsckReport, MountError>`：先 `StorageRoot::open(root, None)`，
  再调 `check_root`。挂载失败作为 `Err` 返回，**不再伪装成一条 `Problem`**——
  「这不是一个存储根」和「这个存储根里有问题」是两种不同的答案。

`Problem::MissingFormatJson` 与 `BadFormatJson` 两个变体：判断它们是否还有产生路径。
若没有就删掉（枚举里留着永不产生的变体会误导 M1 的 `arca verify`）；
删除前确认 `crates/arca-store/tests/fsck.rs` 里对应的测试改成断言 `check_path` 返回 `Err(MountError::...)`。

`crates/arca-cli/src/main.rs` 的 `fsck` 子命令相应改用 `check_path`，
挂载失败时把 `MountError` 的中文信息打到 stderr 并以退出码 **2** 结束
（与逃生舱脚本一致：2 = 身份不明，1 = 有问题，0 = 干净）。

- [ ] **Step 4: 四项门禁 + 端到端验证 + 提交**

```bash
cargo test --workspace
cargo run -p arca-cli -- fsck /tmp/不存在的目录   # 应打印中文挂载错误，退出码 2
git add crates/arca-store crates/arca-cli && git commit -m "arca-store: tmp 残留清理纪律 + fsck 改经 StorageRoot 打开"
```

---

## Self-Review

**1. 范围覆盖**：M0 最终评审列出的 M1 前置项第 4 条（`arca-store` 只有 fsck，
`atomic` 与身份校验都是 TODO；I11 挂载检查无实现无测试，而 `mount.check` 事件与
`mount.identity_mismatch` / `mount.absent` 错误码已定义）——Task 1 覆盖身份校验三态，
Task 2 覆盖 `mount.check` 发射，Task 3 覆盖 `atomic`，Task 4 把 fsck 收口到同一入口。

**2. 有意留给后续切片的**：
- `.txn` 事务日志（TODO(M2)，本计划不碰）
- `PROTOCOL.md` §7 的错误码与 `MountError` 的映射——码表是协议层的事，
  等 M1d 的 CLI `--json` 输出落地时一并做，本切片只保证错误性质可区分
- Windows 上的目录 fsync（Task 3 第 6 点已诚实记为平台局限，属 M3 范围）

**3. 类型一致性**：`StorageRoot::open` 在 Task 1 定义，Task 2 加 `open_traced`、
Task 3 与 Task 4 使用；`AtomicError` 在 Task 3 定义，Task 4 的 `sweep_tmp` 复用；
`MountError` 在 Task 1 定义，Task 4 的 `check_path` 返回它。

**4. 已知的待确认点**（实现者遇到就停下问，不要自行扩大范围）：
`FieldValue` 是否有 `From<bool>` / `From<&str>` 实现（Task 2 Step 3 已标注）。
若需要改 `arca-format` 才能发出 `mount.check` 的字段，那是跨 crate 改动，需要单独决定。

---

## M1 的整体拆分（本计划是第一块）

| 切片 | 内容 | 依赖 |
| --- | --- | --- |
| **M1a（本计划）** | 存储根 IO 地基：打开 + 身份校验（I11）+ 原子提交 + tmp 清理 | 无 |
| **M1b** | `arca-core` 调和状态机（sans-io 三态对账）+ `reconcile.decide` trace 发射 + 确定性模拟测试与 proptest 收敛性 | M1a |
| **M1c** | `arca-git`：`.gitignore` 反选块（全设计最易出错处）+ 清单同步 + pre-push 钩子 + 追踪冲突检测 | 无（可与 M1b 并行） |
| **M1d** | CLI porcelain/plumbing + `file://` 直连同步闭环 + trace 失败落盘；跑通 spec §12.3 的 M1 验收演示 | M1a + M1b + M1c |

每块独立可用、独立可演示，避免 Perkeep 式「大教堂建到一半」。
