//! `arca register`（M1d Task 4）：把一个目录登记为数据集——建
//! `<path>/.arca/dataset.toml`（若已存在则只校验、不覆盖，即"孤儿数据集
//! 显式登记"）、更新 `.gitarca`、更新 `.gitignore` 反选块。
//!
//! **hub 端点由 `register` 按需创建/更新**（要求用户先手工编辑 `.gitarca`
//! 才能开始用是敌意设计）：`hub` 名不存在时用 `hub_instance_id`（缺省则随机
//! 生成）与 `hub_url`（缺省时 `file://` 场景可从 `root_hint` 推出）新建一个
//! `[hub.<name>]` 条目；已存在时校验 `instance_id` 一致（spec §11 防误绑），
//! 允许更新 `url`（"端点无关身份"：地址可以变，身份不能）。
//!
//! 写入前先跑 `tracking::check_vault`，有 `Issue` 就停下报告（I5）——但**豁免
//! 本次调用自己打算解决的那条 `OrphanDataset`**：若 `<path>` 已经有
//! `dataset.toml` 却未登记，`check_vault` 必然会报出这条 Issue，那正是
//! `register` 存在的理由，不能把"我正在修的问题"当成"拦下我"的理由。

use crate::vault::{self, HubRootError, VaultError};
use arca_format::dataset::DatasetConfig;
use arca_format::error::FormatError;
use arca_format::gitarca::{DatasetEntry, HubEntry, Registry};
use arca_format::path_rules::{self, PathStatus};
use arca_git::tracking::{self, Issue};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

pub struct RegisterOptions<'a> {
    /// 数据集路径，相对 vault 根（`path_rules::check` 校验/归一化）。
    pub path: &'a str,
    pub hub_name: &'a str,
    /// 缺省时：hub 已存在则沿用其 `instance_id`；hub 不存在则随机生成。
    pub hub_instance_id: Option<&'a str>,
    /// 缺省时：hub 已存在则沿用其 `url`；hub 不存在则尝试从 `root_hint` 推出
    /// `file://` 形式；两者都没有则报错（不猜）。
    pub hub_url: Option<&'a str>,
    /// 仅用于在新建 hub 条目、且未显式给出 `hub_url` 时推导 `file://` 地址。
    pub root_hint: Option<&'a Path>,
    /// **M2c Task 5**：新建 `dataset.toml` 时用它代替随机生成的
    /// `dataset_id`——"第二台设备加入一个已经在 hub 上存在的数据集"这个
    /// 场景（两机端到端演示的前提）需要它：第一台设备 `adopt` 时随机分配
    /// 了一个 `dataset_id`，第二台设备必须登记同一个 id 才能连到
    /// hub 上同一份数据，不能各自随机生成两个互不相干的 id。**这不是完整
    /// 的"加入现有数据集"引导流程**（那需要发现/配对机制，spec 没有为它
    /// 定协议，规划里点名"多卷映射与 server/client 角色属 M2d"——本质是
    /// 同一类缺口）；这里只补最小的、能让 `arca register --dataset-id
    /// <hex>` 显式声明"我加入的是这个已知 id"的原语，`id` 是通过何种带外
    /// 渠道得知的不在本选项的职责范围内。缺省仍随机生成，不影响既有行为。
    /// 已有 `dataset.toml` 时忽略这个字段（既有身份不能被覆盖，与
    /// `hub_instance_id`/`hub_url` 校验既有一致性同一条纪律）。
    pub dataset_id: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RegisterOutcome {
    pub dataset_id: String,
    pub hub_instance_id: String,
    /// `false` 表示 `<path>/.arca/dataset.toml` 已经存在（孤儿登记场景），
    /// 本次没有新建它，只补齐了 `.gitarca`/`.gitignore`。
    pub created_dataset_toml: bool,
}

