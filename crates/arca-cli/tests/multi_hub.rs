//! 多 hub 独立故障域（客户端侧，M2d Task 3，spec §4.3.2）：一个 vault 里两个
//! 数据集分属两个不同的 hub，断开其一，断言**另一个照常完成同步**——不是
//! 看退出码猜测，而是直接检查对方存储根 `files/` 下真的多了新内容。
//!
//! `tests/e2e.rs` 明确写着"这里只测 sync 本身，不测命令壳"——本文件反过来，
//! 专测命令壳（`arca sync`/`arca status` 不带路径时的多数据集分流），因为
//! `commands::porcelain` 里的 `sync_cmd`/`status_cmd` 系列函数依赖进程级的
//! `std::env::current_dir()`（`cwd()`），不适合在同一个测试二进制里跟其它
//! 测试共享、并发修改这个全局状态（`cargo test` 默认多线程跑各个测试函数）。
//! 每个测试用例改为**真正 spawn 编译好的 `arca` 二进制**，用
//! `Command::current_dir` 显式指定各自独立的工作目录——与其它测试之间没有
//! 任何全局状态可以互相干扰，这也顺带是一次"真实二进制"的端到端验证
//! （而不是直接调库函数）。

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

fn 建vault(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "t"]);
    let out = run(dir, &["init", "."]);
    assert!(out.status.success(), "arca init 失败：{out:?}");
}

/// 建两个数据集 `a`/`b`，各自绑定一个独立的本地存储根（各自的 `file://`
/// hub），都 adopt 一份初始内容，确认两边此刻都健康。
fn 建两个数据集各自独立的hub(vault_dir: &Path, store_a: &Path, store_b: &Path) {
    std::fs::create_dir_all(vault_dir.join("a")).unwrap();
    std::fs::create_dir_all(vault_dir.join("b")).unwrap();
    std::fs::write(vault_dir.join("a/one.txt"), b"a-content").unwrap();
    std::fs::write(vault_dir.join("b/one.txt"), b"b-content").unwrap();

    let out = run(
        vault_dir,
        &[
            "register",
            "a",
            "--hub",
            "hub_a",
            "--hub-url",
            &format!("file://{}", store_a.display()),
        ],
    );
    assert!(out.status.success(), "register a 失败：{out:?}");

    let out = run(
        vault_dir,
        &[
            "register",
            "b",
            "--hub",
            "hub_b",
            "--hub-url",
            &format!("file://{}", store_b.display()),
        ],
    );
    assert!(out.status.success(), "register b 失败：{out:?}");

    let out = run(vault_dir, &["adopt", "a"]);
    assert!(out.status.success(), "adopt a 失败：{out:?}");
    let out = run(vault_dir, &["adopt", "b"]);
    assert!(out.status.success(), "adopt b 失败：{out:?}");
}

/// 核心验收：断开 hub_a（把它的存储根整个移走，模拟"拔盘"/hub 下线）之后，
/// `arca sync`（不带路径，遍历 vault 全部数据集）**不能因为数据集 a 离线就
/// 跳过数据集 b**——b 的新增文件必须真的传到它自己的存储根里，不是只看
/// 退出码猜测传过去了。
#[test]
fn 一个hub不可达时另一个数据集的同步照常完成_断言文件真的传过去了() {
    let vault_dir = tempfile::tempdir().unwrap();
    let store_a = tempfile::tempdir().unwrap();
    let store_b = tempfile::tempdir().unwrap();
    let store_a_path = store_a.path().join("root");
    let store_b_path = store_b.path().join("root");

    建vault(vault_dir.path());
    建两个数据集各自独立的hub(vault_dir.path(), &store_a_path, &store_b_path);

    // 断开 hub_a：把它的存储根整个移走（同一台机器上"拔盘"的最简单模拟，
    // 与 Task 5 拔盘演练同一手法）——之后任何打开它的尝试都会失败。
    let moved_away = store_a.path().join("root-unplugged");
    std::fs::rename(&store_a_path, &moved_away).unwrap();

    // 数据集 b 新增一个文件，等待被这次全量 sync 上传。
    std::fs::write(vault_dir.path().join("b/two.txt"), b"b-second-file").unwrap();

    let out = run(vault_dir.path(), &["sync"]);
    let code = out.status.code().unwrap_or(-1);
    assert_ne!(
        code, 0,
        "hub_a 离线时整体退出码不该是 0（部分失败必须被反映出来）：{out:?}"
    );

    // --- 核心断言：不是看退出码，是直接看 hub_b 的存储根 files/ 下 ---
    let uploaded = store_b_path.join("files/two.txt");
    assert!(
        uploaded.exists(),
        "数据集 b 的新文件必须真的传到它自己的存储根——hub_a 离线不该拖累 b：\
         {uploaded:?} 不存在。sync 输出：{out:?}"
    );
    assert_eq!(std::fs::read(&uploaded).unwrap(), b"b-second-file");

    // hub_a 侧当然不会有这条记录（它离线，没人能写进去）——顺带确认它没有
    // 被跳过导致的"看起来无害的空转"误判为成功。
    assert!(!moved_away.join("files/two.txt").exists());

    // --- arca status（不带路径）同一条纪律：a 离线，b 照常报告 ---
    let out = run(vault_dir.path(), &["status"]);
    let code = out.status.code().unwrap_or(-1);
    assert_ne!(code, 0, "hub_a 离线时 status 整体退出码不该是 0：{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains('a') && (stderr.contains("离线") || stderr.contains("hub_a")),
        "status 输出应指出数据集 a 离线、且说明是哪个 hub：{stderr}"
    );

    // --- 把盘挂回来：hub_a 恢复可达，重新 sync 应能正常完成 ---
    std::fs::rename(&moved_away, &store_a_path).unwrap();
    let out = run(vault_dir.path(), &["sync"]);
    assert!(
        out.status.success(),
        "hub_a 恢复可达后，全量 sync 应该重新变干净：{out:?}"
    );
}
