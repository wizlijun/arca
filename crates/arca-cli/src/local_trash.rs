//! 工作区侧的本地回收站：`<dataset>/.arca/client/trash/<trash_id>.data` +
//! `<trash_id>.meta`（M2d Task 2，FORMAT.md §9.5）。
//!
//! `server` 角色执行 `Action::DeleteLocal` 时的落点（见 `crate::sync` 里
//! `execute_delete_local` 的角色分流）：过了删除四道闸门之后**不 `unlink`**，
//! 把本地副本挪到这里——spec §4.7 的承诺是"server 角色本地永远有完整数据，
//! 任何云侧语义都不会缩减它"，`client` 角色才是"过闸门即移除"。物理销毁
//! 只经未来显式的清理命令（M2 尚未实现），本模块**不提供任何销毁路径**
//! （I3：本切片不得新增销毁路径）。
//!
//! # 与 hub 侧 `.arca/trash/`（`crate::trash`）的关系
//!
//! 同一套字段格式与写入顺序纪律（`.data` 先于 `.meta`；`rename` 天然原子，
//! 绝不 copy+unlink），复用 `crate::trash::{TrashId, TrashMeta}`——这两个
//! 类型的标识编码与 JSON 字段是 root 无关的，没有理由在这里另起一套。
//! 落盘原语不同：hub 侧的 `.arca/trash/` 走 `arca_store::atomic`
//! （`StorageRoot` 专属的 fsync 事务链，服务权威真相）；这里是普通工作区
//! 文件系统上的 `fs::rename` + `fs::write`→`fs::rename`，与姊妹模块
//! `baseline.rs` 同一档持久化强度——本地回收站是"过闸门之后的便利副本"，
//! 它挪进来这件事本身发生的前提是闸门第 4 道已经确认 hub 保留期内可以
//! 找回同一份内容，所以丢失这份本地便利副本（例如 `.meta` 写到一半时
//! 崩溃，留下一个孤儿 `.data`——内容仍在，只是暂时找不到它对应哪个路径）
//! 不构成 I3 意义上的数据损失，不值得为它支付 hub 侧那个级别的持久化成本。
//!
//! # 范围边界
//!
//! 本模块只负责"挪进来"与"读出去"（供测试与未来的找回复用），**不提供
//! 恢复命令**——`arca restore` 目前只认 hub 侧的 `.arca/trash/`
//! （`crate::trash::restore`）；工作区侧的自动恢复命令是后续切片
//! （FORMAT.md §9.5 明确留白），本切片下找回可以先靠直接读取
//! `<dataset>/.arca/client/trash/<trash_id>.data`/`.meta` 两个文件。

use crate::trash::{TrashId, TrashMeta};
use arca_chunk::hash::ContentHash;
use arca_format::error::FormatError;
use arca_format::model::ItemId;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const CLIENT_DIR: &str = ".arca/client";
const TRASH_SUBDIR: &str = "trash";

/// 本地回收站的失败——与 [`crate::trash::TrashError`] 同一套区分纪律：
/// IO 故障与格式故障是不同性质的失败（I5）。
#[derive(Debug)]
pub enum LocalTrashError {
    Io { path: String, reason: String },
    Format(FormatError),
}

impl fmt::Display for LocalTrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocalTrashError::Io { path, reason } => write!(f, "本地回收站 {path}：{reason}"),
            LocalTrashError::Format(e) => write!(f, "本地回收站记录：{e}"),
        }
    }
}

impl std::error::Error for LocalTrashError {}

