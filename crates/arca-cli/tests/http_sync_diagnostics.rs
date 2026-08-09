//! 评审 Minor #3 的核心复现测试：`arca sync <path>` 面对一个连不上的
//! `http://` hub（连接被拒绝，`TransportError::Network`，不是
//! `TransportError::Offline`）时，诊断必须点名数据集与 hub——此前只有
//! `Offline`（服务端 503）会点名，`Network` 只打出一句光秃秃的
//! "网络故障：Connection refused"，单数据集调用路径没有 `sync_all` 的
//! `== path ==` 表头兜底，用户拿到的诊断无法归因是哪个数据集出的问题。
//!
//! 与 `tests/multi_hub.rs`/`tests/replica_warning.rs` 同一条理由：命令壳
//! 依赖进程级 `cwd()`，用真实编译好的 `arca` 二进制 + 独立工作目录跑。

use std::path::Path;
use std::process::Command;

fn arca_bin() -> &'static str {
    env!("CARGO_BIN_EXE_arca")
}

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(arca_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("arca 二进制应能正常启动")
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("需要可用的 git")
        .success();
    assert!(ok, "git {args:?} 在 {dir:?} 失败");
}

#[test]
fn 连不上的http_hub报错点名数据集与hub不是光秃秃的网络故障() {
    let vault_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault_dir.path().join("assets")).unwrap();
    std::fs::write(vault_dir.path().join("assets/a.txt"), b"hello").unwrap();
    git(vault_dir.path(), &["init", "-q"]);
    git(vault_dir.path(), &["config", "user.email", "t@example.com"]);
    git(vault_dir.path(), &["config", "user.name", "t"]);

    let out = run(vault_dir.path(), &["init", "."]);
    assert!(out.status.success(), "arca init 失败：{out:?}");
    // 端口 1 是特权端口，未起任何服务时连接会被立刻拒绝——不依赖真的
    // arcad 进程，`TransportError::Network`（连接被拒绝）与
    // `TransportError::Offline`（服务端 503）是两种不同的失败形状，这里要
    // 测的正是前者此前没有归属信息的问题。
    let out = run(
        vault_dir.path(),
        &[
            "register",
            "assets",
            "--hub",
            "home",
            "--hub-url",
            "http://127.0.0.1:1/unreachable",
        ],
    );
    assert!(out.status.success(), "register 失败：{out:?}");

    let out = run(vault_dir.path(), &["sync", "assets"]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("数据集 assets") && stderr.contains("hub=home"),
        "连接被拒绝这类 Network 失败也必须点名数据集与 hub，不能是一句光秃秃的\
         网络故障：{stderr}"
    );
    assert!(
        stderr.contains("网络故障"),
        "底层原因（网络故障：Connection refused）仍应保留，只是要加归属：{stderr}"
    );
}
