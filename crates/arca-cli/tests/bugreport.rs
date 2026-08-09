//! `arca bugreport`（M2e Task 5，spec §3.3）：一条命令收齐诊断现场。
//!
//! 两类断言，第二类比第一类重要得多：
//!
//! 1. **收全了**——版本、平台、`dataset_id`、角色、hub 可达性、
//!    `.gitignore` 反选块的**实测**结果、本地回收站占用与清单、doctor 结论。
//!    少一样，报障的人就要多被追问一轮。
//! 2. **没多收**——受管文件的内容、本地回收站里 `.data` 的内容，
//!    一个字节都不许出现在报告里。用户会把这份东西整段贴进公开 issue，
//!    **一旦泄漏就是不可撤回的**。所以这里用的判据不是"看起来没有"，
//!    而是往文件里塞一串独一无二的魔法串，然后断言整份报告里搜不到它。
//!
//! 与 `tests/replica_warning.rs`、`tests/multi_hub.rs` 同一条理由：
//! `bugreport_cmd()` 依赖进程级 `cwd()`，所以用真实编译好的 `arca` 二进制
//! 跑，各自独立的工作目录，不共享任何全局状态。

use arca_format::model::ItemId;
use std::path::Path;
use std::process::Command;

/// 受管文件的内容。这串东西**必须不出现在报告里**——它足够独特，
/// 不可能被其它字段偶然产生。
const 文件内容魔法串: &str = "MAGIC-FILE-BODY-9f3a2b1c-must-never-leak";

/// 移进本地回收站那份副本的内容，同上。
const 回收站内容魔法串: &str = "MAGIC-TRASHED-BODY-7e5d4c3b-must-never-leak";

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

/// 建一个已 adopt 完成的数据集，其中受管文件的内容是 [`文件内容魔法串`]，
/// 并让它带上一条本地回收站记录（内容是 [`回收站内容魔法串`]）与
/// `server` 角色——把 bugreport 该报告的东西一次性都造齐。
fn 造有回收站记录的数据集(vault_dir: &Path, store: &Path) {
    std::fs::create_dir_all(vault_dir.join("assets")).unwrap();
    std::fs::write(vault_dir.join("assets/note.txt"), 文件内容魔法串).unwrap();
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

    // 声明 server 角色，并造一条本地回收站记录。这里直接用库函数造，
    // 而不是跑一整轮 tombstone 传播——本文件要验的是 bugreport 怎么**报告**
    // 回收站，回收站怎么被**填满**已经由 `tests/local_trash_cycle.rs` 走
    // 真实数据流验过了。
    let dataset = vault_dir.join("assets");
    let out = run(vault_dir, &["role", "assets", "--set", "server"]);
    assert!(out.status.success(), "role --set server 失败：{out:?}");

    let 待删 = dataset.join("gone.bin");
    std::fs::write(&待删, 回收站内容魔法串).unwrap();
    let id = arca_cli::local_trash::move_to_trash(
        &dataset,
        &待删,
        "gone.bin",
        ItemId::from_bytes([0xab; 16]),
        "2026-08-09T10:00:00Z",
    )
    .unwrap();
    assert!(id.is_some(), "本地回收站记录应已建立");
}

#[test]
fn bugreport_收齐诊断现场的关键字段() {
    let vault = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    造有回收站记录的数据集(vault.path(), store.path());

    let out = run(vault.path(), &["bugreport"]);
    assert!(
        out.status.success(),
        "bugreport 自身应成功（退出码回答的是「这条命令跑成功了吗」，\
         不是「你的库健康吗」）：{out:?}"
    );
    let 报告 = String::from_utf8_lossy(&out.stdout).to_string();

    for 关键字段 in [
        "arca 版本：",
        "平台：",
        "## hub 端点",
        "## 数据集",
        "dataset_id：",
        // M2d 评审专门点名的一条：角色是"解释为什么这台设备行为和那台
        // 不同"的设备本地状态，在此之前所有诊断命令都看不见它。
        "角色（role.toml）：server",
        "hub 可达性：",
        // CLAUDE.md「已知的高危处」：断言的必须是 `git check-ignore` 的
        // 实际结果，不是文本。
        ".gitignore 反选块（实测）：",
        ".arca/manifest（实测）：",
        "本地回收站：",
        "## arca doctor",
        "## 隐私边界",
    ] {
        assert!(
            报告.contains(关键字段),
            "报告缺少关键字段 {关键字段:?}——报障的人会因此多被追问一轮。\n完整报告：\n{报告}"
        );
    }

    // 回收站清单要能看见**原路径**（"哪个文件被挪走了"正是报障时要回答的），
    // 但看得见路径不等于看得见内容——下一个测试守那条线。
    assert!(
        报告.contains("gone.bin"),
        "本地回收站清单应列出原路径：\n{报告}"
    );
}

