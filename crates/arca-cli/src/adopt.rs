//! `arca adopt`（M1d Task 5）——**M1 验收的核心命令**：就地纳管一个已登记的
//! 数据集（已经跑过 `arca register`：`<path>/.arca/dataset.toml` 与
//! `.gitarca` 条目都已存在）。
//!
//! **只做三件事，决策全部委托给 [`crate::sync::sync`]**：
//! 1. 解析/引导存储根（`--root` 覆盖，或从 `.gitarca` 的 hub 条目解析；
//!    存储根不存在 `format.json` 时用 [`arca_store::root::StorageRoot::create`]
//!    引导一个全新的）；
//! 2. 跑一次完整的 `sync` 闭环——扫描本地 → 读基线（首次运行必然
//!    `was_reset`，因而所有本地文件在决策表里都归为 `base=absent, local=added`，
//!    对应 `Upload{parent:None}`；内容恰好已经在远端的文件走零传输
//!    `AdoptBaseline`，见 `arca_core::reconcile` 决策表）→ 按 `Action` 执行。
//!    **这正是"算哈希、上传"与"内容相同的文件走零传输认领"这两条要求的来源
//!    ——`adopt` 不需要另写一套判断，`sync` 已经覆盖了**；
//! 3. 生成清单（`<dataset>/.arca/manifest`，由 `sync` 落地后的基线渲染）、
//!    更新 `.gitignore` 反选块、把此前已被 git 追踪的受管路径逐出 index
//!    （`git rm --cached`，只动索引不动工作树——I6）。
//!
//! **I6：文件原地不动**。`sync` 对 `Upload`/`AdoptBaseline` 从不改动本地
//! 文件（只读取内容、只写存储根）；本模块自己也不 `fs::rename`/`fs::write`
//! 任何一个数据集内的用户文件。`tests/adopt.rs` 用 inode + mtime 断言这一点，
//! 不只断言"文件还在"。
//!
//! **诚实注解（spec §12.3）**：adopt 让后续提交不再增长，但已经 commit 过的
//! 二进制仍留在 git 历史里——`git rm --cached` 只影响索引与未来提交，仓库
//! 体积不会自动回落。这句话必须出现在输出里，[`AdoptOutcome`] 因此不是一个
//! "干净就什么都不说"的类型：调用方（命令壳）永远要把
//! [`AdoptOutcome::HISTORY_NOTE`] 打到 stderr。

use crate::sync;
use crate::vault::{self, HubRootError, VaultError};
use arca_format::dataset::DatasetConfig;
#[cfg(test)]
use arca_format::manifest::Manifest;
use arca_format::model::Actor;
use arca_format::path_rules::{self, PathStatus};
use arca_format::trace::TraceSink;
use arca_git::repo::GitError;
use arca_store::root::{CreateError, MountError, StorageRoot};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// 存量瘦身需要用户自行 `git filter-repo`——见模块顶部「诚实注解」。
/// 调用方（命令壳）必须把这句话打到 stderr，任何一次 `adopt` 都不例外。
pub const HISTORY_NOTE: &str =
    "注意：adopt 只阻止未来的提交继续膨胀。已经 commit 过的二进制仍留在 git 历史里——\
     `git rm --cached` 只影响索引与未来提交，仓库体积不会自动回落。\
     如需清理历史体积，请自行运行 git filter-repo（arca 不代劳）。";

pub struct AdoptOptions<'a> {
    /// 数据集路径，相对 vault 根。
    pub path: &'a str,
    /// 覆盖从 `.gitarca` 解析出的存储根路径（外置盘换挂载点等场景）。
    pub root_override: Option<&'a Path>,
    pub actor: Actor,
    /// 显式承认"就是要在这个（当前打不开的）路径上新建一个存储根"——
    /// 唯一能绕过 [`AdoptError::RootMissingButAdopted`] 这道 I11 拦截的开关
    /// （评审 Critical #2）。默认 `false`：`StorageRoot::open` 报
    /// `MountError::Absent` 时，若这个数据集此前已经被纳管过（见该错误的
    /// doc comment），一律拒绝凭空 create，绝不能让"外置盘被拔了"与
    /// "这是一个全新的根"这两种状态被静默混同。
    pub allow_create_root: bool,
}

