//! `arca init`（M1d Task 4）：在 vault 根建 `.gitarca`（若已存在则校验后不
//! 覆盖）、装 pre-push 钩子（可跳过）。
//!
//! 写入前先跑 `tracking::check_vault`，有 [`arca_git::tracking::Issue`] 就
//! 停下报告，不写任何字节（I5）——包括 `.gitarca` 本就还不存在、要新建的
//! 场景：一个全新 vault 上，`check_vault` 面对的是一份空注册表，通常什么都
//! 查不出来；只有当磁盘上已经有孤儿数据集一类的既存问题时才会拦下 `init`，
//! 提示用户先处理干净再继续，而不是在一个已知有问题的状态上又叠一层。

use crate::vault::{self, GITARCA_FILE};
use arca_format::gitarca::Registry;
use arca_git::hooks::{self, InstallOutcome};
use arca_git::repo::{GitError, Repo};
use arca_git::tracking::{self, Issue};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    Installed,
    AlreadyInstalled,
    Rewritten,
    /// 已存在他人（非 arca）安装的 pre-push 钩子——绝不覆盖（I6），原样报告。
    Refused {
        existing_path: PathBuf,
    },
}

impl From<InstallOutcome> for HookOutcome {
    fn from(o: InstallOutcome) -> Self {
        match o {
            InstallOutcome::Installed => HookOutcome::Installed,
            InstallOutcome::AlreadyInstalled => HookOutcome::AlreadyInstalled,
            InstallOutcome::Rewritten => HookOutcome::Rewritten,
            InstallOutcome::RefusedForeignHook { existing_path } => {
                HookOutcome::Refused { existing_path }
            }
        }
    }
}

#[derive(Debug)]
pub struct InitOutcome {
    /// `.gitarca` 是本次新建的（`false` 表示它已存在、本次只做了校验）。
    pub created_gitarca: bool,
    /// `None` 表示调用方要求跳过钩子安装（`--no-hook`）。
    pub hook: Option<HookOutcome>,
    /// 写入前的巡检发现的问题；非空时本次 `init` 已经停下，未创建
    /// `.gitarca`、未安装钩子（下面两个字段在这种情况下恒为 `false`/`None`）。
    pub issues: Vec<Issue>,
}

impl InitOutcome {
    pub fn stopped(&self) -> bool {
        !self.issues.is_empty()
    }
}

#[derive(Debug)]
pub enum InitError {
    Git(GitError),
    Registry(arca_format::error::FormatError),
    Io { path: String, reason: String },
    Hook(GitError),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitError::Git(e) => write!(f, "{e}"),
            InitError::Registry(e) => write!(f, "{GITARCA_FILE} 处理失败：{e}"),
            InitError::Io { path, reason } => write!(f, "{path}：{reason}"),
            InitError::Hook(e) => write!(f, "安装 pre-push 钩子失败：{e}"),
        }
    }
}

impl std::error::Error for InitError {}

