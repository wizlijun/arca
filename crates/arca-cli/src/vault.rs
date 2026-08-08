//! vault 级共用件：打开 `.gitarca`、原子写回、hub URL → 本地存储根路径解析
//! （M1d Task 4/5/6 共用，`init`/`register`/`adopt`/`sync` 各命令壳都要用）。
//!
//! spec §11 的「端点无关身份」在这里落地：`.gitarca` 的 `[hub.<name>]` 存
//! `url`（"当前怎么连过去"），`dataset.toml` 存 `hub_instance_id`（"认的是
//! 身份不是地址"）。`adopt`/`sync` 默认从注册表解析出存储根路径，`--root`
//! 只是一次性覆盖（外置盘换挂载点、USB sneakernet 插到别的机器上时用）；
//! 覆盖之后 `StorageRoot::open` 仍然校验身份（I11），挂错了照样拦下来。

use arca_format::error::FormatError;
use arca_format::gitarca::{HubEntry, Registry};
use arca_git::repo::{GitError, Repo};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// `.gitarca` 的文件名，vault 根下。
pub const GITARCA_FILE: &str = ".gitarca";

/// 打开 vault 失败——彼此可区分（I5）。
#[derive(Debug)]
pub enum VaultError {
    /// `start` 不在任何 git 工作树内，或 git 本身不可用。
    Git(GitError),
    /// vault 根下没有 `.gitarca`——尚未 `arca init`。
    NotInitialized { path: String },
    /// `.gitarca` 存在但解析/版本校验失败。
    Malformed(FormatError),
    /// `.gitarca` 解析成功但内部一致性校验失败（重复路径、嵌套、引用未登记
    /// 的 hub 等，见 [`Registry::validate`]）。
    Invalid(FormatError),
    /// 读取 `.gitarca` 本身失败，且不是"文件不存在"。
    Io { path: String, reason: String },
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VaultError::Git(e) => write!(f, "{e}"),
            VaultError::NotInitialized { path } => {
                write!(f, "{path} 下没有 {GITARCA_FILE}——请先运行 `arca init`")
            }
            VaultError::Malformed(e) => write!(f, "{GITARCA_FILE} 解析失败：{e}"),
            VaultError::Invalid(e) => write!(f, "{GITARCA_FILE} 内部不一致：{e}"),
            VaultError::Io { path, reason } => write!(f, "读取 {path} 失败：{reason}"),
        }
    }
}

impl std::error::Error for VaultError {}

/// 一个已打开、`.gitarca` 已解析且通过一致性校验的 vault。
#[derive(Debug)]
pub struct Vault {
    pub repo: Repo,
    pub registry: Registry,
}

/// 打开 `start` 所在的 vault：定位工作树根、读 `.gitarca`、解析并校验。
///
/// **不跑 `tracking::check_vault`**——那是操作级别的巡检（磁盘状态 vs 注册表
/// 是否一致），各命令壳按各自的语义决定何时跑、如何处置 `Issue`（例如
/// `register` 需要豁免它自己正打算修复的那条 `OrphanDataset`）。本函数只管
/// "vault 这个概念本身是否成立"：在 git 里、`.gitarca` 存在、解析得出、
/// 内部自洽。
pub fn open(start: &Path) -> Result<Vault, VaultError> {
    let repo = Repo::open(start).map_err(VaultError::Git)?;
    let path = repo.root().join(GITARCA_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(VaultError::NotInitialized {
                path: repo.root().display().to_string(),
            })
        }
        Err(e) => {
            return Err(VaultError::Io {
                path: path.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    let registry = Registry::parse(&text).map_err(VaultError::Malformed)?;
    registry.validate().map_err(VaultError::Invalid)?;
    Ok(Vault { repo, registry })
}

/// 把 `content` 原子写入 `path`（同目录 tmp → rename）。
///
/// 用于 `.gitarca`/`.gitignore`/`dataset.toml` 这类 vault 侧、git 追踪的小
/// 文本文件——不是存储根内容（那走 `arca_store::atomic::write` 更重的 fsync
/// 事务链），但同样不能半截写入：git 工作树里出现一个被截断的 `.gitarca`
/// 比"这次操作失败了"更难诊断。与 `baseline::Baseline::save` 同一纪律
/// （可从 git 历史/远端重新拿到，不需要存储根级别的持久化成本）。
pub fn write_text_atomic(path: &Path, content: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "arca".to_string()),
        std::process::id()
    ));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// 把注册表写回 `<repo root>/.gitarca`。
pub fn write_registry(repo: &Repo, registry: &Registry) -> Result<(), VaultError> {
    let text = registry.to_toml().map_err(VaultError::Malformed)?;
    let path = repo.root().join(GITARCA_FILE);
    write_text_atomic(&path, &text).map_err(|e| VaultError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    })
}

