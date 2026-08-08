//! plumbing 命令（spec §3.2）——输出稳定、可脚本化，格式与退出码进
//! `PROTOCOL.md` §5：
//!
//! - `arca ls <path> --json`：数据集在 hub 侧的当前清单（`RemoteState::Present`
//!   的快照，路径排序）；
//! - `arca cat <path> <hash>`：按内容哈希取字节，原样写 stdout（不是文本命令，
//!   输出可能是任意二进制）；
//! - `arca resolve <path> <file>`：单个路径 → hub 侧身份/版本；
//! - `arca state dump <path> --json`：客户端本地投影（基线）检视——SQLite 是
//!   二进制没关系，git 的 index 也是，前提是有 dump 命令（spec §3.2）。
//!
//! 四个命令都先经 [`crate::dataset::resolve`] 解析出数据集与它绑定的存储根
//! 路径，与 `status`/`verify`/`doctor`（`commands/porcelain.rs`）共用同一套
//! 措辞。`ls`/`cat`/`resolve` 需要一份身份已确认的
//! [`arca_store::root::StorageRoot`]（I11：挂载缺失/身份不符必须显式失败，
//! 绝不能把"打不开"误当成"这个路径没有记录"）；`state dump` 只读本地基线，
//! 不需要打开存储根。
//!
//! Rule of Silence 在这里的具体含义（spec §3.2）：**数据永远走 stdout**——
//! plumbing 存在的意义就是产出可脚本消费的输出，即便结果是"空清单"也要
//! 打印 `[]`，不是"安静"。安静只留给真正意味着"没有这回事"的诊断信息
//! （走 stderr）。

use arca_chunk::hash::ContentHash;
use arca_cli::dataset;
use arca_cli::{baseline, hub};
use arca_format::model::ItemId;
use arca_store::root::StorageRoot;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// `ls`/`cat`/`resolve` 共用的一条 hub 侧记录的 JSON 形状。
#[derive(Serialize)]
struct RemoteEntry {
    path: String,
    item_id: String,
    version_id: String,
    hash: String,
    size: u64,
}

/// 解析数据集并打开身份已确认的存储根；两步的失败分别映射到调用方期望的
/// 退出码——数据集解析失败（未注册等）走退出码 1，存储根身份不明（I11）
/// 走退出码 2，与 `arca fsck`/`arca verify` 的既有约定一致。
fn open_dataset_and_root(
    path: &str,
    root_override: Option<&Path>,
) -> Result<(dataset::ResolvedDataset, StorageRoot), ExitCode> {
    let resolved = dataset::resolve(&cwd(), path, root_override).map_err(|e| {
        eprintln!("{e}");
        ExitCode::from(1)
    })?;
    // M2c Task 5：plumbing（ls/cat/resolve）尚未 Transport 化——`http://`
    // hub 报明确的"这条命令不支持"（`dataset::ResolvedDataset::local_root`
    // 文档），不是退出码 2 的 I11 身份不明。
    let root_path = resolved.local_root().map_err(|e| {
        eprintln!("{e}");
        ExitCode::from(1)
    })?;
    let store_root = StorageRoot::open(root_path, Some(&resolved.cfg.dataset_id)).map_err(|e| {
        eprintln!("{e}");
        ExitCode::from(2)
    })?;
    Ok((resolved, store_root))
}

