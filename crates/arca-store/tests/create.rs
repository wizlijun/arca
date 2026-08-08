//! `StorageRoot::create`：引导一个全新存储根（M1d Task 5/6 的前置能力）。
//!
//! 这段能力补进 `arca-store` 而不是消费者 crate 的理由见 `root.rs` 里
//! `CreateError` 的文档注释：创建逻辑必须与 `open`/`FormatJson::parse` 的
//! 校验逻辑挨在一起，两个消费者（M1 的 arca-cli、M2 的 arcad）都要用到。

use arca_format::hub_layout::layout;
use arca_store::root::{CreateError, StorageRoot};
use std::fs;

const 样例_ID: &str = "9c41000000000000000000000000abcd";
const 样例_时间: &str = "2026-08-08T09:00:00Z";

#[test]
fn 创建全新存储根后能立即打开且身份一致() {
    let dir = tempfile::tempdir().unwrap();
    let root_path = dir.path().join("brand-new-root");

    let created = StorageRoot::create(&root_path, 样例_ID, 样例_时间).unwrap();
    assert_eq!(created.dataset_id(), 样例_ID);

    let opened = StorageRoot::open(&root_path, Some(样例_ID)).unwrap();
    assert_eq!(opened.dataset_id(), 样例_ID);
    assert_eq!(opened.format().created_at, 样例_时间);
    assert_eq!(opened.format().hash_algo, "blake3");
}

#[test]
fn 创建时root目录本身不必预先存在() {
    // adopt 在全新挂载点上建首个数据集时，这个目录很可能还没被创建过。
    let dir = tempfile::tempdir().unwrap();
    let root_path = dir.path().join("a/b/c/not-created-yet");
    assert!(!root_path.exists());

    StorageRoot::create(&root_path, 样例_ID, 样例_时间).unwrap();
    assert!(root_path.is_dir());
}

#[test]
fn 骨架目录全部就绪() {
    let dir = tempfile::tempdir().unwrap();
    StorageRoot::create(dir.path(), 样例_ID, 样例_时间).unwrap();

    for sub in [
        layout::FILES_DIR,
        layout::INDEX_DIR,
        layout::ITEMS_DIR,
        layout::CHUNKS_DIR,
        layout::JOURNAL_DIR,
        layout::TMP_DIR,
        layout::TRASH_DIR,
        layout::UPLOADS_DIR,
        layout::LOCKS_DIR,
    ] {
        assert!(dir.path().join(sub).is_dir(), "{sub} 必须存在");
    }
}

#[test]
fn 拒绝在已有format_json的目录上重复创建() {
    let dir = tempfile::tempdir().unwrap();
    StorageRoot::create(dir.path(), 样例_ID, 样例_时间).unwrap();

    match StorageRoot::create(dir.path(), 样例_ID, 样例_时间) {
        Err(CreateError::AlreadyExists { .. }) => {}
        other => panic!("重复创建必须拒绝，实得 {other:?}"),
    }
}

#[test]
fn 拒绝重复创建即便传入不同的dataset_id() {
    // 拒绝覆盖是绝对的——不因为"这次传的 id 不一样"就网开一面（I5）。
    let dir = tempfile::tempdir().unwrap();
    StorageRoot::create(dir.path(), 样例_ID, 样例_时间).unwrap();

    let 另一个_id = "1111111111111111111111111111aaaa";
    match StorageRoot::create(dir.path(), 另一个_id, 样例_时间) {
        Err(CreateError::AlreadyExists { .. }) => {}
        other => panic!("重复创建必须拒绝，实得 {other:?}"),
    }
    // 原有身份不得被动过。
    let opened = StorageRoot::open(dir.path(), Some(样例_ID)).unwrap();
    assert_eq!(opened.dataset_id(), 样例_ID);
}

#[test]
fn 拒绝不合规编码的dataset_id() {
    let dir = tempfile::tempdir().unwrap();
    match StorageRoot::create(dir.path(), "太短", 样例_时间) {
        Err(CreateError::BadDatasetId { value }) => assert_eq!(value, "太短"),
        other => panic!("应报 BadDatasetId，实得 {other:?}"),
    }
    // 参数校验失败时不应该有任何副作用。
    assert!(!dir.path().join(layout::FORMAT_JSON).exists());
}

#[test]
fn 参数校验失败不留下任何骨架目录() {
    let dir = tempfile::tempdir().unwrap();
    let _ = StorageRoot::create(dir.path(), "不合法id", 样例_时间);
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        0,
        "dataset_id 不合法时不应创建任何骨架目录"
    );
}

#[test]
fn format_json经原子写入tmp目录不留残留() {
    let dir = tempfile::tempdir().unwrap();
    StorageRoot::create(dir.path(), 样例_ID, 样例_时间).unwrap();

    let tmp_dir = dir.path().join(layout::TMP_DIR);
    let leftovers: Vec<_> = fs::read_dir(&tmp_dir).unwrap().collect();
    assert!(
        leftovers.is_empty(),
        "atomic::write 成功后不应留下 tmp 残留"
    );
}
