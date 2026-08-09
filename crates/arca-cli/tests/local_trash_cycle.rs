//! 端到端：`server` 角色的本地回收站从「只写」变成「可管理」（M2e Task 1，
//! FORMAT.md §9.5、spec §4.7/§7）。
//!
//! M2d 的切片评审把这条列为「最大的一条 carry-forward」：`server` 角色每收到
//! 一次远端 tombstone 就往 `<dataset>/.arca/client/trash/` 塞一份完整副本，
//! 而**今天恢复要手读 `.data`/`.meta`**——没有列表、没有保留期概念、
//! `doctor` 看不见、没有任何找回通路。本文件走的是真实数据流（真的
//! `sync()` 一轮 tombstone 传播，不手工往回收站里拼字节），断言这条路现在
//! 是通的：
//!
//! 移入 →（`list`）看得见 →（`usage`）知道占了多少 →（`restore`）逐字节
//! 拿回来 → 记录**仍在**（本模块没有销毁路径，物理销毁只经 `arca gc`，I3）。

use arca_cli::{hub, local_trash, role, trash};
use arca_core::state::RemoteState;
use arca_format::hub_layout::FormatJson;
use arca_format::model::Actor;
use arca_format::trace::NullSink;
use arca_store::root::StorageRoot;
use std::fs;
use std::path::Path;

const DATASET_ID: &str = "9c41000000000000000000000000abcd";

fn actor() -> Actor {
    Actor {
        account: "bruce".into(),
        device: "server-box".into(),
        session: "s1".into(),
    }
}