/// hub URL 解析失败——彼此可区分（I5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubRootError {
    /// `url` 为空串——没有地址可解析，也没有 `--root` 覆盖。
    EmptyUrl,
    /// `url` 用了尚不支持的 transport（`https://` 等属 M2）。按 I5，不认识的
    /// 东西要明确说出来，不是静默失败或当成裸路径尝试。
    UnsupportedTransport { scheme: String },
}

impl fmt::Display for HubRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubRootError::EmptyUrl => write!(f, "hub 的 url 为空，且未提供 --root 覆盖"),
            HubRootError::UnsupportedTransport { scheme } => write!(
                f,
                "该 transport（{scheme}://）属 M2，本版本不支持——M1 只认 file:// 与裸本地路径"
            ),
        }
    }
}

impl std::error::Error for HubRootError {}

/// 解析出这个 hub 对应的本地存储根路径。
///
/// `override_root` 非空时直接采用（`--root` 覆盖，见模块顶部文档）；否则解析
/// `hub.url`：`file://<path>` 剥掉前缀；不含 `://` 的裸路径原样当作本地路径；
/// 含其它 `<scheme>://` 一律报 [`HubRootError::UnsupportedTransport`]，**不**
/// 尝试当作本地路径蒙混过去——那样会把"这个 transport 不支持"误报成一个看似
/// 合理但实际不存在的文件系统错误。
pub fn resolve_hub_root(
    hub: &HubEntry,
    override_root: Option<&Path>,
) -> Result<PathBuf, HubRootError> {
    if let Some(root) = override_root {
        return Ok(root.to_path_buf());
    }
    if hub.url.is_empty() {
        return Err(HubRootError::EmptyUrl);
    }
    if let Some(rest) = hub.url.strip_prefix("file://") {
        return Ok(PathBuf::from(rest));
    }
    if let Some((scheme, _)) = hub.url.split_once("://") {
        return Err(HubRootError::UnsupportedTransport {
            scheme: scheme.to_string(),
        });
    }
    Ok(PathBuf::from(&hub.url))
}

/// 更新 `<repo root>/.gitignore` 的 arca 反选块，覆盖 `dataset_paths` 列出的
/// 全部数据集（通常取 `registry.normalized_datasets()` 的结果——已归一化、
/// 用 `/` 分隔，见 `Registry::normalized_datasets` 的文档）。
pub fn update_gitignore(
    repo_root: &Path,
    dataset_paths: &[String],
) -> Result<(), UpdateIgnoreError> {
    let refs: Vec<&str> = dataset_paths.iter().map(String::as_str).collect();
    let path = repo_root.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(UpdateIgnoreError::Io {
                path: path.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    let updated =
        arca_git::ignore_block::upsert(&existing, &refs).map_err(UpdateIgnoreError::Block)?;
    write_text_atomic(&path, &updated).map_err(|e| UpdateIgnoreError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    })
}

#[derive(Debug)]
pub enum UpdateIgnoreError {
    Block(arca_git::ignore_block::BlockError),
    Io { path: String, reason: String },
}

impl fmt::Display for UpdateIgnoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateIgnoreError::Block(e) => write!(f, "{e}"),
            UpdateIgnoreError::Io { path, reason } => write!(f, "写入 {path} 失败：{reason}"),
        }
    }
}

