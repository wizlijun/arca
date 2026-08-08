//! 噩梦路径（spec §6.3 第 9 条）：git 的日常操作不得误伤受管二进制。
//!
//! 三条都真的建仓库、真的跑对应的 git 命令，然后断言文件系统的实际状态——
//! 断言的是行为，不是文档承诺（与 `tests/ignore_block.rs` 同一纪律）。
//!
//! `git` 不可用时 `建仓库` 直接 panic（`.expect("需要可用的 git")`）：
//! 静默跳过等于没有测试，宁可让 CI 环境明确报错。

use std::path::{Path, PathBuf};
use std::process::Command;

fn 建仓库(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "t"],
    ] {
        let ok = Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .expect("需要可用的 git")
            .success();
        assert!(ok, "git {args:?} 失败");
    }
}

fn 跑(args: &[&str], dir: &Path) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("需要可用的 git")
        .success();
    assert!(ok, "git {args:?} 失败");
}

fn 当前分支(dir: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("需要可用的 git");
    assert!(
        output.status.success(),
        "git rev-parse --abbrev-ref HEAD 失败"
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// 布好一个带受管二进制的最小仓库：`.gitignore` 反选块与清单都被追踪并提交，
/// 另有一份"受管二进制"被忽略（模拟 arca 管理的大文件，原地不动 = I6）。
/// 返回受管二进制的绝对路径与其内容，供后续断言比对。
fn 布置受管仓库(dir: &Path) -> (PathBuf, Vec<u8>) {
    std::fs::create_dir_all(dir.join("assets/.arca/client")).unwrap();
    std::fs::write(
        dir.join(".gitignore"),
        arca_git::ignore_block::render(&["assets"]).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("assets/.arca/manifest"),
        "#%arca-manifest v1\n京都/鸭川.png\tblake3:9f2c\t2411008\t2026-08-04T10:22:31Z\n",
    )
    .unwrap();

    let content = b"pretend-this-is-a-large-managed-binary".to_vec();
    let bin_path = dir.join("assets/京都/鸭川.png");
    std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
    std::fs::write(&bin_path, &content).unwrap();

    // .gitignore 与 .arca/ 元数据（清单）被追踪提交；受管二进制本身因为被忽略，
    // 不会被 `git add -A` 捡起来——这正是我们要验证 git 的日常操作不会碰它的前提。
    跑(&["add", "-A"], dir);
    跑(&["commit", "-q", "-m", "init: 反选块 + 清单"], dir);

    let repo = arca_git::repo::Repo::open(dir).unwrap();
    assert!(
        repo.check_ignore("assets/京都/鸭川.png").unwrap(),
        "前提条件：受管二进制必须确实被 .gitignore 忽略"
    );

    (bin_path, content)
}

/// `git checkout` 切分支后，受管二进制仍在原地（未被删除、未被改名）。
///
/// 受管二进制从未被追踪（它被 .gitignore 忽略），git checkout 的对象只是被追踪
/// 的文件——常规情况下它压根不会碰未追踪/被忽略的文件。这条测试确认的正是
/// "压根不会碰"这个假设在真实 git 行为下成立，双向切换都要过。
#[test]
fn checkout_切分支后受管二进制仍在原地() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    建仓库(dir);
    let (bin_path, content) = 布置受管仓库(dir);
    let main_branch = 当前分支(dir);

    跑(&["checkout", "-q", "-b", "feature"], dir);
    // 在 feature 分支上追加一条清单记录，制造一次真实的、被追踪文件会变化的提交。
    std::fs::write(
        dir.join("assets/.arca/manifest"),
        "#%arca-manifest v1\n京都/街景.mp4\tblake3:c71a\t1884301776\t2026-08-04T10:23:02Z\n京都/鸭川.png\tblake3:9f2c\t2411008\t2026-08-04T10:22:31Z\n",
    )
    .unwrap();
    跑(&["add", "assets/.arca/manifest"], dir);
    跑(&["commit", "-q", "-m", "feature: 追加清单记录"], dir);

    assert!(bin_path.is_file(), "切到 feature 分支前受管二进制必须仍在");
    assert_eq!(std::fs::read(&bin_path).unwrap(), content);

    跑(&["checkout", "-q", &main_branch], dir);
    assert!(
        bin_path.is_file(),
        "checkout 回 {main_branch} 后受管二进制必须仍在原地"
    );
    assert_eq!(
        std::fs::read(&bin_path).unwrap(),
        content,
        "checkout 不得改动受管二进制的内容"
    );

    跑(&["checkout", "-q", "feature"], dir);
    assert!(
        bin_path.is_file(),
        "checkout 回 feature 后受管二进制必须仍在原地"
    );
    assert_eq!(std::fs::read(&bin_path).unwrap(), content);
}

/// `git clean -xdf` **不删除**受管二进制。
///
/// 已知风险（spec §13 风险表「git 操作误伤」）：`-x` 会清理**被忽略**的文件，
/// 而受管二进制正是被 `.gitignore` 反选块忽略的那一类文件——这条测试很可能
/// 真的会失败，而且失败是"真问题"，不是测试写严了。
///
/// 实测（2026-08-08，macOS 本机 git）：**确实失败**——`git clean -xdf` 把
/// `assets/京都/鸭川.png` 连同 `assets/京都/` 空目录一起删掉了，文件不可恢复
/// （git clean 不走回收站/tombstone，是真删）。用户后果：任何在 hub 尚未落地
/// 上传完成前跑过 `git clean -xdf`（常见于"清理构建产物"的肌肉记忆，或
/// CI/部署脚本里的 checkout 清场）的人，会**直接丢失本地唯一副本**——这正是
/// I3「同步路径无销毁权」想挡住的那类事故，但 `git clean` 走的是 git 自己的
/// 销毁路径，arca 拦不住。
///
/// 缓解措施不是想办法让受管文件"看起来不像被忽略"（那会破坏 §4.3 反选块本身
/// 的语义，且对 `git clean` 无效——它清理的判据就是"被忽略"，不是路径形态）。
/// 真正的缓解是 spec §13 已经列好的：`arca doctor` 检出「本地存在但 hub 尚无
/// 副本」的文件并显著告警，把"删了会丢数据"的窗口从"用户不知情"变成
/// "用户被明确提示过"。
///
/// 保留本测试（而非删除或弱化断言）作为该风险的可执行证据：一旦 git 未来的
/// 行为变化，或我们找到别的缓解策略，重新放开即可复核。
#[test]
#[ignore = "已知失败：git clean -xdf 确实会删除受管二进制，因为它被 .gitignore 忽略——\
            这是 spec §13 风险表里的已知风险，缓解措施是 `arca doctor` 检出未上传文件并\
            告警，而不是让这条测试变绿。见上方 doc comment 的完整实测记录。"]
fn git_clean_xdf_不删除受管二进制() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    建仓库(dir);
    let (bin_path, content) = 布置受管仓库(dir);

    跑(&["clean", "-xdf"], dir);

    assert!(
        bin_path.is_file(),
        "受管二进制不应被 git clean -xdf 删除，但它已被删除：{}",
        bin_path.display()
    );
    assert_eq!(std::fs::read(&bin_path).unwrap(), content);
}

