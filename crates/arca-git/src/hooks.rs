//! git 钩子：pre-push 一致性钩子 + 预留钩子点（spec §3.2、§4.4.2）。
//!
//! pre-push（沿用 Git LFS 惯例，由 `arca init` 安装，可拒绝、可 `--no-verify` 绕过）：
//! 本次推送涉及的清单条目未全部在 hub 落地 → 阻止 push，列出未上传文件与进度。
//! **只读不改**：从不修改提交、从不自动 push，只做一致性断言（I5）。
//!
//! 预留钩子点（机制不是策略）：post-pull、pre-adopt、post-conflict。
//!
//! 实际的"清单条目是否都已在 hub 落地"检查属于 M1d。本模块只负责钩子脚本
//! 的**安装与卸载**——脚本本身把检查工作转交给一个约定好的命令名
//! `arca-push-check`（见 [`render_script`]），M1d 落地前该命令还不存在，
//! 脚本据此优雅降级：打印提示并放行，不阻塞用户的 `git push`。

use crate::repo::{GitError, Repo};
use std::path::{Path, PathBuf};

/// 钩子脚本第二行的标记，用来区分"arca 自己装的钩子"与"别的工具/用户手写的钩子"。
/// `install_pre_push` 与 `uninstall_pre_push` 都只认这行标记：不带它的文件绝不
/// 覆盖、绝不删除（I6：不污染用户目录）。
const MARKER: &str =
    "# arca:pre-push-hook v1 (managed by arca-git hooks::install_pre_push; do not edit)";

/// 约定的检查命令名。钩子脚本只负责转发标准 pre-push 参数（远端名、URL）与
/// stdin（待推送的引用列表），真正的"清单是否已在 hub 落地"逻辑由 M1d 实现
/// 并提供这个可执行文件；M1d 落地前它不存在，脚本会检测到并放行。
const CHECK_COMMAND: &str = "arca-push-check";

/// `install_pre_push` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// 之前没有 pre-push 钩子（或钩子目录为空），本次成功安装。
    Installed,
    /// 已经是 arca 自己装的钩子（标记匹配），无需改动——重复安装是幂等的。
    AlreadyInstalled,
    /// 已存在一个不带 arca 标记的 pre-push 钩子（别的工具或用户手写）。
    /// 绝不覆盖，原样报告交给调用方决定（I6）。
    RefusedForeignHook { existing_path: PathBuf },
}

/// 生成 pre-push 钩子脚本正文（POSIX `sh`，含 `#!/bin/sh` 起始行）。
fn render_script() -> String {
    format!(
        "#!/bin/sh\n\
{MARKER}\n\
# pre-push 一致性钩子（spec §4.4.2）：只读不改——从不修改提交、从不自动 push，\n\
# 只断言\"本次推送涉及的清单条目是否都已在 hub 落地\"（I5：绝不猜测）。\n\
#\n\
# 实际检查逻辑属于 M1d，本钩子只把标准 pre-push 参数与 stdin（待推送的引用列表）\n\
# 原样转交给约定的检查命令 `{CHECK_COMMAND}`。M1d 落地前该命令还不存在——\n\
# 找不到就打印提示并放行，不能因为检查还没实现就把用户挡在 `git push` 门外。\n\
if ! command -v {CHECK_COMMAND} >/dev/null 2>&1; then\n\
    echo \"arca: 未找到 {CHECK_COMMAND}，跳过 pre-push 一致性检查（该检查随 M1d 落地）\" >&2\n\
    exit 0\n\
fi\n\
\n\
exec {CHECK_COMMAND} \"$@\"\n"
    )
}

/// 安装 pre-push 钩子（spec §4.4.2）。定位真正生效的 hooks 目录时经
/// [`Repo::git_path`]，因此会正确处理 `core.hooksPath` 一类的重定位。
///
/// 已存在他人的 pre-push 钩子时拒绝覆盖，返回 [`InstallOutcome::RefusedForeignHook`]，
/// 绝不静默替换（I6）。已装过 arca 自己的钩子时返回 `AlreadyInstalled`，不重写文件——
/// 幂等。
pub fn install_pre_push(repo: &Repo) -> Result<InstallOutcome, GitError> {
    let hook_path = repo.git_path("hooks/pre-push")?;

    if hook_path.exists() {
        let content = std::fs::read_to_string(&hook_path)
            .map_err(|e| GitError::Io(format!("读取已有 pre-push 钩子失败：{e}")))?;
        return if content.contains(MARKER) {
            Ok(InstallOutcome::AlreadyInstalled)
        } else {
            Ok(InstallOutcome::RefusedForeignHook {
                existing_path: hook_path,
            })
        };
    }

    if let Some(parent) = hook_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GitError::Io(format!("创建 hooks 目录失败：{e}")))?;
    }
    std::fs::write(&hook_path, render_script())
        .map_err(|e| GitError::Io(format!("写入 pre-push 钩子失败：{e}")))?;
    set_executable(&hook_path)?;
    Ok(InstallOutcome::Installed)
}

