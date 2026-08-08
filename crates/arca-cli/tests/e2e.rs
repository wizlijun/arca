//! 端到端：`file://` 同步闭环（M1d Task 6）——两个独立的"设备"（各自一个 git
//! 工作树 + 数据集目录）共用同一个本地存储根，制造三态调和表里各种真实可达
//! 的组合，跑 `arca_cli::sync::sync`，断言收敛且文件内容正确。
//!
//! 只覆盖 `hub::read_remote` 在 M1 实际能产出的组合——`RemoteState::Tombstoned`
//! 结构上产不出来（见 `crates/arca-cli/src/hub.rs` 模块文档），因此
//! `DeleteLocal`（依赖 `remote=tombstoned`）在 M1 的真实数据流里不可达，
//! 不在本文件覆盖范围（`sync.rs` 的单元测试已经用手工构造的三态直接测过
//! 这条分支的执行逻辑）。

use arca_chunk::hash::ContentHash;
use arca_format::hub_layout::layout;
use arca_format::model::Actor;
use arca_format::trace::NullSink;
use arca_store::root::StorageRoot;
use std::fs;
use std::path::Path;

fn actor(name: &str) -> Actor {
    Actor {
        account: name.to_string(),
        device: name.to_string(),
        session: "s1".to_string(),
    }
}

/// 建一个全新的存储根（引导，不经 `arca register`/`adopt`——这里只测
/// `sync` 本身，不测命令壳）。
fn new_storage_root(dir: &Path) -> StorageRoot {
    StorageRoot::create(
        dir,
        "9c41000000000000000000000000abcd",
        "2026-08-08T09:00:00Z",
    )
    .unwrap()
}

#[test]
fn 两台设备各自新增互不相同的内容_都被对方拉到本地() {
    let store = tempfile::tempdir().unwrap();
    let root = new_storage_root(store.path());

    let device_a = tempfile::tempdir().unwrap();
    let device_b = tempfile::tempdir().unwrap();
    fs::write(device_a.path().join("only-a.txt"), b"content a").unwrap();
    fs::write(device_b.path().join("only-b.txt"), b"content b").unwrap();

    let mut sink = NullSink;
    let report_a1 = arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();
    assert_eq!(report_a1.uploaded, vec!["only-a.txt".to_string()]);

    let report_b1 = arca_cli::sync::sync(device_b.path(), &root, &actor("b"), &mut sink).unwrap();
    // b 第一轮：上传自己的 only-b.txt，同时下载 a 已经上传的 only-a.txt。
    assert_eq!(report_b1.uploaded, vec!["only-b.txt".to_string()]);
    assert_eq!(report_b1.downloaded, vec!["only-a.txt".to_string()]);
    assert_eq!(
        fs::read(device_b.path().join("only-a.txt")).unwrap(),
        b"content a"
    );

    let report_a2 = arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();
    assert_eq!(report_a2.downloaded, vec!["only-b.txt".to_string()]);
    assert_eq!(
        fs::read(device_a.path().join("only-b.txt")).unwrap(),
        b"content b"
    );
    assert!(report_a2.is_clean());

    // 再各跑一次：完全收敛，不应该有任何动作。
    let report_a3 = arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();
    assert!(!report_a3.changed());
    let report_b2 = arca_cli::sync::sync(device_b.path(), &root, &actor("b"), &mut sink).unwrap();
    assert!(!report_b2.changed());
}

#[test]
fn 本地修改在下一轮上传_远端修改在下一轮下载() {
    let store = tempfile::tempdir().unwrap();
    let root = new_storage_root(store.path());
    let mut sink = NullSink;

    let device_a = tempfile::tempdir().unwrap();
    fs::write(device_a.path().join("doc.txt"), b"v1").unwrap();
    arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();

    let device_b = tempfile::tempdir().unwrap();
    arca_cli::sync::sync(device_b.path(), &root, &actor("b"), &mut sink).unwrap();
    assert_eq!(fs::read(device_b.path().join("doc.txt")).unwrap(), b"v1");

    // a 本地修改 → 下一轮 Upload{parent:Some(..)}。
    fs::write(device_a.path().join("doc.txt"), b"v2-from-a").unwrap();
    let report_a = arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();
    assert_eq!(report_a.uploaded, vec!["doc.txt".to_string()]);

    // b 尚未改动 → 下一轮 Download，拿到 a 的修改。
    let report_b = arca_cli::sync::sync(device_b.path(), &root, &actor("b"), &mut sink).unwrap();
    assert_eq!(report_b.downloaded, vec!["doc.txt".to_string()]);
    assert_eq!(
        fs::read(device_b.path().join("doc.txt")).unwrap(),
        b"v2-from-a"
    );
    assert!(report_b.is_clean());
}

