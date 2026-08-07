//! 追踪冲突检测与 vault 一致性检查（spec §4.3.2 处置表）。
//!
//! - 孤儿数据集（有 `.arca/` 但不在注册表）→ 报告并拒绝同步，等待显式 `arca register`；
//! - `hub_instance_id` 与注册表不符 → 拒绝同步（防误绑）；
//! - 数据集嵌套 / 同一路径登记两次 → 拒绝；
//! - 已被 git 追踪的文件落入数据集（`.gitignore` 对已追踪文件无效）→ 检出并报告。
//!
//! `check_vault` **只报告不修复**（I5：状态模糊就停下并可诊断，不尽力自动纠正）——
//! 修复动作（`arca register` / `arca setup` / 让用户手动 `git rm --cached`）
//! 是上层命令的事，这里只负责如实列出问题。

use crate::repo::Repo;
use arca_format::dataset::DatasetConfig;
use arca_format::gitarca::Registry;
use arca_format::path_rules;
use std::fmt;
use std::path::Path;

/// 一致性检查发现的问题。每个变体对应 spec §4.3.2 处置表的一行，
/// 或（`AlreadyTracked`）spec §4.3.1 点名的"用户手工 `git add` 制造的双重管理"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// 目录有 `.arca/dataset.toml` 但路径不在 `.gitarca` 注册表中——
    /// 防止从别处拷来的目录静默激活。
    OrphanDataset { path: String },
    /// `.gitarca` 登记了该路径，但本地找不到对应的 `<path>/.arca/dataset.toml`
    /// （尚未 `arca setup`，或元数据本身缺失）。
    MissingDataset { path: String },
    /// `dataset.toml` 的 `hub_instance_id` 与注册表中该 hub 名登记的
    /// `instance_id` 不符——挂到了别的数据集上（§11 防误绑）。
    HubIdMismatch {
        path: String,
        expected: String,
        found: String,
    },
    /// 两个注册的数据集路径互相嵌套；归属必须唯一。
    NestedDataset { outer: String, inner: String },
    /// 同一路径在 `.gitarca` 中被登记了不止一次。
    DuplicatePath { path: String },
    /// 路径落在某个数据集目录内，却已经被 git 追踪——`.gitignore` 反选块
    /// 对已追踪文件无效，这份数据正被 git 与 arca 双重管理（§4.3.1）。
    AlreadyTracked { path: String },
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::OrphanDataset { path } => write!(
                f,
                "孤儿数据集 {path:?}：目录内有 .arca/ 但未在 .gitarca 注册表登记，\
                 等待显式 `arca register`"
            ),
            Issue::MissingDataset { path } => write!(
                f,
                "缺失数据集 {path:?}：已在 .gitarca 登记，但本地尚无 \
                 <path>/.arca/dataset.toml，需要 `arca setup`"
            ),
            Issue::HubIdMismatch {
                path,
                expected,
                found,
            } => write!(
                f,
                "数据集 {path:?} 的 hub 身份不符：注册表期望 {expected:?}，\
                 dataset.toml 实际是 {found:?}"
            ),
            Issue::NestedDataset { outer, inner } => {
                write!(f, "数据集嵌套：{inner:?} 落在 {outer:?} 内部，归属必须唯一")
            }
            Issue::DuplicatePath { path } => {
                write!(f, "路径 {path:?} 在 .gitarca 中被登记了不止一次")
            }
            Issue::AlreadyTracked { path } => write!(
                f,
                "{path:?} 已被 git 追踪，但落在一个数据集目录内——\
                 同一份数据正被 git 与 arca 双重管理"
            ),
        }
    }
}

/// 对 `repo` 与 `registry` 做一遍一致性巡检，返回发现的所有问题（可能为空）。
///
/// **只报告不修复**（I5）。IO / git 调用本身失败时静默跳过对应的那一小项检查——
/// `check_vault` 是诊断辅助，不是硬性网关；它没有 `Result` 签名，无法把这类失败
/// 单独上报，调用方若需要区分"库是干净的"与"检查没跑起来"，应另行探测
/// （例如先确认 `Repo::open` 与 `Registry::parse` 均成功）。
pub fn check_vault(repo: &Repo, registry: &Registry) -> Vec<Issue> {
    let mut issues = Vec::new();
    collect_duplicate_and_nested(registry, &mut issues);
    collect_orphan_and_missing(repo.root(), registry, &mut issues);
    collect_hub_mismatch_and_tracking(repo, registry, &mut issues);
    issues
}

/// 尽量走 [`path_rules::check`] 规范化后再折叠大小写；路径本身不合规时
/// （理应已被 `Registry::validate` 挡在更早的阶段）退化为只规范化分隔符，
/// 不让一个格式错误的路径导致整个巡检 panic 或提前退出。
fn normalized_casefold(raw: &str) -> String {
    let normalized = path_rules::check(raw).unwrap_or_else(|_| path_rules::normalize(raw));
    path_rules::casefold(&normalized)
}

