//! `.gitignore` 反选块——全设计最易出错处（spec §4.3、§6.3 第 9 条）。
//!
//! **断言的是 `git check-ignore` 的实际行为，不是文本。** 文本比对能通过而行为是错的，
//! 那正是最危险的失败形态：反选没生效则协作者拿不到清单；排除没生效则整个数据集
//! 被误提交进 git。
//!
//! `git` 不可用时下面的 `建仓库` 会直接 panic（`.expect("需要可用的 git")`）——
//! 这是刻意的：静默跳过等于没有测试，宁可让 CI 环境明确报错。

use std::path::Path;
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
fn 受管二进制被忽略而_arca_元数据被追踪() {
    let dir = tempfile::tempdir().unwrap();
    建仓库(dir.path());
    std::fs::create_dir_all(dir.path().join("assets/.arca/client")).unwrap();
    std::fs::write(
        dir.path().join(".gitignore"),
        arca_git::ignore_block::render(&["assets"]),
    )
    .unwrap();

    let repo = arca_git::repo::Repo::open(dir.path()).unwrap();
    // 受管二进制：必须被忽略
    assert!(
        repo.check_ignore("assets/京都/鸭川.png").unwrap(),
        "受管二进制必须被忽略"
    );
    // 元数据：必须被追踪（反选生效）
    assert!(
        !repo.check_ignore("assets/.arca/dataset.toml").unwrap(),
        ".arca/ 必须能进 git"
    );
    assert!(
        !repo.check_ignore("assets/.arca/manifest").unwrap(),
        "清单必须能进 git"
    );
    // 本地投影：必须被忽略（设备差异不进共享配置）
    assert!(
        repo.check_ignore("assets/.arca/client/state.db").unwrap(),
        "client/ 必须被忽略"
    );
}

#[test]
fn 多数据集各自独立生效() {
    let dir = tempfile::tempdir().unwrap();
    建仓库(dir.path());
    std::fs::create_dir_all(dir.path().join("assets/.arca/client")).unwrap();
    std::fs::create_dir_all(dir.path().join("photo/.arca/client")).unwrap();
    std::fs::write(
        dir.path().join(".gitignore"),
        arca_git::ignore_block::render(&["assets", "photo"]),
    )
    .unwrap();

    let repo = arca_git::repo::Repo::open(dir.path()).unwrap();
    assert!(repo.check_ignore("assets/a.png").unwrap());
    assert!(repo.check_ignore("photo/b.jpg").unwrap());
    assert!(!repo.check_ignore("assets/.arca/dataset.toml").unwrap());
    assert!(!repo.check_ignore("photo/.arca/dataset.toml").unwrap());
    assert!(repo.check_ignore("assets/.arca/client/state.db").unwrap());
    assert!(repo.check_ignore("photo/.arca/client/state.db").unwrap());
}

#[test]
fn 块外用户内容不受影响() {
    let dir = tempfile::tempdir().unwrap();
    建仓库(dir.path());
    std::fs::create_dir_all(dir.path().join("assets/.arca/client")).unwrap();
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::write(dir.path().join("target/build.o"), b"x").unwrap();

    let mut gitignore = "*.log\ntarget/\n".to_string();
    gitignore = arca_git::ignore_block::upsert(&gitignore, &["assets"]).unwrap();

    std::fs::write(dir.path().join(".gitignore"), &gitignore).unwrap();

    let repo = arca_git::repo::Repo::open(dir.path()).unwrap();
    // 块外的用户规则依然生效
    assert!(
        repo.check_ignore("target/build.o").unwrap(),
        "用户自带的规则必须继续有效"
    );
    // 块内规则同样生效
    assert!(repo.check_ignore("assets/a.png").unwrap());
    assert!(!repo.check_ignore("assets/.arca/dataset.toml").unwrap());
}

#[test]
fn upsert_幂等_对已有块字节不变() {
    let once = arca_git::ignore_block::upsert("*.log\n", &["assets", "photo"]).unwrap();
    let twice = arca_git::ignore_block::upsert(&once, &["assets", "photo"]).unwrap();
    assert_eq!(once, twice, "对已有块再跑一次必须产出完全相同的字节");
}

