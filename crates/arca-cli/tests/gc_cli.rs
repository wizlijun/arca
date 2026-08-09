//! `arca gc` 的命令壳验收（M2e Task 2，spec §7、I3）——**跑真的二进制**。
//!
//! `gc.rs` 的单元测试覆盖了判断与执行的全部纪律；这里覆盖的是只存在于
//! 命令壳里、库测试够不着的那一层：参数组合的闸门（`--dry-run` 与 `--yes`
//! 同时给出）、退出码、以及"从一条真实的 `arca` 命令行出发，磁盘上到底
//! 发生了什么"。
//!
//! README 第一屏那句「arca 里没有任何一条代码路径能在你不知情时销毁数据」
//! 是被测试守护的承诺（spec §7），本文件是守护它的最外层——用户真正会敲的
//! 就是这些命令行。

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

const ARCA: &str = env!("CARGO_BIN_EXE_arca");

/// 目录树快照：相对路径 → 内容的 BLAKE3。`.git/` 与 `.arca/locks/` 排除在外
/// （前者是 git 自己的账本，跑任何命令都可能动它；后者是 gc 获取跨进程排他
/// 锁时创建的零字节协调文件，见 `gc.rs` 里同名 helper 的说明）。
fn 指纹(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with(".git/") || rel == ".git" || rel.contains(".arca/locks/") {
                continue;
            }
            if ft.is_dir() {
                walk(base, &path, out);
            } else {
                let hash = std::fs::read(&path)
                    .map(|b| arca_chunk::hash::ContentHash::from_bytes(&b).to_text())
                    .unwrap_or_else(|e| format!("<读取失败：{e}>"));
                out.insert(rel, hash);
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("需要可用的 git")
        .success();
    assert!(ok, "git {args:?} 失败");
}

fn arca(vault: &Path, args: &[&str]) -> Output {
    Command::new(ARCA)
        .args(args)
        .current_dir(vault)
        .output()
        .expect("运行 arca 失败")
}

/// 建一个 vault + 一个已同步的数据集，其中 `deleted.bin` 已经走完
/// 「本地删除 → sync 提交 tombstone → 内容进 hub 回收站」的完整流程，
/// `kept.bin` 仍然健在。返回 (vault 目录, 存储根目录)。
fn 造一个有回收站条目的vault() -> (tempfile::TempDir, tempfile::TempDir) {
    let vault = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    git(vault.path(), &["init", "-q"]);
    git(vault.path(), &["config", "user.email", "t@example.com"]);
    git(vault.path(), &["config", "user.name", "t"]);
    std::fs::write(vault.path().join(".gitarca"), "schema = 1\n").unwrap();
    std::fs::create_dir_all(vault.path().join("assets")).unwrap();
    std::fs::write(vault.path().join("assets/kept.bin"), b"keep me").unwrap();
    std::fs::write(vault.path().join("assets/deleted.bin"), b"delete me").unwrap();

    let root = store.path().join("root");
    let out = arca(
        vault.path(),
        &[
            "register",
            "assets",
            "--hub",
            "home",
            "--hub-url",
            &format!("file://{}", root.display()),
        ],
    );
    assert!(out.status.success(), "register 失败：{out:?}");

    let out = arca(vault.path(), &["adopt", "assets", "--create-root"]);
    assert!(out.status.success(), "adopt 失败：{out:?}");

    std::fs::remove_file(vault.path().join("assets/deleted.bin")).unwrap();
    let out = arca(vault.path(), &["sync", "assets"]);
    assert!(out.status.success(), "sync 失败：{out:?}");
    // 前置条件：内容确实进了 hub 回收站，不是被销毁了（I3）。
    let trash_has_data = std::fs::read_dir(root.join(".arca/trash"))
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".data"));
    assert!(trash_has_data, "前置条件：删除的内容应已进入 hub 回收站");

    (vault, store)
}

/// **默认行为就是不销毁。** 不加任何开关跑 `arca gc`，即便所有条目都已过
/// 保留期（`--retention-days 0`），磁盘也必须逐字节不变。
#[test]
fn 默认不加任何开关时一个字节都不销毁() {
    let (vault, store) = 造一个有回收站条目的vault();

    let before = 指纹(store.path());
    let out = arca(vault.path(), &["gc", "assets", "--retention-days", "0"]);
    let after = 指纹(store.path());

    assert!(out.status.success(), "dry-run 出清单应当成功退出：{out:?}");
    assert_eq!(
        before, after,
        "`arca gc` 不加 --yes 时绝不能改变文件系统的任何一个字节"
    );
    // 清单确实出来了（stdout，可脚本消费）。
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l.starts_with("gc-plan\t")),
        "dry-run 必须把销毁清单打到 stdout：{stdout:?}"
    );
    assert!(
        stdout.contains("deleted.bin"),
        "清单里应点名具体路径：{stdout:?}"
    );
    // 并且明确告诉用户什么都没销毁。
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("没有销毁任何东西"),
        "必须明说本次没有销毁：{stderr:?}"
    );
}

