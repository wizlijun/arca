//! git 仓库的薄封装——不重新实现 git 语义，只调真的 `git` 子进程（spec §4.3–§4.4）。
//!
//! `arca-git` 不受 `arca-core` 的 sans-io 约束：这里就是要跑 `std::process::Command`。

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// git 相关操作的失败。彼此可区分（I5：如实报告失败的性质，不折叠成一种"出错了"）。
#[derive(Debug)]
pub enum GitError {
    /// `Repo::open` 传入的路径（跟随符号链接后）不存在——路径本身没有对应的
    /// 文件系统项，或是指向不存在目标的悬空符号链接（`open` 会先检查这一点，
    /// 不会让它走到 spawn `git` 子进程那一步）。与 `GitNotFound` 分开报告：
    /// 两者都会让 `git rev-parse` 触发 `ENOENT`，但成因和修复方向完全不同——
    /// `PathNotFound` 该去检查路径，`GitNotFound` 该去装 git（评审 Important #3，I5）。
    PathNotFound { path: PathBuf },
    /// 目标路径存在但不是一个 git 工作树：要么根本不是目录（例如一个普通文件），
    /// 要么是目录但 `git rev-parse --show-toplevel` 失败（不在任何工作树内）。
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
    /// 子进程调度/管道读取本身失败，或钩子安装/卸载时的文件系统操作失败
    /// （非"找不到 git"的其它 IO 错误——两类调用方都不需要比"IO 错误"更细的区分）。
    Io(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::PathNotFound { path } => {
                write!(f, "{} 不存在", path.display())
            }
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
    /// 打开 `path`，校验它确实处在一个 git 工作树里，并把 `root()` 归一化到
    /// **工作树根**（`git rev-parse --show-toplevel`），不管调用方传入的是根目录
    /// 本身还是工作树内的任意子目录。不创建、不初始化任何内容。
    ///
    /// 归一化到工作树根是必须的：`git ls-files` 一类命令的输出是**相对 cwd**的
    /// 路径，而 `tracking::check_vault` 用 `repo.root()` 拼接数据集路径、比对
    /// `ls_files()` 的结果。若把调用方传入的任意子目录原样存成 `root`（旧实现），
    /// M1d 的 `arca init`/`adopt`/`doctor` 一旦像 git 一样从非仓库根的 cwd 调用，
    /// 就会产生假阳性（把子目录当根，路径拼接整体偏移）与假阴性（真正的问题因为
    /// 路径对不上而查不出来）（评审 Important #5）。
    ///
    /// 校验路径本身时**跟随符号链接**（`std::fs::metadata`，而不是
    /// `symlink_metadata`）并显式判断 `is_dir()`：
    /// - 路径不存在，或是指向不存在目标的悬空符号链接 → [`GitError::PathNotFound`]；
    /// - 路径存在但不是目录（例如一个普通文件）→ [`GitError::NotARepo`]
    ///   （不可能是工作树根，但"该去检查 git 是否装好"完全是错误的修复方向）；
    /// - 路径是目录 → 继续用 `git rev-parse --show-toplevel` 判定是否在工作树内。
    ///
    /// 旧实现用 `symlink_metadata` 且不判 `is_dir()`：普通文件与悬空符号链接的
    /// `Command::current_dir` 都会触发 `ENOENT`，与"PATH 里找不到 git 可执行文件"
    /// 触发的 `ENOENT` 在 `io::ErrorKind` 层面无法区分，两者都被误报成
    /// [`GitError::GitNotFound`]，把用户指向"请确认已安装 git"这个错误的方向
    /// （评审 Important #3）。这里改用跟随链接的 `metadata`，与 `hooks.rs`
    /// 安装钩子时改用**不**跟随链接的 `symlink_metadata`（评审 Important #4）
    /// 刚好是对称的一对修复：处理"这是不是一个可信的工作树根"要跟随链接看真实
    /// 目标，处理"要不要在这个路径上写文件"要不跟随链接、拒绝任何符号链接。
    pub fn open(path: &Path) -> Result<Self, GitError> {
        let metadata = std::fs::metadata(path).map_err(|_| GitError::PathNotFound {
            path: path.to_path_buf(),
        })?;
        if !metadata.is_dir() {
            return Err(GitError::NotARepo {
                path: path.to_path_buf(),
            });
        }

        let output = run(Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(path))?;
        if !output.status.success() {
            return Err(GitError::NotARepo {
                path: path.to_path_buf(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let top = text.trim_end_matches(['\n', '\r']);
        if top.is_empty() {
            return Err(GitError::NotARepo {
                path: path.to_path_buf(),
            });
        }
        Ok(Repo {
            root: PathBuf::from(top),
        })
    }

    /// 仓库工作树根路径（`git rev-parse --show-toplevel` 的归一化结果，
    /// 不一定等于 `open` 时传入的路径——见 [`Repo::open`] 的 doc comment）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 调真的 `git check-ignore`（**index 感知**：默认与 git 自身行为一致，
    /// 已被追踪的路径一律报"未忽略"，见下方 doc comment 与 [`Repo::check_ignore_no_index`]），
    /// 返回 `path` 当前是否会被忽略。
    ///
    /// **退出码语义**（不是"成功/失败"）：`0` = 该路径被忽略、`1` = 未被忽略、
    /// 其余（通常 `128`）= 用法错误或其它异常，只有这种情况才映射为 `Err`。
    /// 把 `1` 当成命令失败是最容易犯的错——那样"未被忽略"这个完全正常的结果
    /// 会被误判为异常。
    pub fn check_ignore(&self, path: &str) -> Result<bool, GitError> {
        self.check_ignore_impl(&[], path)
    }

    /// 与 [`Repo::check_ignore`] 相同，但加 `--no-index`：忽略判定**不参考
    /// index**，只看 `.gitignore` 规则本身是否会命中该路径。
    ///
    /// **为什么需要这个变体**（评审 Important #6）：`git check-ignore` 默认查
    /// index，已被追踪的路径一律返回"未忽略"，哪怕 `.gitignore` 规则本身写错了
    /// （例如反选块漏了一行、把本该保留的 `.arca/` 元数据也排除掉）。`arca doctor`
    /// 恰恰最需要在"元数据已经被提交、块后来被改坏"这种场景下验证反选规则本身
    /// 是否正确——用默认的 `check_ignore` 断言会假通过（进程退出码显示"未忽略"，
    /// 但那只是因为文件已经被追踪，不代表规则写对了）。CLAUDE.md 要求
    /// `arca doctor` 断言的是 `git check-ignore` 的**实际结果**，这个方法就是
    /// 让调用方能选择"实际结果"具体指哪一种 index 语义。
    pub fn check_ignore_no_index(&self, path: &str) -> Result<bool, GitError> {
        self.check_ignore_impl(&["--no-index"], path)
    }

    fn check_ignore_impl(&self, extra_args: &[&str], path: &str) -> Result<bool, GitError> {
        let output = run(Command::new("git")
            .args(["check-ignore", "-q"])
            .args(extra_args)
            .arg("--")
            .arg(path)
            .current_dir(&self.root))?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            code => {
                let mut cmd = "git check-ignore -q".to_string();
                for arg in extra_args {
                    cmd.push(' ');
                    cmd.push_str(arg);
                }
                cmd.push_str(" -- ");
                cmd.push_str(path);
                Err(GitError::CommandFailed {
                    cmd,
                    code,
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                })
            }
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

    /// `git rm --cached -- <paths>`：把已追踪的路径从 index 里移除，**只影响索引
    /// 与未来提交，绝不触碰工作树里的文件**（I6：受管文件原地不动）。
    ///
    /// `arca adopt` 的用途：一份"既有附件"在 arca 接管之前可能已经被 `git add`
    /// 过（`.gitignore` 反选块对已追踪路径无效），adopt 写好 `.gitignore` 块后
    /// 还必须把这些路径逐出 index，否则它们会继续被 git 追踪、继续随每次
    /// `git commit` 增长仓库体积——这正是 adopt 存在的意义（阻止未来膨胀）。
    ///
    /// `paths` 为空时直接返回 `Ok(())`，不 spawn 子进程——`git rm --cached --`
    /// 后面不带任何路径会报参数错误，不是"什么都不用做"的静默成功。
    pub fn rm_cached(&self, paths: &[String]) -> Result<(), GitError> {
        if paths.is_empty() {
            return Ok(());
        }
        let output = run(Command::new("git")
            .args(["rm", "--cached", "-q", "--"])
            .args(paths)
            .current_dir(&self.root))?;
        if !output.status.success() {
            return Err(GitError::CommandFailed {
                cmd: format!("git rm --cached -q -- {}", paths.join(" ")),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(())
    }

    /// 解析 `$GIT_DIR` 下的相对路径（`git rev-parse --git-path <rel>`），返回绝对路径。
    ///
    /// 这会考虑 `core.hooksPath` 一类的重定位——钩子安装/卸载（`hooks` 模块）据此定位
    /// 真正生效的 hooks 目录，而不是想当然地拼 `.git/hooks`（那在 `core.hooksPath`
    /// 被设置时就是错的）。
    pub fn git_path(&self, rel: &str) -> Result<PathBuf, GitError> {
        let output = run(Command::new("git")
            .args(["rev-parse", "--git-path"])
            .arg(rel)
            .current_dir(&self.root))?;
        if !output.status.success() {
            return Err(GitError::CommandFailed {
                cmd: format!("git rev-parse --git-path {rel}"),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let rel_path = text.trim_end_matches(['\n', '\r']);
        Ok(self.root.join(rel_path))
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
    fn 打开不存在的路径返回_path_not_found_而不是_git_not_found() {
        let dir = std::env::temp_dir();
        // 用一个几乎不可能存在的路径。
        let path = dir.join("arca-git-repo-open-test-definitely-not-a-repo-xyz123");
        match Repo::open(&path) {
            Err(GitError::PathNotFound { path: reported }) => assert_eq!(reported, path),
            other => panic!("应返回 PathNotFound，实际是 {other:?}"),
        }
    }

    #[test]
    fn 打开存在但不是_git_仓库的路径返回_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        // 路径确实存在，但从未 `git init` 过。
        match Repo::open(dir.path()) {
            Err(GitError::NotARepo { path }) => assert_eq!(path, dir.path()),
            other => panic!("应返回 NotARepo，实际是 {other:?}"),
        }
    }

    fn 建仓库(dir: &Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .expect("需要可用的 git")
                .success();
            assert!(ok, "git {args:?} 失败");
        }
    }

    // --- 评审 Important #3：普通文件 / 悬空符号链接不该被误报成 GitNotFound ---

    #[test]
    fn 打开普通文件返回_not_a_repo_而不是_git_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("just-a-file");
        std::fs::write(&file, b"not a directory").unwrap();
        match Repo::open(&file) {
            Err(GitError::NotARepo { path }) => assert_eq!(path, file),
            other => panic!("应返回 NotARepo，实际是 {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn 打开悬空符号链接返回_path_not_found_而不是_git_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("dangling-link");
        std::os::unix::fs::symlink(dir.path().join("does-not-exist"), &link).unwrap();
        match Repo::open(&link) {
            Err(GitError::PathNotFound { path }) => assert_eq!(path, link),
            other => panic!("应返回 PathNotFound，实际是 {other:?}"),
        }
    }

    // --- 评审 Important #5：从工作树内任意子目录 open 也要归一化到工作树根 ---

    #[test]
    fn 从工作树子目录打开仍归一化到仓库根() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();

        let from_root = Repo::open(dir.path()).unwrap();
        let from_subdir = Repo::open(&dir.path().join("docs")).unwrap();
        assert_eq!(
            from_root.root(),
            from_subdir.root(),
            "无论从仓库根还是从子目录 open，root() 都必须归一化到同一个工作树根，\
             否则 M1d 的 `arca doctor`/`adopt` 从子目录调用时会算出错误的相对路径"
        );
    }

    // --- rm_cached：只动 index，不碰工作树文件（I6） ---

    #[test]
    fn rm_cached_移除已追踪路径但不删工作树文件() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        std::fs::write(dir.path().join("leaked.bin"), b"content").unwrap();
        let ok = Command::new("git")
            .args(["add", "leaked.bin"])
            .current_dir(dir.path())
            .status()
            .expect("需要可用的 git")
            .success();
        assert!(ok, "git add 失败");

        let repo = Repo::open(dir.path()).unwrap();
        assert!(repo.ls_files().unwrap().contains(&"leaked.bin".to_string()));

        repo.rm_cached(&["leaked.bin".to_string()]).unwrap();

        assert!(
            !repo.ls_files().unwrap().contains(&"leaked.bin".to_string()),
            "rm_cached 后不应再被 git 追踪"
        );
        assert!(
            dir.path().join("leaked.bin").is_file(),
            "工作树里的文件必须原地保留，不受影响（I6）"
        );
        assert_eq!(
            std::fs::read(dir.path().join("leaked.bin")).unwrap(),
            b"content"
        );
    }

    #[test]
    fn rm_cached_空列表是无操作不报错() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();
        assert!(repo.rm_cached(&[]).is_ok());
    }

    // --- 评审 Important #6：check_ignore 默认是 index 感知的，doctor 需要 --no-index ---

    #[test]
    fn check_ignore_index_感知_已追踪文件即便匹配规则也报未忽略() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/leaked.bin"), b"leaked").unwrap();
        let ok = Command::new("git")
            .args(["add", "-f", "assets/leaked.bin"])
            .current_dir(dir.path())
            .status()
            .expect("需要可用的 git")
            .success();
        assert!(ok, "git add 失败");

        std::fs::write(
            dir.path().join(".gitignore"),
            crate::ignore_block::render(&["assets"]).unwrap(),
        )
        .unwrap();

        let repo = Repo::open(dir.path()).unwrap();
        // 默认（index 感知，与 git 自身行为一致）：已追踪路径一律报"未忽略"，
        // 哪怕 .gitignore 规则本身会匹配它。
        assert!(
            !repo.check_ignore("assets/leaked.bin").unwrap(),
            "已追踪文件按 git 默认语义必须报未忽略"
        );
        // --no-index：只看规则本身、不查 index——这里规则确实会匹配该路径，
        // 这正是 `arca doctor` 断言"反选块本身是否写对"时需要的语义。
        assert!(
            repo.check_ignore_no_index("assets/leaked.bin").unwrap(),
            "--no-index 必须只看 .gitignore 规则本身，忽略 index 状态"
        );
    }
}
