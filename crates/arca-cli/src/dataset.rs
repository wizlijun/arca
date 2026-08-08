//! 数据集解析（M1d Task 7 起）：从 vault 根 + 数据集相对路径，解析出数据集
//! 目录、`dataset.toml` 配置、以及它绑定的存储根路径。
//!
//! `sync_cmd`（Task 6，`commands/porcelain.rs`）已经把"打开 vault → 校验路径
//! → 读 dataset.toml → 在 `.gitarca` 里找登记条目 → 解析 hub → 解析存储根
//! 路径"这串逻辑写过一遍；Task 7 新增的 `status`/`verify`/`doctor`/plumbing
//! 四类命令壳都需要同一串逻辑——抽出来是为了让"数据集尚未登记"这类错误的
//! 措辞在所有命令里保持一致，不是为了性能。
//!
//! **不打开存储根**——是否需要身份校验（I11）由调用方决定：`verify` 需要先
//! 严格校验身份再复用 `arca_store::fsck::check_path`（那个函数本身不预设
//! 期望身份，是诊断工具的设计），`ls`/`cat`/`resolve`/`status`/`doctor` 需要
//! 一份已确认身份的 [`arca_store::root::StorageRoot`] 去读 `RemoteState`。
//! 把"打开存储根"这一步留给调用方，本模块只负责"这个存储根在哪"。

use crate::vault::{self, HubRootError, VaultError};
use arca_format::dataset::DatasetConfig;
use arca_format::path_rules::{self, PathStatus};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 一个已解析、但尚未打开存储根的数据集。
#[derive(Debug)]
pub struct ResolvedDataset {
    /// 数据集目录的绝对路径（vault 根 + 归一化后的相对路径）。
    pub dataset_dir: PathBuf,
    /// 归一化后的、相对 vault 根的路径（`/` 分隔）。
    pub normalized_path: String,
    pub cfg: DatasetConfig,
    /// 这个数据集绑定的 hub 对应的本地存储根路径（`vault::resolve_hub_root`
    /// 的产物，尚未 `StorageRoot::open`）。
    pub root_path: PathBuf,
}

