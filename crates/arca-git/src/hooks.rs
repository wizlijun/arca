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
//!
//! `arca-push-check` 命令名契约：它是**独立可执行文件**（放进 `$PATH`），
//! 不是 `arca` 的子命令——钩子脚本只会 `command -v arca-push-check`/
//! `exec arca-push-check "$@"`，从不 `exec arca push-check`。M1d 落地时
//! 无论选哪个 crate 提供它，都必须保留这个二进制名字，否则所有已安装的
//! pre-push 钩子会集体失效（优雅降级为"永远放行"，而不是报错——降级本身
//! 是设计好的，但命令名一变就没人能触发真正的检查了）。
//!
//! TODO(M1)：`arca doctor` 检出「本地存在但 hub 尚无副本」的义务钉在这里，
//! 让它 grep 可达——现在这条义务只活在 spec §13 与散落的散文里。见
//! `tests/nightmare.rs` 里 `git_clean_xdf_不删除受管二进制` 的完整实测记录：
//! `git clean -xdf`/`-Xdf` 都会真删还没推送到 hub 的受管二进制，`arca doctor`
//! 检出未上传文件并显著告警是目前唯一的缓解措施，而 `arca-push-check`
//! （本模块的钩子脚本转交检查的对象）与 `arca doctor` 大概率共享同一段
//! "清单条目是否已在 hub 落地"判断逻辑，落地时应当放在同一处、别写两遍。

use crate::repo::{GitError, Repo};
use std::path::{Path, PathBuf};

/// 钩子脚本第二行标记的**版本无关前缀**，只用来判断"这是不是 arca 自己装的
/// 钩子"，不要求正文逐字节相同。`install_pre_push`/`uninstall_pre_push` 都
/// 只认这个前缀，不带它的文件绝不覆盖、绝不删除（I6：不污染用户目录）。
///
/// 与完整的 [`MARKER`]（含版本号）分开：用带版本号的整行做"识别"曾经导致
/// 两个真实后果——用户把 arca 装的钩子改坏但保留了标记行，`content.contains(MARKER)`
/// 仍然精确匹配，`install_pre_push` 永远判定 `AlreadyInstalled`、不重写，
/// 一致性检查从此静默地永远不跑；脚本模板将来升到 v2，存量 v1 钩子的标记行
/// 不再匹配新 `MARKER`，会被误判成别人装的钩子而拒绝安装/升级（评审 Important #7）。
/// 前缀匹配把"识别归属"与"内容是否需要重写"拆成两个独立判断，见
/// [`install_pre_push`]。
const MARKER_PREFIX: &str = "# arca:pre-push-hook";

/// 当前模板版本的完整标记行，写入新生成的钩子脚本正文。
const MARKER: &str =
    "# arca:pre-push-hook v1 (managed by arca-git hooks::install_pre_push; do not edit)";

/// 约定的检查命令名。钩子脚本只负责转发标准 pre-push 参数（远端名、URL）与
/// stdin（待推送的引用列表），真正的"清单是否已在 hub 落地"逻辑由 M1d 实现
/// 并提供这个可执行文件；M1d 落地前它不存在，脚本会检测到并放行。是**独立
/// 可执行文件**的名字，不是 `arca` 的子命令（见模块 doc comment）。
const CHECK_COMMAND: &str = "arca-push-check";

/// `install_pre_push` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// 之前没有 pre-push 钩子（或钩子目录为空），本次成功安装。
    Installed,
    /// 已经是 arca 自己装的钩子（标记前缀匹配）且正文与当前模板完全一致，
    /// 无需改动——重复安装是幂等的。
    AlreadyInstalled,
    /// 已认领的 arca 钩子（标记前缀匹配）但正文与当前模板不一致，已用当前
    /// 模板重写。两种成因都会落到这个变体：用户手改了内容但保留了标记行
    /// （借此机会修复回可工作状态）；或脚本模板升级到新版本，旧版本装的
    /// 钩子据此原地升级（评审 Important #7：钩子需要修复/升级通路）。
    Rewritten,
    /// 已存在一个不带 arca 标记前缀的 pre-push 钩子（别的工具或用户手写），
    /// 或者该路径本身是一个符号链接（无论指向何处、是否悬空——写入/设置
    /// 可执行位都会跟随链接，可能落到仓库外任意路径，arca 从不以符号链接
    /// 形式安装钩子，遇到的符号链接必然不是自己装的，评审 Important #4）。
    /// 绝不覆盖，原样报告交给调用方决定（I6）。
    RefusedForeignHook { existing_path: PathBuf },
}

