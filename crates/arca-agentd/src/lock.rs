//! agentd 的单实例锁（M3a Task 1）。
//!
//! # 为什么锁在 vault 侧而不是存储根侧
//!
//! `arca_store::lock` 已经有一把跨进程锁，但它落在**存储根**里——而存储根
//! 随时可能离线（I11），agentd 恰恰需要在离线期间继续活着（它要负责在盘
//! 挂回来时接上）。把 agentd 的单实例锁挂在一个可能不存在的目录上，等于
//! 让「能不能启动」取决于「外置盘插没插」，那是两件不相干的事。
//!
//! 所以这里另起一把，落在 `<vault>/.arca/agentd.lock`——vault 根就是
//! git 仓库根，agentd 在跑的前提本来就是它存在。
//!
//! # 为什么用 flock 而不是 pid 文件
//!
//! pid 文件的经典问题是**它不会随进程死亡而失效**：agentd 被 `kill -9`
//! 之后留下的 pid 文件，下一次启动要么误判「已经有实例在跑」而拒绝启动
//! （用户被迫手工删文件），要么去 kill 那个 pid——而 pid 早就被系统复用给
//! 别的进程了。`flock`/`LockFileEx` 由内核在进程退出时**无条件**释放，
//! 不论退出得多难看。这与 `arca_store::lock` 是同一条理由，见其模块文档。

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// 锁文件相对 vault 根的位置。
const LOCK_REL: &str = ".arca/agentd.lock";

#[derive(Debug)]
pub enum LockError {
    /// 已经有一个 agentd 在这个 vault 上跑着。**这不是错误状态而是正常的
    /// 竞争结果**，但它必须让第二个实例明确退出，不能静默争抢（I5）。
    AlreadyRunning {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        reason: String,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::AlreadyRunning { path } => write!(
                f,
                "这个 vault 上已经有一个 arca-agentd 在运行（锁：{}）。\
                 同一个 vault 同时跑两个 agentd 会让两条调和回路互相覆盖对方的\
                 基线与游标，因此本实例拒绝启动。如果你确信上一个已经不在了，\
                 它的锁会随进程退出被内核自动释放——不需要手工删除任何文件",
                path.display()
            ),
            LockError::Io { path, reason } => {
                write!(f, "{}：无法取得 agentd 单实例锁：{reason}", path.display())
            }
        }
    }
}

impl std::error::Error for LockError {}

/// 持有中的单实例锁。**析构即释放**（内核在 fd 关闭时释放 flock），
/// 所以调用方只要把它拿在手里活到进程结束即可。
#[derive(Debug)]
pub struct AgentLock {
    // 字段本身不被读取——它的意义完全在于"活着"：一旦 Drop，fd 关闭，
    // 内核释放 flock。
    _file: File,
    path: PathBuf,
}

impl AgentLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 为 `vault_root` 取得单实例锁。**不阻塞**：拿不到就立刻返回
/// [`LockError::AlreadyRunning`]，而不是排队等——一个"等着接班"的 agentd
/// 没有意义，用户想要的是明确的「已经有一个在跑了」。
pub fn acquire(vault_root: &Path) -> Result<AgentLock, LockError> {
    let path = vault_root.join(LOCK_REL);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(&path, e))?;
    }
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| io_err(&path, e))?;

    // 与 `arca_store::lock::acquire` 同一条纪律：完全限定语法调用
    // `fs4::FileExt`，不写成 `file.try_lock()`——Rust 1.89 给
    // `std::fs::File` 加了同名 inherent 方法，方法解析里 inherent 优先，
    // 同一行代码在两种工具链下会调用两个不同的实现（详见那边的长注释）。
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(AgentLock { _file: file, path }),
        Err(fs4::TryLockError::WouldBlock) => Err(LockError::AlreadyRunning { path }),
        Err(fs4::TryLockError::Error(e)) => Err(io_err(&path, e)),
    }
}

fn io_err(path: &Path, e: io::Error) -> LockError {
    LockError::Io {
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 第一个实例拿到锁_第二个被明确拒绝() {
        let vault = tempfile::tempdir().unwrap();
        let first = acquire(vault.path()).unwrap();
        assert!(first.path().ends_with("agentd.lock"));

        match acquire(vault.path()) {
            Err(LockError::AlreadyRunning { .. }) => {}
            other => panic!("第二个实例必须被拒绝，实得 {other:?}"),
        }
    }

    /// 第一个实例退出（`AgentLock` 被 drop）之后，锁必须真的可用了——
    /// 否则 agentd 重启一次就永久起不来。
    #[test]
    fn 前一个实例退出后锁可以被重新取得() {
        let vault = tempfile::tempdir().unwrap();
        {
            let _first = acquire(vault.path()).unwrap();
        }
        acquire(vault.path()).expect("上一个已经 drop，锁应当可用");
    }

    /// 错误信息里必须出现锁文件路径——报障的人第一件事就是问「哪个文件」。
    #[test]
    fn 已在运行的错误信息点名锁文件路径() {
        let vault = tempfile::tempdir().unwrap();
        let _first = acquire(vault.path()).unwrap();
        let msg = acquire(vault.path()).unwrap_err().to_string();
        assert!(msg.contains("agentd.lock"), "{msg}");
        // 并且要明说不需要手工删文件——这是 pid 文件时代留给用户的坏习惯。
        assert!(msg.contains("不需要手工删除"), "{msg}");
    }
}