/// 解析失败——彼此可区分（I5）。措辞与 `adopt.rs::AdoptError`/
/// `commands/porcelain.rs::sync_cmd` 的既有提示保持一致。
#[derive(Debug)]
pub enum ResolveError {
    Vault(VaultError),
    PathInvalid(PathStatus),
    /// 数据集尚未 `arca register`（找不到 `<path>/.arca/dataset.toml`）。
    NotRegistered {
        path: String,
    },
    DatasetTomlCorrupt(arca_format::error::FormatError),
    /// 数据集路径未在 `.gitarca` 登记，无法解析出它的 hub。
    NotInRegistry {
        path: String,
    },
    HubNotFound {
        hub: String,
    },
    HubRoot(HubRootError),
    Io {
        path: String,
        reason: String,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::Vault(e) => write!(f, "{e}"),
            ResolveError::PathInvalid(s) => write!(f, "数据集路径不合规：{}", s.as_str()),
            ResolveError::NotRegistered { path } => write!(
                f,
                "{path} 尚未登记为数据集（读不到 dataset.toml）——请先运行 `arca register {path}`"
            ),
            ResolveError::DatasetTomlCorrupt(e) => write!(f, "dataset.toml 解析失败：{e}"),
            ResolveError::NotInRegistry { path } => {
                write!(f, "{path} 未在 .gitarca 登记——请先运行 `arca register`")
            }
            ResolveError::HubNotFound { hub } => write!(f, "hub {hub:?} 未在 .gitarca 登记"),
            ResolveError::HubRoot(e) => write!(f, "{e}"),
            ResolveError::Io { path, reason } => write!(f, "{path}：{reason}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// `start`：vault 内任意路径。`path`：数据集相对 vault 根的路径。
/// `root_override`：`--root` 覆盖（外置盘换挂载点等场景）。
pub fn resolve(
    start: &Path,
    path: &str,
    root_override: Option<&Path>,
) -> Result<ResolvedDataset, ResolveError> {
    let vault::Vault { repo, registry } = vault::open(start).map_err(ResolveError::Vault)?;

    let normalized_path = path_rules::check(path).map_err(ResolveError::PathInvalid)?;
    let dataset_dir = repo.root().join(&normalized_path);

    let dataset_toml_path = dataset_dir.join(".arca").join("dataset.toml");
    let cfg_text = match fs::read_to_string(&dataset_toml_path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ResolveError::NotRegistered {
                path: normalized_path,
            })
        }
        Err(e) => {
            return Err(ResolveError::Io {
                path: dataset_toml_path.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    let cfg = DatasetConfig::parse(&cfg_text).map_err(ResolveError::DatasetTomlCorrupt)?;

    let entry = registry
        .datasets()
        .iter()
        .find(|e| path_rules::casefold(&e.path) == path_rules::casefold(&normalized_path))
        .ok_or_else(|| ResolveError::NotInRegistry {
            path: normalized_path.clone(),
        })?;
    let hub = registry
        .hub(&entry.hub)
        .ok_or_else(|| ResolveError::HubNotFound {
            hub: entry.hub.clone(),
        })?;
    let root_path = vault::resolve_hub_root(hub, root_override).map_err(ResolveError::HubRoot)?;

    Ok(ResolvedDataset {
        dataset_dir,
        normalized_path,
        cfg,
        root_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::register::{self, RegisterOptions};
    use crate::vault::GITARCA_FILE;
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
    fn 已登记的数据集解析出预期的存储根路径() {
        let vault_dir = tempfile::tempdir().unwrap();
        建仓库(vault_dir.path());
        fs::write(vault_dir.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_dir.path().join("assets")).unwrap();

        let store_dir = tempfile::tempdir().unwrap();
        let root_path = store_dir.path().join("root");
        register::register(
            vault_dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: Some(&format!("file://{}", root_path.display())),
                root_hint: None,
            },
        )
        .unwrap();

        let resolved = resolve(vault_dir.path(), "assets", None).unwrap();
        assert_eq!(resolved.normalized_path, "assets");
        assert_eq!(resolved.root_path, root_path);
        // `Repo::open` 把工作树根归一化成 canonical 路径（macOS 上
        // `/var` 是指向 `/private/var` 的符号链接）——两侧都 canonicalize
        // 再比较，不依赖 tempfile 产出的路径字面量恰好等于 canonical 形式。
        assert_eq!(
            resolved.dataset_dir.canonicalize().unwrap(),
            vault_dir.path().join("assets").canonicalize().unwrap()
        );
    }

    #[test]
    fn 未注册的数据集报_not_registered() {
        let vault_dir = tempfile::tempdir().unwrap();
        建仓库(vault_dir.path());
        fs::write(vault_dir.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_dir.path().join("assets")).unwrap();

        let err = resolve(vault_dir.path(), "assets", None).unwrap_err();
        assert!(matches!(err, ResolveError::NotRegistered { .. }));
    }

    #[test]
    fn root_override覆盖注册表里的url() {
        let vault_dir = tempfile::tempdir().unwrap();
        建仓库(vault_dir.path());
        fs::write(vault_dir.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_dir.path().join("assets")).unwrap();

        let store_dir = tempfile::tempdir().unwrap();
        let registered_root = store_dir.path().join("registered-root");
        register::register(
            vault_dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: Some(&format!("file://{}", registered_root.display())),
                root_hint: None,
            },
        )
        .unwrap();

        let overridden = store_dir.path().join("override-root");
        let resolved = resolve(vault_dir.path(), "assets", Some(&overridden)).unwrap();
        assert_eq!(resolved.root_path, overridden);
        assert_ne!(resolved.root_path, registered_root);
    }
}