/// `git stash` 不影响受管二进制（默认不带 `-u`/`-a` 时，stash 只处理已追踪文件的
/// 改动，未追踪/被忽略的文件原样留在工作树里）。stash 与 pop 都要验证一遍。
#[test]
fn stash_不影响受管二进制() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    建仓库(dir);
    let (bin_path, content) = 布置受管仓库(dir);

    // 制造一处已追踪文件的未提交改动，让 stash 有真实的东西可存。
    std::fs::write(
        dir.join("assets/.arca/manifest"),
        "#%arca-manifest v1\n京都/鸭川.png\tblake3:9f2c\t2411008\t2026-08-04T10:22:31Z\n京都/新增.jpg\tblake3:aa11\t100\t2026-08-05T00:00:00Z\n",
    )
    .unwrap();

    跑(&["stash", "-q"], dir);
    assert!(bin_path.is_file(), "git stash 后受管二进制必须仍在原地");
    assert_eq!(std::fs::read(&bin_path).unwrap(), content);
    // stash 应当已经把已追踪文件的改动收走。
    let manifest_after_stash = std::fs::read_to_string(dir.join("assets/.arca/manifest")).unwrap();
    assert!(
        !manifest_after_stash.contains("新增.jpg"),
        "stash 应当收走已追踪文件的未提交改动"
    );

    跑(&["stash", "pop", "-q"], dir);
    assert!(bin_path.is_file(), "git stash pop 后受管二进制必须仍在原地");
    assert_eq!(std::fs::read(&bin_path).unwrap(), content);
    let manifest_after_pop = std::fs::read_to_string(dir.join("assets/.arca/manifest")).unwrap();
    assert!(
        manifest_after_pop.contains("新增.jpg"),
        "stash pop 应当把改动还原回来"
    );
}