#[derive(Debug)]
pub struct AdoptOutcome {
    pub dataset_id: String,
    pub report: sync::SyncReport,
    /// 本次是否引导了一个全新的存储根（此前 `format.json` 不存在）。
    pub bootstrapped_storage_root: bool,
    /// 因 `arca adopt` 而被逐出 git index 的路径（曾经被 git 追踪，工作树内
    /// 文件未被触碰——I6）。
    pub untracked_from_git: Vec<String>,
}

#[derive(Debug)]
pub enum AdoptError {
    Vault(VaultError),
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
    /// 存储根存在但身份不符（I11）——挂错了盘。
    Mount(MountError),
    /// 存储根缺失（`MountError::Absent`），但这个数据集**此前已经被纳管
    /// 过**——`<dataset>/.arca/manifest` 存在，那是进 git 的、"这个数据集
    /// 曾经跑过一次成功的 adopt"的权威信号（评审 Critical #2）。基线
    /// （`.arca/client/`）不能用来当这道判断的安全网：它按 I9 是可抛弃投影，
    /// 唯一能保证的就是"会被丢弃"，用一个保证会消失的信号去守一个绝不能
    /// 静默发生的操作，方向是反的。
    ///
    /// 这种情况下 `Absent` 就是"卷真的不见了"（外置盘被拔、挂载点变了），
    /// 不是"这是一个还没建过的全新根"——绝不静默 `StorageRoot::create`：
    /// 那会在幽灵挂载点（例如内部磁盘上一个恰好同名的空目录）上凭空造出
    /// 一个新的、内容为空的存储根，因为 `dataset_id` 与真正的 hub 相同，
    /// 之后 `StorageRoot::open` 会欣然接受它，把真正的 hub 呈现成"什么都
    /// 没有"——这正是 I11 明令禁止的"绝不呈现为空库"的镜像形态。
    ///
    /// 唯一的绕过方式是显式传入 `AdoptOptions::allow_create_root = true`
    /// （CLI 是 `--create-root`）：调用方必须自己说出"就是要在这里新建"，
    /// 这个决定绝不能由 arca 替用户做。
    RootMissingButAdopted {
        path: String,
        root: String,
    },
    Create(CreateError),
    PathInvalid(PathStatus),
    Sync(sync::SyncError),
    Manifest(arca_format::error::FormatError),
    Git(GitError),
    Io {
        path: String,
        reason: String,
    },
}

impl fmt::Display for AdoptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdoptError::Vault(e) => write!(f, "{e}"),
            AdoptError::NotRegistered { path } => {
                write!(
                    f,
                    "{path} 尚未登记为数据集——请先运行 `arca register {path}`"
                )
            }
            AdoptError::DatasetTomlCorrupt(e) => write!(f, "dataset.toml 解析失败：{e}"),
            AdoptError::NotInRegistry { path } => {
                write!(f, "{path} 未在 .gitarca 登记——请先运行 `arca register`")
            }
            AdoptError::HubNotFound { hub } => write!(f, "hub {hub:?} 未在 .gitarca 登记"),
            AdoptError::HubRoot(e) => write!(f, "{e}"),
            AdoptError::Mount(e) => write!(f, "{e}"),
            AdoptError::RootMissingButAdopted { path, root } => write!(
                f,
                "{path} 此前已被纳管过（{path}/.arca/manifest 存在），但存储根 {root} 缺失——\
                 按 I11 视为离线，绝不在幽灵挂载点上凭空新建根。请确认正确的挂载点/--root；\
                 如确实要在这个新位置初始化一个全新的存储根，请显式加 --create-root"
            ),
            AdoptError::Create(e) => write!(f, "{e}"),
            AdoptError::PathInvalid(s) => write!(f, "数据集路径不合规：{}", s.as_str()),
            AdoptError::Sync(e) => write!(f, "{e}"),
            AdoptError::Manifest(e) => write!(f, "生成清单失败：{e}"),
            AdoptError::Git(e) => write!(f, "{e}"),
            AdoptError::Io { path, reason } => write!(f, "{path}：{reason}"),
        }
    }
}

impl std::error::Error for AdoptError {}