#[test]
fn 三方分叉产生冲突_不动任何一侧的数据() {
    let store = tempfile::tempdir().unwrap();
    let root = new_storage_root(store.path());
    let mut sink = NullSink;

    let device_a = tempfile::tempdir().unwrap();
    fs::write(device_a.path().join("doc.txt"), b"base").unwrap();
    arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();

    let device_b = tempfile::tempdir().unwrap();
    arca_cli::sync::sync(device_b.path(), &root, &actor("b"), &mut sink).unwrap();

    // b 修改并上传（远端推进）。
    fs::write(device_b.path().join("doc.txt"), b"from-b").unwrap();
    arca_cli::sync::sync(device_b.path(), &root, &actor("b"), &mut sink).unwrap();

    // a 在自己尚未拉取 b 的修改前，本地也独立改了——三方分叉。
    fs::write(device_a.path().join("doc.txt"), b"from-a").unwrap();
    let report_a = arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();

    assert_eq!(report_a.conflicts, vec!["doc.txt".to_string()]);
    assert!(!report_a.is_clean());
    assert_eq!(
        fs::read(device_a.path().join("doc.txt")).unwrap(),
        b"from-a"
    );
    assert_eq!(
        fs::read(store.path().join("files/doc.txt")).unwrap(),
        b"from-b",
        "冲突时绝不动远端数据"
    );

    // 冲突不阻塞其它路径：a 上再跑一次仍然稳定报出同一个冲突，不会越跑越坏。
    let report_a2 = arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();
    assert_eq!(report_a2.conflicts, vec!["doc.txt".to_string()]);
    assert_eq!(
        fs::read(device_a.path().join("doc.txt")).unwrap(),
        b"from-a"
    );
}

#[test]
fn 远端凭空消失时停下等人而不是猜测成删除() {
    let store = tempfile::tempdir().unwrap();
    let root = new_storage_root(store.path());
    let mut sink = NullSink;

    let device = tempfile::tempdir().unwrap();
    fs::write(device.path().join("doc.txt"), b"content").unwrap();
    let first = arca_cli::sync::sync(device.path(), &root, &actor("a"), &mut sink).unwrap();
    assert!(first.is_clean());

    // 人为破坏存储根：直接删掉 index 记录（模拟"记录凭空消失"，不经过任何
    // 合法的 arca 操作——journal/tombstone 在 M1 里根本没有落地的地方）。
    let key = arca_format::path_rules::index_key("doc.txt");
    let index_path = store.path().join(layout::index_path(&key));
    fs::remove_file(&index_path).unwrap();

    let second = arca_cli::sync::sync(device.path(), &root, &actor("a"), &mut sink).unwrap();
    assert_eq!(second.needs_human, vec!["doc.txt".to_string()]);
    assert!(!second.is_clean());
    // 绝不猜测成"远端删了"——本地文件必须原样保留。
    assert_eq!(fs::read(device.path().join("doc.txt")).unwrap(), b"content");
}

#[test]
fn 大量文件在两台设备之间稳定收敛() {
    let store = tempfile::tempdir().unwrap();
    let root = new_storage_root(store.path());
    let mut sink = NullSink;

    let device_a = tempfile::tempdir().unwrap();
    for i in 0..50 {
        fs::write(
            device_a.path().join(format!("file-{i:03}.bin")),
            format!("content-{i}").as_bytes(),
        )
        .unwrap();
    }
    let report_a = arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink).unwrap();
    assert_eq!(report_a.uploaded.len(), 50);

    let device_b = tempfile::tempdir().unwrap();
    let report_b = arca_cli::sync::sync(device_b.path(), &root, &actor("b"), &mut sink).unwrap();
    assert_eq!(report_b.downloaded.len(), 50);
    for i in 0..50 {
        assert_eq!(
            fs::read(device_b.path().join(format!("file-{i:03}.bin"))).unwrap(),
            format!("content-{i}").as_bytes()
        );
    }

    // 收敛：两边都再跑一次，完全静默。
    assert!(
        !arca_cli::sync::sync(device_a.path(), &root, &actor("a"), &mut sink)
            .unwrap()
            .changed()
    );
    assert!(
        !arca_cli::sync::sync(device_b.path(), &root, &actor("b"), &mut sink)
            .unwrap()
            .changed()
    );
}

#[test]
fn 存储根内容哈希与本地一致() {
    let store = tempfile::tempdir().unwrap();
    let root = new_storage_root(store.path());
    let mut sink = NullSink;

    let device = tempfile::tempdir().unwrap();
    let content = b"\x00\x01\xfe\xff binary content \xff\x00";
    fs::write(device.path().join("bin.dat"), content).unwrap();
    arca_cli::sync::sync(device.path(), &root, &actor("a"), &mut sink).unwrap();

    let stored = fs::read(store.path().join("files/bin.dat")).unwrap();
    assert_eq!(stored, content);
    assert_eq!(
        ContentHash::from_bytes(&stored),
        ContentHash::from_bytes(content)
    );
}
