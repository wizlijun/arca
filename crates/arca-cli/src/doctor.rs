//! `arca doctor`（M1d Task 7）：vault 一致性巡检 + 「本地存在但 hub 尚无
//! 副本」告警，外加一个既有 `Issue` 变体的呈现纪律。
//!
//! # 债一：`git clean -xdf` 风险的唯一缓解措施
//!
//! M1c 实测确认 `git clean -xdf`（以及 `-Xdf`）**真的会删掉受管二进制**——
//! 真删、不留 tombstone、找不回来（`.gitignore` 反选块让它们对 `git status`
//! 不可见，`-x` 恰恰专挑"被忽略的文件"下手）。项目决定接受这个风险不绕过
//! （摘出反选块会破坏它，后果是整个数据集进 git，更糟——见
//! `crates/arca-git/src/hooks.rs` 的 `TODO(M1)`）。缓解措施就是这里：检出
//! 「本地存在、但 hub 索引里完全没有这条记录」的文件，显著告警——用户在
//! 跑 `git clean` 前扫一眼就该看见。
//!
//! # 债二：`Issue::CheckIncomplete` 必须显式呈现
//!
//! `arca_git::tracking::check_vault` 的 [`Issue::CheckIncomplete`] 意味着
//! 「这项检查没跑成功」，**不是「检查通过」**。`doctor` 只是把 `check_vault`
//! 返回的每一条 `Issue`（含这一变体）原样纳入报告、原样打印——不单独过滤、
//! 不折叠成"没有其它问题就是干净"，它的 `Display` 本身已经把"本次结果不
//! 完整，不能当作库是干净的"说清楚（见 `tracking.rs`）。命令壳只需要保证
//! 把它当成与其它 `Issue` 同等严重（进同一份"有问题"清单、让退出码非零），
//! 不能因为它"看起来不像一个具体错误"就单独降级成安静。

use crate::{dataset, hub, scan};
use arca_format::gitarca::Registry;
use arca_format::trace::NullSink;
use arca_git::repo::Repo;
use arca_git::tracking::{self, Issue};
use arca_store::root::{MountError, StorageRoot};
use std::path::Path;

/// 单个已在 `.gitarca` 登记、且本地也有 `dataset.toml` 的数据集的巡检结果。
/// （磁盘上完全缺失的数据集已经由 `check_vault` 的 `Issue::MissingDataset`
/// 覆盖，不在这里重复处理。）
#[derive(Debug)]
pub enum DatasetHealth {
    /// 存储根打开成功，本地扫描与远端读取都跑完了。
    Checked {
        path: String,
        /// 本地存在、但 hub 索引里完全没有这个路径记录的文件——
        /// `git clean -xdf` 会把它们永久删掉且无法找回（模块顶部「债一」）。
        local_only: Vec<String>,
    },
    /// 存储根打不开（I11：未挂载或卷身份不符）——数据集离线。**绝不能因此
    /// 假装"本地没有未同步文件"**：那本该是 `local_only` 检查要回答的问题，
    /// 离线状态下这项检查根本没跑，必须与 `Checked{local_only: vec![]}`
    /// 明确区分，不能静默退化成后者（I5、I11）。
    Offline { path: String, reason: MountError },
    /// 扫描本地或读远端失败——真正的 IO/格式故障，与"检出了问题"是不同
    /// 性质的结果。
    CheckFailed { path: String, reason: String },
}

#[derive(Debug, Default)]
pub struct DoctorReport {
    /// `arca_git::tracking::check_vault` 的原始输出，**原样收纳、不过滤**
    /// （含 `Issue::CheckIncomplete`，见模块顶部「债二」）。
    pub vault_issues: Vec<Issue>,
    pub datasets: Vec<DatasetHealth>,
}

impl DoctorReport {
    /// 是否完全干净：没有 vault 一致性问题、没有数据集离线/巡检失败、
    /// 也没有任何数据集存在本地独有文件。
    pub fn is_clean(&self) -> bool {
        self.vault_issues.is_empty()
            && self.datasets.iter().all(|d| match d {
                DatasetHealth::Checked { local_only, .. } => local_only.is_empty(),
                DatasetHealth::Offline { .. } | DatasetHealth::CheckFailed { .. } => false,
            })
    }

    /// 是否存在身份不明的数据集（I11）——命令壳据此把退出码提到 2
    /// （与 `arca fsck`/`arca sync` 的"2 = 身份不明"约定一致）。
    pub fn has_offline(&self) -> bool {
        self.datasets
            .iter()
            .any(|d| matches!(d, DatasetHealth::Offline { .. }))
    }
}

