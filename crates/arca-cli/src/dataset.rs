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

/// hub 连接目标——M2c Task 5「`dataset.rs`：解析 `http://` 的 hub url」：
/// M1d 时 `resolve()` 只认 `file://`/裸路径，遇到其它 scheme 一律报
/// 「该 transport 属 M2，本版本不支持」（`vault::HubRootError::UnsupportedTransport`）；
/// 本切片让 `resolve()` 认出 `http://`，产出这个枚举而不是裸 `PathBuf`，
/// 调用方据此决定走 [`arca_store::root::StorageRoot`] 还是
/// [`crate::transport::http::HttpTransport`]。
#[derive(Debug, Clone)]
pub enum HubTarget {
    /// `file://`/裸路径解析出的本地存储根路径，尚未 `StorageRoot::open`
    /// （与 M1d 起的既有行为完全一致）。
    Local(PathBuf),
    /// `http://` / `https://` hub 的基址——`<scheme>://<host>[:port]`，不含
    /// 末尾 `/`、不含 `/v1/datasets/...` 后缀（数据集坐标由
    /// [`ResolvedDataset::cfg`] 的 `dataset_id` 另外提供，两者合起来才是
    /// 完整端点，见 `transport::http::HttpTransport::new`）。
    Http {
        base_url: String,
        /// `.gitarca` 里为这个 hub 记录的 TLS 指纹 pin（M2e Task 4，
        /// FORMAT.md §9.1）。只对 `https://` 有意义；`http://` 恒为 `None`
        /// （明文连接没有证书可 pin——而且如果一个 `http://` hub 配了 pin，
        /// 那是配置错误，[`resolve`] 会拒绝，见其实现）。
        tls_pin: Option<String>,
    },
}

impl HubTarget {
    /// 这个目标是不是 `https://`——命令壳据此决定要不要走
    /// [`crate::tls::decide`]（明文 `http://` 没有证书可验）。
    pub fn is_tls(&self) -> bool {
        matches!(self, HubTarget::Http { base_url, .. } if base_url.starts_with("https://"))
    }
}

/// 一个已解析、但尚未打开存储根/建立连接的数据集。
#[derive(Debug)]
pub struct ResolvedDataset {
    /// 数据集目录的绝对路径（vault 根 + 归一化后的相对路径）。
    pub dataset_dir: PathBuf,
    /// 归一化后的、相对 vault 根的路径（`/` 分隔）。
    pub normalized_path: String,
    pub cfg: DatasetConfig,
    /// 这个数据集在 `.gitarca` 里绑定的 hub 名（`[hub.<名>]` 的键名，与
    /// `target` 是同一份绑定的两个视角：这个是人类认得的符号名，`target`
    /// 是据此解析出的连接地址）。M2d Task 3：多 hub 独立故障域下，命令壳
    /// 报告"哪个数据集离线"时必须同时说清"是哪个 hub"，不能只报路径——
    /// 一个 vault 有多个数据集分属不同 hub 时，光报路径不足以让用户判断
    /// 该去检查哪个 hub 的可达性。
    pub hub_name: String,
    /// 这个数据集绑定的 hub——本地存储根路径或 `http://` 基址，见
    /// [`HubTarget`]。
    pub target: HubTarget,
}

impl ResolvedDataset {
    /// 只支持本地存储根的命令（`status`/`verify`/`doctor`/plumbing/`adopt`——
    /// M2c Task 5 只把 `arca sync` 接通 `http://`，见
    /// `docs/superpowers/plans/2026-08-08-m2c-journal-longpoll.md` 自评
    /// 「多卷映射与 server/client 角色属 M2d」一节，其它命令的 Transport
    /// 化同理留给后续切片）用它取路径；`target` 是 `Http` 时返回一个明确
    /// 的、可诊断的错误，不是让调用方自己 `match` 时手忙脚乱地拼错误信息。
    pub fn local_root(&self) -> Result<&Path, ResolveError> {
        match &self.target {
            HubTarget::Local(p) => Ok(p),
            HubTarget::Http { base_url, .. } => Err(ResolveError::LocalOnlyCommand {
                hub_url: base_url.clone(),
            }),
        }
    }
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
    /// 这条命令只支持本地存储根（`file://`），hub 是 `http://` 时报这个——
    /// 见 [`ResolvedDataset::local_root`] 文档。
    LocalOnlyCommand {
        hub_url: String,
    },
    /// 明文 `http://` hub 上配了 `tls_pin`——配置错误，拒绝而不是忽略
    /// （M2e Task 4，见 [`resolve`] 里的注释）。
    PinOnPlaintextHub {
        hub: String,
        url: String,
    },
    /// `tls_pin` 的字节格式不合规（FORMAT.md §9.1）。
    BadTlsPin {
        hub: String,
        reason: String,
    },
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
            ResolveError::LocalOnlyCommand { hub_url } => write!(
                f,
                "{hub_url} 是 http:// hub——这条命令目前只支持本地（file://）存储根，\
                 改用 `arca sync` 之类已接通 http:// 的命令"
            ),
            ResolveError::PinOnPlaintextHub { hub, url } => write!(
                f,
                "hub {hub:?} 的 url 是明文 {url}，却配置了 tls_pin——这条连接根本不走 TLS，\
                 pin 不会生效。已停止（绝不静默忽略一个会让你误以为受保护的配置，I5）：\
                 要么把 url 改成 https://，要么删掉 tls_pin。"
            ),
            ResolveError::BadTlsPin { hub, reason } => {
                write!(f, "hub {hub:?} 的 tls_pin 不合规：{reason}")
            }
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

