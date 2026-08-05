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

    let 最终 = fs::read_to_string(dir.path().join("files/race.txt")).unwrap();
    assert!(
        最终.starts_with("版本-") && 最终.len() == "版本-000".len(),
        "必须是某一次写入的完整内容，实得 {最终:?}"
    );
}