/// 注册表内部的重复路径 / 嵌套路径检查，收集**所有**违规而不是像
/// `Registry::validate` 那样在第一条就返回——巡检要一次性把问题都摆出来。
fn collect_duplicate_and_nested(registry: &Registry, issues: &mut Vec<Issue>) {
    let mut seen: Vec<(String, String)> = Vec::new();
    for entry in registry.datasets() {
        let folded = normalized_casefold(&entry.path);
        for (existing_path, existing_folded) in &seen {
            if existing_folded == &folded {
                issues.push(Issue::DuplicatePath {
                    path: entry.path.clone(),
                });
            } else if existing_folded.starts_with(&format!("{folded}/")) {
                // 当前条目是外层，先前登记的落在它内部。
                issues.push(Issue::NestedDataset {
                    outer: entry.path.clone(),
                    inner: existing_path.clone(),
                });
            } else if folded.starts_with(&format!("{existing_folded}/")) {
                // 先前登记的是外层，当前条目落在它内部。
                issues.push(Issue::NestedDataset {
                    outer: existing_path.clone(),
                    inner: entry.path.clone(),
                });
            }
        }
        seen.push((entry.path.clone(), folded));
    }
}

/// 递归扫描 `root` 下含 `.arca/dataset.toml` 的目录，返回相对 `root`、
/// 用 `/` 分隔的路径列表。命中一个数据集根后不再往它内部继续下钻——
/// 数据集内容（可能是几十万个受管文件）不该被当成候选归属目录扫描，
/// 也不跳进 `.git/`。
fn scan_dataset_roots(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    scan_dir(root, root, &mut found);
    found
}

fn scan_dir(base: &Path, dir: &Path, found: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if path.join(".arca").join("dataset.toml").is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                found.push(to_slash(rel));
            }
            continue;
        }
        scan_dir(base, &path, found);
    }
}