#[derive(Debug)]
pub enum RegisterError {
    Vault(VaultError),
    /// 写入前巡检发现的问题（已豁免本次调用自己要解决的那条 OrphanDataset）。
    Issues(Vec<Issue>),
    PathInvalid(PathStatus),
    /// hub 名已存在，但调用方传入的 `hub_instance_id` 与其不符（防误绑）。
    HubInstanceMismatch {
        hub: String,
        expected: String,
        found: String,
    },
    /// `<path>` 已有 `dataset.toml`，但其 `hub_instance_id` 与本次解析出的
    /// 不符——这份既有身份不能被静默改绑到另一个 hub。
    DatasetHubMismatch {
        path: String,
        dataset_hub_instance_id: String,
        hub_instance_id: String,
    },
    /// `<path>` 已在 `.gitarca` 登记，且登记的 hub 名与本次不同——拒绝静默改绑。
    AlreadyRegisteredUnderOtherHub {
        path: String,
        existing_hub: String,
        requested_hub: String,
    },
    /// hub 是新建的，但既没有 `--hub-url` 也没有 `root_hint` 可以推导。
    MissingHubUrl,
    /// `--dataset-id` 给出的值不是合法的 32 位小写十六进制。
    BadDatasetId {
        value: String,
    },
    UnsupportedHubUrl(HubRootError),
    Registry(FormatError),
    Ignore(arca_git::ignore_block::BlockError),
    Io {
        path: String,
        reason: String,
    },
    DatasetTomlCorrupt(FormatError),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterError::Vault(e) => write!(f, "{e}"),
            RegisterError::Issues(issues) => {
                write!(f, "写入前巡检发现 {} 个问题，已停止", issues.len())
            }
            RegisterError::PathInvalid(s) => write!(f, "数据集路径不合规：{}", s.as_str()),
            RegisterError::HubInstanceMismatch {
                hub,
                expected,
                found,
            } => write!(
                f,
                "hub {hub:?} 已登记 instance_id {expected:?}，与传入的 {found:?} 不符"
            ),
            RegisterError::DatasetHubMismatch {
                path,
                dataset_hub_instance_id,
                hub_instance_id,
            } => write!(
                f,
                "{path} 已有 dataset.toml，其 hub_instance_id {dataset_hub_instance_id:?} \
                 与本次解析出的 {hub_instance_id:?} 不符"
            ),
            RegisterError::AlreadyRegisteredUnderOtherHub {
                path,
                existing_hub,
                requested_hub,
            } => write!(
                f,
                "{path} 已登记在 hub {existing_hub:?} 下，与本次请求的 {requested_hub:?} 不同"
            ),
            RegisterError::MissingHubUrl => {
                write!(f, "新建 hub 需要 --hub-url，或提供 --root 以便从中推导")
            }
            RegisterError::BadDatasetId { value } => write!(
                f,
                "--dataset-id {value:?} 不是合法的 32 位小写十六进制（FORMAT.md §1）"
            ),
            RegisterError::UnsupportedHubUrl(e) => write!(f, "{e}"),
            RegisterError::Registry(e) => write!(f, ".gitarca 处理失败：{e}"),
            RegisterError::Ignore(e) => write!(f, "{e}"),
            RegisterError::Io { path, reason } => write!(f, "{path}：{reason}"),
            RegisterError::DatasetTomlCorrupt(e) => write!(f, "dataset.toml 解析失败：{e}"),
        }
    }
}

impl std::error::Error for RegisterError {}