/// 对 `repo`/`registry` 描述的整个 vault 跑一次巡检。`root_override` 与
/// `arca sync --root` 同一语义（外置盘换挂载点场景），对 vault 下**所有**
/// 数据集统一生效——doctor 是全 vault 巡检，不是单数据集命令。
pub fn doctor(repo: &Repo, registry: &Registry, root_override: Option<&Path>) -> DoctorReport {
    let vault_issues = tracking::check_vault(repo, registry);
    let mut datasets = Vec::new();

    for entry in registry.datasets() {
        // 数据集本地缺失（对应 check_vault 的 MissingDataset）在这里会解析
        // 成 NotRegistered；已经被 vault_issues 覆盖，不重复诊断。其余解析
        // 失败（hub 未登记等）同理，均已由 check_vault 的其它 Issue 变体
        // 覆盖或本就意味着注册表本身不一致——doctor 的这一部分只关心"能
        // 解析到存储根之后，这个数据集健康与否"。
        let Ok(resolved) = dataset::resolve(repo.root(), &entry.path, root_override) else {
            continue;
        };

        let mut sink = NullSink;
        match StorageRoot::open(&resolved.root_path, Some(&resolved.cfg.dataset_id)) {
            Ok(store_root) => {
                let health = match check_dataset(&resolved.dataset_dir, &store_root, &mut sink) {
                    Ok(local_only) => DatasetHealth::Checked {
                        path: resolved.normalized_path,
                        local_only,
                    },
                    Err(reason) => DatasetHealth::CheckFailed {
                        path: resolved.normalized_path,
                        reason,
                    },
                };
                datasets.push(health);
            }
            Err(e) => datasets.push(DatasetHealth::Offline {
                path: resolved.normalized_path,
                reason: e,
            }),
        }
    }

    DoctorReport {
        vault_issues,
        datasets,
    }
}

/// 扫描本地 + 读远端，返回"本地存在、远端索引没有"的路径列表（按路径排序，
/// 继承 `scan::scan_dataset`/`hub::read_remote` 各自的确定性排序）。
fn check_dataset(
    dataset_dir: &Path,
    store_root: &StorageRoot,
    sink: &mut dyn arca_format::trace::TraceSink,
) -> Result<Vec<String>, String> {
    let scan_result = scan::scan_dataset(dataset_dir, sink).map_err(|e| e.to_string())?;
    let remote = hub::read_remote(store_root).map_err(|e| e.to_string())?;
    Ok(scan_result
        .files
        .keys()
        .filter(|p| !remote.contains_key(p.as_str()))
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::register::{self, RegisterOptions};
    use crate::sync;
    use crate::vault::GITARCA_FILE;
    use arca_format::model::Actor;
    use arca_format::trace::NullSink;
    use std::fs;
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

    fn actor() -> Actor {
        Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        }
    }

    /// 建一个已 register 的数据集，返回 (vault_dir, store_dir)。
    fn 建已登记的数据集(files: &[(&str, &[u8])]) -> (tempfile::TempDir, tempfile::TempDir) {
        let vault_dir = tempfile::tempdir().unwrap();
        建仓库(vault_dir.path());
        fs::write(vault_dir.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_dir.path().join("assets")).unwrap();
        for (rel, content) in files {
            let full = vault_dir.path().join("assets").join(rel);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
        }

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

        (vault_dir, store_dir)
    }

    #[test]
    fn 干净的vault且已同步的数据集没有任何问题() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);

        // 引导存储根并把文件同步上去（doctor 不负责引导，需要一个已存在的
        // 存储根——与 adopt/sync 分工一致）。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text =
            fs::read_to_string(vault_dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        let store_root = arca_store::root::StorageRoot::create(
            &root_path,
            &cfg.dataset_id,
            "2026-08-08T09:00:00Z",
        )
        .unwrap();
        let mut sink = NullSink;
        sync::sync(
            &vault_dir.path().join("assets"),
            &store_root,
            &actor(),
            &mut sink,
        )
        .unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        assert!(report.vault_issues.is_empty());
        assert!(report.is_clean(), "{report:?}");
        assert!(!report.has_offline());
    }

    #[test]
    fn 未同步的本地文件被检出为local_only() {
        let (vault_dir, _store_dir) =
            建已登记的数据集(&[("never-synced.bin", b"precious")]);

        // 只引导存储根（相当于 adopt 之前），不跑 sync——模拟"数据集已登记，
        // 但本地文件从未上传过一次"的场景，正是 `git clean -xdf` 会造成
        // 数据丢失的那种状态。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text =
            fs::read_to_string(vault_dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        arca_store::root::StorageRoot::create(&root_path, &cfg.dataset_id, "2026-08-08T09:00:00Z")
            .unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        assert_eq!(report.datasets.len(), 1);
        match &report.datasets[0] {
            DatasetHealth::Checked { path, local_only } => {
                assert_eq!(path, "assets");
                assert_eq!(local_only, &vec!["never-synced.bin".to_string()]);
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(!report.is_clean());
    }

    #[test]
    fn 未引导的存储根报告offline而不是假装干净() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        // 存储根从未被 create 过——挂载点缺失（I11）。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let report = doctor(&vault.repo, &vault.registry, None);

        assert_eq!(report.datasets.len(), 1);
        assert!(matches!(report.datasets[0], DatasetHealth::Offline { .. }));
        assert!(report.has_offline());
        assert!(!report.is_clean(), "离线数据集绝不能被判定为干净");
    }

    #[test]
    fn check_incomplete会体现在vault_issues里且被视为不干净() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        // 先在 .git 还在时打开 vault（Repo::open 需要它），再破坏 .git——
        // 让 check_vault 内部后续的 git 调用失败，产出 CheckIncomplete。
        // 与 tracking.rs 里同名场景的测试同一顺序。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        fs::remove_dir_all(vault_dir.path().join(".git")).unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        assert!(
            report
                .vault_issues
                .iter()
                .any(|i| matches!(i, Issue::CheckIncomplete { .. })),
            "{:?}",
            report.vault_issues
        );
        assert!(!report.is_clean());
    }
}