fn to_slash(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// 孤儿数据集（磁盘上有、注册表没有）与缺失数据集（注册表有、磁盘上没有）。
fn collect_orphan_and_missing(repo_root: &Path, registry: &Registry, issues: &mut Vec<Issue>) {
    let on_disk = scan_dataset_roots(repo_root);
    let on_disk_folded: Vec<String> = on_disk.iter().map(|p| normalized_casefold(p)).collect();

    for (found, folded) in on_disk.iter().zip(on_disk_folded.iter()) {
        let registered = registry
            .datasets()
            .iter()
            .any(|e| &normalized_casefold(&e.path) == folded);
        if !registered {
            issues.push(Issue::OrphanDataset {
                path: found.clone(),
            });
        }
    }
    for entry in registry.datasets() {
        let folded = normalized_casefold(&entry.path);
        if !on_disk_folded.iter().any(|f| f == &folded) {
            issues.push(Issue::MissingDataset {
                path: entry.path.clone(),
            });
        }
    }
}

/// hub 身份一致性 + 追踪冲突：只对"注册表有登记、且本地 `dataset.toml`
/// 能读出来"的数据集做这两项检查（缺失的数据集已经被 `MissingDataset` 覆盖，
/// 无需在这里重复报告）。
fn collect_hub_mismatch_and_tracking(repo: &Repo, registry: &Registry, issues: &mut Vec<Issue>) {
    // git 调用失败时静默按"没有已追踪文件"处理——见本模块顶部关于
    // check_vault 没有 Result 签名的说明。
    let tracked_files = repo.ls_files().unwrap_or_default();

    for entry in registry.datasets() {
        let dataset_toml_path = repo
            .root()
            .join(&entry.path)
            .join(".arca")
            .join("dataset.toml");
        let Ok(text) = std::fs::read_to_string(&dataset_toml_path) else {
            continue;
        };
        let Ok(cfg) = DatasetConfig::parse(&text) else {
            continue;
        };

        if let Some(hub) = registry.hub(&entry.hub) {
            if hub.instance_id != cfg.hub_instance_id {
                issues.push(Issue::HubIdMismatch {
                    path: entry.path.clone(),
                    expected: hub.instance_id.clone(),
                    found: cfg.hub_instance_id.clone(),
                });
            }
        }

        let ds_prefix = format!("{}/", entry.path.trim_matches('/'));
        let arca_prefix = format!("{ds_prefix}.arca/");
        for f in &tracked_files {
            if f.starts_with(&ds_prefix) && !f.starts_with(&arca_prefix) {
                issues.push(Issue::AlreadyTracked { path: f.clone() });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::gitarca::{DatasetEntry, HubEntry};
    use std::collections::BTreeMap;
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

    fn 写数据集(root: &Path, path: &str, dataset_id: &str, hub_instance_id: &str) {
        let dir = root.join(path).join(".arca");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("dataset.toml"),
            format!(
                "schema = 1\ndataset_id = \"{dataset_id}\"\nhub_instance_id = \"{hub_instance_id}\"\n"
            ),
        )
        .unwrap();
    }

    fn 单_hub_注册表(
        hub_name: &str,
        instance_id: &str,
        datasets: Vec<DatasetEntry>,
    ) -> Registry {
        let mut hub = BTreeMap::new();
        hub.insert(
            hub_name.to_string(),
            HubEntry {
                instance_id: instance_id.to_string(),
                url: "https://example.com".to_string(),
            },
        );
        Registry::new(hub, datasets)
    }

    const DS_ID: &str = "9c41000000000000000000000000abcd";
    const HUB_ID: &str = "3f2a000000000000000000000000beef";

    #[test]
    fn 干净的_vault_没有问题() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        写数据集(dir.path(), "assets", DS_ID, HUB_ID);

        let registry = 单_hub_注册表(
            "home",
            HUB_ID,
            vec![DatasetEntry {
                path: "assets".to_string(),
                hub: "home".to_string(),
            }],
        );
        let repo = Repo::open(dir.path()).unwrap();
        assert_eq!(check_vault(&repo, &registry), Vec::new());
    }

    #[test]
    fn 检出孤儿数据集() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        写数据集(dir.path(), "orphan", DS_ID, HUB_ID);

        let registry = 单_hub_注册表("home", HUB_ID, vec![]);
        let repo = Repo::open(dir.path()).unwrap();
        let issues = check_vault(&repo, &registry);
        assert!(issues.contains(&Issue::OrphanDataset {
            path: "orphan".to_string()
        }));
    }

    #[test]
    fn 检出缺失数据集() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        // 注册表登记了 "ghost"，但磁盘上从未创建过它的 .arca/dataset.toml。

        let registry = 单_hub_注册表(
            "home",
            HUB_ID,
            vec![DatasetEntry {
                path: "ghost".to_string(),
                hub: "home".to_string(),
            }],
        );
        let repo = Repo::open(dir.path()).unwrap();
        let issues = check_vault(&repo, &registry);
        assert!(issues.contains(&Issue::MissingDataset {
            path: "ghost".to_string()
        }));
    }

    #[test]
    fn 检出_hub_身份不符() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let wrong_hub_id = "00000000000000000000000000000000";
        写数据集(dir.path(), "assets", DS_ID, wrong_hub_id);

        let registry = 单_hub_注册表(
            "home",
            HUB_ID,
            vec![DatasetEntry {
                path: "assets".to_string(),
                hub: "home".to_string(),
            }],
        );
        let repo = Repo::open(dir.path()).unwrap();
        let issues = check_vault(&repo, &registry);
        assert!(issues.contains(&Issue::HubIdMismatch {
            path: "assets".to_string(),
            expected: HUB_ID.to_string(),
            found: wrong_hub_id.to_string(),
        }));
    }

    #[test]
    fn 检出数据集嵌套() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        写数据集(dir.path(), "assets", DS_ID, HUB_ID);
        写数据集(dir.path(), "assets/inner", DS_ID, HUB_ID);

        let registry = 单_hub_注册表(
            "home",
            HUB_ID,
            vec![
                DatasetEntry {
                    path: "assets".to_string(),
                    hub: "home".to_string(),
                },
                DatasetEntry {
                    path: "assets/inner".to_string(),
                    hub: "home".to_string(),
                },
            ],
        );
        let repo = Repo::open(dir.path()).unwrap();
        let issues = check_vault(&repo, &registry);
        assert!(issues.contains(&Issue::NestedDataset {
            outer: "assets".to_string(),
            inner: "assets/inner".to_string(),
        }));
    }

    #[test]
    fn 检出重复路径() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        写数据集(dir.path(), "assets", DS_ID, HUB_ID);

        let registry = 单_hub_注册表(
            "home",
            HUB_ID,
            vec![
                DatasetEntry {
                    path: "assets".to_string(),
                    hub: "home".to_string(),
                },
                DatasetEntry {
                    path: "assets".to_string(),
                    hub: "home".to_string(),
                },
            ],
        );
        let repo = Repo::open(dir.path()).unwrap();
        let issues = check_vault(&repo, &registry);
        assert!(issues.contains(&Issue::DuplicatePath {
            path: "assets".to_string()
        }));
    }

    #[test]
    fn 检出已被_git_追踪的文件落入数据集() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        写数据集(dir.path(), "assets", DS_ID, HUB_ID);

        // 关键场景：在 arca 接管之前，这个文件已经被 `git add` 过。
        // .gitignore 对已追踪文件无效，所以它会继续被 git 追踪。
        std::fs::write(dir.path().join("assets/leaked.bin"), b"leaked").unwrap();
        let ok = Command::new("git")
            .args(["add", "assets/leaked.bin"])
            .current_dir(dir.path())
            .status()
            .expect("需要可用的 git")
            .success();
        assert!(ok, "git add 失败");

        let registry = 单_hub_注册表(
            "home",
            HUB_ID,
            vec![DatasetEntry {
                path: "assets".to_string(),
                hub: "home".to_string(),
            }],
        );
        let repo = Repo::open(dir.path()).unwrap();
        let issues = check_vault(&repo, &registry);
        assert!(issues.contains(&Issue::AlreadyTracked {
            path: "assets/leaked.bin".to_string()
        }));
        // .arca/dataset.toml 本身也被 git 追踪，但那是应有行为，不该被误判。
        assert!(!issues.contains(&Issue::AlreadyTracked {
            path: "assets/.arca/dataset.toml".to_string()
        }));
    }
}
