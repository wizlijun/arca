//! 存储根完整性巡检（spec §7、§4.5）。
//!
//! **只读**：fsck 报告问题，从不修复、从不删除（I3：同步路径无销毁权——
//! 修复动作属于显式命令）。发现悬空引用 → 停下报告，绝不猜测（I5）。
//!
//! 消费者：`arca fsck`（CLI）与 M2 的 `arcad` / `arca gc`——
//! gc 与 fsck 共享引用计数校验。

use arca_chunk::hash::ContentHash;
use arca_format::hub_layout::{layout, FormatJson};
use arca_format::{items, path_rules};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// 一条巡检发现的问题。变体覆盖 FORMAT.md §5–§8 定义的各类损坏形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// `format.json` 缺失——存储根身份不明，见 [`check_root`] 顶部的立即返回。
    MissingFormatJson,
    /// `format.json` 存在但无法解析或版本不受支持。
    BadFormatJson(String),
    /// 当前版本在 `files/` 下缺失（`ErrorKind::NotFound`）。
    MissingFile { path: String },
    /// `files/` 下的字节内容哈希与版本记录不一致。
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    /// `files/` 下的字节大小与版本记录不一致。
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    /// item 没有任何 index 记录指向它——悬空引用。
    OrphanIndex { key: String },
    /// `items/<xx>/<item_id>.jsonl` 无法解析为合法版本链。
    BrokenChain { item: String, reason: String },
    /// `chunks/<xx>/<hash>.zst` 读出来了，但解压失败或解压后哈希与文件名不一致——
    /// 内容本身有问题，不是"读不到"（那是 [`Problem::IoError`]）。
    CorruptChunk { hash: String },
    /// 读取失败但不是"文件不存在"（权限、损坏的挂载点等 IO 错误）——与「内容不对」
    /// 是不同性质的故障，绝不可折叠成同一个诊断（I5：如实报告失败的性质）。
    IoError { path: String, reason: String },
}

/// 巡检报告：发现的问题 + 已检查的文件/块计数。
///
/// 同一存储根状态下 `check_root` 必须逐条产生同一份报告（确定性遍历，见
/// [`read_dir_sorted`]）——fsck 是诊断工具，报告本身也要可复现、可 diff。
#[derive(Debug, Default)]
pub struct FsckReport {
    pub problems: Vec<Problem>,
    pub checked_files: usize,
    pub checked_chunks: usize,
}

/// 巡检一个 hub 存储根的完整性。**只读**：不修改、不删除任何文件（I3）。
///
/// `format.json` 缺失或不可解析时立即返回——存储根身份不明，绝不继续遍历
/// items 去猜测里面有什么（I5：未挂载的卷绝不能被当成空库，I11）。
pub fn check_root(root: &Path) -> FsckReport {
    let mut report = FsckReport::default();

    // 1. format.json 必须存在且可解析——这是卷身份标记（I11）
    let format_path = root.join(layout::FORMAT_JSON);
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
    let items_dir = root.join(layout::ITEMS_DIR);
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
            let Some(current) = chain.last() else {
                continue;
            };
            let Some(logical) = lookup_path(root, &current.item_id.to_hex()) else {
                report.problems.push(Problem::OrphanIndex {
                    key: current.item_id.to_hex(),
                });
                continue;
            };
            let physical = root.join(layout::FILES_DIR).join(&logical);
            report.checked_files += 1;
            match fs::read(&physical) {
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    report.problems.push(Problem::MissingFile { path: logical })
                }
                Err(e) => report.problems.push(Problem::IoError {
                    path: logical,
                    reason: e.to_string(),
                }),
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

    // 3. 块存储：每个块解压后哈希必须与文件名一致。读不到（IO 错误）与读到了但
    //    内容不对（解压失败/哈希不符）是两种不同性质的故障，分别报告（I5）。
    for shard in read_dir_sorted(&root.join(layout::CHUNKS_DIR)) {
        for chunk_file in read_dir_sorted(&shard) {
            report.checked_chunks += 1;
            let name = chunk_file
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let packed = match fs::read(&chunk_file) {
                Ok(bytes) => bytes,
                Err(e) => {
                    report.problems.push(Problem::IoError {
                        path: chunk_file.display().to_string(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let ok = arca_chunk::compress::decompress(&packed)
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
///
/// M0 用线性扫描（O(n²)：每个 item 都要重新遍历一遍整个 index 目录），
/// 存储根规模有限，可接受；M2 换成内存索引（哈希表）后应替换本函数。
fn lookup_path(root: &Path, item_id_hex: &str) -> Option<String> {
    for shard in read_dir_sorted(&root.join(layout::INDEX_DIR)) {
        for record in read_dir_sorted(&shard) {
            let Ok(text) = fs::read_to_string(&record) else {
                continue;
            };
            let Ok(parsed) = arca_format::index::IndexRecord::parse(&text) else {
                continue;
            };
            if parsed.item_id.to_hex() == item_id_hex {
                // 路径必须合规，否则视为损坏记录而非可用映射
                return path_rules::check(&parsed.path).ok();
            }
        }
    }
    None
}

/// 排序读目录：使 fsck 的输出确定（同一状态必产生同一报告）。文件系统的
/// `read_dir` 顺序不保证，不排序会让同一存储根两次巡检产生不同顺序的报告。
fn read_dir_sorted(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    paths
}