#[test]
fn remove_只删块_不影响_check_ignore_的其余判断() {
    let dir = tempfile::tempdir().unwrap();
    建仓库(dir.path());
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::write(dir.path().join("target/build.o"), b"x").unwrap();

    let with_block = arca_git::ignore_block::upsert("target/\n", &["assets"]).unwrap();
    let removed = arca_git::ignore_block::remove(&with_block).unwrap();
    assert_eq!(removed, "target/\n", "remove 只删块，块外内容原样保留");

    std::fs::write(dir.path().join(".gitignore"), &removed).unwrap();
    let repo = arca_git::repo::Repo::open(dir.path()).unwrap();
    assert!(repo.check_ignore("target/build.o").unwrap());
    // 块已删除，assets 不再被忽略（因为规则已经不在 .gitignore 里了）。
    std::fs::create_dir_all(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/a.png"), b"x").unwrap();
    assert!(!repo.check_ignore("assets/a.png").unwrap());
}

#[test]
fn 数据集路径含非_ascii_时反选仍正确() {
    let dir = tempfile::tempdir().unwrap();
    建仓库(dir.path());
    std::fs::create_dir_all(dir.path().join("资料/.arca/client")).unwrap();
    std::fs::write(
        dir.path().join(".gitignore"),
        arca_git::ignore_block::render(&["资料"]),
    )
    .unwrap();

    let repo = arca_git::repo::Repo::open(dir.path()).unwrap();
    assert!(repo.check_ignore("资料/笔记本.pdf").unwrap());
    assert!(!repo.check_ignore("资料/.arca/dataset.toml").unwrap());
    assert!(!repo.check_ignore("资料/.arca/manifest").unwrap());
    assert!(repo.check_ignore("资料/.arca/client/state.db").unwrap());
}

/// task-1（反选块）与 task-2（`check_vault`）串起来的端到端场景：
/// `.gitignore` 反选块本身写对了，不代表 vault 就是一致的——
/// 一个在 arca 接管前就被 `git add` 过的文件，`.gitignore` 对它完全无效，
/// 只有 `check_vault` 的 `AlreadyTracked` 检查能把这种双重管理揪出来。
#[test]
fn check_ignore_写对了不等于_vault_一致_已追踪文件仍被_check_vault_揪出() {
    use arca_format::gitarca::{DatasetEntry, HubEntry, Registry};
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    建仓库(dir.path());

    let dataset_id = "9c41000000000000000000000000abcd";
    let hub_id = "3f2a000000000000000000000000beef";
    std::fs::create_dir_all(dir.path().join("assets/.arca")).unwrap();
    std::fs::write(
        dir.path().join("assets/.arca/dataset.toml"),
        format!("schema = 1\ndataset_id = \"{dataset_id}\"\nhub_instance_id = \"{hub_id}\"\n"),
    )
    .unwrap();

    // 反选块写对了：这一步先用 task-1 的断言方式确认。
    std::fs::write(
        dir.path().join(".gitignore"),
        arca_git::ignore_block::render(&["assets"]),
    )
    .unwrap();
    let repo = arca_git::repo::Repo::open(dir.path()).unwrap();
    assert!(!repo.check_ignore("assets/.arca/dataset.toml").unwrap());

    // 但在 arca 接管、写这份 .gitignore 之前，有人已经手工 `git add` 过一个
    // 数据集内的文件——.gitignore 对已追踪文件无效，它会继续被追踪。
    std::fs::write(dir.path().join("assets/leaked.bin"), b"leaked").unwrap();
    let ok = Command::new("git")
        .args(["add", "-f", "assets/leaked.bin"])
        .current_dir(dir.path())
        .status()
        .expect("需要可用的 git")
        .success();
    assert!(ok, "git add 失败");

    let mut hub = BTreeMap::new();
    hub.insert(
        "home".to_string(),
        HubEntry {
            instance_id: hub_id.to_string(),
            url: "https://example.com".to_string(),
        },
    );
    let registry = Registry::new(
        hub,
        vec![DatasetEntry {
            path: "assets".to_string(),
            hub: "home".to_string(),
        }],
    );

    let issues = arca_git::tracking::check_vault(&repo, &registry);
    assert!(
        issues.contains(&arca_git::tracking::Issue::AlreadyTracked {
            path: "assets/leaked.bin".to_string()
        }),
        "check_vault 必须揪出已被 git 追踪却落入数据集目录的文件：{issues:?}"
    );
}
