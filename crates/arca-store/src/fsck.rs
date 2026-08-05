//! 存储根完整性巡检（spec §7、§4.5）。
//!
//! **只读**：fsck 报告问题，从不修复、从不删除（I3：同步路径无销毁权——
//! 修复动作属于显式命令）。发现悬空引用 → 停下报告，绝不猜测（I5）。
//!
//! 消费者：`arca fsck`（CLI）与 M2 的 `arcad` / `arca gc`——
//! gc 与 fsck 共享引用计数校验。

use crate::root::{MountError, StorageRoot};
use arca_chunk::hash::ContentHash;
use arca_format::hub_layout::layout;
use arca_format::items;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// 一条巡检发现的问题。变体覆盖 FORMAT.md §5–§8 定义的各类损坏形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
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
    /// 一条 index 记录本身无法读取或解析（文件损坏、权限问题、内容不是合法
    /// `IndexRecord`）。与 [`Problem::OrphanIndex`] 是两种不同性质的故障，绝不能
    /// 折叠成同一个诊断：`OrphanIndex` 说的是"确实没有这条记录"，`CorruptIndex`
    /// 说的是"记录存在但读不出来"——后者继续静默跳过会让 fsck 对它本该指向的那个
    /// 文件的位腐烂视而不见，同时报出误导性的原因（评审 Important #4）。
    CorruptIndex { path: String, reason: String },
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

/// 巡检一个已打开、身份已确认的存储根的完整性。**只读**：不修改、不删除
/// 任何文件（I3）。
///
/// 不做身份检查——调用方（[`check_path`]，或直接持有 `StorageRoot` 的
/// 其它巡检入口）在拿到 `StorageRoot` 时已经做过（I11）。「这不是一个存储
/// 根」与「这个存储根里有问题」是两种不同的答案：前者在 `StorageRoot::open`
/// 阶段就已经是 `Err(MountError)`，不会走到这里；本函数只处理后者，把
/// 「有问题」如实累积进 [`FsckReport`]，绝不提前返回、绝不猜测（I5）。
pub fn check_root(root: &StorageRoot) -> FsckReport {
    let root = root.path();
    let mut report = FsckReport::default();

    // 1. 逐条 item：当前版本必须在 files/ 存在，且哈希与大小一致。
    //    index/ 只需扫描一遍：build_index 把「记录存在但损坏」（CorruptIndex）
    //    与「压根没有这条记录」（OrphanIndex，在下面按 item_id 查 index_map 时判定）
    //    分开报告，不静默吞掉前者（评审 Important #4）。`corrupt_index_items` 记录
    //    了损坏记录里仍能提取出的 item_id：这类 item 已经在 index_map 之外单独报过
    //    CorruptIndex，不应再额外报一条误导性的 OrphanIndex（"压根没有记录"其实
    //    不成立，只是这条记录读不出可用的路径）。
    let (index_map, corrupt_index_items) = build_index(root, &mut report);
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
            let Some(logical) = index_map.get(&current.item_id.to_hex()).cloned() else {
                if !corrupt_index_items.contains(&current.item_id.to_hex()) {
                    report.problems.push(Problem::OrphanIndex {
                        key: current.item_id.to_hex(),
                    });
                }
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

    // 2. 块存储：每个块解压后哈希必须与文件名一致。读不到（IO 错误）与读到了但
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

/// `check_root` 的便捷壳：先打开并校验存储根身份，再巡检。CLI 与其它
/// 只有路径、没有现成 `StorageRoot` 的调用方用这个。
///
/// 挂载失败（根不存在、身份读不出来等）作为 `Err(MountError)` 返回，不
/// 伪装成一条 `Problem`——「这不是一个存储根」和「这个存储根里有问题」是
/// 两种不同的答案（I11）。不传 `expected_dataset_id`：fsck 是只读巡检，不
/// 预设期望的身份，能打开、身份标记能解析出来即可。
pub fn check_path(root: &Path) -> Result<FsckReport, MountError> {
    let opened = StorageRoot::open(root, None)?;
    Ok(check_root(&opened))
}

/// 单次扫描 `index/` 目录，建立 `item_id → 路径` 映射。
///
/// 之前的实现（`lookup_path`）按 item 反复重新遍历整个 index 目录（O(n²)），
/// 且用 `let Ok(..) = .. else { continue }` 把「读取失败」「JSON 解析失败」
/// 两种损坏与「这条记录不是我要找的那条」这种正常情形折叠成同一种
/// `continue`——于是一条损坏的 index 记录会被当成"不匹配"悄悄跳过，最终让
/// fsck 对着它本该指向的 item 报 `OrphanIndex`（"没有索引记录"），这是另一个
/// 且错误的诊断，同时完全跳过了该 item 文件的哈希/大小校验（评审 Important #4）。
///
/// 改为单次扫描：既修掉了诊断错误（损坏记录单独计入 `CorruptIndex`，不影响
/// 其余记录的可用性），也顺带把 O(n²) 降到 O(n)——存储根规模有限时前者不是
/// 性能问题，但既然要重写就没有理由保留它。
///
/// 返回值第二项是从损坏记录里仍能宽松提取出 item_id 的集合（见
/// [`arca_format::index::extract_item_id_lenient`]）：`path` 不合规不代表
/// item_id 本身也不可信，调用方用它来避免对"记录存在但坏了"的 item 额外报出
/// 误导性的 `OrphanIndex`（"压根没有记录"）。
fn build_index(root: &Path, report: &mut FsckReport) -> (HashMap<String, String>, HashSet<String>) {
    let mut by_item = HashMap::new();
    let mut corrupt_items: HashSet<String> = HashSet::new();
    for shard in read_dir_sorted(&root.join(layout::INDEX_DIR)) {
        for record in read_dir_sorted(&shard) {
            let text = match fs::read_to_string(&record) {
                Ok(t) => t,
                Err(e) => {
                    report.problems.push(Problem::CorruptIndex {
                        path: record.display().to_string(),
                        reason: format!("读取失败：{e}"),
                    });
                    continue;
                }
            };
            match arca_format::index::IndexRecord::parse(&text) {
                Ok(parsed) => {
                    // IndexRecord::parse 内部已经过 path_rules::check，能解析
                    // 出来的 path 必然合规，这里不需要再次校验。
                    by_item.insert(parsed.item_id.to_hex(), parsed.path);
                }
                Err(e) => {
                    report.problems.push(Problem::CorruptIndex {
                        path: record.display().to_string(),
                        reason: format!("解析失败：{e}"),
                    });
                    if let Some(item_id) = arca_format::index::extract_item_id_lenient(&text) {
                        corrupt_items.insert(item_id.to_hex());
                    }
                }
            }
        }
    }
    (by_item, corrupt_items)
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
