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
    /// 某一小项检查因为 IO / git 调用失败而没能跑起来——**不代表"检查了、
    /// 没问题"**。`check_vault` 的调用方看到这个变体，就必须知道本次巡检
    /// 不完整：不能把"结果里没有其它 Issue"当成"库是干净的"（I5；评审
    /// Important #2）。`check` 是触发失败的检查项标识（如
    /// `"already_tracked"`、`"orphan_scan"`），`reason` 是失败原因的人类可读描述。
    CheckIncomplete { check: &'static str, reason: String },
    /// 数据集引用了一个 `.gitarca` 里没有登记的 hub 名。`Registry::validate` 会
    /// 拒绝这种注册表，但 `check_vault` 不假设调用方总是先跑过 `validate()`——
    /// 防御性巡检必须独立发现同一个问题，否则 §11 防误绑检查（`HubIdMismatch`）
    /// 会因为找不到 hub 条目而被静默跳过，误报出方向完全错误的
    /// `MissingDataset`/"需要 `arca setup`"（评审 Important #2）。
    UnknownHub { path: String, hub: String },
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
            Issue::CheckIncomplete { check, reason } => write!(
                f,
                "巡检未完成（{check}）：{reason}——本次结果不完整，\
                 不能当作\"库是干净的\""
            ),
            Issue::UnknownHub { path, hub } => write!(
                f,
                "数据集 {path:?} 引用了未登记的 hub {hub:?}；.gitarca 本身不一致，\
                 需要先修好注册表（spec §4.3.2）"
            ),
        }
    }
}

/// 对 `repo` 与 `registry` 做一遍一致性巡检，返回发现的所有问题（可能为空）。
///
/// **只报告不修复**（I5）。IO / git 调用本身失败时，对应的那一小项检查会被跳过，
/// 但 `check_vault` **不会静默吞掉这个事实**：它会为每一处跳过 push 一条
/// [`Issue::CheckIncomplete`]。调用方看到返回值里出现这个变体，就必须知道
/// 本次巡检不完整，不能把"结果里没有其它 Issue"当成"库是干净的"——
/// `Issue` 是可扩展枚举，新增变体不需要改 `check_vault` 的签名（评审 Important #2）。
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
/// 用 `/` 分隔的路径列表，按字节序排序。命中一个数据集根后不再往它内部
/// 继续下钻——数据集内容（可能是几十万个受管文件）不该被当成候选归属目录
/// 扫描，也不跳进 `.git/`。
///
/// 排序是必须的：`std::fs::read_dir` 的产出顺序不保证稳定，不排序会导致
/// 同一磁盘状态两次调用产出不同顺序的 `Issue` 列表（评审 Minor #4，
/// 与 `ignore_block::render` 显式排序的对称要求一致）。
///
/// **任何一步 IO 失败都必须留痕**（评审 Important #1，doc comment 承诺过的
/// 「为每一处跳过 push 一条 `CheckIncomplete`」）：不可读的目录、遍历中途读
/// 目录项失败、`file_type()` 失败，都会各自 push 一条 `CheckIncomplete`，
/// 而不是像旧实现那样 `Err(_) => return` / `entries.flatten()` /
/// `let Ok(..) = .. else { continue }` 悄悄跳过——那样会把"目录不可读"
/// 误报成"这里没有孤儿数据集"，返回一个看起来干净、实则不完整的结果。
fn scan_dataset_roots(root: &Path, issues: &mut Vec<Issue>) -> Vec<String> {
    let mut found = Vec::new();
    scan_dir(root, root, &mut found, issues);
    found.sort_unstable();
    found
}

