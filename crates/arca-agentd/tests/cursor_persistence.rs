//! agentd 的增量游标在**真实运行**中被持久化并被复用（M3a Task 3，
//! FORMAT.md §9.6）。
//!
//! `src/cursor.rs` 的单元测试覆盖了读写与损坏处置；这里覆盖的是只有跑起来
//! 才能验的那一层：**`--once` 也必须落盘游标**。
//!
//! 这一条是实机跑出来的：第一版只在长驻回路里落盘，`--once` 直接 return，
//! 于是每次脚本化调用（演练、CI、cron）都从头做一次全量对账——更糟的是，
//! **游标持久化这条路径也就永远不会被这些流程走到**。一个只在长驻模式下
//! 才生效的持久化，等于一个没被日常验证覆盖的持久化。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc;

const DATASET_ID: &str = "9c41000000000000000000000000abcd";

fn arca_bin() -> std::path::PathBuf {
    let me = std::path::Path::new(env!("CARGO_BIN_EXE_arca-agentd"));
    let p = me.with_file_name(format!("arca{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "本测试需要 arca 二进制：{}", p.display());
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
    assert!(Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("需要可用的 git")
        .success());
}

/// 最小 mock hub：`/state` 回空清单，`/changes` 回一个固定游标。
/// **不是 `arcad` 的替身**——`arca-cli`/`arca-agentd` 是 MIT、`arcad` 是
/// AGPL-3.0-only，即便只是 dev-dependency 也不能反向依赖（CLAUDE.md
/// 「许可证分层」）。对真实 `arcad` 的验证走端到端演示。
fn serve(cursor: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        tx.send(()).ok();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split(' ').nth(1).unwrap_or("").to_string();
            loop {
                let mut h = String::new();
                match reader.read_line(&mut h) {
                    Ok(0) => break,
                    Ok(_) if h == "\r\n" || h == "\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let body = if path.contains("/changes") {
                format!(r#"{{"events":[],"cursor":"{cursor}"}}"#)
            } else {
                "[]".to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    rx.recv().unwrap();
    format!("http://{addr}")
}

fn 建vault(base_url: &str) -> tempfile::TempDir {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("assets")).unwrap();
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
            base_url,
            "--dataset-id",
            DATASET_ID,
        ],
    );
    assert!(out.status.success(), "register 失败：{out:?}");
    vault
}

fn 游标文件(vault: &Path) -> std::path::PathBuf {
    vault.join("assets/.arca/client/changes-cursor")
}

/// `--once` 跑完之后游标必须落盘——否则演练/CI/cron 这些脚本化流程永远
/// 走不到持久化这条路径。
#[test]
fn once模式也落盘游标() {
    let cursor = format!("{}:7", "a".repeat(32));
    let base = serve(Box::leak(cursor.clone().into_boxed_str()));
    let vault = 建vault(&base);

    let out = agentd(vault.path(), &["--once"]);
    assert!(
        out.status.success(),
        "--once 应当成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let 落盘 = std::fs::read_to_string(游标文件(vault.path()))
        .expect("--once 之后游标必须已落盘（FORMAT.md §9.6）");
    assert_eq!(落盘.trim(), cursor, "落盘的游标应当是 hub 给出的那个");
}

/// 第二次跑时必须**复用**已落盘的游标，而不是当作第一次——判据是诊断输出里
/// 不再出现「无游标」。
#[test]
fn 第二次运行复用已落盘的游标() {
    let cursor = format!("{}:7", "b".repeat(32));
    let base = serve(Box::leak(cursor.into_boxed_str()));
    let vault = 建vault(&base);

    let first = agentd(vault.path(), &["--once"]);
    assert!(String::from_utf8_lossy(&first.stderr).contains("无游标"));

    let second = agentd(vault.path(), &["--once"]);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !stderr.contains("无游标"),
        "第二次运行不该再报「无游标」——游标没被复用：\n{stderr}"
    );
}

/// 游标文件损坏 → agentd **照常启动**并做一次全量对账，同时留下可诊断的
/// 一句话。绝不因为一个可再生的小文件坏了就拒绝自动同步（FORMAT.md §9.6）。
#[test]
fn 游标损坏时照常启动并留下诊断() {
    let cursor = format!("{}:7", "c".repeat(32));
    let base = serve(Box::leak(cursor.into_boxed_str()));
    let vault = 建vault(&base);

    assert!(agentd(vault.path(), &["--once"]).status.success());
    std::fs::write(游标文件(vault.path()), "这不是游标").unwrap();

    let out = agentd(vault.path(), &["--once"]);
    assert!(
        out.status.success(),
        "游标坏了不该让 agentd 起不来：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("读不懂"),
        "必须留下可诊断的一句（「第一次跑」和「上次写坏了」是两件事）：\n{stderr}"
    );
    // 并且自愈：这一轮之后游标又是好的了。
    let 修好 = std::fs::read_to_string(游标文件(vault.path())).unwrap();
    assert!(修好.contains(':'), "应当被重新写成合法游标：{修好:?}");
}
