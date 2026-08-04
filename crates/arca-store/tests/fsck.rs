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
        "0".repeat(32),
        hash.to_text(),
        content.len()
    );
    fs::write(
        root.join(".arca/items/3f/3f2a000000000000000000000000beef.jsonl"),
        format!("{item_line}\n"),
    )
    .unwrap();

    let key = arca_format::path_rules::index_key("note.txt");
    let shard = root.join(".arca/index").join(&key.to_hex()[..2]);
    fs::create_dir_all(&shard).unwrap();
    fs::write(
        shard.join(format!("{}.json", key.to_hex())),
        r#"{"v":1,"item_id":"3f2a000000000000000000000000beef","path":"note.txt"}"#,
    )
    .unwrap();

    hash
}

#[test]
fn 健康的存储根零问题() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    let report = check_root(dir.path());
    assert!(
        report.problems.is_empty(),
        "不应有问题，实得 {:?}",
        report.problems
    );
    assert_eq!(report.checked_files, 1);
}

#[test]
fn 检出内容被篡改() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    fs::write(dir.path().join("files/note.txt"), b"tampered!!").unwrap();

    let report = check_root(dir.path());
    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::HashMismatch { .. })),
        "应检出哈希不匹配，实得 {:?}",
        report.problems
    );
}

#[test]
fn 检出当前版本文件缺失() {
    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    fs::remove_file(dir.path().join("files/note.txt")).unwrap();

    let report = check_root(dir.path());
    assert!(report
        .problems
        .iter()
        .any(|p| matches!(p, Problem::MissingFile { .. })));
}

#[test]
fn 缺少_format_json_时报告而不是崩溃() {
    let dir = tempfile::tempdir().unwrap();
    let report = check_root(dir.path());
    assert!(report
        .problems
        .iter()
        .any(|p| matches!(p, Problem::MissingFormatJson)));
}

/// 权限错误与「文件不存在」是不同性质的故障，不可折叠成同一个诊断（I5）。
/// 用 chmod 0o000 模拟「读不了」而非「不存在」——但 chmod 对 root 无效、部分文件系统
/// 也不支持权限位，所以 chmod 之后先自证一次假设是否成立，不成立就跳过（打印说明，
/// 不静默跳过），不假设当前一定以非 root 身份运行。
#[test]
#[cfg(unix)]
fn 检出文件读取权限错误而不是缺失() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());
    let file = dir.path().join("files/note.txt");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();

    // chmod 对 root 无效，某些文件系统也不支持权限位；
    // 先验证假设成立，不成立就跳过而不是假装测过了。
    if fs::read(&file).is_ok() {
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!("跳过：当前用户不受 chmod 0o000 限制（root 或文件系统不支持权限位）");
        return;
    }

    let report = check_root(dir.path());
    // 恢复权限，否则 tempdir 在 Drop 时清理不掉这个文件。
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::IoError { .. })),
        "权限错误应报告为 IoError，不是 MissingFile，实得 {:?}",
        report.problems
    );
    assert!(
        !report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::MissingFile { .. })),
        "权限错误不应被误报成文件缺失，实得 {:?}",
        report.problems
    );
}

/// 同上，但覆盖 chunks/ 分支：读不到块（IO 错误）与读到了但内容不对
/// （`CorruptChunk`）必须分开报告。
#[test]
#[cfg(unix)]
fn 检出块文件权限错误而不是内容损坏() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    造一个健康的存储根(dir.path());

    let content = b"chunk content";
    let hash = ContentHash::from_bytes(content);
    let packed = arca_chunk::compress::compress(content).unwrap();
    let hex = hash.to_hex();
    let shard = dir.path().join(".arca/chunks").join(&hex[..2]);
    fs::create_dir_all(&shard).unwrap();
    let chunk_path = shard.join(format!("{hex}.zst"));
    fs::write(&chunk_path, packed).unwrap();
    fs::set_permissions(&chunk_path, fs::Permissions::from_mode(0o000)).unwrap();

    // chmod 对 root 无效，某些文件系统也不支持权限位；
    // 先验证假设成立，不成立就跳过而不是假装测过了。
    if fs::read(&chunk_path).is_ok() {
        fs::set_permissions(&chunk_path, fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!("跳过：当前用户不受 chmod 0o000 限制（root 或文件系统不支持权限位）");
        return;
    }

    let report = check_root(dir.path());
    fs::set_permissions(&chunk_path, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::IoError { .. })),
        "权限错误应报告为 IoError，不是 CorruptChunk，实得 {:?}",
        report.problems
    );
    assert!(
        !report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::CorruptChunk { .. })),
        "权限错误不应被误报成块内容损坏，实得 {:?}",
        report.problems
    );
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