/// 生成 pre-push 钩子脚本正文（POSIX `sh`，含 `#!/bin/sh` 起始行）。
///
/// 用逐行 `push_str` 而不是一个跨多行、靠 `\` 续行拼起来的 `format!` 字符串——
/// Rust 的字符串续行会吃掉下一行**全部**前导空白，之前这里想写的 `if` 块缩进
/// （`    echo ...`/`    exit 0`）实际上从未出现在生成的脚本里，纯观感问题但
/// 足够让人怀疑自己是不是记错了缩进（评审 Minor）。逐行拼接不依赖续行语义，
/// 缩进所见即所得。
fn render_script() -> String {
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str(MARKER);
    s.push('\n');
    s.push_str("# pre-push 一致性钩子（spec §4.4.2）：只读不改——从不修改提交、从不自动 push，\n");
    s.push_str("# 只断言\"本次推送涉及的清单条目是否都已在 hub 落地\"（I5：绝不猜测）。\n");
    s.push_str("#\n");
    s.push_str(
        "# 实际检查逻辑属于 M1d，本钩子只把标准 pre-push 参数与 stdin（待推送的引用列表）\n",
    );
    s.push_str(&format!(
        "# 原样转交给约定的检查命令 `{CHECK_COMMAND}`。M1d 落地前该命令还不存在——\n"
    ));
    s.push_str("# 找不到就打印提示并放行，不能因为检查还没实现就把用户挡在 `git push` 门外。\n");
    s.push_str(&format!(
        "if ! command -v {CHECK_COMMAND} >/dev/null 2>&1; then\n"
    ));
    s.push_str(&format!(
        "    echo \"arca: 未找到 {CHECK_COMMAND}，跳过 pre-push 一致性检查（该检查随 M1d 落地）\" >&2\n"
    ));
    s.push_str("    exit 0\n");
    s.push_str("fi\n");
    s.push('\n');
    s.push_str(&format!("exec {CHECK_COMMAND} \"$@\"\n"));
    s
}

/// 安装 pre-push 钩子（spec §4.4.2）。定位真正生效的 hooks 目录时经
/// [`Repo::git_path`]，因此会正确处理 `core.hooksPath` 一类的重定位。
///
/// 已存在他人的 pre-push 钩子（或该路径本身是符号链接）时拒绝覆盖，返回
/// [`InstallOutcome::RefusedForeignHook`]，绝不静默替换（I6）。已装过 arca
/// 自己的钩子时：正文与当前模板一致返回 `AlreadyInstalled`（幂等，不重写
/// 文件）；正文不一致（被改坏，或旧版本模板）返回 `Rewritten`（评审
/// Important #7）。
///
/// **判断"钩子是否已存在"用 [`std::fs::symlink_metadata`]（不跟随链接），
/// 不用 `Path::exists()`**（评审 Important #4）：`exists()` 对悬空符号链接
/// 返回 `false`——若 `hooks/pre-push` 是一个指向仓库外、目标不存在的悬空
/// 链接，旧实现会误判"没装过钩子"，落到 `fs::write`（跟随链接写入）与
/// `set_executable`（同样跟随链接 chmod），把一个可执行文件创建/赋权在
/// **仓库外任意路径**——这与 M1a 在 `arca-store` 加固 tmp 符号链接是同一类
/// 边界，标准应当一致：不跟随链接判断存在性，是符号链接就拒绝，无论悬空
/// 与否、无论指向哪里。
pub fn install_pre_push(repo: &Repo) -> Result<InstallOutcome, GitError> {
    let hook_path = repo.git_path("hooks/pre-push")?;

    match std::fs::symlink_metadata(&hook_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Ok(InstallOutcome::RefusedForeignHook {
                    existing_path: hook_path,
                });
            }
            let content = std::fs::read_to_string(&hook_path)
                .map_err(|e| GitError::Io(format!("读取已有 pre-push 钩子失败：{e}")))?;
            if !content.contains(MARKER_PREFIX) {
                return Ok(InstallOutcome::RefusedForeignHook {
                    existing_path: hook_path,
                });
            }
            let template = render_script();
            if content == template {
                return Ok(InstallOutcome::AlreadyInstalled);
            }
            // 已认领但内容对不上当前模板：可能是用户手改坏了、也可能是旧版本
            // 模板需要升级，两种情况都用当前模板重写，恢复到可工作状态。
            std::fs::write(&hook_path, &template)
                .map_err(|e| GitError::Io(format!("重写 pre-push 钩子失败：{e}")))?;
            set_executable(&hook_path)?;
            return Ok(InstallOutcome::Rewritten);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 真的不存在，继续走全新安装路径。
        }
        Err(e) => {
            return Err(GitError::Io(format!("检查已有 pre-push 钩子失败：{e}")));
        }
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

