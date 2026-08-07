//! git 仓库的薄封装——不重新实现 git 语义，只调真的 `git` 子进程（spec §4.3–§4.4）。
//!
//! `arca-git` 不受 `arca-core` 的 sans-io 约束：这里就是要跑 `std::process::Command`。

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// git 相关操作的失败。彼此可区分（I5：如实报告失败的性质，不折叠成一种"出错了"）。
#[derive(Debug)]
pub enum GitError {
    /// 目标路径不是一个 git 仓库（`git rev-parse --is-inside-work-tree` 失败）。
    NotARepo { path: PathBuf },
    /// 系统上找不到可执行的 `git`。
    GitNotFound,
    /// git 命令跑起来了，但退出码表示出错（而不是"未命中"这类正常的非零码——
    /// 例如 `check_ignore` 自己会先把 `git check-ignore` 的 0/1 消化掉，
    /// 不会走到这个分支）。
    CommandFailed {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },
    /// 子进程调度/管道读取本身失败（非"找不到 git"的其它 IO 错误）。
    Io(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::NotARepo { path } => {
                write!(f, "{} 不是一个 git 仓库", path.display())
            }
            GitError::GitNotFound => write!(f, "找不到可执行的 git 命令，请确认已安装 git"),
            GitError::CommandFailed { cmd, code, stderr } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "被信号终止".to_string());
                write!(f, "命令 `{cmd}` 失败（退出码 {code}）：{stderr}")
            }
            GitError::Io(msg) => write!(f, "调用 git 失败：{msg}"),
        }
    }
}

impl std::error::Error for GitError {}

fn map_spawn_error(e: std::io::Error) -> GitError {
    if e.kind() == std::io::ErrorKind::NotFound {
        GitError::GitNotFound
    } else {
        GitError::Io(e.to_string())
    }
}

/// 一个已确认存在的 git 仓库；后续操作都在其工作树根下执行。
#[derive(Debug)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// 打开 `path`，校验它确实处在一个 git 工作树里
    /// （`git rev-parse --is-inside-work-tree`）。不创建、不初始化任何内容。
    pub fn open(path: &Path) -> Result<Self, GitError> {
        let output = run(Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(path))?;
        if !output.status.success() {
            return Err(GitError::NotARepo {
                path: path.to_path_buf(),
            });
        }
        Ok(Repo {
            root: path.to_path_buf(),
        })
    }

    /// 仓库工作树根路径（即 `open` 时传入的路径）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 调真的 `git check-ignore`，返回 `path` 当前是否会被忽略。
    ///
    /// **退出码语义**（不是"成功/失败"）：`0` = 该路径被忽略、`1` = 未被忽略、
    /// 其余（通常 `128`）= 用法错误或其它异常，只有这种情况才映射为 `Err`。
    /// 把 `1` 当成命令失败是最容易犯的错——那样"未被忽略"这个完全正常的结果
    /// 会被误判为异常。
    pub fn check_ignore(&self, path: &str) -> Result<bool, GitError> {
        let output = run(Command::new("git")
            .args(["check-ignore", "-q", "--"])
            .arg(path)
            .current_dir(&self.root))?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            code => Err(GitError::CommandFailed {
                cmd: format!("git check-ignore -q -- {path}"),
                code,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
        }
    }

    /// 列出当前被 git 追踪的文件（相对仓库根的路径），用于追踪冲突检测
    /// （`tracking::check_vault` 的 `AlreadyTracked`）。
    pub fn ls_files(&self) -> Result<Vec<String>, GitError> {
        let output = run(Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(&self.root))?;
        if !output.status.success() {
            return Err(GitError::CommandFailed {
                cmd: "git ls-files -z".to_string(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(text
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    }
}

fn run(cmd: &mut Command) -> Result<Output, GitError> {
    cmd.output().map_err(map_spawn_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 完整的行为覆盖（真的建仓库、真的跑 `git check-ignore`）在
    // `tests/ignore_block.rs`——那才是本模块唯一有意义的验证方式，见其文件头注释。

    #[test]
    fn 打开不存在的仓库返回错误() {
        let dir = std::env::temp_dir();
        // 用一个几乎不可能是 git 仓库、也不可能存在的路径。
        let path = dir.join("arca-git-repo-open-test-definitely-not-a-repo-xyz123");
        let result = Repo::open(&path);
        assert!(result.is_err());
    }
}
