//! 进程间接口：agentd 与 CLI 之间的状态面（M3b Task 4，FORMAT.md §9.7）。
//!
//! 占位符适配层（arca-winfs 直连 / arca-macfs 经 XPC，spec §3）需要的双向
//! IPC 属 M3d/M4——那是另一件事，不该为了它现在就架一个 socket。
//!
//! 目前只有一个方向：agentd **写**心跳，`arca status` **读**它。因为目前
//! 需要回答的只是「自动同步在不在跑、上次成功是什么时候」，而一个原子写入
//! 的小 JSON 文件就能诚实地回答它，且天然满足「agentd 不在时读取方照常
//! 工作」（分层降级关系，spec §3.1）。
//!
//! # 心跳文件存在 ≠ agentd 在运行
//!
//! `kill -9` 时来不及删它。所以**新鲜度由读取方校验**：`beat_at` 太旧就
//! 只能说「可能已不在运行」。拿着一个三天前的心跳报告「自动同步正常」，
//! 比什么都不报告更糟——后者让人去查，前者让人放心。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 心跳文件相对 vault 根的位置。**vault 侧**而不是数据集侧——一个 agentd
/// 进程管整个 vault 的全部数据集，心跳是进程级事实。
const STATUS_REL: &str = ".arca/agentd-status.json";

/// 心跳写入间隔。读取方用它的倍数判断陈旧。
pub const BEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Heartbeat {
    pub schema: u32,
    pub pid: u32,
    pub started_at: String,
    /// **这次心跳的时刻**——新鲜度判据。
    pub beat_at: String,
    pub datasets: Vec<DatasetBeat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetBeat {
    pub path: String,
    pub hub: String,
    /// 本地文件监听是否在用。为假表示这个数据集退回了纯周期模式——
    /// 用户看见「本地改动要等一会儿才同步」时，这一行就是答案。
    pub watching: bool,
    pub last_ok_at: Option<String>,
    pub last_error: Option<String>,
}

// **本模块只写不读。** 读取方是 `arca-cli` 的 `arca status`——它按
// FORMAT.md §9.7 自己解析，不共享类型：依赖方向是 agentd → cli，
// 反过来会成环。在这里再放一个 `read` 只会得到一个零生产调用者的 API
// （M2c 评审 I7 抓过同构的问题），而两份读法一旦并存就会分叉。

fn path_of(vault_root: &Path) -> PathBuf {
    vault_root.join(STATUS_REL)
}

/// 写心跳。tmp → rename：中途崩溃要么留下旧值要么留下新值，绝不留下半份
/// JSON——读取方看见半份 JSON 只能报「读不懂」，而那会被误读成「agentd 坏了」。
pub fn write(vault_root: &Path, beat: &Heartbeat) -> Result<(), String> {
    let path = path_of(vault_root);
    let parent = path.parent().expect("心跳路径必然有父目录");
    fs::create_dir_all(parent).map_err(|e| format!("{}：{e}", parent.display()))?;
    let text = serde_json::to_string_pretty(beat).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("{}：{e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| format!("{}：{e}", path.display()))
}

/// 删除心跳（优雅退出时）。不存在不是错误。
pub fn remove(vault_root: &Path) {
    let _ = fs::remove_file(path_of(vault_root));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 心跳() -> Heartbeat {
        Heartbeat {
            schema: SCHEMA,
            pid: 1234,
            started_at: "2026-08-09T10:00:00Z".into(),
            beat_at: "2026-08-09T10:00:15Z".into(),
            datasets: vec![DatasetBeat {
                path: "assets".into(),
                hub: "home".into(),
                watching: true,
                last_ok_at: Some("2026-08-09T10:00:14Z".into()),
                last_error: None,
            }],
        }
    }

    /// 写出来的东西要是**合法 JSON 且带 schema**——读取方（`arca status`）
    /// 按 FORMAT.md §9.7 解析它，两边只靠这份格式文档对齐。
    #[test]
    fn 写出的心跳含schema且可被独立解析() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), &心跳()).unwrap();
        let text = fs::read_to_string(d.path().join(STATUS_REL)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["schema"].as_u64(), Some(SCHEMA as u64));
        assert_eq!(v["pid"].as_u64(), Some(1234));
        assert!(v["beat_at"].as_str().is_some(), "新鲜度判据必须在");
        assert_eq!(v["datasets"][0]["watching"].as_bool(), Some(true));
    }

    #[test]
    fn remove之后文件消失且重复remove不报错() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), &心跳()).unwrap();
        remove(d.path());
        assert!(!d.path().join(STATUS_REL).exists());
        remove(d.path());
    }

    #[test]
    fn 写入之后不留tmp残留() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), &心跳()).unwrap();
        let leftovers: Vec<_> = fs::read_dir(d.path().join(".arca"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
