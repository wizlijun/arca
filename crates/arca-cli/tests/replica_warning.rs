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

/// 评审 I1 的核心复现测试：副本数告警是一条独立于"这次调和跑得干不干净"
/// 的策略建议（PROTOCOL.md §3.2 的三态语义里没有它的位置），绝不能让一个
/// 刚 `adopt`、完全同步的默认角色（`client`）数据集因为这条告警退出非零——
/// 那会破坏「`arca status` 退出 0 = 都同步好了」这个基本承诺，还会通过
/// `status_all`/`sync_all` 取最大值的规则把**一个**默认角色数据集的告警
/// 传染成整个 vault 退出非零，反过来变相强迫用户为了消掉退出码去声明
/// `server`（一个"永不主动释放空间"的强承诺）。
#[test]
fn client角色下已知副本数低于阈值时status打印告警但退出码仍为0() {
    let vault_dir = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    建已同步干净的数据集(vault_dir.path(), store.path());

    // 未显式设置角色——默认 client，已知副本数只有 hub 自己这 1 份。
    let out = run(vault_dir.path(), &["status", "assets"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "数据集本身已完全同步——副本数告警不该让 status 退出非零：{out:?}"
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

/// 评审 I2 的核心复现测试：`known_server_copies` 只读
/// `<dataset>/.arca/client/role.toml`，从不打开 `StorageRoot`——不需要真的
/// 连上 hub（这里甚至没有起 `arcad`，`http://` 地址纯粹语法层面合法即可）
/// 就能验证副本告警不再被"这条命令只支持 file://"的 gate 挡在前面。此前
/// `report_replica_warning_if_any` 排在 `local_root()` 之后，`http://` 绑定
/// 的数据集——M2b/M2c 确立的主线配置——永远看不到这条告警。
#[test]
fn http绑定的数据集也能看到副本告警() {
    let vault_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault_dir.path().join("assets")).unwrap();
    std::fs::write(vault_dir.path().join("assets/note.txt"), b"content").unwrap();
    git(vault_dir.path(), &["init", "-q"]);
    git(vault_dir.path(), &["config", "user.email", "t@example.com"]);
    git(vault_dir.path(), &["config", "user.name", "t"]);

    let out = run(vault_dir.path(), &["init", "."]);
    assert!(out.status.success(), "arca init 失败：{out:?}");
    // 故意不起 arcad——`local_root()` 对 `http://` 恒失败，与是否连得上无关，
    // 这正是本测试要验证的：副本告警必须排在这道 gate 之前才能被看见。
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

    let out = run(vault_dir.path(), &["status", "assets"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("已知的 server 副本数"),
        "http:// 绑定的数据集也应该看到副本告警，而不是被 local_root() 的 gate 挡住：{stderr}"
    );
    // `status` 本身仍然如实报告"这条命令不支持 http://"（M2c Task 5 遗留
    // 范围），I2 只要求告警不再被这道 gate 挡住，不改变这道 gate 本身。
    assert_ne!(out.status.code(), Some(0));
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
