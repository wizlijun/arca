//! I11 场景矩阵：未挂载的卷绝不能被当成空库。
//!
//! 这些不是形式测试——把「根不存在」当成「库是空的」，同步引擎会认为远端删光了文件，
//! 于是触发删除对账清掉用户本地数据。每一条都对应一种真实的挂载故障。

use arca_format::trace::{EventKind, FieldValue, VecSink};
use arca_store::root::{MountError, RootEscape, StorageRoot};
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
    assert_eq!(root.join("files").unwrap().file_name().unwrap(), "files");
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
        Err(MountError::Malformed { path, .. }) => {
            assert!(
                path.ends_with(".arca/format.json"),
                "Malformed 必须点名具体路径，供扫多个挂载点的 fsck 定位，实得 {path:?}"
            );
        }
        other => panic!("必须报 Malformed，实得 {other:?}"),
    }
}

#[test]
fn 期望身份格式非法时报_bad_expected_id_而不是_identity_mismatch() {
    // 调用方参数错误与卷身份不符是两类不同的失败（不是「卷」的问题）
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    match StorageRoot::open(dir.path(), Some("大写不合法ID")) {
        Err(MountError::BadExpectedId { value }) => {
            assert_eq!(value, "大写不合法ID");
        }
        other => panic!("必须报 BadExpectedId，实得 {other:?}"),
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

#[test]
fn join_拒绝绝对路径逃逸() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let root = StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    match root.join("/etc/passwd") {
        Err(RootEscape { .. }) => {}
        other => panic!("绝对路径必须被拒绝，实得 {other:?}"),
    }
}

#[test]
fn join_拒绝父目录引用逃逸() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let root = StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    match root.join("../../逃逸") {
        Err(RootEscape { .. }) => {}
        other => panic!("`..` 父目录引用必须被拒绝，实得 {other:?}"),
    }
}

#[test]
fn join_放行存储根内的正常相对路径() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let root = StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    let joined = root.join("files/正常路径").unwrap();
    assert_eq!(joined, dir.path().join("files/正常路径"));
}

#[test]
fn join_不误伤名字里含两个点但不是父引用的路径() {
    // `a..b` 是合法文件名，不能被 `..` 的字符串匹配误伤（须按路径分量判断）
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let root = StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    let joined = root.join("a..b/文件").unwrap();
    assert_eq!(joined, dir.path().join("a..b/文件"));
}

#[test]
fn 成功打开会发一条_mount_check_且_ok_为真() {
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let mut sink = VecSink::new();
    StorageRoot::open_traced(dir.path(), Some(样例_ID), 1_000, &mut sink).unwrap();

    let 记录 = sink.records();
    assert_eq!(记录.len(), 1, "应恰好发一条事件");
    assert_eq!(记录[0].event, EventKind::MountCheck);
    assert_eq!(记录[0].field("ok"), Some(&FieldValue::from(true)));
    assert_eq!(
        记录[0].field("found"),
        Some(&FieldValue::from(样例_ID.to_string()))
    );
}

#[test]
fn 身份不符也会发_mount_check_且带上两侧的值() {
    // 失败路径的 trace 比成功路径更重要——它是事故现场的线索
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), "1111111111111111111111111111aaaa");
    let mut sink = VecSink::new();
    let 结果 = StorageRoot::open_traced(dir.path(), Some(样例_ID), 2_000, &mut sink);
    assert!(结果.is_err());

    let 记录 = sink.records();
    assert_eq!(记录[0].event, EventKind::MountCheck);
    assert_eq!(记录[0].field("ok"), Some(&FieldValue::from(false)));
    assert_eq!(
        记录[0].field("expect"),
        Some(&FieldValue::from(样例_ID.to_string()))
    );
    assert_eq!(
        记录[0].field("found"),
        Some(&FieldValue::from(
            "1111111111111111111111111111aaaa".to_string()
        ))
    );
}

#[test]
fn 根缺失时的_mount_check_的_found_为空() {
    let dir = tempfile::tempdir().unwrap();
    let mut sink = VecSink::new();
    let _ = StorageRoot::open_traced(
        &dir.path().join("从未挂载"),
        Some(样例_ID),
        3_000,
        &mut sink,
    );

    let 记录 = sink.records();
    assert_eq!(记录[0].event, EventKind::MountCheck);
    assert_eq!(记录[0].field("ok"), Some(&FieldValue::from(false)));
    assert_eq!(
        记录[0].field("found"),
        Some(&FieldValue::from(String::new())),
        "根缺失时 found 必须是空字符串，而不是省略该字段"
    );
}

#[test]
fn open_不发任何事件() {
    // Rule of Silence 的对应物：不注入 sink 就不该有开销
    let dir = tempfile::tempdir().unwrap();
    造存储根(dir.path(), 样例_ID);
    let sink = VecSink::new();
    StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    assert!(sink.records().is_empty());
}