pub fn adopt(
    start: &Path,
    opts: AdoptOptions,
    sink: &mut dyn TraceSink,
) -> Result<AdoptOutcome, AdoptError> {
    let vault::Vault { repo, registry } = vault::open(start).map_err(AdoptError::Vault)?;

    let normalized_path = path_rules::check(opts.path).map_err(AdoptError::PathInvalid)?;
    let dataset_dir = repo.root().join(&normalized_path);

    let dataset_toml_path = dataset_dir.join(".arca").join("dataset.toml");
    let cfg_text = match fs::read_to_string(&dataset_toml_path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(AdoptError::NotRegistered {
                path: normalized_path,
            })
        }
        Err(e) => {
            return Err(AdoptError::Io {
                path: dataset_toml_path.display().to_string(),
                reason: e.to_string(),
            })
        }
    };
    let cfg = DatasetConfig::parse(&cfg_text).map_err(AdoptError::DatasetTomlCorrupt)?;

    let entry = registry
        .datasets()
        .iter()
        .find(|e| path_rules::casefold(&e.path) == path_rules::casefold(&normalized_path))
        .ok_or_else(|| AdoptError::NotInRegistry {
            path: normalized_path.clone(),
        })?;
    let hub = registry
        .hub(&entry.hub)
        .ok_or_else(|| AdoptError::HubNotFound {
            hub: entry.hub.clone(),
        })?;

    let root_path =
        vault::resolve_hub_root(hub, opts.root_override).map_err(AdoptError::HubRoot)?;

    // 数据集自己携带着"它是否曾被 adopt 过"的信号：`.arca/manifest` 进
    // git，与基线（会被丢弃的本地投影，I9）不同，它的存在与否本身就是可信
    // 的历史证据（评审 Critical #2）。只用来判断"要不要允许在 Absent 时凭空
    // create"，不参与后续任何调和逻辑。
    let manifest_marker = dataset_dir.join(".arca").join("manifest");
    let previously_adopted = manifest_marker.is_file();

    let bootstrapped_storage_root;
    let root = match StorageRoot::open(&root_path, Some(&cfg.dataset_id)) {
        Ok(root) => {
            bootstrapped_storage_root = false;
            root
        }
        Err(MountError::Absent { path }) => {
            if previously_adopted && !opts.allow_create_root {
                return Err(AdoptError::RootMissingButAdopted {
                    path: normalized_path,
                    root: path,
                });
            }
            let created_at = crate::clock::now_rfc3339();
            let root = StorageRoot::create(&root_path, &cfg.dataset_id, &created_at)
                .map_err(AdoptError::Create)?;
            bootstrapped_storage_root = true;
            root
        }
        Err(e) => return Err(AdoptError::Mount(e)),
    };

    let report = sync::sync(&dataset_dir, &root, &opts.actor, sink).map_err(AdoptError::Sync)?;

    // 清单已经由 `sync::sync` 在收尾时从最终基线重新生成（评审 Important #4：
    // 清单不能只在 `adopt` 里生成一次，`sync` 每次收尾都要保持它与基线同步）。
    // `adopt` 不需要再单独生成一遍。

    vault::update_gitignore(
        repo.root(),
        &registry
            .normalized_datasets()
            .map_err(AdoptError::Manifest)?
            .into_iter()
            .map(|e| e.path)
            .collect::<Vec<_>>(),
    )
    .map_err(|e| match e {
        vault::UpdateIgnoreError::Block(b) => {
            AdoptError::Manifest(arca_format::error::FormatError::Malformed {
                line: 0,
                reason: b.to_string(),
            })
        }
        vault::UpdateIgnoreError::Io { path, reason } => AdoptError::Io { path, reason },
    })?;

    let untracked_from_git =
        untrack_managed_files(&repo, &normalized_path).map_err(AdoptError::Git)?;

    Ok(AdoptOutcome {
        dataset_id: cfg.dataset_id,
        report,
        bootstrapped_storage_root,
        untracked_from_git,
    })
}

