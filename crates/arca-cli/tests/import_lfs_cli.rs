//! `arca import lfs` 的端到端验收（M5c，spec §8）。
//!
//! `import_lfs.rs` 的单元测试覆盖了解析与校验纪律；这里覆盖命令壳那一层：
//! 默认 dry-run、退出码、以及**用户真正会敲的那条命令行到底把磁盘改成了
//! 什么样**。
//!
//! 夹具**不需要装 git-lfs**——LFS 的对象布局
//! （`.git/lfs/objects/<xx>/<yy>/<oid>`）是固定的，手工拼出来就行。
//! 这也正是实现侧不调 `git lfs` 的理由：迁入是获客的第一入口，
//! 多一步「先装 git-lfs」就少一批人。

use std::path::Path;
use std::process::{Command, Output};

fn arca(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("arca 二进制应能正常启动")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 造一个带 LFS 指针的仓库。`tamper` 非空时把落盘的对象换成别的字节
/// （模拟传输损坏 / 被人动过）；`place = false` 时干脆不放对象
/// （模拟没跑过 `git lfs pull`）。
fn 放指针(root: &Path, name: &str, content: &[u8], place: bool, tamper: Option<&[u8]>) {
    let oid = sha256_hex(content);
    let p = root.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(
        &p,
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            content.len()
        ),
    )
    .unwrap();
    if place {
        let d = root
            .join(".git/lfs/objects")
            .join(&oid[0..2])
            .join(&oid[2..4]);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(&oid), tamper.unwrap_or(content)).unwrap();
    }
}

fn 建仓库() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(d.path())
        .status()
        .unwrap()
        .success());
    放指针(d.path(), "assets/好的.png", b"GOOD-IMAGE-BYTES", true, None);
    放指针(d.path(), "assets/缺对象.png", b"MISSING", false, None);
    放指针(
        d.path(),
        "assets/被篡改.png",
        b"ORIGINAL",
        true,
        Some(b"TAMPERED"),
    );
    std::fs::write(d.path().join("笔记.md"), "# 一篇笔记\n正文").unwrap();
    d
}

fn 是指针(p: &Path) -> bool {
    std::fs::read_to_string(p)
        .map(|t| t.starts_with("version https://git-lfs.github.com/spec/v1"))
        .unwrap_or(false)
}

/// 默认是 dry-run：出清单，**一个字节都不改**。与 `arca gc` 同一条纪律。
#[test]
fn 默认dry_run不改动任何文件() {
    let d = 建仓库();
    let out = arca(d.path(), &["import", "lfs"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("lfs-ready\tassets/好的.png"), "{stdout}");
    assert!(
        是指针(&d.path().join("assets/好的.png")),
        "dry-run 之后它必须还是指针"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("没有改动任何文件"), "{stderr}");
    assert!(stderr.contains("--yes"), "要告诉用户下一步：{stderr}");
}

#[test]
fn yes之后把指针换成真实内容() {
    let d = 建仓库();
    let out = arca(d.path(), &["import", "lfs", "--yes"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("lfs-migrated"),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read(d.path().join("assets/好的.png")).unwrap(),
        b"GOOD-IMAGE-BYTES",
        "内容必须逐字节等于对象"
    );
}

/// **本文件里最重要的一条。** 两种校验失败下，指针都必须原封不动——
/// 覆盖它会同时毁掉指针（oid 是找回原内容的唯一线索）并留下一份
/// 看起来迁移成功的错误文件。
#[test]
fn 校验失败的两种情况下指针都原封不动() {
    let d = 建仓库();
    let 缺 = d.path().join("assets/缺对象.png");
    let 篡 = d.path().join("assets/被篡改.png");
    let 缺原文 = std::fs::read_to_string(&缺).unwrap();
    let 篡原文 = std::fs::read_to_string(&篡).unwrap();

    let out = arca(d.path(), &["import", "lfs", "--yes"]);

    assert_eq!(std::fs::read_to_string(&缺).unwrap(), 缺原文);
    assert_eq!(
        std::fs::read_to_string(&篡).unwrap(),
        篡原文,
        "对象内容与 oid 不符时绝不能覆盖指针"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("git lfs pull"),
        "缺对象要给出下一步：{stderr}"
    );
    assert!(
        stderr.contains("不是指针指的那一份"),
        "篡改要说清是什么问题：{stderr}"
    );
}

/// 有文件被跳过 → 退出码非 0。「一半迁成功了」不该看起来像全成功。
#[test]
fn 有跳过时退出码非零() {
    let d = 建仓库();
    let out = arca(d.path(), &["import", "lfs", "--yes"]);
    assert!(!out.status.success(), "有 2 个被跳过，不该报成功");

    // 全都健康时才是 0。
    let clean = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(clean.path())
        .status()
        .unwrap()
        .success());
    放指针(clean.path(), "a.png", b"FINE", true, None);
    let ok = arca(clean.path(), &["import", "lfs", "--yes"]);
    assert!(ok.status.success(), "{ok:?}");
}

/// 普通文件（笔记、二进制）不被碰，也不进报告。
#[test]
fn 非指针文件不被碰() {
    let d = 建仓库();
    arca(d.path(), &["import", "lfs", "--yes"]);
    assert_eq!(
        std::fs::read_to_string(d.path().join("笔记.md")).unwrap(),
        "# 一篇笔记\n正文"
    );
}

/// 不是 LFS 仓库时安静成功——绝大多数仓库本来就没用 LFS，
/// 对它们报错是噪音。
#[test]
fn 没有任何指针时安静成功() {
    let d = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(d.path())
        .status()
        .unwrap()
        .success());
    std::fs::write(d.path().join("readme.md"), "hi").unwrap();
    let out = arca(d.path(), &["import", "lfs"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "没有指针就没有数据行");
}

/// 不在 git 仓库里 → 明确报错并说清原因，不是 panic、不是静默成功。
#[test]
fn 不在git仓库里时明确报错() {
    let d = tempfile::tempdir().unwrap();
    let out = arca(d.path(), &["import", "lfs"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("git 仓库"),
        "{out:?}"
    );
}