/// 卸载 pre-push 钩子：只删 arca 自己装的那个（标记匹配）。钩子不存在，或存在但
/// 不带 arca 标记（别的工具/用户装的），都原样跳过，绝不误删（I6）。
pub fn uninstall_pre_push(repo: &Repo) -> Result<(), GitError> {
    let hook_path = repo.git_path("hooks/pre-push")?;
    if !hook_path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&hook_path)
        .map_err(|e| GitError::Io(format!("读取已有 pre-push 钩子失败：{e}")))?;
    if !content.contains(MARKER) {
        return Ok(());
    }
    std::fs::remove_file(&hook_path)
        .map_err(|e| GitError::Io(format!("删除 pre-push 钩子失败：{e}")))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), GitError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| GitError::Io(format!("读取钩子文件权限失败：{e}")))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .map_err(|e| GitError::Io(format!("设置钩子文件可执行权限失败：{e}")))?;
    Ok(())
}

// 非 unix 平台（如 Windows）没有可执行位这个概念；git-for-windows 通过
// shebang/文件扩展名识别可执行钩子，无需额外设置权限。
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), GitError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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

    #[test]
    fn 安装后钩子存在且可执行且带有标记() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let outcome = install_pre_push(&repo).unwrap();
        assert_eq!(outcome, InstallOutcome::Installed);

        let hook_path = dir.path().join(".git/hooks/pre-push");
        assert!(hook_path.is_file(), "pre-push 钩子文件必须存在");
        let content = std::fs::read_to_string(&hook_path).unwrap();
        assert!(content.starts_with("#!/bin/sh\n"));
        assert!(content.contains(MARKER));
        assert!(content.contains(CHECK_COMMAND));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook_path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "钩子文件必须对 owner/group/other 均可执行"
            );
        }
    }

    #[test]
    fn 重复安装是幂等的() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let first = install_pre_push(&repo).unwrap();
        assert_eq!(first, InstallOutcome::Installed);
        let hook_path = dir.path().join(".git/hooks/pre-push");
        let content_after_first = std::fs::read_to_string(&hook_path).unwrap();

        let second = install_pre_push(&repo).unwrap();
        assert_eq!(second, InstallOutcome::AlreadyInstalled);
        let content_after_second = std::fs::read_to_string(&hook_path).unwrap();
        assert_eq!(
            content_after_first, content_after_second,
            "重复安装不应改动已有的 arca 钩子文件"
        );
    }

    #[test]
    fn 存在他人钩子时拒绝且不覆盖() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = dir.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-push");
        let foreign_content = "#!/bin/sh\necho 别的工具装的钩子\n";
        std::fs::write(&hook_path, foreign_content).unwrap();

        let outcome = install_pre_push(&repo).unwrap();
        match outcome {
            InstallOutcome::RefusedForeignHook { existing_path } => {
                assert_eq!(existing_path, hook_path);
            }
            other => panic!("应当拒绝覆盖他人钩子，实际得到 {other:?}"),
        }

        let content_after = std::fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content_after, foreign_content, "他人的钩子内容必须原样保留");
    }

    #[test]
    fn 卸载只删_arca_自己装的钩子() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        install_pre_push(&repo).unwrap();
        let hook_path = dir.path().join(".git/hooks/pre-push");
        assert!(hook_path.is_file());

        uninstall_pre_push(&repo).unwrap();
        assert!(!hook_path.exists(), "arca 自己装的钩子必须被卸载删除");
    }

    #[test]
    fn 卸载不删除他人的钩子() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = dir.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-push");
        let foreign_content = "#!/bin/sh\necho 别的工具装的钩子\n";
        std::fs::write(&hook_path, foreign_content).unwrap();

        uninstall_pre_push(&repo).unwrap();

        assert!(hook_path.is_file(), "他人的钩子不应被卸载删除");
        assert_eq!(
            std::fs::read_to_string(&hook_path).unwrap(),
            foreign_content
        );
    }

    #[test]
    fn 卸载不存在的钩子是无操作() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        // 没装过钩子，卸载应当直接成功，不报错。
        assert!(uninstall_pre_push(&repo).is_ok());
    }
}