/// 把这个数据集目录下、已经被 git 追踪的路径逐出 index（`.gitignore` 对
/// 已追踪路径无效——这正是 adopt 要处理的"既有附件"场景：接管之前它可能
/// 已经被 `git add` 过）。`.arca/` 下的元数据路径永远保留追踪（那些本就
/// 应该进 git）。
fn untrack_managed_files(
    repo: &arca_git::repo::Repo,
    dataset_path: &str,
) -> Result<Vec<String>, GitError> {
    let tracked = repo.ls_files()?;
    let prefix = format!("{}/", dataset_path.trim_matches('/'));
    let arca_prefix = format!("{prefix}.arca/");
    let to_untrack: Vec<String> = tracked
        .into_iter()
        .filter(|f| f.starts_with(&prefix) && !f.starts_with(&arca_prefix))
        .collect();
    repo.rm_cached(&to_untrack)?;
    Ok(to_untrack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::register::{self, RegisterOptions};
    use crate::vault::GITARCA_FILE;
    use arca_format::trace::NullSink;
    use std::os::unix::fs::MetadataExt;
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

    /// 建一个已 init + register 的 vault，数据集目录里放好若干文件；
    /// 返回 (vault_dir, storage_root_dir)。
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
                dataset_id: None,
            },
        )
        .unwrap();

        (vault_dir, store_dir)
    }

    #[test]
    fn adopt引导全新存储根并上传全部文件() {
        let (vault_dir, _store_dir) =
            建已登记的数据集(&[("a.txt", b"hello"), ("sub/b.txt", b"world")]);

        let mut sink = NullSink;
        let outcome = adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        assert!(outcome.bootstrapped_storage_root);
        assert_eq!(outcome.report.uploaded.len(), 2);
        assert!(outcome.report.is_clean());
    }

    /// **评审 C2 攻击重跑**：`arca adopt`（走 `sync::sync`，file:// 路径）
    /// 此前只写 `files/`+`items/`+`index/`，从不追加 journal 事件——用户
    /// 描述的实测："arca adopt（两个文件）+ arca sync（一个文件）之后
    /// journal 0 字节"。修复后 journal 必须包含每个上传文件各一条
    /// `Op::Upsert` 事件，`GET .../changes` 的消费者（M3 agentd 的地基）
    /// 才能看到完整的变更历史，不是一份"这些文件从未发生过任何事"的假象
    /// （I9：真相在 hub 的 journal）。
    #[test]
    fn adopt后journal包含每个上传文件的upsert事件_评审c2攻击重跑() {
        let (vault_dir, store_dir) =
            建已登记的数据集(&[("a.txt", b"hello"), ("sub/b.txt", b"world")]);

        let mut sink = NullSink;
        let outcome = adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();
        assert_eq!(outcome.report.uploaded.len(), 2);

        let root =
            arca_store::root::StorageRoot::open(&store_dir.path().join("root"), None).unwrap();
        let (_cursor, events) = crate::journal::read_all(&root).unwrap();
        assert_eq!(
            events.len(),
            2,
            "adopt 上传的两个文件都应该各自留下一条 journal 事件，实得 {events:?}"
        );
        let mut paths: Vec<&str> = events
            .iter()
            .map(|e| {
                assert_eq!(e.op, arca_format::journal::Op::Upsert);
                e.path.as_str()
            })
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.txt", "sub/b.txt"]);

        // 再来一次 `arca sync`（不经 adopt，直接调用同一个 file:// 闭环）：
        // 新增第三个文件，journal 应该追加第三条事件，不是仍然停在两条。
        fs::write(vault_dir.path().join("assets/c.txt"), b"third").unwrap();
        let report =
            crate::sync::sync(&vault_dir.path().join("assets"), &root, &actor(), &mut sink)
                .unwrap();
        assert_eq!(report.uploaded, vec!["c.txt".to_string()]);

        let (_cursor, events) = crate::journal::read_all(&root).unwrap();
        assert_eq!(events.len(), 3, "实得 {events:?}");
        assert!(events
            .iter()
            .any(|e| e.path == "c.txt" && e.op == arca_format::journal::Op::Upsert));
    }

    #[test]
    fn adopt后清单生成且内容与基线一致() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        let mut sink = NullSink;
        adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        let manifest_path = vault_dir.path().join("assets/.arca/manifest");
        assert!(manifest_path.is_file());
        let manifest = Manifest::parse(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.entries().len(), 1);
        assert_eq!(manifest.entries()[0].path, "a.txt");
        assert_eq!(
            manifest.entries()[0].hash,
            arca_chunk::hash::ContentHash::from_bytes(b"hello")
        );
    }

    #[test]
    fn adopt后gitignore反选块就位() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        let mut sink = NullSink;
        adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        let text = fs::read_to_string(vault_dir.path().join(".gitignore")).unwrap();
        assert!(text.contains("/assets/*"));
        assert!(text.contains("!/assets/.arca/"));
    }

    #[test]
    fn i6_文件原地不动_inode与mtime不变() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        let file_path = vault_dir.path().join("assets/a.txt");
        let before = fs::metadata(&file_path).unwrap();

        let mut sink = NullSink;
        adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        let after = fs::metadata(&file_path).unwrap();
        assert_eq!(
            before.ino(),
            after.ino(),
            "inode 必须不变——adopt 绝不重写/替换文件"
        );
        assert_eq!(
            before.mtime(),
            after.mtime(),
            "mtime 必须不变——adopt 只读取内容，绝不重新写入本地文件"
        );
        assert_eq!(fs::read(&file_path).unwrap(), b"hello", "内容必须原样保留");
    }

    #[test]
    fn 验收_git_status在add与commit后是干净的_清单进git_二进制不进git() {
        let (vault_dir, _store_dir) =
            建已登记的数据集(&[("a.txt", "hello二进制内容".as_bytes())]);
        let mut sink = NullSink;
        adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        let dir = vault_dir.path();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} 失败：{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "adopt assets"]);

        // 断言 1：git status 干净。
        let status = run(&["status", "--porcelain"]);
        assert!(
            status.stdout.is_empty(),
            "git status 应该干净：{}",
            String::from_utf8_lossy(&status.stdout)
        );

        // 断言 2：清单进 git。
        let ls_files = run(&["ls-files"]);
        let tracked = String::from_utf8_lossy(&ls_files.stdout);
        assert!(tracked.contains("assets/.arca/manifest"));
        assert!(tracked.contains("assets/.arca/dataset.toml"));
        assert!(tracked.contains(".gitignore"));

        // 断言 3：受管二进制不进 git。
        assert!(!tracked.contains("assets/a.txt"));
        let repo = arca_git::repo::Repo::open(dir).unwrap();
        assert!(
            repo.check_ignore_no_index("assets/a.txt").unwrap(),
            ".gitignore 反选规则本身必须匹配这个受管路径"
        );
    }

    #[test]
    fn 既有附件先被git追踪_adopt后从index逐出但工作树文件保留() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"legacy content")]);
        let dir = vault_dir.path();

        // 模拟"既有附件"：adopt 之前，这个文件已经被人手工 git add 过。
        let ok = Command::new("git")
            .args(["add", "-f", "assets/a.txt"])
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let repo_before = arca_git::repo::Repo::open(dir).unwrap();
        assert!(repo_before
            .ls_files()
            .unwrap()
            .contains(&"assets/a.txt".to_string()));

        let mut sink = NullSink;
        let outcome = adopt(
            dir,
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        assert_eq!(outcome.untracked_from_git, vec!["assets/a.txt".to_string()]);
        let repo_after = arca_git::repo::Repo::open(dir).unwrap();
        assert!(
            !repo_after
                .ls_files()
                .unwrap()
                .contains(&"assets/a.txt".to_string()),
            "adopt 之后不应再被 git index 追踪"
        );
        assert!(
            dir.join("assets/a.txt").is_file(),
            "工作树里的文件必须原地保留（I6）"
        );
        assert_eq!(
            fs::read(dir.join("assets/a.txt")).unwrap(),
            b"legacy content"
        );
    }

    #[test]
    fn 未先register直接adopt报错() {
        let vault_dir = tempfile::tempdir().unwrap();
        建仓库(vault_dir.path());
        fs::write(vault_dir.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_dir.path().join("assets")).unwrap();
        fs::write(vault_dir.path().join("assets/a.txt"), b"x").unwrap();

        let mut sink = NullSink;
        let err = adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap_err();
        assert!(matches!(err, AdoptError::NotRegistered { .. }));
    }

    #[test]
    fn 内容相同的文件走零传输认领() {
        // 两次各自"新建"数据集（不同的 vault，共用同一个存储根），adopt 后
        // 第二次应当以 AdoptBaseline 收敛，而不是重复上传。
        let store_dir = tempfile::tempdir().unwrap();
        let root_path = store_dir.path().join("root");

        let vault_a = tempfile::tempdir().unwrap();
        建仓库(vault_a.path());
        fs::write(vault_a.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_a.path().join("assets")).unwrap();
        fs::write(vault_a.path().join("assets/shared.txt"), b"same content").unwrap();
        register::register(
            vault_a.path(),
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
        let mut sink = NullSink;
        adopt(
            vault_a.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        let dataset_id =
            fs::read_to_string(vault_a.path().join("assets/.arca/dataset.toml")).unwrap();
        let dataset_id = DatasetConfig::parse(&dataset_id).unwrap().dataset_id;
        let hub_instance_id = vault::open(vault_a.path())
            .unwrap()
            .registry
            .hub("home")
            .unwrap()
            .instance_id
            .clone();

        let vault_b = tempfile::tempdir().unwrap();
        建仓库(vault_b.path());
        fs::write(vault_b.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_b.path().join("assets")).unwrap();
        fs::write(vault_b.path().join("assets/shared.txt"), b"same content").unwrap();
        register::register(
            vault_b.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: Some(&hub_instance_id),
                hub_url: Some(&format!("file://{}", root_path.display())),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap();
        // 让第二个 vault 声明同一个 dataset_id（模拟两台设备各自导入了同一批
        // 内容，最终都指向同一个数据集）。
        let cfg_path = vault_b.path().join("assets/.arca/dataset.toml");
        let cfg = DatasetConfig {
            schema: 1,
            dataset_id: dataset_id.clone(),
            hub_instance_id,
            public_base_url: None,
            url_style: None,
        };
        fs::write(&cfg_path, cfg.to_toml().unwrap()).unwrap();

        let outcome_b = adopt(
            vault_b.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        assert!(!outcome_b.bootstrapped_storage_root);
        assert_eq!(outcome_b.report.adopted, vec!["shared.txt".to_string()]);
        assert!(
            outcome_b.report.uploaded.is_empty(),
            "零传输：不应该重复上传"
        );
    }

    /// 评审 Critical #2 的核心复现测试：数据集此前已经被成功 adopt 过
    /// （`.arca/manifest` 存在），随后存储根整个"消失"（外置盘被拔——把
    /// 整个存储根目录搬走模拟这个场景）。第二次 `adopt` 必须拒绝、且绝不
    /// 在幽灵挂载点上创建任何东西——这正是评审实测到的"幽灵挂载点上凭空
    /// 造出一个新存储根，真正的 hub 被静默呈现成空库"的那条路径。
    #[test]
    fn 已纳管过的数据集存储根消失时adopt拒绝创建新根() {
        let (vault_dir, store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        let mut sink = NullSink;
        adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();
        assert!(
            vault_dir.path().join("assets/.arca/manifest").is_file(),
            "测试前置条件：第一次 adopt 应该已经生成清单"
        );

        // 模拟"外置盘被拔"：整个存储根目录消失，挂载点路径本身还在
        // （`root_path` 是 `store_dir/root`），但 `format.json` 及一切内容
        // 都不见了——这正是幽灵挂载点场景（评审用例把根目录整体移走）。
        let root_path = store_dir.path().join("root");
        assert!(root_path.exists(), "测试前置条件：存储根应该已经存在");
        fs::remove_dir_all(&root_path).unwrap();

        let err = adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap_err();

        assert!(
            matches!(err, AdoptError::RootMissingButAdopted { .. }),
            "应报 RootMissingButAdopted，实得 {err:?}"
        );
        // 绝不能在幽灵挂载点上凭空创建任何东西——I11：未挂载就是未挂载，
        // 不能被静默呈现成"一个刚建好的空库"。
        assert!(
            !root_path.exists(),
            "拒绝时绝不应该创建任何目录，实际却出现了 {}",
            root_path.display()
        );
    }

    /// `--create-root`（`AdoptOptions::allow_create_root`）是唯一能绕过上面
    /// 那道拦截的显式开关：用户明确说"就是要在这里新建"时才创建。
    #[test]
    fn 已纳管过的数据集加上allow_create_root后可以显式重建() {
        let (vault_dir, store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        let mut sink = NullSink;
        adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();

        let root_path = store_dir.path().join("root");
        fs::remove_dir_all(&root_path).unwrap();

        let outcome = adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: true,
            },
            &mut sink,
        )
        .unwrap();
        assert!(outcome.bootstrapped_storage_root);
        assert!(root_path.join(".arca/format.json").is_file());
    }

    /// 对照组：从未 adopt 过（`.arca/manifest` 不存在）时，`Absent` 就是
    /// "这是一个还没建过的全新根"——应当正常引导，不需要 `--create-root`。
    /// （`adopt引导全新存储根并上传全部文件` 已经隐含覆盖了这一路径，这里
    /// 显式把断言写清楚：manifest 确实不存在，且不需要 allow_create_root。）
    #[test]
    fn 从未adopt过时manifest不存在存储根照常正常引导() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        assert!(
            !vault_dir.path().join("assets/.arca/manifest").exists(),
            "测试前置条件：首次 adopt 之前不应该有清单"
        );

        let mut sink = NullSink;
        let outcome = adopt(
            vault_dir.path(),
            AdoptOptions {
                path: "assets",
                root_override: None,
                actor: actor(),
                allow_create_root: false,
            },
            &mut sink,
        )
        .unwrap();
        assert!(outcome.bootstrapped_storage_root);
    }
}