/// `--dry-run` 与 `--yes` 同时给出是矛盾的意图——必须停下报错，绝不
/// "取其一"继续（I5）。这是命令壳独有的闸门，库测试够不着。
#[test]
fn dry_run与yes同时给出时停下报错且什么都不做() {
    let (vault, store) = 造一个有回收站条目的vault();

    let before = 指纹(store.path());
    let out = arca(
        vault.path(),
        &[
            "gc",
            "assets",
            "--retention-days",
            "0",
            "--dry-run",
            "--yes",
        ],
    );
    assert_eq!(
        before,
        指纹(store.path()),
        "矛盾的参数组合下绝不能动任何东西"
    );
    assert!(!out.status.success(), "矛盾的参数组合必须以非零退出");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("不能同时给出"), "{stderr:?}");
}

/// 加了 `--yes` 才真的销毁——而且**只销毁已过保留期的**：这里用默认的
/// 180 天保留期，刚删的那条一条都不该动。
#[test]
fn yes下未过保留期的条目仍然完好() {
    let (vault, store) = 造一个有回收站条目的vault();

    let before = 指纹(store.path());
    let out = arca(vault.path(), &["gc", "assets", "--yes"]);
    let after = 指纹(store.path());

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        before, after,
        "默认 180 天保留期内，即使加了 --yes 也一个字节都不该被销毁"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("仍在保留期内"),
        "应当明说为什么什么都没清：{stderr:?}"
    );
}

/// 过期 + `--yes`：真的销毁，且**报告列出了销毁清单**（spec §7：
/// 「gc 报告列出销毁清单」）；同时数据集里健在的文件与 hub 上它的副本
/// 必须毫发无损。
#[test]
fn 过期条目在yes下被销毁且报告列出清单_健在文件不受影响() {
    let (vault, store) = 造一个有回收站条目的vault();
    let root = store.path().join("root");

    let out = arca(
        vault.path(),
        &["gc", "assets", "--retention-days", "0", "--yes"],
    );
    assert!(out.status.success(), "{out:?}");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l.starts_with("gc-destroyed\t")),
        "spec §7：gc 报告必须列出销毁清单：{stdout:?}"
    );
    assert!(stdout.contains("deleted.bin"), "{stdout:?}");

    // 回收站里那份内容真的没了。
    let leftover: Vec<_> = std::fs::read_dir(root.join(".arca/trash"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leftover.is_empty(),
        "过期条目的 .data/.meta 都该被销毁：{leftover:?}"
    );

    // 关键：**没被删的那个文件**在 hub 上与工作区里都毫发无损。
    assert_eq!(
        std::fs::read(root.join("files/kept.bin")).unwrap(),
        b"keep me",
        "gc 绝不能碰当前存活的内容"
    );
    assert_eq!(
        std::fs::read(vault.path().join("assets/kept.bin")).unwrap(),
        b"keep me"
    );
    // 历史不被销毁（FORMAT.md §7.3）：journal 与版本链还在。
    assert!(root.join(".arca/journal").exists());
    assert!(root
        .join(".arca/items")
        .read_dir()
        .unwrap()
        .next()
        .is_some());
}

/// `--local` 走的是工作区侧的本地回收站，**不需要存储根在线**——把整个
/// 存储根删掉（等价于外置盘拔了）之后仍然能清理本机回收站。
#[test]
fn local分支在hub离线时照样能跑() {
    let (vault, store) = 造一个有回收站条目的vault();
    // 本机工作区侧回收站是空的（这台设备是默认的 client 角色），这里只
    // 验证"hub 离线不影响 --local 这条路径"这件事本身。
    std::fs::remove_dir_all(store.path()).unwrap();

    let out = arca(
        vault.path(),
        &["gc", "assets", "--local", "--retention-days", "0"],
    );
    assert!(
        out.status.success(),
        "--local 不该因为 hub 离线而失败：{out:?}"
    );
}

/// 反过来：**不带** `--local` 时存储根离线必须按 I11 报离线、退出码 2，
/// 绝不能悄悄当成"回收站是空的、没什么可清理"。
#[test]
fn hub侧gc在存储根离线时报离线而不是当空库() {
    let (vault, store) = 造一个有回收站条目的vault();
    std::fs::remove_dir_all(store.path()).unwrap();

    let out = arca(
        vault.path(),
        &["gc", "assets", "--retention-days", "0", "--yes"],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "I11：存储根离线应退出码 2：{out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("离线"), "{stderr:?}");
}
