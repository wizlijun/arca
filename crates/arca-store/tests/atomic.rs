//! 原子写入：崩溃后要么看到旧内容、要么看到新内容，绝不看到半截。

use arca_store::atomic;
use arca_store::root::StorageRoot;
use std::fs;
use std::path::Path;

const 样例_ID: &str = "9c41000000000000000000000000abcd";

fn 造存储根(root: &Path) {
    fs::create_dir_all(root.join(".arca/tmp")).unwrap();
    fs::create_dir_all(root.join("files")).unwrap();
    fs::write(
        root.join(".arca/format.json"),
        format!(
            r#"{{"v":1,"format":1,"dataset_id":"{样例_ID}","hash_algo":"blake3","created_at":"2026-08-05T10:00:00Z"}}"#
        ),
    )
    .unwrap();
}

#[test]
fn 写入新文件() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/note.txt", b"hello arca").unwrap();
    assert_eq!(
        fs::read(dir.path().join("files/note.txt")).unwrap(),
        b"hello arca"
    );
}

#[test]
fn 覆盖既有文件是原子替换() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/note.txt", b"old").unwrap();
    atomic::write(&root, "files/note.txt", b"new content").unwrap();
    assert_eq!(
        fs::read(dir.path().join("files/note.txt")).unwrap(),
        b"new content"
    );
}

#[test]
fn 写入后_tmp_目录不残留() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/note.txt", b"x").unwrap();
    let 残留 = fs::read_dir(dir.path().join(".arca/tmp")).unwrap().count();
    assert_eq!(残留, 0, "成功写入后不得在 tmp 留下临时文件");
}

#[test]
fn 自动创建目标的父目录() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/京都/鸭川.png", b"png bytes").unwrap();
    assert!(dir.path().join("files/京都/鸭川.png").exists());
}

#[test]
fn 多层新建目录写入后整条目录链都存在() {
    // create_dir_all 可能一次性新建好几层（评审 Important #2）：只 fsync
    // 最深一层不够，`files` 下指向 `一/二/三` 各层的目录项也必须落盘，
    // 否则崩溃后可能出现「write() 报告成功，但中间某层目录其实不存在，
    // 文件不可达」。这里测的是功能性完整（目录结构齐全），fsync 是否真
    // 落盘无法在单元测试里直接观测，但结构完整是它的前提。
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    let 内容 = "深层内容".as_bytes();
    atomic::write(&root, "files/一/二/三/四.txt", 内容).unwrap();

    assert!(dir.path().join("files/一").is_dir());
    assert!(dir.path().join("files/一/二").is_dir());
    assert!(dir.path().join("files/一/二/三").is_dir());
    assert_eq!(
        fs::read(dir.path().join("files/一/二/三/四.txt")).unwrap(),
        内容
    );
}

#[test]
fn 空内容也能写() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    atomic::write(&root, "files/empty", b"").unwrap();
    assert_eq!(
        fs::read(dir.path().join("files/empty")).unwrap(),
        Vec::<u8>::new()
    );
}

#[test]
fn 并发写同一路径最终得到其中一个完整版本() {
    // 不测「哪一个赢」——测的是绝不会出现半截内容
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());

    std::thread::scope(|s| {
        for i in 0..8 {
            let 根路径 = dir.path().to_path_buf();
            s.spawn(move || {
                let root = StorageRoot::open(&根路径, None).unwrap();
                let 内容 = format!("版本-{i:03}");
                atomic::write(&root, "files/race.txt", 内容.as_bytes()).unwrap();
            });
        }
    });

    let 候选集: std::collections::HashSet<String> =
        (0..8).map(|i| format!("版本-{i:03}")).collect();
    let 最终 = fs::read_to_string(dir.path().join("files/race.txt")).unwrap();
    assert!(
        候选集.contains(&最终),
        "必须恰好等于 8 个候选值之一，实得 {最终:?}"
    );
}

#[test]
fn 清理孤儿临时文件() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    fs::write(dir.path().join(".arca/tmp/orphan-1"), b"crash residue").unwrap();
    fs::write(dir.path().join(".arca/tmp/orphan-2"), b"more residue").unwrap();

    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 报告 = atomic::sweep_tmp(&root).unwrap();
    assert_eq!(报告.removed, 2);
    assert!(报告.refused.is_empty());
    assert_eq!(
        fs::read_dir(dir.path().join(".arca/tmp")).unwrap().count(),
        0
    );
}

#[test]
fn tmp_下出现目录时拒绝而不是递归删除() {
    // I5：不理解的状态要停下报告，不能变成「我删掉了不理解的东西」（I3）
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    fs::create_dir(dir.path().join(".arca/tmp/意外目录")).unwrap();
    fs::write(dir.path().join(".arca/tmp/意外目录/内含文件"), b"x").unwrap();

    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 报告 = atomic::sweep_tmp(&root).unwrap();
    assert_eq!(报告.removed, 0);
    assert_eq!(报告.refused.len(), 1, "应报告拒绝处理的条目");
    assert!(
        dir.path().join(".arca/tmp/意外目录/内含文件").exists(),
        "绝不递归删除"
    );
}

#[cfg(unix)]
#[test]
fn tmp_下出现符号链接时拒绝() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let 目标 = dir.path().join("files/重要文件");
    fs::write(&目标, "绝不能被顺着链接删掉".as_bytes()).unwrap();
    std::os::unix::fs::symlink(&目标, dir.path().join(".arca/tmp/link")).unwrap();

    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 报告 = atomic::sweep_tmp(&root).unwrap();
    assert_eq!(报告.removed, 0);
    assert_eq!(报告.refused.len(), 1);
    assert!(目标.exists(), "符号链接指向的文件必须完好");
}

