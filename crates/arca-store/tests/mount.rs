//! I11 场景矩阵：未挂载的卷绝不能被当成空库。
//!
//! 这些不是形式测试——把「根不存在」当成「库是空的」，同步引擎会认为远端删光了文件，
//! 于是触发删除对账清掉用户本地数据。每一条都对应一种真实的挂载故障。

use arca_store::root::{MountError, StorageRoot};
use std::fs;
use std::path::Path;

const 样例_ID: &str = "9c41000000000000000000000000abcd";

fn 造存储根(root: &Path, dataset_id: &str) {
    fs::create_dir_all(root.join(".arca")).unwrap();
    fs::create_dir_all(root.join("files")).unwrap();
    fs::write(
        root.join(".arca/format.json"),
        format!(
            r#"{{"v":1,"format":1,"dataset_id":"{dataset_id}","hash_algo":"blake3","created_at":"2026-08-05T10:00:00Z"}}"#
        ),
    )
    .unwrap();
}

#[test]
fn 健康的存储根可以打开() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let root = StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    assert_eq!(root.dataset_id(), 样例_ID);
    assert_eq!(root.join("files").file_name().unwrap(), "files");
}

#[test]
fn 不指定期望身份时也能打开() {
    // fsck 这类只读巡检不一定知道期望的 dataset_id
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    assert!(StorageRoot::open(dir.path(), None).is_ok());
}

#[test]
fn 根目录整个不存在时报_absent_而不是空库() {
    let dir = tempfile::tempdir().unwrap();
    let 不存在 = dir.path().join("从未挂载");
    match StorageRoot::open(&不存在, Some(样例_ID)) {
        Err(MountError::Absent { .. }) => {}
        other => panic!("必须报 Absent，实得 {other:?}"),
    }
}

#[test]
fn 根存在但_format_json_缺失时报_absent() {
    // 这正是「挂载点下面有个本地建的空壳目录」的形态
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("files")).unwrap();
    match StorageRoot::open(dir.path(), Some(样例_ID)) {
        Err(MountError::Absent { .. }) => {}
        other => panic!("必须报 Absent，实得 {other:?}"),
    }
}

#[test]
fn 身份不符时报_identity_mismatch_并带上两侧的值() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), "1111111111111111111111111111aaaa");
    match StorageRoot::open(dir.path(), Some(样例_ID)) {
        Err(MountError::IdentityMismatch { expected, found }) => {
            assert_eq!(expected, 样例_ID);
            assert_eq!(found, "1111111111111111111111111111aaaa");
        }
        other => panic!("必须报 IdentityMismatch，实得 {other:?}"),
    }
}

#[test]
fn format_json_损坏时报_malformed_而不是_absent() {
    // 「读不出身份」与「没有身份」是不同的故障，不可混为一谈（I5）
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".arca")).unwrap();
    fs::write(dir.path().join(".arca/format.json"), "{ 这不是 JSON").unwrap();
    match StorageRoot::open(dir.path(), Some(样例_ID)) {
        Err(MountError::Malformed(_)) => {}
        other => panic!("必须报 Malformed，实得 {other:?}"),
    }
}

#[test]
fn 打开是只读的绝不创建任何东西() {
    // I3：本 crate 无销毁权，也不该在探测时留下副作用
    let dir = tempfile::tempdir().unwrap();
    let 空 = dir.path().join("空目录");
    fs::create_dir(&空).unwrap();
    let _ = StorageRoot::open(&空, Some(样例_ID));
    let 条目数 = fs::read_dir(&空).unwrap().count();
    assert_eq!(条目数, 0, "打开失败的探测不得创建任何文件或目录");
}