fn 造存储根(dir: &Path) -> StorageRoot {
    fs::create_dir_all(dir.join(".arca")).unwrap();
    fs::create_dir_all(dir.join("files")).unwrap();
    for sub in [".arca/tmp", ".arca/trash", ".arca/journal"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let format = FormatJson {
        format: 1,
        dataset_id: DATASET_ID.to_string(),
        hash_algo: "blake3".to_string(),
        created_at: "2026-08-09T09:00:00Z".to_string(),
    };
    fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    StorageRoot::open(dir, Some(DATASET_ID)).unwrap()
}

/// 完整跑一遍「本设备是 server 角色 → 另一台设备删了这个文件 → 本设备
/// sync 收到 tombstone → 本地副本进本地回收站」，返回 (数据集目录, 存储根目录)。
/// 与 `sync.rs` 里同名场景的构造手法一致：hub 侧的 tombstone 用真实的
/// `trash::move_to_trash` + `journal::append` 造（等价于另一台设备刚提交过
/// 一次删除），本设备这一侧全部走真实的 `sync()`。
fn 造一次server角色的本地回收站条目(
    content: &[u8],
    deleted_at: &str,
) -> (tempfile::TempDir, tempfile::TempDir) {
    let dataset = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let root = 造存储根(store.path());

    role::write(dataset.path(), role::Role::Server).unwrap();
    fs::write(dataset.path().join("photo.png"), content).unwrap();

    let mut sink = NullSink;
    arca_cli::sync::sync(dataset.path(), &root, &actor(), &mut sink).unwrap();

    let (item_id, version_id) = match hub::read_remote(&root).unwrap().get("photo.png").unwrap() {
        RemoteState::Present {
            item_id,
            version_id,
            ..
        } => (*item_id, version_id.clone()),
        other => panic!("应为 Present，实得 {other:?}"),
    };
    trash::move_to_trash(&root, "photo.png", item_id, deleted_at).unwrap();
    let seq = arca_cli::journal::next_seq(&root).unwrap();
    arca_cli::journal::append(
        &root,
        &arca_format::journal::JournalEvent {
            seq,
            op: arca_format::journal::Op::Tombstone,
            item_id,
            version_id,
            path: "photo.png".to_string(),
            from: None,
            actor: actor(),
            at: deleted_at.to_string(),
        },
    )
    .unwrap();

    let report = arca_cli::sync::sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
    assert_eq!(
        report.deleted_to_local_trash,
        vec!["photo.png".to_string()],
        "server 角色应把本地副本移进本地回收站而不是移除：{report:?}"
    );
    assert!(!dataset.path().join("photo.png").exists());

    (dataset, store)
}

#[test]
fn server角色移入的条目可被列出_可统计占用_可逐字节恢复() {
    let content = b"\x89PNG\r\n\x1a\n binary payload \x00\xff";
    let (dataset, _store) =
        造一次server角色的本地回收站条目(content, "2026-08-09T10:00:00Z");
    let root = dataset.path();

    // 1. 看得见——`list()` 列出这条记录，`meta` 逐字段可用。
    let entries = local_trash::list(root).unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].meta.path, "photo.png");
    assert_eq!(entries[0].meta.size, content.len() as u64);
    // `deleted_at` 记的是**移入本地回收站的时刻**（FORMAT.md §9.5 逐字定义），
    // 不是 hub 侧那条 tombstone 事件的时刻——两者语义不同：hub 侧回答"这个
    // 文件什么时候被谁删的"，本地回收站回答"这台设备什么时候把自己那份挪进
    // 回收站的"，保留期也是从后者起算（这份副本在这台设备上躺了多久）。
    // 因此这里只断言它是 `sync()` 那一刻的墙上时钟能解析出来的形状，不钉住
    // 具体数值。
    let recorded_at = entries[0].meta.deleted_at.clone();
    assert!(
        arca_cli::clock::parse_rfc3339(&recorded_at).is_some(),
        "deleted_at 应是合法的 RFC 3339：{recorded_at:?}"
    );

    // 2. 知道占了多少 + 保留期状态（这条刚移入，180 天内）。
    let now = "2026-08-09T11:00:00Z";
    let usage = local_trash::usage(root, now, trash::DEFAULT_RETENTION_DAYS).unwrap();
    assert_eq!(usage.entries, 1);
    assert_eq!(usage.bytes, content.len() as u64);
    assert_eq!(
        usage.oldest_deleted_at.as_deref(),
        Some(recorded_at.as_str())
    );

    // 3. 逐字节拿回来。
    let restored = local_trash::restore(root, "photo.png", now).unwrap();
    assert_eq!(restored.protected, None);
    assert_eq!(
        fs::read(root.join("photo.png")).unwrap(),
        content,
        "恢复出来的内容必须与移入前逐字节一致"
    );

    // 4. I3：记录仍在——本模块没有任何销毁路径，物理销毁只经 `arca gc`。
    assert_eq!(
        local_trash::list(root).unwrap().len(),
        1,
        "恢复不该删除回收站记录"
    );
}

/// 保留期只改变"是否列进未来 `arca gc` 的候选"，**不改变可恢复性**——
/// 一条早已过期的记录在没跑过 `arca gc` 之前照样能一条命令找回（I3）。
///
/// `deleted_at` 由 `sync()` 用墙上时钟写入（见上一个测试的注释），没法在
/// 测试里钉成 2020 年，所以这里改从另一头制造"过期"：把 `now` 推到很久
/// 之后。判据完全等价（`deleted_at + 保留期 > now` 是同一个不等式），
/// 而且更贴近真实场景——真实里过期的方式本就是时间往前走，不是记录往回退。
#[test]
fn 过了保留期的条目仍然可以恢复_保留期只影响gc候选判断() {
    let (dataset, _store) =
        造一次server角色的本地回收站条目(b"old bytes", "2026-08-09T10:00:00Z");
    let root = dataset.path();

    let far_future = "2030-01-01T00:00:00Z";
    let usage = local_trash::usage(root, far_future, trash::DEFAULT_RETENTION_DAYS).unwrap();
    assert_eq!(usage.expired, 1, "180 天之后这条已过保留期：{usage:?}");

    // 关键断言：过期 ≠ 不可恢复。没跑过 `arca gc` 就一条都不会消失（I3）。
    local_trash::restore(root, "photo.png", far_future).unwrap();
    assert_eq!(fs::read(root.join("photo.png")).unwrap(), b"old bytes");
}
