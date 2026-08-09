//! agentd 的多 hub 独立故障域（M3a Task 2，spec §4.3.2、I11）。
//!
//! 「一个 hub 不可达时，只有它承载的数据集进入离线态，其余数据集完全不受
//! 影响」——这条在 M2d 已经为**手动命令**验过，本文件验的是 **agentd**：
//! 它把每个数据集跑成一个独立 task，所以最容易写错的那个形态（用一个 `?`
//! 中止整个循环）从结构上就不该存在。但「不该存在」要被证明。
//!
//! **判据是字节，不是退出码。** M2d 的评审专门强调过这一点：健康数据集的
//! 新文件必须真的落到它自己的 store 里，光看退出码证明不了任何事——一个
//! 什么都没做就返回的实现同样能给出"正确"的退出码。

use std::path::Path;
use std::process::{Command, Output};

/// `arca` 二进制的路径。
///
/// 不能用 `env!("CARGO_BIN_EXE_arca")`——那个环境变量只对**同一个 package**
/// 的二进制有定义，而 `arca` 属于 `arca-cli`。改从 agentd 自己的二进制路径
/// 推：两者由同一次 `cargo test` 构建进同一个 target 目录，是兄弟文件。
fn arca_bin() -> std::path::PathBuf {
    let me = std::path::Path::new(env!("CARGO_BIN_EXE_arca-agentd"));
    let p = me.with_file_name(format!("arca{}", std::env::consts::EXE_SUFFIX));
    assert!(
        p.exists(),
        "{}不存在——本测试需要 arca 二进制，请跑 `cargo test --workspace` \
         或先 `cargo build -p arca-cli`",
        p.display()
    );
    p
}

fn arca(dir: &Path, args: &[&str]) -> Output {
    Command::new(arca_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("arca 二进制应能正常启动")
}

fn agentd(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arca-agentd"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("arca-agentd 二进制应能正常启动")
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

/// 一个 vault、两个数据集、两个各自独立的存储根。
fn 建两数据集两hub() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
    let vault = tempfile::tempdir().unwrap();
    let store_a = tempfile::tempdir().unwrap();
    let store_b = tempfile::tempdir().unwrap();

    for name in ["alpha", "beta"] {
        std::fs::create_dir_all(vault.path().join(name)).unwrap();
        std::fs::write(vault.path().join(name).join("seed.bin"), name).unwrap();
    }
    git(vault.path(), &["init", "-q"]);
    git(vault.path(), &["config", "user.email", "t@example.com"]);
    git(vault.path(), &["config", "user.name", "t"]);
    assert!(arca(vault.path(), &["init", "."]).status.success());

    for (name, store) in [("alpha", store_a.path()), ("beta", store_b.path())] {
        let out = arca(
            vault.path(),
            &[
                "register",
                name,
                "--hub",
                name, // 每个数据集一个自己的 hub 名 → 独立故障域
                "--hub-url",
                &format!("file://{}", store.display()),
            ],
        );
        assert!(out.status.success(), "register {name} 失败：{out:?}");
        let out = arca(vault.path(), &["adopt", name]);
        assert!(out.status.success(), "adopt {name} 失败：{out:?}");
    }
    (vault, store_a, store_b)
}

/// 断开 alpha 的 hub（把存储根整个移走，等价于外置盘被拔），
/// beta 的新文件必须**真的传上去**。
#[test]
fn 一个hub不可达时另一个数据集的字节照样落地() {
    let (vault, store_a, store_b) = 建两数据集两hub();

    // 拔盘：把 alpha 的存储根改名。用改名而不是删除——I11 要区分的是
    // 「盘不在」而不是「盘在但空了」，改名后原路径彻底不存在，正是前者。
    let 拔走的 = store_a.path().with_extension("unplugged");
    std::fs::rename(store_a.path(), &拔走的).unwrap();

    // 两侧各放一个新文件。
    std::fs::write(vault.path().join("alpha/new.bin"), b"ALPHA-NEW").unwrap();
    std::fs::write(vault.path().join("beta/new.bin"), b"BETA-NEW").unwrap();

    let out = agentd(vault.path(), &["--once"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // 关键断言：**beta 的字节真的落地了**。
    let beta_landed = store_b.path().join("files/new.bin");
    assert!(
        beta_landed.exists(),
        "alpha 的 hub 不可达不该妨碍 beta 同步——beta 的新文件没落地。\nagentd 输出：\n{stderr}"
    );
    assert_eq!(
        std::fs::read(&beta_landed).unwrap(),
        b"BETA-NEW",
        "beta 落地的内容不对"
    );

    // alpha 必须被明确报告为有问题，**并且说清是哪个 hub**——一个 vault 里
    // 多个数据集分属不同 hub 时，光报路径不足以让用户判断该去查哪个。
    assert!(
        stderr.contains("alpha"),
        "必须点名出问题的数据集：\n{stderr}"
    );
    assert!(
        stderr.contains("hub=alpha"),
        "必须点名是哪个 hub（M2d 的教训）：\n{stderr}"
    );

    // alpha 的本地文件**一个都没被删**——离线绝不能被当成「远端把它删了」（I11）。
    assert!(
        vault.path().join("alpha/seed.bin").exists(),
        "hub 离线期间绝不能删本地文件"
    );
    assert!(vault.path().join("alpha/new.bin").exists());

    // agentd 整体退出非 0：确实有一个数据集没能同步，不该报成功。
    assert!(
        !out.status.success(),
        "有数据集离线时 agentd --once 不该报成功：\n{stderr}"
    );

    // 把盘挂回去，agentd 应当自己接上——不需要任何手工干预。
    std::fs::rename(&拔走的, store_a.path()).unwrap();
    let out = agentd(vault.path(), &["--once"]);
    assert!(
        out.status.success(),
        "盘挂回来之后应当恢复：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(store_a.path().join("files/new.bin")).unwrap(),
        b"ALPHA-NEW",
        "盘挂回来之后 alpha 的积压应当被补上"
    );
}

/// 一个数据集在 `.gitarca` 里指向一个根本解析不出来的 hub，**不能让整个
/// daemon 起不来**——这是 `for` 循环里那个 `?` 的经典形态。
#[test]
fn 一个数据集解析失败不影响其余数据集启动回路() {
    let (vault, _store_a, store_b) = 建两数据集两hub();

    // 把 alpha 的 dataset.toml 弄坏，让 resolve 失败。
    let cfg = vault.path().join("alpha/.arca/dataset.toml");
    assert!(cfg.exists(), "前置条件：dataset.toml 应当存在");
    std::fs::write(&cfg, "这不是合法的 TOML {{{").unwrap();

    std::fs::write(vault.path().join("beta/new.bin"), b"BETA-STILL-WORKS").unwrap();
    let out = agentd(vault.path(), &["--once"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        store_b.path().join("files/new.bin").exists(),
        "alpha 解析失败不该妨碍 beta 启动回路并完成同步：\n{stderr}"
    );
    assert!(
        stderr.contains("alpha") && stderr.contains("解析失败"),
        "必须明确报告是哪个数据集解析失败：\n{stderr}"
    );
}
