//! fsck 巡检的集成测试。构造真实的存储根目录，注入损坏，断言可诊断。

use arca_chunk::hash::ContentHash;
use arca_store::fsck::{check_root, Problem};
use std::fs;
use std::path::Path;

/// 造一个最小但合法的存储根：一个文件、一条版本记录、一条索引记录。
fn 造一个健康的存储根(root: &Path) -> ContentHash {
    let content = b"hello arca";
    let hash = ContentHash::from_bytes(content);

    fs::create_dir_all(root.join("files")).unwrap();
    fs::write(root.join("files/note.txt"), content).unwrap();

    fs::create_dir_all(root.join(".arca/items/3f")).unwrap();
    fs::create_dir_all(root.join(".arca/index")).unwrap();
    fs::write(
        root.join(".arca/format.json"),
        r#"{"v":1,"format":1,"dataset_id":"9c41000000000000000000000000abcd","hash_algo":"blake3","created_at":"2026-08-04T10:00:00Z"}"#,
    ).unwrap();

    let item_line = format!(
        r#"{{"v":1,"version_id":"20260804T102302Z-{}","item_id":"3f2a000000000000000000000000beef","parent":null,"hash":"{}","size":{},"mtime":"2026-08-04T10:00:00Z","actor":{{"account":"","device":"","session":""}},"committed_at":"2026-08-04T10:00:00Z"}}"#,
        "0".repeat(32), hash.to_text(), content.len()
    );
    fs::write(root.join(".arca/items/3f/3f2a000000000000000000000000beef.jsonl"), format!("{item_line}\n")).unwrap();

    let key = arca_format::path_rules::index_key("note.txt");
    let shard = root.join(".arca/index").join(&key.to_hex()[..2]);
    fs::create_dir_all(&shard).unwrap();
    fs::write(
        shard.join(format!("{}.json", key.to_hex())),
        r#"{"v":1,"item_id":"3f2a000000000000000000000000beef","path":"note.txt"}"#,
    ).unwrap();

    hash
}

#[test]
fn 健康的存储根零问题() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    let report = check_root(dir.path());
    assert!(report.problems.is_empty(), "不应有问题，实得 {:?}", report.problems);
    assert_eq!(report.checked_files, 1);
}

#[test]
fn 检出内容被篡改() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    fs::write(dir.path().join("files/note.txt"), b"tampered!!").unwrap();

    let report = check_root(dir.path());
    assert!(
        report.problems.iter().any(|p| matches!(p, Problem::HashMismatch { .. })),
        "应检出哈希不匹配，实得 {:?}", report.problems
    );
}

#[test]
fn 检出当前版本文件缺失() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    fs::remove_file(dir.path().join("files/note.txt")).unwrap();

    let report = check_root(dir.path());
    assert!(report.problems.iter().any(|p| matches!(p, Problem::MissingFile { .. })));
}

#[test]
fn 缺少_format_json_时报告而不是崩溃() {
    let dir = tempfile::tempdir().unwrap();
    let report = check_root(dir.path());
    assert!(report.problems.iter().any(|p| matches!(p, Problem::MissingFormatJson)));
}

#[test]
fn fsck_绝不修改任何文件() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    fs::write(dir.path().join("files/note.txt"), b"tampered!!").unwrap();

    let 前 = fs::read(dir.path().join("files/note.txt")).unwrap();
    let _ = check_root(dir.path());
    let 后 = fs::read(dir.path().join("files/note.txt")).unwrap();
    assert_eq!(前, 后, "fsck 是只读诊断，绝无销毁权（I3）");
}