/// 卸载 pre-push 钩子：只删 arca 自己装的那个（标记前缀匹配）。钩子不存在，
/// 或存在但不带 arca 标记前缀（别的工具/用户装的），都原样跳过，绝不误删（I6）。
///
/// 同样用 `symlink_metadata` 判断而不跟随链接（与 [`install_pre_push`] 对称）：
/// arca 从不以符号链接形式安装钩子，这个路径上遇到的任何符号链接都必然不是
/// arca 自己装的，直接跳过、绝不删除（评审 Important #4）。
pub fn uninstall_pre_push(repo: &Repo) -> Result<(), GitError> {
    let hook_path = repo.git_path("hooks/pre-push")?;
    let meta = match std::fs::symlink_metadata(&hook_path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(GitError::Io(format!("检查已有 pre-push 钩子失败：{e}"))),
    };
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&hook_path)
        .map_err(|e| GitError::Io(format!("读取已有 pre-push 钩子失败：{e}")))?;
    if !content.contains(MARKER_PREFIX) {
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

        // 经 `repo.git_path` 定位，而不是从 `dir.path()` 手拼——`Repo::open`
        // 现在把 `root()` 归一化到 `git rev-parse --show-toplevel` 的结果，
        // 在 tempdir 路径本身含符号链接的平台上（如 macOS 的 /var → /private/var）
        // 可能与 `dir.path()` 不是同一段字节，但指向同一个文件。
        let hook_path = repo.git_path("hooks/pre-push").unwrap();
        assert!(hook_path.is_file(), "pre-push 钩子文件必须存在");
        let content = std::fs::read_to_string(&hook_path).unwrap();
        assert!(content.starts_with("#!/bin/sh\n"));
        assert!(content.contains(MARKER));
        assert!(content.contains(CHECK_COMMAND));
        // 评审 Minor：`if` 块的缩进曾经被 Rust 字符串续行吃掉，生成的脚本里
        // `echo`/`exit 0` 实际上没有缩进。改成逐行 `push_str` 后必须真的所见即所得。
        assert!(
            content.contains("\n    echo \"arca: 未找到"),
            "if 块内的 echo 必须保留 4 空格缩进：{content:?}"
        );
        assert!(
            content.contains("\n    exit 0\n"),
            "if 块内的 exit 0 必须保留 4 空格缩进：{content:?}"
        );

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
        let hook_path = repo.git_path("hooks/pre-push").unwrap();
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

        let hooks_dir = repo.git_path("hooks").unwrap();
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
        let hook_path = repo.git_path("hooks/pre-push").unwrap();
        assert!(hook_path.is_file());

        uninstall_pre_push(&repo).unwrap();
        assert!(!hook_path.exists(), "arca 自己装的钩子必须被卸载删除");
    }

    #[test]
    fn 卸载不删除他人的钩子() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = repo.git_path("hooks").unwrap();
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

    // --- 评审 Important #4：install/uninstall 绝不跟随符号链接 ---

    #[test]
    #[cfg(unix)]
    fn install_拒绝悬空符号链接_不在仓库外创建文件() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = repo.git_path("hooks").unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-push");

        // 复现评审场景：pre-push 是一个指向仓库外、目标不存在的悬空符号链接。
        let outside = tempfile::tempdir().unwrap();
        let outside_target = outside.path().join("not-created-yet");
        std::os::unix::fs::symlink(&outside_target, &hook_path).unwrap();

        let outcome = install_pre_push(&repo).unwrap();
        match outcome {
            InstallOutcome::RefusedForeignHook { existing_path } => {
                assert_eq!(existing_path, hook_path);
            }
            other => panic!("悬空符号链接必须被拒绝，实际得到 {other:?}"),
        }

        // 关键断言：旧实现会在这里 `fs::write` 跟随链接，把文件创建在仓库外。
        assert!(
            std::fs::symlink_metadata(&outside_target).is_err(),
            "绝不能在仓库外的符号链接目标处创建文件"
        );
        // hook_path 本身仍然是符号链接，没有被替换成普通文件。
        assert!(
            std::fs::symlink_metadata(&hook_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "拒绝安装时不应该改动 hook_path 本身"
        );
    }

    #[test]
    #[cfg(unix)]
    fn install_拒绝指向仓库外现有文件的符号链接_不改动目标内容() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = repo.git_path("hooks").unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-push");

        let outside = tempfile::tempdir().unwrap();
        let outside_target = outside.path().join("some-other-file");
        std::fs::write(&outside_target, b"not an arca hook").unwrap();
        std::os::unix::fs::symlink(&outside_target, &hook_path).unwrap();

        let outcome = install_pre_push(&repo).unwrap();
        assert!(
            matches!(outcome, InstallOutcome::RefusedForeignHook { .. }),
            "指向仓库外文件的符号链接必须被拒绝，实际得到 {outcome:?}"
        );
        assert_eq!(
            std::fs::read(&outside_target).unwrap(),
            b"not an arca hook",
            "绝不能改动符号链接指向的仓库外文件内容"
        );
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_不跟随也不删除悬空符号链接() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = repo.git_path("hooks").unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-push");
        std::os::unix::fs::symlink("/does/not/exist/anywhere", &hook_path).unwrap();

        assert!(uninstall_pre_push(&repo).is_ok());
        assert!(
            std::fs::symlink_metadata(&hook_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "arca 从不以符号链接形式安装钩子，遇到的符号链接不是自己装的，绝不删除"
        );
    }

    // --- 评审 Important #7：钩子需要修复/升级通路 ---

    #[test]
    fn install_修复被改坏但保留标记行的钩子() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = repo.git_path("hooks").unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-push");
        // 用户手改了正文，但标记行还在——旧实现会判定 AlreadyInstalled，
        // 永远不重写，一致性检查从此静默失效。
        let tampered = format!("#!/bin/sh\n{MARKER}\necho 用户手改过这里\n");
        std::fs::write(&hook_path, &tampered).unwrap();

        let outcome = install_pre_push(&repo).unwrap();
        assert_eq!(outcome, InstallOutcome::Rewritten);
        assert_eq!(
            std::fs::read_to_string(&hook_path).unwrap(),
            render_script(),
            "被改坏的钩子必须用当前模板重写"
        );
    }

    #[test]
    fn install_升级旧版本模板标记的钩子() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = repo.git_path("hooks").unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-push");
        // 模拟"旧版本"标记：前缀相同、版本号与正文都不同于当前模板。
        let old_version =
            "#!/bin/sh\n# arca:pre-push-hook v0 (managed by arca-git; do not edit)\necho old\n";
        std::fs::write(&hook_path, old_version).unwrap();

        let outcome = install_pre_push(&repo).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::Rewritten,
            "版本前缀匹配但正文过时的钩子必须被识别为 arca 自己的并原地升级，\
             而不是被当成陌生钩子拒绝"
        );
        assert_eq!(
            std::fs::read_to_string(&hook_path).unwrap(),
            render_script()
        );
    }

    #[test]
    fn install_内容与当前模板完全一致时仍是幂等的() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        install_pre_push(&repo).unwrap();
        let outcome = install_pre_push(&repo).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::AlreadyInstalled,
            "正文与当前模板完全一致时必须是 AlreadyInstalled，不是 Rewritten"
        );
    }

    #[test]
    fn uninstall_能删除旧版本模板标记的钩子() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let repo = Repo::open(dir.path()).unwrap();

        let hooks_dir = repo.git_path("hooks").unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-push");
        let old_version =
            "#!/bin/sh\n# arca:pre-push-hook v0 (managed by arca-git; do not edit)\necho old\n";
        std::fs::write(&hook_path, old_version).unwrap();

        uninstall_pre_push(&repo).unwrap();
        assert!(
            !hook_path.exists(),
            "标记前缀匹配的旧版本钩子也必须能被卸载删除"
        );
    }
}