fn io_err(path: &Path, e: io::Error) -> LocalTrashError {
    LocalTrashError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

fn trash_dir(dataset_root: &Path) -> PathBuf {
    dataset_root.join(CLIENT_DIR).join(TRASH_SUBDIR)
}

fn data_path(dir: &Path, id: TrashId) -> PathBuf {
    dir.join(format!("{}.data", id.to_hex()))
}

fn meta_path(dir: &Path, id: TrashId) -> PathBuf {
    dir.join(format!("{}.meta", id.to_hex()))
}

/// 把工作区里 `source`（对应逻辑路径 `path`）的内容移进本地回收站，返回
/// 分配到的 `trash_id`。
///
/// `source` 此刻已经不存在时返回 `Ok(None)`——与
/// `gates::check_delete_transport` 第 3 道闸门"本地文件此刻已经不存在也算
/// 通过"、以及 `sync.rs` 里 `client` 角色分支对 `fs::remove_file` 的
/// `NotFound` 处理同一条幂等纪律：不管是用户手动删了、还是这次调用是重跑，
/// "现在没有可挪的源"这件事本身不是错误。
///
/// `deleted_at` 由调用方注入（不在本函数内读系统时钟）——与
/// `trash::move_to_trash`、`items::Version` 的 `committed_at` 同一条纪律：
/// 确定性测试需要能固定这个值。
pub fn move_to_trash(
    dataset_root: &Path,
    source: &Path,
    path: &str,
    item_id: ItemId,
    deleted_at: &str,
) -> Result<Option<TrashId>, LocalTrashError> {
    let dir = trash_dir(dataset_root);
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;

    let trash_id = TrashId::new_random();
    let data = data_path(&dir, trash_id);

    // `.data` 先于 `.meta`（FORMAT.md §9.5，与 hub 侧 §7.3 同一条纪律）：
    // `rename` 同文件系统内天然原子，绝不 copy+unlink（复制完成与删除源
    // 之间不留窗口）。
    match fs::rename(source, &data) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(source, e)),
    }

    // hash/size 算的是刚移动到位的 `.data`——读的是 rename 已经落地的那份
    // 内容（与 `trash::move_to_trash` 同一条纪律，评审 Critical #2 的镜像）。
    let bytes = fs::read(&data).map_err(|e| io_err(&data, e))?;
    let hash = ContentHash::from_bytes(&bytes);
    let size = bytes.len() as u64;

    let meta = TrashMeta {
        path: path.to_string(),
        item_id,
        deleted_at: deleted_at.to_string(),
        hash,
        size,
    };
    let text = meta.to_json().map_err(LocalTrashError::Format)?;
    let meta_p = meta_path(&dir, trash_id);
    let tmp = dir.join(format!("{}.meta.tmp", trash_id.to_hex()));
    fs::write(&tmp, text.as_bytes()).map_err(|e| io_err(&tmp, e))?;
    fs::rename(&tmp, &meta_p).map_err(|e| io_err(&meta_p, e))?;

    Ok(Some(trash_id))
}

/// 读一条本地回收站记录的 `.data` 内容——供测试与「原文件仍可从本地回收站
/// 找回」的验证使用（模块顶部「范围边界」：本切片不提供自动恢复命令）。
pub fn read_content(dataset_root: &Path, id: TrashId) -> Result<Vec<u8>, LocalTrashError> {
    let path = data_path(&trash_dir(dataset_root), id);
    fs::read(&path).map_err(|e| io_err(&path, e))
}

/// 读一条本地回收站记录的 `.meta`。
pub fn read_meta(dataset_root: &Path, id: TrashId) -> Result<TrashMeta, LocalTrashError> {
    let path = meta_path(&trash_dir(dataset_root), id);
    let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
    TrashMeta::parse(&text).map_err(LocalTrashError::Format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::model::ItemId;

    fn item_id() -> ItemId {
        ItemId::from_bytes([0x3f; 16])
    }

    #[test]
    fn 移入回收站后原路径不存在_内容可从回收站读回() {
        let dir = tempfile::tempdir().unwrap();
        let dataset_root = dir.path();
        let source = dataset_root.join("note.txt");
        fs::write(&source, b"hello arca").unwrap();

        let trash_id = move_to_trash(
            dataset_root,
            &source,
            "note.txt",
            item_id(),
            "2026-08-09T00:00:00Z",
        )
        .unwrap()
        .expect("源文件存在时必须分配 trash_id");

        assert!(!source.exists(), "内容应已被移走，原路径不应再存在");
        assert_eq!(read_content(dataset_root, trash_id).unwrap(), b"hello arca");

        let meta = read_meta(dataset_root, trash_id).unwrap();
        assert_eq!(meta.path, "note.txt");
        assert_eq!(meta.item_id, item_id());
        assert_eq!(meta.size, 10);
    }

    #[test]
    fn 源已不存在时返回none而不是报错() {
        let dir = tempfile::tempdir().unwrap();
        let dataset_root = dir.path();
        let source = dataset_root.join("从未创建过.txt");

        let outcome = move_to_trash(
            dataset_root,
            &source,
            "从未创建过.txt",
            item_id(),
            "2026-08-09T00:00:00Z",
        )
        .unwrap();
        assert_eq!(outcome, None);
    }

    #[test]
    fn 两次移入分配不同的trash_id() {
        let dir = tempfile::tempdir().unwrap();
        let dataset_root = dir.path();
        fs::write(dataset_root.join("a.txt"), b"a").unwrap();
        fs::write(dataset_root.join("b.txt"), b"b").unwrap();

        let id_a = move_to_trash(
            dataset_root,
            &dataset_root.join("a.txt"),
            "a.txt",
            item_id(),
            "2026-08-09T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        let id_b = move_to_trash(
            dataset_root,
            &dataset_root.join("b.txt"),
            "b.txt",
            item_id(),
            "2026-08-09T00:00:00Z",
        )
        .unwrap()
        .unwrap();

        assert_ne!(id_a, id_b);
    }
}