pub fn register(start: &Path, opts: RegisterOptions) -> Result<RegisterOutcome, RegisterError> {
    let vault::Vault { repo, registry } = vault::open(start).map_err(RegisterError::Vault)?;

    let normalized_path = path_rules::check(opts.path).map_err(RegisterError::PathInvalid)?;

    let issues: Vec<Issue> = tracking::check_vault(&repo, &registry)
        .into_iter()
        .filter(|issue| !matches!(issue, Issue::OrphanDataset { path } if path == &normalized_path))
        .collect();
    if !issues.is_empty() {
        return Err(RegisterError::Issues(issues));
    }

    let dataset_toml_path = repo
        .root()
        .join(&normalized_path)
        .join(".arca")
        .join("dataset.toml");
    let existing_cfg = match fs::read_to_string(&dataset_toml_path) {
        Ok(text) => Some(DatasetConfig::parse(&text).map_err(RegisterError::DatasetTomlCorrupt)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(RegisterError::Io {
                path: dataset_toml_path.display().to_string(),
                reason: e.to_string(),
            })
        }
    };

    // --- 解析/新建 hub 条目 -------------------------------------------------
    let hub_instance_id = match registry.hub(opts.hub_name) {
        Some(existing) => {
            if let Some(want) = opts.hub_instance_id {
                if want != existing.instance_id {
                    return Err(RegisterError::HubInstanceMismatch {
                        hub: opts.hub_name.to_string(),
                        expected: existing.instance_id.clone(),
                        found: want.to_string(),
                    });
                }
            }
            existing.instance_id.clone()
        }
        None => opts
            .hub_instance_id
            .map(str::to_string)
            .unwrap_or_else(crate::ids::random_hex32),
    };

    let hub_url = match registry.hub(opts.hub_name) {
        Some(existing) => opts
            .hub_url
            .map(str::to_string)
            .unwrap_or_else(|| existing.url.clone()),
        None => match opts.hub_url {
            Some(u) => u.to_string(),
            None => {
                let root = opts.root_hint.ok_or(RegisterError::MissingHubUrl)?;
                format!("file://{}", root.display())
            }
        },
    };
    // 及早校验 URL 是否可解析（`file://`/裸路径/`http://`），即便这一步的
    // 结果这次调用不会立刻用到——好过把一个日后打不开的 hub 条目写进
    // `.gitarca`。`http://` 不经 `vault::resolve_hub_root`（M2c Task 5：
    // 那个函数只解析本地存储根路径，`http://` 走的是
    // `dataset::resolve`/`HubTarget::Http`，不是它的职责）——这里只做一次
    // 语法层面的"这是不是一个非空的 http:// 地址"检查，真正的连通性/
    // 数据集匹配留到 `arca sync` 实际发起请求时暴露。
    if !hub_url.starts_with("http://") {
        vault::resolve_hub_root(
            &HubEntry {
                instance_id: hub_instance_id.clone(),
                url: hub_url.clone(),
            },
            None,
        )
        .map_err(RegisterError::UnsupportedHubUrl)?;
    } else if hub_url == "http://" {
        return Err(RegisterError::UnsupportedHubUrl(HubRootError::EmptyUrl));
    }

    // --- 既有 dataset.toml 的身份必须与本次解析出的 hub 一致 ----------------
    if let Some(cfg) = &existing_cfg {
        if cfg.hub_instance_id != hub_instance_id {
            return Err(RegisterError::DatasetHubMismatch {
                path: normalized_path.clone(),
                dataset_hub_instance_id: cfg.hub_instance_id.clone(),
                hub_instance_id,
            });
        }
    }

    let dataset_id = match &existing_cfg {
        Some(c) => c.dataset_id.clone(),
        None => match opts.dataset_id {
            Some(id) => {
                if !arca_format::model::is_hex32(id) {
                    return Err(RegisterError::BadDatasetId {
                        value: id.to_string(),
                    });
                }
                id.to_string()
            }
            None => crate::ids::random_hex32(),
        },
    };

    // --- 重建注册表：保留全部既有 hub/dataset，追加/更新这一条 --------------
    let mut hub_map: BTreeMap<String, HubEntry> = registry
        .hubs()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    hub_map.insert(
        opts.hub_name.to_string(),
        HubEntry {
            instance_id: hub_instance_id.clone(),
            url: hub_url,
        },
    );

    let mut dataset_list: Vec<DatasetEntry> = registry.datasets().to_vec();
    let folded_target = path_rules::casefold(&normalized_path);
    match dataset_list
        .iter()
        .find(|e| path_rules::casefold(&e.path) == folded_target)
    {
        Some(existing) if existing.hub != opts.hub_name => {
            return Err(RegisterError::AlreadyRegisteredUnderOtherHub {
                path: normalized_path.clone(),
                existing_hub: existing.hub.clone(),
                requested_hub: opts.hub_name.to_string(),
            });
        }
        Some(_) => {} // 已登记在同一个 hub 下——幂等，无需改动。
        None => dataset_list.push(DatasetEntry {
            path: normalized_path.clone(),
            hub: opts.hub_name.to_string(),
        }),
    }

    let new_registry = Registry::new(hub_map, dataset_list);
    new_registry.validate().map_err(RegisterError::Registry)?;

    // --- 落盘：dataset.toml（若需要新建）→ .gitarca → .gitignore -----------
    let created_dataset_toml = existing_cfg.is_none();
    if created_dataset_toml {
        let cfg = DatasetConfig {
            schema: 1,
            dataset_id: dataset_id.clone(),
            hub_instance_id: hub_instance_id.clone(),
            public_base_url: None,
            url_style: None,
        };
        let text = cfg.to_toml().map_err(RegisterError::Registry)?;
        let dir = dataset_toml_path.parent().expect("总有 .arca 这一层父目录");
        fs::create_dir_all(dir).map_err(|e| RegisterError::Io {
            path: dir.display().to_string(),
            reason: e.to_string(),
        })?;
        vault::write_text_atomic(&dataset_toml_path, &text).map_err(|e| RegisterError::Io {
            path: dataset_toml_path.display().to_string(),
            reason: e.to_string(),
        })?;
    }

    vault::write_registry(&repo, &new_registry).map_err(RegisterError::Vault)?;

    let all_paths: Vec<String> = new_registry
        .normalized_datasets()
        .map_err(RegisterError::Registry)?
        .into_iter()
        .map(|e| e.path)
        .collect();
    vault::update_gitignore(repo.root(), &all_paths).map_err(|e| match e {
        vault::UpdateIgnoreError::Block(b) => RegisterError::Ignore(b),
        vault::UpdateIgnoreError::Io { path, reason } => RegisterError::Io { path, reason },
    })?;

    Ok(RegisterOutcome {
        dataset_id,
        hub_instance_id,
        created_dataset_toml,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn 初始化vault(dir: &Path) {
        建仓库(dir);
        fs::write(dir.join(GITARCA_FILE), "schema = 1\n").unwrap();
    }

    #[test]
    fn 新数据集声明_从零创建dataset_toml与gitarca条目() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("assets")).unwrap();

        let outcome = register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: Some("file:///mnt/nas/assets"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap();

        assert!(outcome.created_dataset_toml);
        assert!(dir.path().join("assets/.arca/dataset.toml").is_file());

        let vault = vault::open(dir.path()).unwrap();
        assert_eq!(vault.registry.datasets().len(), 1);
        assert_eq!(vault.registry.datasets()[0].path, "assets");
        assert_eq!(
            vault.registry.hub("home").unwrap().instance_id,
            outcome.hub_instance_id
        );
        assert_eq!(
            vault.registry.hub("home").unwrap().url,
            "file:///mnt/nas/assets"
        );

        let cfg_text = fs::read_to_string(dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = DatasetConfig::parse(&cfg_text).unwrap();
        assert_eq!(cfg.dataset_id, outcome.dataset_id);
        assert_eq!(cfg.hub_instance_id, outcome.hub_instance_id);

        let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gitignore.contains("/assets/*"));
    }

    #[test]
    fn hub_url未给且无root_hint时报错() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("assets")).unwrap();

        let err = register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: None,
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, RegisterError::MissingHubUrl));
    }

    #[test]
    fn hub_url缺省时从root_hint推导() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("assets")).unwrap();

        let outcome = register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: None,
                root_hint: Some(Path::new("/mnt/usb/assets")),
                dataset_id: None,
            },
        )
        .unwrap();
        let vault = vault::open(dir.path()).unwrap();
        assert_eq!(
            vault.registry.hub("home").unwrap().url,
            "file:///mnt/usb/assets"
        );
        let _ = outcome;
    }

    #[test]
    fn 孤儿数据集显式登记不覆盖既有dataset_toml() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("orphan/.arca")).unwrap();
        let original = "schema = 1\ndataset_id = \"9c41000000000000000000000000abcd\"\nhub_instance_id = \"3f2a000000000000000000000000beef\"\n";
        fs::write(dir.path().join("orphan/.arca/dataset.toml"), original).unwrap();

        let outcome = register(
            dir.path(),
            RegisterOptions {
                path: "orphan",
                hub_name: "home",
                hub_instance_id: Some("3f2a000000000000000000000000beef"),
                hub_url: Some("file:///mnt/nas/orphan"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap();

        assert!(!outcome.created_dataset_toml);
        assert_eq!(outcome.dataset_id, "9c41000000000000000000000000abcd");
        assert_eq!(
            fs::read_to_string(dir.path().join("orphan/.arca/dataset.toml")).unwrap(),
            original,
            "既有 dataset.toml 必须逐字节保留"
        );

        let vault = vault::open(dir.path()).unwrap();
        assert_eq!(vault.registry.datasets().len(), 1);
        assert_eq!(vault.registry.datasets()[0].path, "orphan");
    }

    #[test]
    fn 孤儿数据集的hub_instance_id不符时报错() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("orphan/.arca")).unwrap();
        fs::write(
            dir.path().join("orphan/.arca/dataset.toml"),
            "schema = 1\ndataset_id = \"9c41000000000000000000000000abcd\"\nhub_instance_id = \"00000000000000000000000000000000\"\n",
        )
        .unwrap();

        let err = register(
            dir.path(),
            RegisterOptions {
                path: "orphan",
                hub_name: "home",
                hub_instance_id: Some("3f2a000000000000000000000000beef"),
                hub_url: Some("file:///mnt/nas/orphan"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, RegisterError::DatasetHubMismatch { .. }));
    }

    #[test]
    fn hub已存在时instance_id不符报错() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("a")).unwrap();
        register(
            dir.path(),
            RegisterOptions {
                path: "a",
                hub_name: "home",
                hub_instance_id: Some("3f2a000000000000000000000000beef"),
                hub_url: Some("file:///mnt/nas/a"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap();

        fs::create_dir_all(dir.path().join("b")).unwrap();
        let err = register(
            dir.path(),
            RegisterOptions {
                path: "b",
                hub_name: "home",
                hub_instance_id: Some("00000000000000000000000000000000"),
                hub_url: None,
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, RegisterError::HubInstanceMismatch { .. }));
    }

    #[test]
    fn 重复注册同一路径到同一hub是幂等的() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        let opts = || RegisterOptions {
            path: "assets",
            hub_name: "home",
            hub_instance_id: Some("3f2a000000000000000000000000beef"),
            hub_url: Some("file:///mnt/nas/assets"),
            root_hint: None,
            dataset_id: None,
        };
        let first = register(dir.path(), opts()).unwrap();
        let second = register(dir.path(), opts()).unwrap();
        assert_eq!(first.dataset_id, second.dataset_id);
        assert!(!second.created_dataset_toml);

        let vault = vault::open(dir.path()).unwrap();
        assert_eq!(vault.registry.datasets().len(), 1, "不应该产生重复条目");
    }

    #[test]
    fn 改绑到不同hub名被拒绝() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: Some("file:///mnt/nas/assets"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap();

        let err = register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "office",
                hub_instance_id: None,
                hub_url: Some("file:///mnt/nas2/assets"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap_err();
        // "office" 是全新 hub 名，随机生成的 instance_id 必然与既有
        // dataset.toml 里记录的（属于 "home" 的）instance_id 不符——这条更
        // 具体的身份冲突先被查出来，比"改绑到了别的 hub 名"这个更粗粒度的
        // 结论更早、更精确地解释了到底哪里不一致。
        assert!(matches!(err, RegisterError::DatasetHubMismatch { .. }));
    }

    #[test]
    fn 同一instance_id下改绑到不同hub名被拒绝() {
        // 与上一条区分：这里两个 hub 名指向*同一个* instance_id（同一个物理
        // hub 换了个在 .gitarca 里的叫法），dataset.toml 的身份检查能通过，
        // 真正该拦住的是"这个路径在注册表里已经登记在另一个 hub 名下"。
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: Some("3f2a000000000000000000000000beef"),
                hub_url: Some("file:///mnt/nas/assets"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap();

        let err = register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home-alias",
                hub_instance_id: Some("3f2a000000000000000000000000beef"),
                hub_url: Some("file:///mnt/nas/assets"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            RegisterError::AlreadyRegisteredUnderOtherHub { .. }
        ));
    }

    #[test]
    fn https_url被拒绝() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        fs::create_dir_all(dir.path().join("assets")).unwrap();

        let err = register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: Some("https://nas.example.com/assets"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, RegisterError::UnsupportedHubUrl(_)));
    }

    #[test]
    fn 无关问题存在时停下报告() {
        let dir = tempfile::tempdir().unwrap();
        初始化vault(dir.path());
        // 制造一个与本次登记目标无关的孤儿数据集。
        fs::create_dir_all(dir.path().join("other/.arca")).unwrap();
        fs::write(
            dir.path().join("other/.arca/dataset.toml"),
            "schema = 1\ndataset_id = \"9c41000000000000000000000000abcd\"\nhub_instance_id = \"3f2a000000000000000000000000beef\"\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("assets")).unwrap();

        let err = register(
            dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: Some("file:///mnt/nas/assets"),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap_err();
        match err {
            RegisterError::Issues(issues) => {
                assert!(issues
                    .iter()
                    .any(|i| matches!(i, Issue::OrphanDataset { path } if path == "other")));
            }
            other => panic!("应报 Issues，实得 {other:?}"),
        }
    }
}
