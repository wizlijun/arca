//! 本地扫描：遍历数据集目录，产出确定性的 [`LocalState`] 集合（M1d Task 1）。
//!
//! 三态调和（`arca_core::reconcile::decide`）的三个输入端之一——本模块只负责
//! 「本地现在是什么样」，不做任何判断（判断留给 `arca-core`，见 CLAUDE.md
//! 「架构约束」：CLI 不得另写一套 if-else）。
//!
//! **跳过 `.arca/`**（数据集根目录下的元数据目录不是受管内容，见 `docs/…m1d…`
//! 计划 Task 1）；每个文件都过 [`path_rules::check`]，不合规的路径绝不静默
//! 跳过——进 [`ScanResult::rejected`] 并发一条 `path.reject` trace 事件
//! （FORMAT.md §10.3）。BLAKE3 使用流式 API 分块喂（[`ContentHash::hasher`]），
//! 不会把整份文件读进内存——1 万张照片 2 分钟内是 M1 验收标准（spec §12.3），
//! 而照片目录里常见几十 MB 的 RAW。
//!
//! # 与 brief 字面签名的一处刻意偏离
//!
//! Task 1 brief 给的签名是 `scan_dataset(root, sink) -> ScanResult`（无
//! `Result`）。这里改成了 `Result<ScanResult, ScanError>`：遍历目录或哈希某个
//! 文件时若真的发生 IO 错误（权限、目录在扫描期间消失等），**绝不能把它悄悄
//! 从 `files` 里漏掉**——`LocalState` 只有 `Absent`/`Present` 两种形状，一个
//! 因为读不到而"没能放进结果"的文件，在下游 `decide()` 眼里和"真的被删除了"
//! 完全无法区分，可能被误判成 `DeleteLocal` 而对着一份其实还在、只是这次没读到
//! 的文件动手（I3：同步路径无销毁权；I5：绝不猜测）。真正的 IO 故障必须让
//! 整次扫描停下并报告，而不是悄悄降级成"这个文件不存在"。
//!
//! # 符号链接的处置：跳过并计入 `rejected`
//!
//! 跟随符号链接有两个问题：其一，若链接指向数据集内部的另一文件，内容会被
//! 算两次（一次算作真身，一次算作链接），产出的 `files` 就不再是路径到内容的
//! 单射；其二，链接可能指向数据集外部（甚至整个文件系统的任意位置），跟随等于
//! 把数据集之外的内容悄悄纳入扫描结果，违反"扫描结果只反映数据集内容"这个
//! 前提，也可能把不属于用户数据集的敏感文件意外吸收进来。跳过是显式、可诊断
//! 的处置（I5：绝不猜测该不该跟随、跟到哪算完），因此符号链接与 `path_rules`
//! 不合规的路径一样进 `rejected`——但原因不同，用 [`RejectReason::Symlink`]
//! 区分（详见该类型的 doc comment）。

use arca_chunk::hash::ContentHash;
use arca_core::state::LocalState;
use arca_format::path_rules::{self, PathStatus};
use arca_format::trace::{EventKind, TraceRecord, TraceSink};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// 数据集根目录下、扫描时跳过的元数据目录名（`<dataset>/.arca/`）。
const ARCA_DIR_NAME: &str = ".arca";

/// 流式哈希读取缓冲区大小：64 KiB 是 IO 吞吐与内存占用之间常见的折中点，
/// 与"别把整个文件读进内存"的约束配套（brief Task 1）。
const HASH_BUFFER_BYTES: usize = 64 * 1024;

/// 扫描阶段拒绝一个路径的原因。
///
/// 与 [`PathStatus`] 分开是刻意的：`PathStatus` 只描述路径**字符串**本身合不
/// 合规（`path_rules::check` 的产出），是 `arca-format` 的职责；符号链接与
/// "既不是文件也不是目录也不是符号链接"（设备文件、套接字等）是关于**这个
/// 路径在文件系统上是什么**的判断，是扫描阶段（`arca-cli`）自己的职责，混进
/// `PathStatus` 会让那个纯字符串规则的类型承担超出它职责的语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// 路径字符串本身不合规，原因见 [`PathStatus`]。
    Path(PathStatus),
    /// 符号链接——处置见本模块顶部 doc comment。
    Symlink,
    /// 既非常规文件、目录，也非符号链接（设备文件、套接字、FIFO 等）。
    Unsupported,
}