    // M2c Task 5：`--root` 覆盖永远赢（与 M1d 起既有语义一致，`--root` 是
    // "外置盘换挂载点"这类纯本地场景的一次性覆盖，不可能覆盖成一个
    // `http://` 地址）；否则才看 `hub.url` 是不是 `http://`。裸 `http://`
    // 之外的其它 scheme（`https://` 等）继续交给 `vault::resolve_hub_root`
    // 报「该 transport 属 M2，本版本不支持」，行为不变。
    let target = if let Some(root) = root_override {
        HubTarget::Local(root.to_path_buf())
    } else if let Some((scheme, rest)) = hub
        .url
        .strip_prefix("http://")
        .map(|r| ("http", r))
        .or_else(|| hub.url.strip_prefix("https://").map(|r| ("https", r)))
    {
        let base = rest.trim_end_matches('/');
        if base.is_empty() {
            return Err(ResolveError::HubRoot(HubRootError::EmptyUrl));
        }
        // M2e Task 4：pin 只对 `https://` 有意义。给一个明文 `http://` hub
        // 配 `tls_pin` 是配置错误——**拒绝而不是忽略**（I5）：静默忽略会让
        // 用户以为自己已经受 pin 保护，而实际上这条连接连 TLS 都没有。
        if scheme == "http" && hub.tls_pin.is_some() {
            return Err(ResolveError::PinOnPlaintextHub {
                hub: entry.hub.clone(),
                url: hub.url.clone(),
            });
        }
        // pin 的字节格式在这里就校验（FORMAT.md §9.1）——一个写错的 pin
        // 应该在解析配置时就报出来，不是等到握手那一刻才发现。
        if let Some(pin) = &hub.tls_pin {
            crate::tls::parse_pin(pin).map_err(|e| ResolveError::BadTlsPin {
                hub: entry.hub.clone(),
                reason: e.to_string(),
            })?;
        }
        HubTarget::Http {
            base_url: format!("{scheme}://{base}"),
            tls_pin: hub.tls_pin.clone(),
        }
    } else {
        HubTarget::Local(vault::resolve_hub_root(hub, None).map_err(ResolveError::HubRoot)?)
    };

    Ok(ResolvedDataset {
        dataset_dir,
        normalized_path,
        cfg,
        hub_name: entry.hub.clone(),
        target,
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
                dataset_id: None,
            },
        )
        .unwrap();

        let resolved = resolve(vault_dir.path(), "assets", None).unwrap();
        assert_eq!(resolved.normalized_path, "assets");
        assert_eq!(resolved.local_root().unwrap(), root_path);
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
                dataset_id: None,
            },
        )
        .unwrap();

        let overridden = store_dir.path().join("override-root");
        let resolved = resolve(vault_dir.path(), "assets", Some(&overridden)).unwrap();
        assert_eq!(resolved.local_root().unwrap(), overridden);
        assert_ne!(resolved.local_root().unwrap(), registered_root);
    }

    #[test]
    fn http_hub的url被解析成httptarget而不是报不支持() {
        let vault_dir = tempfile::tempdir().unwrap();
        建仓库(vault_dir.path());
        fs::write(vault_dir.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_dir.path().join("assets")).unwrap();

        register::register(
            vault_dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: Some("http://127.0.0.1:18420/"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap();

        let resolved = resolve(vault_dir.path(), "assets", None).unwrap();
        match &resolved.target {
            HubTarget::Http { base_url, .. } => assert_eq!(base_url, "http://127.0.0.1:18420"),
            other => panic!("应为 HubTarget::Http，实得 {other:?}"),
        }
        assert!(matches!(
            resolved.local_root().unwrap_err(),
            ResolveError::LocalOnlyCommand { .. }
        ));
    }
}