/// `arca ls <path> --json`：hub 侧当前清单，按路径排序。
pub fn ls_cmd(path: &str, root: Option<&Path>) -> ExitCode {
    let (_resolved, store_root) = match open_dataset_and_root(path, root) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let remote = match hub::read_remote(&store_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    let entries: Vec<RemoteEntry> = remote
        .into_iter()
        .filter_map(|(p, state)| remote_entry(p, &state))
        .collect();
    print_json(&entries)
}

/// `arca cat <path> <hash>`：按内容哈希取字节，原样写 stdout（不追加换行、
/// 不做任何编码转换——输出可能是任意二进制，管道给别的工具用）。
pub fn cat_cmd(path: &str, hash: &str, root: Option<&Path>) -> ExitCode {
    let wanted = match ContentHash::parse(hash) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("哈希 {hash:?} 不合规：{e}");
            return ExitCode::from(1);
        }
    };
    let (_resolved, store_root) = match open_dataset_and_root(path, root) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let remote = match hub::read_remote(&store_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    // 多个路径可能共享同一份内容（去重）——按路径排序后取第一个命中，
    // 结果确定（`hub::read_remote` 产出的是 `BTreeMap`）。
    let found = remote.iter().find(|(_, state)| match state {
        arca_core::state::RemoteState::Present { hash, .. } => *hash == wanted,
        _ => false,
    });
    let Some((hit_path, _)) = found else {
        eprintln!("未找到哈希为 {hash} 的内容");
        return ExitCode::from(1);
    };

    let file_path = match store_root.join(&format!(
        "{}/{}",
        arca_format::hub_layout::layout::FILES_DIR,
        hit_path
    )) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let bytes = match std::fs::read(&file_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取 {} 失败：{e}", file_path.display());
            return ExitCode::from(1);
        }
    };
    match std::io::stdout().write_all(&bytes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("写 stdout 失败：{e}");
            ExitCode::from(1)
        }
    }
}

/// `arca resolve <path> <file>`：单个路径 → hub 侧身份/版本。
pub fn resolve_cmd(path: &str, file: &str, root: Option<&Path>) -> ExitCode {
    let (_resolved, store_root) = match open_dataset_and_root(path, root) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let normalized = match arca_format::path_rules::check(file) {
        Ok(p) => p,
        Err(status) => {
            eprintln!("路径 {file:?} 不合规：{}", status.as_str());
            return ExitCode::from(1);
        }
    };
    let remote = match hub::read_remote(&store_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let Some(state) = remote.get(&normalized) else {
        eprintln!("{normalized} 在 hub 侧没有记录");
        return ExitCode::from(1);
    };
    let Some(entry) = remote_entry(normalized, state) else {
        eprintln!("内部不变量被破坏：解析到的记录不是 Present");
        return ExitCode::from(1);
    };
    print_json(&entry)
}

/// `arca state dump <path> --json`：客户端本地投影（基线）检视，不需要打开
/// 存储根——基线纯粹是本地状态（I9：可抛弃投影）。
pub fn state_dump_cmd(path: &str, root: Option<&Path>) -> ExitCode {
    let resolved = match dataset::resolve(&cwd(), path, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let loaded = match baseline::load(&resolved.dataset_dir) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    #[derive(Serialize)]
    struct BaselineEntry {
        path: String,
        item_id: String,
        version_id: String,
        hash: String,
        size: u64,
    }
    #[derive(Serialize)]
    struct StateDump {
        was_reset: bool,
        reset_reason: Option<String>,
        entries: Vec<BaselineEntry>,
    }

    let entries: Vec<BaselineEntry> = loaded
        .iter()
        .filter_map(|(p, state)| match state {
            arca_core::state::BaseState::Present {
                item_id,
                version_id,
                hash,
                size,
            } => Some(BaselineEntry {
                path: p.clone(),
                item_id: item_id.to_hex(),
                version_id: version_id.as_str().to_string(),
                hash: hash.to_text(),
                size: *size,
            }),
            arca_core::state::BaseState::Absent => None,
        })
        .collect();

    let dump = StateDump {
        was_reset: loaded.was_reset(),
        reset_reason: loaded.reset_reason().map(|r| r.to_string()),
        entries,
    };
    print_json(&dump)
}

fn remote_entry(path: String, state: &arca_core::state::RemoteState) -> Option<RemoteEntry> {
    match state {
        arca_core::state::RemoteState::Present {
            item_id,
            version_id,
            hash,
            size,
        } => Some(RemoteEntry {
            path,
            item_id: item_id_hex(item_id),
            version_id: version_id.as_str().to_string(),
            hash: hash.to_text(),
            size: *size,
        }),
        // M1 结构上不产出 Tombstoned（见 hub.rs 模块文档）；Absent 从不会
        // 出现在 read_remote 的返回值里。两者都不构成一条清单记录。
        _ => None,
    }
}

fn item_id_hex(id: &ItemId) -> String {
    id.to_hex()
}

fn print_json<T: Serialize>(value: &T) -> ExitCode {
    match serde_json::to_string(value) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("JSON 序列化失败：{e}");
            ExitCode::from(1)
        }
    }
}