/// `start`：vault 内任意路径（含根本身）。`install_hook` 为 `false` 时对应
/// `arca init --no-hook`。
pub fn init(start: &Path, install_hook: bool) -> Result<InitOutcome, InitError> {
    let repo = Repo::open(start).map_err(InitError::Git)?;
    let gitarca_path = repo.root().join(GITARCA_FILE);

    let (registry, created) = match fs::read_to_string(&gitarca_path) {
        Ok(text) => {
            let reg = Registry::parse(&text).map_err(InitError::Registry)?;
            reg.validate().map_err(InitError::Registry)?;
            (reg, false)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            (Registry::new(BTreeMap::new(), Vec::new()), true)
        }
        Err(e) => {
            return Err(InitError::Io {
                path: gitarca_path.display().to_string(),
                reason: e.to_string(),
            })
        }
    };

    let issues = tracking::check_vault(&repo, &registry);
    if !issues.is_empty() {
        return Ok(InitOutcome {
            created_gitarca: false,
            hook: None,
            issues,
        });
    }

    if created {
        let text = registry.to_toml().map_err(InitError::Registry)?;
        vault::write_text_atomic(&gitarca_path, &text).map_err(|e| InitError::Io {
            path: gitarca_path.display().to_string(),
            reason: e.to_string(),
        })?;
    }

    let hook = if install_hook {
        Some(
            hooks::install_pre_push(&repo)
                .map_err(InitError::Hook)?
                .into(),
        )
    } else {
        None
    };

    Ok(InitOutcome {
        created_gitarca: created,
        hook,
        issues: Vec::new(),
    })
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
    fn 全新仓库init创建gitarca并装钩子() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());

        let outcome = init(dir.path(), true).unwrap();
        assert!(!outcome.stopped());
        assert!(outcome.created_gitarca);
        assert_eq!(outcome.hook, Some(HookOutcome::Installed));
        assert!(dir.path().join(GITARCA_FILE).is_file());

        let repo = Repo::open(dir.path()).unwrap();
        let hook_path = repo.git_path("hooks/pre-push").unwrap();
        assert!(hook_path.is_file());
    }

    #[test]
    fn no_hook时不安装钩子() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let outcome = init(dir.path(), false).unwrap();
        assert_eq!(outcome.hook, None);
        let repo = Repo::open(dir.path()).unwrap();
        let hook_path = repo.git_path("hooks/pre-push").unwrap();
        assert!(!hook_path.exists());
    }

    #[test]
    fn 已存在的gitarca不被覆盖() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let original = "schema = 1\n[hub.home]\ninstance_id = \"3f2a000000000000000000000000beef\"\nurl = \"file:///tmp/x\"\n";
        fs::write(dir.path().join(GITARCA_FILE), original).unwrap();

        let outcome = init(dir.path(), false).unwrap();
        assert!(!outcome.created_gitarca);
        assert_eq!(
            fs::read_to_string(dir.path().join(GITARCA_FILE)).unwrap(),
            original,
            "已存在的 .gitarca 必须逐字节保留，不得被覆盖"
        );
    }

    #[test]
    fn 已存在但内部不一致的gitarca报错而不是继续() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        fs::write(
            dir.path().join(GITARCA_FILE),
            "schema = 1\n[[dataset]]\npath = \"a\"\nhub = \"ghost\"\n",
        )
        .unwrap();
        assert!(init(dir.path(), false).is_err());
    }

    #[test]
    fn 磁盘上已有孤儿数据集时停下报告而不写入任何东西() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        fs::create_dir_all(dir.path().join("orphan/.arca")).unwrap();
        fs::write(
            dir.path().join("orphan/.arca/dataset.toml"),
            "schema = 1\ndataset_id = \"9c41000000000000000000000000abcd\"\nhub_instance_id = \"3f2a000000000000000000000000beef\"\n",
        )
        .unwrap();

        let outcome = init(dir.path(), true).unwrap();
        assert!(outcome.stopped());
        assert!(!outcome.created_gitarca);
        assert_eq!(outcome.hook, None);
        assert!(
            !dir.path().join(GITARCA_FILE).exists(),
            "有 Issue 时不应该写入 .gitarca"
        );
        let repo = Repo::open(dir.path()).unwrap();
        let hook_path = repo.git_path("hooks/pre-push").unwrap();
        assert!(!hook_path.exists(), "有 Issue 时不应该安装钩子");
    }

    #[test]
    fn 从工作树子目录调用也能正确定位vault根() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        fs::create_dir_all(dir.path().join("sub")).unwrap();

        let outcome = init(&dir.path().join("sub"), false).unwrap();
        assert!(outcome.created_gitarca);
        assert!(dir.path().join(GITARCA_FILE).is_file());
    }

    #[test]
    fn 重复init是幂等的() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let first = init(dir.path(), true).unwrap();
        assert_eq!(first.hook, Some(HookOutcome::Installed));
        let second = init(dir.path(), true).unwrap();
        assert!(!second.created_gitarca);
        assert_eq!(second.hook, Some(HookOutcome::AlreadyInstalled));
    }
}