impl std::error::Error for UpdateIgnoreError {}

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
    fn 打开缺少_gitarca_的仓库报_not_initialized() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        match open(dir.path()) {
            Err(VaultError::NotInitialized { .. }) => {}
            other => panic!("应报 NotInitialized，实得 {other:?}"),
        }
    }

    #[test]
    fn 打开合法vault成功() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        fs::write(dir.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        let vault = open(dir.path()).unwrap();
        assert_eq!(vault.registry.datasets().len(), 0);
    }

    #[test]
    fn 打开内部不一致的_gitarca_报_invalid() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        // 引用了未登记的 hub——Registry::validate 应当拒绝。
        fs::write(
            dir.path().join(GITARCA_FILE),
            "schema = 1\n[[dataset]]\npath = \"a\"\nhub = \"ghost\"\n",
        )
        .unwrap();
        match open(dir.path()) {
            Err(VaultError::Invalid(_)) => {}
            other => panic!("应报 Invalid，实得 {other:?}"),
        }
    }

    #[test]
    fn write_text_atomic往返一致() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/dir/file.txt");
        write_text_atomic(&path, "内容\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "内容\n");
    }

    #[test]
    fn write_text_atomic不留tmp残留() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        write_text_atomic(&path, "x").unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "只应留下目标文件本身");
    }

    #[test]
    fn resolve_hub_root_file前缀被剥掉() {
        let hub = HubEntry {
            instance_id: "x".into(),
            url: "file:///Volumes/disk1/arca-photo".into(),
        };
        assert_eq!(
            resolve_hub_root(&hub, None).unwrap(),
            PathBuf::from("/Volumes/disk1/arca-photo")
        );
    }

    #[test]
    fn resolve_hub_root_裸路径原样当作本地路径() {
        let hub = HubEntry {
            instance_id: "x".into(),
            url: "/mnt/nas/photo".into(),
        };
        assert_eq!(
            resolve_hub_root(&hub, None).unwrap(),
            PathBuf::from("/mnt/nas/photo")
        );
    }

    #[test]
    fn resolve_hub_root_https_报不支持而不是当成路径() {
        let hub = HubEntry {
            instance_id: "x".into(),
            url: "https://nas.example.com/photo".into(),
        };
        match resolve_hub_root(&hub, None) {
            Err(HubRootError::UnsupportedTransport { scheme }) => assert_eq!(scheme, "https"),
            other => panic!("应报 UnsupportedTransport，实得 {other:?}"),
        }
    }

    #[test]
    fn resolve_hub_root_override覆盖url() {
        let hub = HubEntry {
            instance_id: "x".into(),
            url: "https://nas.example.com/photo".into(),
        };
        // --root 覆盖优先于 url 解析，即便 url 本身是不支持的 transport。
        let overridden = resolve_hub_root(&hub, Some(Path::new("/mnt/usb/photo"))).unwrap();
        assert_eq!(overridden, PathBuf::from("/mnt/usb/photo"));
    }

    #[test]
    fn resolve_hub_root_空url且无覆盖时报错() {
        let hub = HubEntry {
            instance_id: "x".into(),
            url: String::new(),
        };
        assert_eq!(
            resolve_hub_root(&hub, None).unwrap_err(),
            HubRootError::EmptyUrl
        );
    }

    #[test]
    fn update_gitignore创建全新文件并写入反选块() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        update_gitignore(dir.path(), &["assets".to_string()]).unwrap();
        let text = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(text.contains("/assets/*"));
        assert!(text.contains("!/assets/.arca/"));
    }

    #[test]
    fn update_gitignore保留块外用户内容() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        update_gitignore(dir.path(), &["assets".to_string()]).unwrap();
        let text = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(text.starts_with("node_modules/\n"));
    }
}