impl RejectReason {
    /// 稳定短标识，写入 `path.reject` trace 事件的 `status` 字段。
    /// `Path` 变体直接复用 [`PathStatus::as_str`]，其余两个是本模块自定义的
    /// 补充值——FORMAT.md §10.3 未把 `path.reject` 的 `status` 字段钉死为
    /// 只能取 `PathStatus` 的封闭取值集合，新增值不构成破坏性变更。
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::Path(status) => status.as_str(),
            RejectReason::Symlink => "symlink",
            RejectReason::Unsupported => "unsupported_type",
        }
    }
}

/// 扫描结果：按路径排序的 `BTreeMap`，保证同一目录状态两次扫描产生同一结果。
#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: BTreeMap<String, LocalState>,
    pub rejected: Vec<(String, RejectReason)>,
    pub bytes: u64,
}

/// 扫描失败——真正的 IO 故障，与"这个文件不合规所以被拒绝"是不同性质的结果
/// （见本模块顶部「刻意偏离」一节）。
#[derive(Debug)]
pub enum ScanError {
    Io { path: String, reason: String },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Io { path, reason } => write!(f, "扫描 {path} 失败：{reason}"),
        }
    }
}

impl std::error::Error for ScanError {}

fn io_err(path: &Path, e: io::Error) -> ScanError {
    ScanError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// 遍历 `root`（数据集根目录），产出本地文件状态集合。
///
/// 只读：不修改、不创建任何文件。`sink` 接收 `path.reject`（逐条不合规/不支持
/// 的路径）与 `scan.summary`（一条汇总，FORMAT.md §10.3）两类事件。
pub fn scan_dataset(root: &Path, sink: &mut dyn TraceSink) -> Result<ScanResult, ScanError> {
    let start = Instant::now();
    let mut result = ScanResult::default();
    walk_dir(root, root, &mut result, sink, &start)?;

    sink.record(
        TraceRecord::new(EventKind::ScanSummary, elapsed_us(&start))
            .with("files", result.files.len() as u64)
            .with("bytes", result.bytes)
            .with("rejected", result.rejected.len() as u64),
    );
    Ok(result)
}

fn elapsed_us(start: &Instant) -> u64 {
    start.elapsed().as_micros() as u64
}

fn walk_dir(
    current: &Path,
    dataset_root: &Path,
    result: &mut ScanResult,
    sink: &mut dyn TraceSink,
    start: &Instant,
) -> Result<(), ScanError> {
    for entry_path in read_dir_sorted(current)? {
        // 元数据目录只在数据集根目录这一层跳过；跳过判断先于任何 stat 调用——
        // 若 `.arca` 本身是符号链接或别的怪东西，也不必深究，直接跳过。
        if current == dataset_root && entry_path.file_name() == Some(OsStr::new(ARCA_DIR_NAME)) {
            continue;
        }

        // symlink_metadata 不跟随符号链接——这是判断"这个条目本身是不是符号
        // 链接"的唯一正确方式；fs::metadata 会跟随，用它就看不出符号链接了。
        let meta = fs::symlink_metadata(&entry_path).map_err(|e| io_err(&entry_path, e))?;
        let file_type = meta.file_type();

        let rel = entry_path
            .strip_prefix(dataset_root)
            .expect("遍历产出的路径必然位于 dataset_root 之下");
        let rel_str = path_to_slash(rel);

        if file_type.is_symlink() {
            reject(result, sink, start, rel_str, RejectReason::Symlink);
            continue;
        }
        if file_type.is_dir() {
            walk_dir(&entry_path, dataset_root, result, sink, start)?;
            continue;
        }
        if !file_type.is_file() {
            reject(result, sink, start, rel_str, RejectReason::Unsupported);
            continue;
        }

        match path_rules::check(&rel_str) {
            Ok(normalized) => {
                let (hash, size) = hash_file(&entry_path)?;
                result.bytes += size;
                result
                    .files
                    .insert(normalized, LocalState::Present { hash, size });
            }
            Err(status) => {
                reject(result, sink, start, rel_str, RejectReason::Path(status));
            }
        }
    }
    Ok(())
}

fn reject(
    result: &mut ScanResult,
    sink: &mut dyn TraceSink,
    start: &Instant,
    path: String,
    reason: RejectReason,
) {
    sink.record(
        TraceRecord::new(EventKind::PathReject, elapsed_us(start))
            .with("path", path.clone())
            .with("status", reason.as_str()),
    );
    result.rejected.push((path, reason));
}

/// 排序读目录：使扫描的遍历顺序确定（文件系统的 `read_dir` 顺序不保证）。
/// 最终结果落进 `BTreeMap` 已经保证了 `files` 本身的确定性，这里额外排序是
/// 为了让 `rejected`（`Vec`，插入顺序即输出顺序）与 trace 事件的发出顺序
/// 也是确定的——诊断轨迹本身也要求可复现（spec §11.2）。
fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, ScanError> {
    let entries = fs::read_dir(dir).map_err(|e| io_err(dir, e))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

/// 把数据集根之下的相对路径转成 `/` 拼接的字符串，喂给 `path_rules::check`。
///
/// `path_rules::normalize` 本身也接受 `\` 作为分隔符，但这里先统一转成 `/`——
/// Unix 上 `Path` 的组件天然就是 `/` 分隔，`to_string_lossy` 不会引入 `\`；
/// 非 UTF-8 的路径字节会被替换成 U+FFFD，这类路径本就会在后续
/// `path_rules::check` 里因含非法字符被拒绝，此处宽松转换不会掩盖问题。
fn path_to_slash(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// 流式计算一个文件的 BLAKE3 哈希与字节数，不把整份内容读进内存。
fn hash_file(path: &Path) -> Result<(ContentHash, u64), ScanError> {
    let mut file = File::open(path).map_err(|e| io_err(path, e))?;
    let mut hasher = ContentHash::hasher();
    let mut buf = vec![0u8; HASH_BUFFER_BYTES];
    let mut size: u64 = 0;
    loop {
        let n = file.read(&mut buf).map_err(|e| io_err(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hasher.finish(), size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::trace::VecSink;
    use std::fs;

    fn write(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn 正常目录扫描出全部文件并按路径排序() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("b.txt"), b"second");
        write(&dir.path().join("a.txt"), b"first");
        write(&dir.path().join("sub/c.txt"), b"third");

        let mut sink = VecSink::new();
        let result = scan_dataset(dir.path(), &mut sink).unwrap();

        let paths: Vec<&String> = result.files.keys().collect();
        assert_eq!(paths, vec!["a.txt", "b.txt", "sub/c.txt"]);
        assert!(result.rejected.is_empty());
        assert_eq!(
            result.bytes,
            "first".len() as u64 + "second".len() as u64 + "third".len() as u64
        );

        match result.files.get("a.txt").unwrap() {
            LocalState::Present { hash, size } => {
                assert_eq!(*hash, ContentHash::from_bytes(b"first"));
                assert_eq!(*size, 5);
            }
            LocalState::Absent => panic!("应为 Present"),
        }

        // scan.summary 汇总事件必须发出，字段与实际计数一致。
        let summaries = sink.of_kind(&EventKind::ScanSummary);
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].field("files"),
            Some(&arca_format::trace::FieldValue::U64(3))
        );
        assert_eq!(
            summaries[0].field("rejected"),
            Some(&arca_format::trace::FieldValue::U64(0))
        );
    }

    #[test]
    fn 含不合规路径的文件进入rejected并发trace事件() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("good.txt"), b"ok");
        // "CON.txt" 是 Windows 保留名，path_rules::check 会拒绝（ReservedName）。
        write(&dir.path().join("CON.txt"), b"bad");

        let mut sink = VecSink::new();
        let result = scan_dataset(dir.path(), &mut sink).unwrap();

        assert_eq!(result.files.len(), 1);
        assert!(result.files.contains_key("good.txt"));
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(
            result.rejected[0],
            (
                "CON.txt".to_string(),
                RejectReason::Path(PathStatus::ReservedName)
            )
        );

        let rejects = sink.of_kind(&EventKind::PathReject);
        assert_eq!(rejects.len(), 1);
        assert_eq!(
            rejects[0].field("status"),
            Some(&arca_format::trace::FieldValue::Str(
                std::borrow::Cow::Borrowed("reserved_name")
            ))
        );
    }

    #[test]
    fn 空目录扫描出空结果() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = VecSink::new();
        let result = scan_dataset(dir.path(), &mut sink).unwrap();
        assert!(result.files.is_empty());
        assert!(result.rejected.is_empty());
        assert_eq!(result.bytes, 0);
    }

    #[test]
    fn arca目录被跳过() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".arca/client/baseline.jsonl"), b"v1");
        write(&dir.path().join(".arca/manifest"), b"manifest");
        write(&dir.path().join("real.txt"), b"content");

        let mut sink = VecSink::new();
        let result = scan_dataset(dir.path(), &mut sink).unwrap();

        assert_eq!(result.files.len(), 1);
        assert!(result.files.contains_key("real.txt"));
        assert!(result.rejected.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn 符号链接被跳过并计入rejected而不是被跟随() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("real.txt"), b"content");
        symlink(dir.path().join("real.txt"), dir.path().join("link.txt")).unwrap();

        let mut sink = VecSink::new();
        let result = scan_dataset(dir.path(), &mut sink).unwrap();

        // 只有真身进 files，符号链接不会导致内容被算两次。
        assert_eq!(result.files.len(), 1);
        assert!(result.files.contains_key("real.txt"));
        assert_eq!(
            result.rejected,
            vec![("link.txt".to_string(), RejectReason::Symlink)]
        );
    }

    #[test]
    #[cfg(unix)]
    fn 指向数据集外部的符号链接同样被跳过() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("secret.txt"), b"outside content");
        symlink(
            outside.path().join("secret.txt"),
            dir.path().join("escape.txt"),
        )
        .unwrap();

        let mut sink = VecSink::new();
        let result = scan_dataset(dir.path(), &mut sink).unwrap();

        assert!(result.files.is_empty());
        assert_eq!(
            result.rejected,
            vec![("escape.txt".to_string(), RejectReason::Symlink)]
        );
    }

    #[test]
    fn 大文件的流式哈希与一次性哈希结果一致() {
        // 内容超过一个哈希缓冲区（64 KiB），确保多次 read 循环拼出的哈希正确。
        let dir = tempfile::tempdir().unwrap();
        let content = vec![0x5au8; HASH_BUFFER_BYTES * 3 + 1234];
        write(&dir.path().join("big.bin"), &content);

        let mut sink = VecSink::new();
        let result = scan_dataset(dir.path(), &mut sink).unwrap();

        match result.files.get("big.bin").unwrap() {
            LocalState::Present { hash, size } => {
                assert_eq!(*hash, ContentHash::from_bytes(&content));
                assert_eq!(*size, content.len() as u64);
            }
            LocalState::Absent => panic!("应为 Present"),
        }
    }

    #[test]
    fn 根目录不存在时返回io错误而不是空结果() {
        let mut sink = VecSink::new();
        let missing = Path::new("/tmp/arca-scan-test-definitely-missing-xyz-123");
        let err = scan_dataset(missing, &mut sink).unwrap_err();
        assert!(matches!(err, ScanError::Io { .. }));
    }

    #[test]
    fn 两次扫描同一目录产生完全相同的结果() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("x.txt"), b"x");
        write(&dir.path().join("y/z.txt"), b"z");

        let mut sink1 = VecSink::new();
        let first = scan_dataset(dir.path(), &mut sink1).unwrap();
        let mut sink2 = VecSink::new();
        let second = scan_dataset(dir.path(), &mut sink2).unwrap();

        assert_eq!(first.files, second.files);
        assert_eq!(first.rejected, second.rejected);
        assert_eq!(first.bytes, second.bytes);
    }
}