fn scan_dir(base: &Path, dir: &Path, found: &mut Vec<String>, issues: &mut Vec<Issue>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            issues.push(Issue::CheckIncomplete {
                check: "orphan_scan",
                reason: format!("读取目录 {} 失败：{e}", dir.display()),
            });
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                issues.push(Issue::CheckIncomplete {
                    check: "orphan_scan",
                    reason: format!("遍历目录 {} 时读取目录项失败：{e}", dir.display()),
                });
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                issues.push(Issue::CheckIncomplete {
                    check: "orphan_scan",
                    reason: format!("读取 {} 的文件类型失败：{e}", entry.path().display()),
                });
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if path.join(".arca").join("dataset.toml").is_file() {
            match path.strip_prefix(base) {
                Ok(rel) => found.push(to_slash(rel)),
                // 逻辑上不可达（`path` 恒是 `base` 的递归下钻结果），但绝不能
                // 因此裸吞：一旦条件变化导致真的走到这里，宁可报出
                // CheckIncomplete 也不要悄悄漏掉一个数据集根。
                Err(_) => issues.push(Issue::CheckIncomplete {
                    check: "orphan_scan",
                    reason: format!(
                        "{} 不在 {} 之下，无法计算相对路径",
                        path.display(),
                        base.display()
                    ),
                }),
            }
            continue;
        }
        scan_dir(base, &path, found, issues);
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
    let on_disk = scan_dataset_roots(repo_root, issues);
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
    // git 调用失败时按"没有已追踪文件"继续（下面 AlreadyTracked 检测因此
    // 什么都查不出来），但必须如实报告这一项检查没跑成功——不能让调用方
    // 把"没查出问题"误读成"查过了、没问题"（评审 Important #2）。
    let tracked_files = match repo.ls_files() {
        Ok(files) => files,
        Err(e) => {
            issues.push(Issue::CheckIncomplete {
                check: "already_tracked",
                reason: format!("git ls-files 失败：{e}"),
            });
            Vec::new()
        }
    };

    for entry in registry.datasets() {
        let dataset_toml_path = repo
            .root()
            .join(&entry.path)
            .join(".arca")
            .join("dataset.toml");
        let text = match std::fs::read_to_string(&dataset_toml_path) {
            Ok(text) => text,
            // 文件本就不存在：已经由 collect_orphan_and_missing 的
            // MissingDataset 覆盖，不重复报告。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // 存在但读不出来（权限、IO 错误等）：这是真正的"检查没跑起来"，
            // 且会连带跳过 HubIdMismatch——spec §11 的防误绑安全检查，
            // 比少几条 AlreadyTracked 更值得警惕，必须显式报告。
            Err(e) => {
                issues.push(Issue::CheckIncomplete {
                    check: "hub_id_mismatch/already_tracked",
                    reason: format!("读取 {} 失败：{e}", dataset_toml_path.display()),
                });
                continue;
            }
        };
        let cfg = match DatasetConfig::parse(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                issues.push(Issue::CheckIncomplete {
                    check: "hub_id_mismatch/already_tracked",
                    reason: format!("解析 {} 失败：{e}", dataset_toml_path.display()),
                });
                continue;
            }
        };

        match registry.hub(&entry.hub) {
            Some(hub) => {
                if hub.instance_id != cfg.hub_instance_id {
                    issues.push(Issue::HubIdMismatch {
                        path: entry.path.clone(),
                        expected: hub.instance_id.clone(),
                        found: cfg.hub_instance_id.clone(),
                    });
                }
            }
            // hub 名本身不存在：这理应被 `Registry::validate` 挡住，但
            // `check_vault` 不能假设调用方已经 validate 过——静默跳过会连带
            // 吞掉这条数据集本可以做的 HubIdMismatch 检查，且不留任何痕迹
            // （评审 Important #2）。
            None => issues.push(Issue::UnknownHub {
                path: entry.path.clone(),
                hub: entry.hub.clone(),
            }),
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
                tls_pin: None,
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

    // --- 评审 Important #2：静默降级必须换成 Issue::CheckIncomplete ---

    #[test]
    fn git_命令失败时报告_check_incomplete_而不是当成没有已追踪文件() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        写数据集(dir.path(), "assets", DS_ID, HUB_ID);
        let repo = Repo::open(dir.path()).unwrap();

        // 破坏仓库本身（删掉 .git），让后续 `git ls-files` 调用失败——
        // 模拟"检查没跑起来"，必须与"检查了、确实没有已追踪文件"区分开。
        std::fs::remove_dir_all(dir.path().join(".git")).unwrap();

        let registry = 单_hub_注册表(
            "home",
            HUB_ID,
            vec![DatasetEntry {
                path: "assets".to_string(),
                hub: "home".to_string(),
            }],
        );
        let issues = check_vault(&repo, &registry);
        assert!(
            issues.iter().any(
                |i| matches!(i, Issue::CheckIncomplete { check, .. } if *check == "already_tracked")
            ),
            "git ls-files 失败必须报告 CheckIncomplete，而不是静默按\"无已追踪文件\"处理：{issues:?}"
        );
    }

    #[test]
    fn dataset_toml_读取失败时报告_check_incomplete_不吞掉_hub_id_mismatch_检查() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        // 注册表登记了 "assets"，但 `<path>/.arca/dataset.toml` 这个路径本身
        // 被造成了一个目录——读取必然失败，且不是 NotFound（NotFound 那种
        // 情形已经由 MissingDataset 覆盖，不该在这里重复报告）。
        std::fs::create_dir_all(dir.path().join("assets/.arca/dataset.toml")).unwrap();

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
        assert!(
            issues.iter().any(|i| matches!(
                i,
                Issue::CheckIncomplete { check, .. } if check.contains("hub_id_mismatch")
            )),
            "dataset.toml 读取失败必须报告 CheckIncomplete——它连带跳过的 \
             HubIdMismatch 是 spec §11 的防误绑安全检查，不能被静默吞掉：{issues:?}"
        );
    }

    #[test]
    fn dataset_toml_解析失败时报告_check_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let toml_dir = dir.path().join("assets/.arca");
        std::fs::create_dir_all(&toml_dir).unwrap();
        std::fs::write(toml_dir.join("dataset.toml"), "not valid toml {{{").unwrap();

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
        assert!(
            issues.iter().any(|i| matches!(
                i,
                Issue::CheckIncomplete { check, .. } if check.contains("hub_id_mismatch")
            )),
            "dataset.toml 解析失败必须报告 CheckIncomplete：{issues:?}"
        );
    }

    /// chmod 对 root 无效、部分文件系统也不支持权限位；先自证一次假设是否
    /// 成立，不成立就跳过（打印说明，不静默跳过），不假设当前一定以非 root
    /// 身份运行（与 arca-store `tests/fsck.rs` 同一纪律）。
    #[test]
    #[cfg(unix)]
    fn 不可读目录里的孤儿数据集报告_check_incomplete_而不是当作库是干净的() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        let locked = dir.path().join("locked");
        写数据集(&locked, "orphan", DS_ID, HUB_ID);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("跳过：当前用户不受 chmod 0o000 限制（root 或文件系统不支持权限位）");
            return;
        }

        let registry = 单_hub_注册表("home", HUB_ID, vec![]);
        let repo = Repo::open(dir.path()).unwrap();
        let issues_while_locked = check_vault(&repo, &registry);
        // 恢复权限，否则 tempdir 在 Drop 时清理不掉这个目录；且要在断言之前做，
        // 避免断言失败时把一个不可清理的目录留在临时目录里。
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        // 复现评审构造的场景：旧实现在这里会静默吞掉 read_dir 失败，返回空列表，
        // 也就是"库是干净的"——这正是本条 Important 要堵住的假阴性。
        assert!(
            issues_while_locked.iter().any(|i| matches!(
                i,
                Issue::CheckIncomplete { check, .. } if *check == "orphan_scan"
            )),
            "目录不可读必须报告 CheckIncomplete(\"orphan_scan\")，绝不能让 check_vault \
             返回看起来干净的空列表：{issues_while_locked:?}"
        );

        // 恢复权限后重新巡检：孤儿数据集必须被正确检出，证明差别只在于
        // "本次巡检是否完整"有没有被如实报告，而不是扫描逻辑本身有问题。
        let issues_after_restore = check_vault(&repo, &registry);
        assert!(
            issues_after_restore.contains(&Issue::OrphanDataset {
                path: "locked/orphan".to_string()
            }),
            "恢复权限后必须正确报出孤儿数据集：{issues_after_restore:?}"
        );
    }

    // --- 评审 Important #2：hub 名不存在时不能被静默跳过 ---

    #[test]
    fn 引用不存在的_hub_名报告_unknown_hub_而不是当作库是干净的() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        写数据集(dir.path(), "assets", DS_ID, HUB_ID);

        // 注册表本身就不一致：数据集引用了一个没有 [hub.*] 条目的 hub 名。
        // `Registry::validate()` 会拒绝这种注册表，但 `check_vault` 不能假设
        // 调用方已经先跑过 validate。
        let registry = Registry::new(
            BTreeMap::new(),
            vec![DatasetEntry {
                path: "assets".to_string(),
                hub: "ghost-hub".to_string(),
            }],
        );
        let repo = Repo::open(dir.path()).unwrap();
        let issues = check_vault(&repo, &registry);

        assert!(
            issues.contains(&Issue::UnknownHub {
                path: "assets".to_string(),
                hub: "ghost-hub".to_string(),
            }),
            "引用不存在的 hub 名必须被明确报告，而不是让 check_vault 返回空列表：{issues:?}"
        );
    }
}
