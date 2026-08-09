//! server 副本数告警（M2d Task 4，spec §4.5）：`arca status` 在已知副本数
//! 低于阈值（默认 2）时告警，且措辞必须诚实——"已知的 server 副本数"，不是
//! 全局真相（本切片没有 hub 侧登记，无法得知其它设备的角色）。
//!
//! 与 `tests/multi_hub.rs` 同一条理由：`status_cmd`/`role_cmd` 依赖进程级
//! `cwd()`，用真实编译好的 `arca` 二进制 + 各自独立的工作目录跑，不共享
//! 任何全局状态。

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

/// 建一个已 adopt 完成、hub 侧完全干净（无待办）的数据集——这样后续的
/// `arca status` 输出只可能来自副本数告警本身，不会被其它诊断噪音混淆。
fn 建已同步干净的数据集(vault_dir: &Path, store: &Path) {
    std::fs::create_dir_all(vault_dir.join("assets")).unwrap();
    std::fs::write(vault_dir.join("assets/note.txt"), b"content").unwrap();
    git(vault_dir, &["init", "-q"]);
    git(vault_dir, &["config", "user.email", "t@example.com"]);
    git(vault_dir, &["config", "user.name", "t"]);

    let out = run(vault_dir, &["init", "."]);
    assert!(out.status.success(), "arca init 失败：{out:?}");
    let out = run(
        vault_dir,
        &[
            "register",
            "assets",
            "--hub",
            "home",
            "--hub-url",
            &format!("file://{}", store.display()),
        ],
    );
    assert!(out.status.success(), "register 失败：{out:?}");
    let out = run(vault_dir, &["adopt", "assets"]);
    assert!(out.status.success(), "adopt 失败：{out:?}");
}

#[test]
fn client角色下已知副本数低于阈值时status告警且退出码非零() {
    let vault_dir = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    建已同步干净的数据集(vault_dir.path(), store.path());

    // 未显式设置角色——默认 client，已知副本数只有 hub 自己这 1 份。
    let out = run(vault_dir.path(), &["status", "assets"]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "已知副本数低于阈值时 status 不该安静退出 0：{out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("已知的 server 副本数"),
        "告警文案必须出现「已知的」这个限定词，不能宣称是全局真相：{stderr}"
    );
    assert!(
        stderr.contains("并非全局真相") || stderr.contains("下限"),
        "告警文案必须明确说明这不是全局真相，只是本设备已知的下限：{stderr}"
    );
}

#[test]
fn server角色下已知副本数达到阈值时status不告警() {
    let vault_dir = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    建已同步干净的数据集(vault_dir.path(), store.path());

    let out = run(vault_dir.path(), &["role", "assets", "--set", "server"]);
    assert!(out.status.success(), "设置 server 角色失败：{out:?}");

    // hub 自己 1 份 + 本设备 server 角色 1 份 = 2，达到默认阈值，数据集本身
    // 又完全同步——应该完全安静、退出码 0（Rule of Silence）。
    let out = run(vault_dir.path(), &["status", "assets"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "已知副本数达到阈值且数据集干净时，status 应安静退出 0：{out:?}"
    );
    assert!(
        out.stderr.is_empty(),
        "达到阈值时不应打印任何警告：{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}