#[cfg(unix)]
#[test]
fn tmp_本身是符号链接时拒绝清理而不是跟随链接删除别处的文件() {
    // I3 与 I5 的交叉：`.arca/tmp` 若被换成指向别的真实数据目录的符号
    // 链接（管理员用 ln -s 把 tmp 挪到别的卷、或同步工具带进来的链接），
    // `read_dir` 会跟随链接——条目级别再严格的 symlink_metadata 判断也
    // 救不回一个整体建在别处的目录。必须整体拒绝，绝不删除任何东西。
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    fs::remove_dir(dir.path().join(".arca/tmp")).unwrap();

    let 别处 = tempfile::tempdir().unwrap();
    fs::write(
        别处.path().join("重要数据.txt"),
        "绝不能被当成孤儿临时文件删掉".as_bytes(),
    )
    .unwrap();
    fs::write(别处.path().join("另一份数据.bin"), "也不能被删".as_bytes()).unwrap();
    std::os::unix::fs::symlink(别处.path(), dir.path().join(".arca/tmp")).unwrap();

    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 结果 = atomic::sweep_tmp(&root);
    assert!(
        结果.is_err(),
        "tmp 本身是符号链接时必须拒绝并停下，不能跟随链接清理，实得 {结果:?}"
    );
    assert!(
        别处.path().join("重要数据.txt").exists(),
        "链接目标目录里的文件必须完好"
    );
    assert!(
        别处.path().join("另一份数据.bin").exists(),
        "链接目标目录里的文件必须完好"
    );
}

#[test]
fn batch写入多个文件后commit全部落盘() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    let mut batch = atomic::Batch::new(&root);
    batch.write("files/a.txt", b"content a").unwrap();
    batch.write("files/sub/b.txt", b"content b").unwrap();
    batch.commit().unwrap();

    assert_eq!(
        fs::read(dir.path().join("files/a.txt")).unwrap(),
        b"content a"
    );
    assert_eq!(
        fs::read(dir.path().join("files/sub/b.txt")).unwrap(),
        b"content b"
    );
}

#[test]
fn batch写入后tmp目录不残留() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    let mut batch = atomic::Batch::new(&root);
    for i in 0..5 {
        batch
            .write(&format!("files/f{i}.txt"), format!("内容{i}").as_bytes())
            .unwrap();
    }
    batch.commit().unwrap();

    let 残留 = fs::read_dir(dir.path().join(".arca/tmp")).unwrap().count();
    assert_eq!(残留, 0, "批量写入成功后不得在 tmp 留下临时文件");
}

#[test]
fn batch对共享同一目录链的写入去重待确认目录数() {
    // 持久性论证的核心可观察断言：同一批次里若干次写入落在同一个目录（或
    // 共享同一段祖先链）时，`pending_dirs` 不应随写入次数线性增长——这正是
    // 批量 API 相对逐文件 `write` 省下来的那部分开销（去重，而不是省略）。
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    let mut batch = atomic::Batch::new(&root);
    batch.write("files/shared/a.txt", b"a").unwrap();
    let after_one = batch.pending_dirs();
    batch.write("files/shared/b.txt", b"b").unwrap();
    let after_two = batch.pending_dirs();
    assert_eq!(
        after_one, after_two,
        "写入同一目录下的第二个文件不应增加待确认的目录数"
    );

    batch.write("files/other/c.txt", b"c").unwrap();
    let after_three = batch.pending_dirs();
    assert!(
        after_three > after_two,
        "写入一个全新的目录链必须让待确认目录数增加"
    );

    batch.commit().unwrap();
}

#[test]
fn batch未调用commit时写入内容依然完整可读() {
    // 调用方因为 `?` 提前返回而整体丢弃 `Batch`（未调用 `commit`）时，批次内
    // 已经 rename 成功的写入不应该被当成"没发生过"——内容级别的持久化（tmp
    // → fsync 文件 → rename）在 `write` 内已经逐次完成，`commit` 只负责补齐
    // 目录项落盘的确认，不负责决定内容是否存在（I3：不能因为没提交就丢数据）。
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    {
        let mut batch = atomic::Batch::new(&root);
        batch
            .write("files/never-committed.txt", b"still here")
            .unwrap();
        // 故意不调用 commit，模拟调用方中途失败提前返回。
    }

    assert_eq!(
        fs::read(dir.path().join("files/never-committed.txt")).unwrap(),
        b"still here"
    );
}

#[test]
fn batch内重复写入同一路径最终得到最新版本() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path());
    let root = StorageRoot::open(dir.path(), None).unwrap();

    let mut batch = atomic::Batch::new(&root);
    batch.write("files/note.txt", b"old").unwrap();
    batch.write("files/note.txt", b"new content").unwrap();
    batch.commit().unwrap();

    assert_eq!(
        fs::read(dir.path().join("files/note.txt")).unwrap(),
        b"new content"
    );
}

#[test]
fn tmp_目录不存在时清理是无操作() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".arca")).unwrap();
    fs::write(
        dir.path().join(".arca/format.json"),
        format!(
            r#"{{"v":1,"format":1,"dataset_id":"{样例_ID}","hash_algo":"blake3","created_at":"2026-08-05T10:00:00Z"}}"#
        ),
    )
    .unwrap();
    let root = StorageRoot::open(dir.path(), None).unwrap();
    let 报告 = atomic::sweep_tmp(&root).unwrap();
    assert_eq!(报告.removed, 0);
}
