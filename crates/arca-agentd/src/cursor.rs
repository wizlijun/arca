//! agentd 的增量游标持久化（M3a Task 3，FORMAT.md §9.6）。
//!
//! 记住「上次已经消费到 journal 的哪里」，重启后从这里接上而不是重新扫全库。
//!
//! # 读取侧的纪律：读不懂就当作没有
//!
//! 这与本项目别处的 I5「绝不猜测」不冲突，值得说清楚，否则下一个人会想把
//! 「解析失败」改成硬错误：
//!
//! - **`role.toml` 解析失败必须是错误**——角色决定「远端删除到达时本地副本
//!   是被移除还是被保留」，猜错的后果是数据没了。
//! - **游标解析失败当作没有**——它的后果只是退化成一次全量对账，而全量对账
//!   本来就是这个系统的地基（spec §5.2 三重保险的最后一层）。为一个可再生的
//!   小文件坏了就拒绝启动 daemon，是把一个自愈的问题变成一个需要人干预的问题。
//!
//! 判据不是「要不要猜」，而是「猜错的代价是什么」。这里没有猜测：
//! 「当作没有」是一个明确定义的、保守的退化。

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use arca_format::journal::Cursor;

/// 游标文件相对数据集目录的位置（FORMAT.md §9.6）。
const CURSOR_REL: &str = ".arca/client/changes-cursor";

fn path_of(dataset_dir: &Path) -> PathBuf {
    dataset_dir.join(CURSOR_REL)
}

/// 读取的结果。区分「没有」与「坏了」不是为了让调用方分别处理——两者的
/// 处置完全相同（全量对账）——而是为了让**诊断**能说清楚是哪一种：
/// 「第一次跑」和「上次写坏了」对排障的人是两件事。
#[derive(Debug)]
pub enum Loaded {
    /// 文件不存在：第一次跑，或者被人删过。
    Absent,
    /// 文件在但读不懂。`reason` 供诊断输出。
    Unreadable {
        reason: String,
    },
    Cursor(Cursor),
}

impl fmt::Display for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Loaded::Absent => write!(f, "无游标（将做一次全量对账）"),
            Loaded::Unreadable { reason } => {
                write!(f, "游标文件读不懂（{reason}）——当作没有，将做一次全量对账")
            }
            Loaded::Cursor(c) => write!(f, "游标 {c}"),
        }
    }
}

impl Loaded {
    pub fn as_cursor(&self) -> Option<&Cursor> {
        match self {
            Loaded::Cursor(c) => Some(c),
            _ => None,
        }
    }
}

pub fn load(dataset_dir: &Path) -> Loaded {
    let path = path_of(dataset_dir);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Loaded::Absent,
        Err(e) => {
            return Loaded::Unreadable {
                reason: format!("{}：{e}", path.display()),
            }
        }
    };
    match Cursor::parse(text.trim()) {
        Ok(c) => Loaded::Cursor(c),
        Err(e) => Loaded::Unreadable {
            reason: format!("{}：{e}", path.display()),
        },
    }
}

/// 写入游标。tmp → rename（与 `role.toml`、`baseline.jsonl` 同一纪律）：
/// 中途崩溃要么留下旧值要么留下新值，绝不留下半行。
///
/// **不 fsync**：与 `trace_sink` 同一条理由——这个文件丢了的后果只是一次
/// 全量对账，为它付 fsync 的代价（每轮一次，长期运行的 daemon）不划算。
/// 真正不能丢的是 journal 与 `.txn`，各自已有 fsync 保证。
pub fn save(dataset_dir: &Path, cursor: &Cursor) -> Result<(), String> {
    let path = path_of(dataset_dir);
    let parent = path.parent().expect("游标路径必然有父目录");
    fs::create_dir_all(parent).map_err(|e| format!("{}：{e}", parent.display()))?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{cursor}\n")).map_err(|e| format!("{}：{e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| format!("{}：{e}", path.display()))
}

/// 丢弃游标（收到 `reset_required` 之后）。文件本来就不存在不是错误。
pub fn clear(dataset_dir: &Path) -> Result<(), String> {
    let path = path_of(dataset_dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}：{e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 游标() -> Cursor {
        Cursor {
            epoch: "a".repeat(32),
            seq: 42,
        }
    }

    #[test]
    fn 往返一致() {
        let d = tempfile::tempdir().unwrap();
        save(d.path(), &游标()).unwrap();
        assert_eq!(load(d.path()).as_cursor(), Some(&游标()));
    }

    #[test]
    fn 文件不存在是absent而不是错误() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(load(d.path()), Loaded::Absent));
    }

    /// 读不懂 → `Unreadable`，**不是 panic、不是错误**。agentd 会照常启动
    /// 并做一次全量对账。
    #[test]
    fn 内容损坏时当作没有并给出可诊断的理由() {
        let d = tempfile::tempdir().unwrap();
        save(d.path(), &游标()).unwrap();
        fs::write(d.path().join(CURSOR_REL), "这不是游标").unwrap();
        match load(d.path()) {
            Loaded::Unreadable { reason } => {
                assert!(reason.contains("changes-cursor"), "{reason}");
            }
            other => panic!("应为 Unreadable，实得 {other:?}"),
        }
    }

    /// 路径穿越形态的 epoch 必须被 `Cursor::parse` 挡住——`epoch` 会被拼进
    /// `journal/<epoch>.jsonl` 的磁盘路径片段。这条防线在 `arca-format` 里，
    /// 这里断言它确实作用在本模块的读取路径上。
    #[test]
    fn 路径穿越形态的游标被当作读不懂() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".arca/client")).unwrap();
        fs::write(d.path().join(CURSOR_REL), "../../../../etc/passwd:0").unwrap();
        assert!(matches!(load(d.path()), Loaded::Unreadable { .. }));
    }

    #[test]
    fn 尾随换行与空白被容忍() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".arca/client")).unwrap();
        fs::write(
            d.path().join(CURSOR_REL),
            format!("  {}:42  \n\n", "a".repeat(32)),
        )
        .unwrap();
        assert_eq!(load(d.path()).as_cursor(), Some(&游标()));
    }

    #[test]
    fn clear之后回到absent且重复clear不报错() {
        let d = tempfile::tempdir().unwrap();
        save(d.path(), &游标()).unwrap();
        clear(d.path()).unwrap();
        assert!(matches!(load(d.path()), Loaded::Absent));
        clear(d.path()).unwrap();
    }

    /// 写入不留半行：tmp → rename 之后目录里不该有 `.tmp` 残留。
    #[test]
    fn 写入之后不留tmp残留() {
        let d = tempfile::tempdir().unwrap();
        save(d.path(), &游标()).unwrap();
        let leftovers: Vec<_> = fs::read_dir(d.path().join(".arca/client"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
