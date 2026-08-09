//! `arca status` 报告 agentd 的情况（M3b Task 4，FORMAT.md §9.7）。
//!
//! 三条纪律，各自一个测试：
//!
//! 1. **没有心跳 → 一个字都不说。** 手动模式是基线（spec §3.1），agentd
//!    没在跑是完全正常的状态；措辞更不能暗示「你应该起一个 agentd」。
//! 2. **心跳陈旧 → 说「可能已不在运行」。** `kill -9` 时来不及删心跳文件，
//!    所以文件存在 ≠ 进程存在。拿着一个三天前的心跳报告「自动同步正常」，
//!    比什么都不报告更糟——后者让人去查，前者让人放心。
//! 3. **绝不影响退出码。** 这是一句旁注，不是判断。`arca status` 的退出码
//!    回答的是「你的库同步好了吗」，与「有没有 daemon 在帮你」无关——
//!    M2d 的评审在副本数告警上抓过同构的问题（它当时让干净的数据集退出 1）。
//!
//! 心跳由测试**直接构造**而不是真起一个 agentd：本文件验的是读取侧的措辞
//! 与退出码，写入侧由 `arca-agentd` 自己的测试覆盖。两边只靠 FORMAT.md §9.7
//! 这份格式文档对齐（`arca-cli` 不能依赖 `arca-agentd`，那会成环）。

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

/// 一个已 adopt 完成、完全同步干净的数据集——这样 `status` 的输出里只会
/// 有 agentd 那一节的内容，不被别的诊断噪音混淆。
fn 建干净vault() -> (tempfile::TempDir, tempfile::TempDir) {
    let vault = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("assets")).unwrap();
    std::fs::write(vault.path().join("assets/a.bin"), b"content").unwrap();
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
    (vault, store)
}

fn 写心跳(vault: &Path, beat_at: &str, watching: bool, last_error: Option<&str>) {
    let err = match last_error {
        Some(e) => format!("\"{e}\""),
        None => "null".to_string(),
    };
    let body = format!(
        r#"{{"schema":1,"pid":4242,"started_at":"2026-08-09T00:00:00Z","beat_at":"{beat_at}",
           "datasets":[{{"path":"assets","hub":"home","watching":{watching},
           "last_ok_at":"2026-08-09T00:00:10Z","last_error":{err}}}]}}"#
    );
    std::fs::create_dir_all(vault.join(".arca")).unwrap();
    std::fs::write(vault.join(".arca/agentd-status.json"), body).unwrap();
}

fn 现在() -> String {
    // 与 arca 自己用的是同一个时钟实现，避免时区/格式漂移。
    arca_cli::clock::now_rfc3339()
}

#[test]
fn 没有心跳时一个字都不提agentd() {
    let (vault, _s) = 建干净vault();
    let out = arca(vault.path(), &["status", "assets"]);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !all.contains("agentd"),
        "agentd 没在跑是完全正常的状态，不该提它（更不该暗示应该起一个）：\n{all}"
    );
}

#[test]
fn 心跳新鲜时报告运行中并说明监听方式() {
    let (vault, _s) = 建干净vault();
    写心跳(vault.path(), &现在(), true, None);

    let out = arca(vault.path(), &["status", "assets"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("agentd：运行中"), "{stderr}");
    assert!(stderr.contains("4242"), "应报出 pid 供人 kill/ps：{stderr}");
    assert!(stderr.contains("实时监听"), "{stderr}");
    assert!(
        out.status.success(),
        "agentd 那一节绝不影响退出码：{stderr}"
    );
}

/// `watching=false` 是「本地改动为什么要等一会儿才同步」的答案——必须说出来。
#[test]
fn 未启用监听时说明退回了纯周期() {
    let (vault, _s) = 建干净vault();
    写心跳(vault.path(), &现在(), false, None);
    let out = arca(vault.path(), &["status", "assets"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("纯周期"), "{stderr}");
}

/// **本文件里最重要的一条。** `kill -9` 之后心跳文件还在，但进程早就没了。
#[test]
fn 心跳陈旧时说可能已不在运行而不是报告正常() {
    let (vault, _s) = 建干净vault();
    写心跳(vault.path(), "2020-01-01T00:00:00Z", true, None);

    let out = arca(vault.path(), &["status", "assets"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("可能已不在运行"),
        "拿着一个 2020 年的心跳绝不能报告「运行中」：\n{stderr}"
    );
    assert!(
        !stderr.contains("agentd：运行中"),
        "不该同时给出两种相反的说法：\n{stderr}"
    );
    assert!(
        out.status.success(),
        "陈旧心跳只是一句旁注，不该影响退出码：{stderr}"
    );
}

/// 更高的 `schema` **拒绝解读**，不拿旧结构硬解（I10「只向前迁移」＋
/// I5「绝不猜测」）——硬解的后果是把一份看不懂的状态说成看懂了。
#[test]
fn 更高的schema被拒绝解读而不是硬解() {
    let (vault, _s) = 建干净vault();
    std::fs::create_dir_all(vault.path().join(".arca")).unwrap();
    std::fs::write(
        vault.path().join(".arca/agentd-status.json"),
        r#"{"schema":99,"pid":1,"beat_at":"2026-08-09T00:00:00Z","datasets":[]}"#,
    )
    .unwrap();

    let out = arca(vault.path(), &["status", "assets"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("schema=99"), "{stderr}");
    assert!(!stderr.contains("agentd：运行中"), "{stderr}");
    assert!(out.status.success(), "{stderr}");
}

/// 心跳文件损坏 → 说一句读不懂，**不 panic、不影响退出码**。
#[test]
fn 心跳损坏时可诊断且不影响退出码() {
    let (vault, _s) = 建干净vault();
    std::fs::create_dir_all(vault.path().join(".arca")).unwrap();
    std::fs::write(vault.path().join(".arca/agentd-status.json"), "{ 半份 JSON").unwrap();

    let out = arca(vault.path(), &["status", "assets"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("读不懂"), "{stderr}");
    assert!(out.status.success(), "{stderr}");
}
