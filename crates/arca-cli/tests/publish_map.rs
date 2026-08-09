//! `arca publish-map` 的端到端验收（M5a，spec §4.9）。
//!
//! 两条性质，第一条是隐私边界、第二条是 M5 的头条验收：
//!
//! 1. **默认只收录被 md 引用到的资源。** 直接公开整个数据集会暴露没被任何
//!    已发布笔记引用的文件——这是隐私事故的常见来源，且**不可撤回**
//!    （一旦挂上公网 CDN 就已经被抓取、被缓存）。`--all` 必须显式给出。
//! 2. **一个 blob 都不读。** 映射完全由清单构造（路径/哈希/大小三样，
//!    正是链接重写需要的全部）。CI 可以在不下载任何二进制的前提下构建站点。
//!
//! 第 2 条怎么验：把受管二进制的**内容改掉但不重新同步**，再跑一次
//! `publish-map`，断言输出与改之前**逐字节相同**。如果命令读了 blob，
//! 哈希会跟着变——这比数 syscall 更直接，也不依赖平台。

use std::path::Path;
use std::process::{Command, Output};

fn arca(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("arca 二进制应能正常启动")
}

fn git(dir: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("需要可用的 git")
        .success());
}

/// 一个 vault：一篇 md 引用了一张图，另一张图**没有任何引用**。
fn 建vault() -> (tempfile::TempDir, tempfile::TempDir) {
    let vault = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("assets/京都")).unwrap();
    std::fs::write(vault.path().join("assets/京都/鸭川.png"), b"IMAGE").unwrap();
    std::fs::write(vault.path().join("assets/私密照.png"), b"SECRET").unwrap();
    std::fs::write(
        vault.path().join("笔记.md"),
        "# 京都\n![鸭川](assets/京都/鸭川.png)\n外链 ![x](https://example.com/x.png)\n",
    )
    .unwrap();

    git(vault.path(), &["init", "-q"]);
    git(vault.path(), &["config", "user.email", "t@example.com"]);
    git(vault.path(), &["config", "user.name", "t"]);
    assert!(arca(vault.path(), &["init", "."]).status.success());
    let out = arca(
        vault.path(),
        &[
            "register",
            "assets",
            "--hub",
            "home",
            "--hub-url",
            &format!("file://{}", store.path().display()),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(arca(vault.path(), &["adopt", "assets"]).status.success());

    // 配上公开基址。
    let cfg = vault.path().join("assets/.arca/dataset.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!(
            "{}\npublic_base_url = \"https://cdn.example.com/assets\"\n",
            text.trim_end()
        ),
    )
    .unwrap();

    (vault, store)
}

/// **本文件里最重要的一条。** 未被引用的文件绝不能出现在发布映射里。
#[test]
fn 默认只收录被引用的资源() {
    let (vault, _s) = 建vault();
    let out = arca(vault.path(), &["publish-map"]);
    assert!(out.status.success(), "{out:?}");
    let json = String::from_utf8_lossy(&out.stdout);

    assert!(
        json.contains("assets/京都/鸭川.png"),
        "被引用的应当在：{json}"
    );
    assert!(
        !json.contains("私密照"),
        "**未被任何 md 引用的文件出现在了发布映射里**——一旦挂上公网 CDN \
         就已经被抓取、被缓存，不可撤回。\n{json}"
    );
    // 外链不是本 vault 的资源，不该被收进来。
    assert!(!json.contains("example.com/x.png"), "{json}");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--all"),
        "必须告诉用户「全量公开要显式 --all」：{stderr}"
    );
}

/// `--all` 才全量——**扩大暴露面必须是显式动作**。
#[test]
fn all显式给出时才全量公开() {
    let (vault, _s) = 建vault();
    let out = arca(vault.path(), &["publish-map", "--all"]);
    assert!(out.status.success(), "{out:?}");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains("私密照"), "--all 下应当全量：{json}");
}

/// **M5 的头条验收：一个 blob 都不读。**
///
/// 把受管二进制的内容改掉但**不重新同步**（清单不变），再跑一次，
/// 断言输出逐字节相同。命令若读了 blob，哈希会跟着变。
#[test]
fn 生成映射时一个blob都不读() {
    let (vault, _s) = 建vault();
    let 第一次 = arca(vault.path(), &["publish-map", "--all"]);
    assert!(第一次.status.success());

    // 内容彻底换掉，但不 sync——清单里的哈希/大小保持原样。
    std::fs::write(
        vault.path().join("assets/京都/鸭川.png"),
        b"COMPLETELY-DIFFERENT-CONTENT-MUCH-LONGER-THAN-BEFORE",
    )
    .unwrap();
    std::fs::write(vault.path().join("assets/私密照.png"), b"ALSO-CHANGED").unwrap();

    let 第二次 = arca(vault.path(), &["publish-map", "--all"]);
    assert!(第二次.status.success());
    assert_eq!(
        String::from_utf8_lossy(&第一次.stdout),
        String::from_utf8_lossy(&第二次.stdout),
        "映射必须完全由清单构造——输出变了说明命令读了 blob，\
         而「CI 不下载任何二进制也能构建站点」这条承诺就不成立了"
    );
}

/// 没配 `public_base_url` → **拒绝**，不产出半份映射。
#[test]
fn 没配public_base_url时拒绝() {
    let (vault, _s) = 建vault();
    let cfg = vault.path().join("assets/.arca/dataset.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    let 去掉 = text
        .lines()
        .filter(|l| !l.starts_with("public_base_url"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&cfg, 去掉).unwrap();

    let out = arca(vault.path(), &["publish-map"]);
    assert!(!out.status.success(), "应当拒绝：{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("public_base_url"), "{stderr}");
    assert!(stderr.contains("死链"), "要说清为什么不凑合：{stderr}");
}

/// `--out` 写文件；不给就打到 stdout（Rule of Silence 之外的数据走 stdout）。
#[test]
fn out参数把映射写进文件() {
    let (vault, _s) = 建vault();
    let target = vault.path().join("publish-map.json");
    let out = arca(
        vault.path(),
        &["publish-map", "--all", "--out", target.to_str().unwrap()],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "写文件时不该同时打到 stdout");
    let json = std::fs::read_to_string(&target).unwrap();
    assert!(json.contains("\"schema\": 1"), "{json}");
    assert!(json.contains("鸭川.png"), "{json}");
}