/// 隐私边界：**报告里一个字节的文件内容都不许有**。
///
/// 这是 bugreport 最容易出事的地方——它天生想"多收一点好方便排查"，
/// 而用户会把整份输出贴进公开 issue。所以判据是可执行的：魔法串搜不到。
#[test]
fn bugreport_绝不含任何受管文件或回收站副本的内容() {
    let vault = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    造有回收站记录的数据集(vault.path(), store.path());

    let out = run(vault.path(), &["bugreport"]);
    let 全部输出 = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // 先自证这两串确实在磁盘上——否则本测试会因为"根本没造出来"而假绿，
    // 那是最坏的一种通过（M0 的逃生舱脚本被抓过三次同构的问题）。
    assert!(
        std::fs::read_to_string(vault.path().join("assets/note.txt"))
            .unwrap()
            .contains(文件内容魔法串),
        "夹具失效：受管文件里没有魔法串，本测试将无法证明任何事"
    );
    let 回收站目录 = vault.path().join("assets/.arca/client/trash");
    let 回收站里有魔法串 = std::fs::read_dir(&回收站目录)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            std::fs::read(e.path())
                .map(|b| String::from_utf8_lossy(&b).contains(回收站内容魔法串))
                .unwrap_or(false)
        });
    assert!(
        回收站里有魔法串,
        "夹具失效：本地回收站里没有魔法串，本测试将无法证明任何事"
    );

    // 真正的判据。
    assert!(
        !全部输出.contains(文件内容魔法串),
        "**受管文件的内容泄漏进了 bugreport**。用户会把这份输出贴进公开 issue，\
         泄漏不可撤回。\n完整输出：\n{全部输出}"
    );
    assert!(
        !全部输出.contains(回收站内容魔法串),
        "**本地回收站副本的内容泄漏进了 bugreport**。\n完整输出：\n{全部输出}"
    );
}

/// 回归：trace 那一节必须列 `<state>/trace/` 下的**会话文件**，不是 `<state>`
/// 本身。第一版列错了一层，报告里只有一行「`trace` 目录、1664 字节」——
/// 几十个真正的会话文件一个都看不见，而这一节的全部价值就是让人知道
/// **有哪些会话可以附上**。实机跑一份报告才看出来，光跑测试看不出来。
///
/// 用 `HOME`/`XDG_STATE_HOME` 把落点指到临时目录，两个候选位置都造一份，
/// 哪个平台命中哪个（Windows 走 `%LOCALAPPDATA%`，本测试不覆盖）。
#[cfg(unix)]
#[test]
fn trace那一节列的是会话文件而不是trace目录本身() {
    let home = tempfile::tempdir().unwrap();
    let 会话文件名 = "20260809T104233Z-abcdef.jsonl";
    for state in [
        home.path().join("arca"),              // Linux：XDG_STATE_HOME=<tmp>
        home.path().join("Library/Logs/arca"), // macOS：HOME=<tmp>
    ] {
        let dir = state.join("trace");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(会话文件名), b"{}\n").unwrap();
    }

    let 空目录 = tempfile::tempdir().unwrap();
    let out = Command::new(arca_bin())
        .arg("bugreport")
        .current_dir(空目录.path())
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path())
        .output()
        .expect("arca 二进制应能正常启动");
    let 报告 = String::from_utf8_lossy(&out.stdout);

    assert!(
        报告.contains(会话文件名),
        "trace 那一节应列出会话文件本身。\n完整报告：\n{报告}"
    );
    assert!(
        报告.contains("/trace"),
        "列出的目录应是 <state>/trace/，不是 <state>：\n{报告}"
    );
}

/// 不在 vault 里也要能用——报障的人未必站在正确的目录下，而"本机 + 版本 +
/// trace 落盘"这部分信息与 vault 无关，仍然值得收。此时**不是错误**：
/// 强行退出非 0 会让人以为 bugreport 坏了，转而去手工拼信息。
#[test]
fn 不在vault里时仍然报告本机信息并成功退出() {
    let 空目录 = tempfile::tempdir().unwrap();
    let out = run(空目录.path(), &["bugreport"]);
    assert!(out.status.success(), "不在 vault 内不该是错误：{out:?}");

    let 报告 = String::from_utf8_lossy(&out.stdout);
    assert!(报告.contains("arca 版本："), "{报告}");
    assert!(报告.contains("## 最近的 trace 落盘"), "{报告}");
    assert!(
        报告.contains("## vault"),
        "应当明确说明 vault 打不开，而不是静默省略：{报告}"
    );
}
