//! 三态调和决策表：穷举 18 格 + I3 可执行断言（无任何路径销毁数据）。
//!
//! 数据驱动：每一条 [`Case`] 对应 `reconcile` 模块 doc comment 里表格的一行
//! （两行——`absent|added|present` 与 `present|modified|modified`——因为哈希
//! 相同/不同两种子结果，各自拆成两条 `Case`，所以下面共 20 条，覆盖全部 18 格）。

use arca_chunk::hash::ContentHash;
use arca_core::reconcile::{decide, Action, Reason};
use arca_core::state::{BaseState, LocalState, RemoteState};
use arca_format::model::{ItemId, VersionId};

fn iid(byte: u8) -> ItemId {
    ItemId::from_bytes([byte; 16])
}

fn vid(seed: u8) -> VersionId {
    VersionId::new("20260805T093012Z", &format!("{:032x}", seed as u128)).unwrap()
}

fn hash(label: &str) -> ContentHash {
    ContentHash::from_bytes(label.as_bytes())
}

const ITEM: u8 = 0x11;

fn base_absent() -> BaseState {
    BaseState::Absent
}

fn base_present() -> BaseState {
    BaseState::Present {
        item_id: iid(ITEM),
        version_id: vid(0),
        hash: hash("base"),
        size: 4,
    }
}

fn local_absent() -> LocalState {
    LocalState::Absent
}

fn local_present(h: ContentHash) -> LocalState {
    LocalState::Present { hash: h, size: 4 }
}

fn remote_absent() -> RemoteState {
    RemoteState::Absent
}

fn remote_present(h: ContentHash) -> RemoteState {
    RemoteState::Present {
        item_id: iid(ITEM),
        version_id: vid(1),
        hash: h,
        size: 4,
    }
}

fn remote_tombstoned() -> RemoteState {
    RemoteState::Tombstoned {
        item_id: iid(ITEM),
        version_id: vid(1),
    }
}

/// 只关心 `Action` 的判别式（变体），不关心携带的字段——字段值另有专门断言。
fn action_kind(action: &Action) -> &'static str {
    action.as_str()
}

struct Case {
    label: &'static str,
    base: BaseState,
    local: LocalState,
    remote: RemoteState,
    expect_action: &'static str,
    expect_reason: Reason,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            label: "absent|absent|absent",
            base: base_absent(),
            local: local_absent(),
            remote: remote_absent(),
            expect_action: "noop",
            expect_reason: "nothing_anywhere",
        },
        Case {
            label: "absent|added|absent",
            base: base_absent(),
            local: local_present(hash("local")),
            remote: remote_absent(),
            expect_action: "upload",
            expect_reason: "local_new",
        },
        Case {
            label: "absent|absent|present",
            base: base_absent(),
            local: local_absent(),
            remote: remote_present(hash("remote")),
            expect_action: "download",
            expect_reason: "remote_new",
        },
        Case {
            label: "absent|added|present（哈希相同→零传输认领）",
            base: base_absent(),
            local: local_present(hash("same")),
            remote: remote_present(hash("same")),
            expect_action: "adopt_baseline",
            expect_reason: "converged_independently",
        },
        Case {
            label: "absent|added|present（哈希不同→冲突）",
            base: base_absent(),
            local: local_present(hash("local")),
            remote: remote_present(hash("remote")),
            expect_action: "conflict",
            expect_reason: "both_new_divergent",
        },
        Case {
            label: "present|unchanged|unchanged",
            base: base_present(),
            local: local_present(hash("base")),
            remote: remote_present(hash("base")),
            expect_action: "noop",
            expect_reason: "all_in_sync",
        },
        Case {
            label: "present|modified|unchanged",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_present(hash("base")),
            expect_action: "upload",
            expect_reason: "local_modified",
        },
        Case {
            label: "present|unchanged|modified",
            base: base_present(),
            local: local_present(hash("base")),
            remote: remote_present(hash("remote")),
            expect_action: "download",
            expect_reason: "remote_modified",
        },
        Case {
            label: "present|modified|modified（哈希相同→零传输认领）",
            base: base_present(),
            local: local_present(hash("converged")),
            remote: remote_present(hash("converged")),
            expect_action: "adopt_baseline",
            expect_reason: "converged_independently",
        },
        Case {
            label: "present|modified|modified（哈希不同→三方冲突）",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_present(hash("remote")),
            expect_action: "conflict",
            expect_reason: "three_way_divergent",
        },
        Case {
            label: "present|absent|unchanged",
            base: base_present(),
            local: local_absent(),
            remote: remote_present(hash("base")),
            expect_action: "tombstone_remote",
            expect_reason: "local_deleted",
        },
        Case {
            label: "present|absent|modified",
            base: base_present(),
            local: local_absent(),
            remote: remote_present(hash("remote")),
            expect_action: "download",
            expect_reason: "delete_vs_modify",
        },
        Case {
            label: "present|unchanged|tombstoned",
            base: base_present(),
            local: local_present(hash("base")),
            remote: remote_tombstoned(),
            expect_action: "delete_local",
            expect_reason: "remote_tombstoned",
        },
        Case {
            label: "present|modified|tombstoned",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_tombstoned(),
            expect_action: "conflict",
            expect_reason: "modify_vs_delete",
        },
        Case {
            label: "present|absent|tombstoned",
            base: base_present(),
            local: local_absent(),
            remote: remote_tombstoned(),
            expect_action: "noop",
            expect_reason: "both_deleted",
        },
        Case {
            label: "present|absent|absent",
            base: base_present(),
            local: local_absent(),
            remote: remote_absent(),
            expect_action: "needs_human",
            expect_reason: "remote_vanished_without_tombstone",
        },
        Case {
            label: "absent|absent|tombstoned",
            base: base_absent(),
            local: local_absent(),
            remote: remote_tombstoned(),
            expect_action: "noop",
            expect_reason: "tombstone_for_unknown_item",
        },
        Case {
            label: "absent|added|tombstoned",
            base: base_absent(),
            local: local_present(hash("local")),
            remote: remote_tombstoned(),
            expect_action: "upload",
            expect_reason: "local_new_over_tombstone",
        },
        Case {
            label: "present|modified|absent",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_absent(),
            expect_action: "needs_human",
            expect_reason: "remote_vanished_without_tombstone",
        },
        Case {
            label: "present|unchanged|absent",
            base: base_present(),
            local: local_present(hash("base")),
            remote: remote_absent(),
            expect_action: "needs_human",
            expect_reason: "remote_vanished_without_tombstone",
        },
    ]
}

#[test]
fn 决策表_每一格都产出预期的动作与理由() {
    for case in cases() {
        let decision = decide(&case.base, &case.local, &case.remote);
        assert_eq!(
            action_kind(&decision.action),
            case.expect_action,
            "行 {} 的 action 不符：得到 {:?}",
            case.label,
            decision.action
        );
        assert_eq!(
            decision.reason, case.expect_reason,
            "行 {} 的 reason 不符：得到 {:?}",
            case.label, decision.reason
        );
    }
}

/// 决策表覆盖的 18 个概念格（去重后应恰为 18 个不同的 label 前缀，
/// 两个哈希相同/不同子情形算同一格）。
#[test]
fn 概念格数恰为_18() {
    let mut rows: Vec<&str> = cases()
        .iter()
        .map(|c| c.label.split('（').next().unwrap())
        .collect();
    rows.sort_unstable();
    rows.dedup();
    assert_eq!(rows.len(), 18, "{rows:?}");
}

/// I3 的可执行断言：**没有任何一个 `Action` 判别式是销毁语义**。
///
/// 不用「恒返回 false 的 `destroys_data()`」这种自证式断言（那只是把承诺原样
/// 抄一遍，测试不了任何东西）；而是维护一份明确的、经人工审查过的「非销毁」
/// 判别式白名单，逐条核对决策表任何一行产出的 `Action` 都落在白名单内。
/// 白名单本身的正当性写在这里：
/// - `noop` / `upload` / `download` / `adopt_baseline`：显然不涉及销毁；
/// - `delete_local`：只移除本地副本，权威副本仍在 hub trash 保留期内；
/// - `tombstone_remote`：写入的是墓碑记录，不是删除数据；
/// - `conflict` / `needs_human`：都是「停下，不擅自处置」。
///
/// 若将来有人给 `Action` 加一个真正销毁数据的变体（例如立即 purge），
/// 这份白名单不会自动放行它——除非有人把它加进来并在这里说明为什么安全，
/// 这正是这条测试要逼出来的审查动作。
#[test]
fn i3_决策表任何一行都不产出销毁语义的动作() {
    const NON_DESTRUCTIVE: [&str; 8] = [
        "noop",
        "upload",
        "download",
        "adopt_baseline",
        "delete_local",
        "tombstone_remote",
        "conflict",
        "needs_human",
    ];
    for case in cases() {
        let decision = decide(&case.base, &case.local, &case.remote);
        let kind = action_kind(&decision.action);
        assert!(
            NON_DESTRUCTIVE.contains(&kind),
            "行 {} 产出了不在非销毁白名单内的 action：{kind}",
            case.label
        );
    }
}

/// `reason` 取值集合与 `FORMAT.md` §10.3 逐字一致；一些 reason 被多格共用
/// （`converged_independently`、`remote_vanished_without_tombstone`）是刻意的，
/// 断言的是「集合」，不是「行数与 reason 数一一对应」。
#[test]
fn reason_取值集合与_format_md_一致() {
    let mut reasons: Vec<Reason> = cases().iter().map(|c| c.expect_reason).collect();
    reasons.sort_unstable();
    reasons.dedup();

    let mut expected: Vec<Reason> = vec![
        "nothing_anywhere",
        "local_new",
        "remote_new",
        "converged_independently",
        "both_new_divergent",
        "all_in_sync",
        "local_modified",
        "remote_modified",
        "three_way_divergent",
        "local_deleted",
        "delete_vs_modify",
        "remote_tombstoned",
        "modify_vs_delete",
        "both_deleted",
        "remote_vanished_without_tombstone",
        "tombstone_for_unknown_item",
        "local_new_over_tombstone",
    ];
    expected.sort_unstable();

    assert_eq!(reasons, expected);
}

/// `FORMAT.md` §10.1 示例钉死的取值：`action:"conflict"`、`reason:"three_way_divergent"`，
/// 且该场景下 `local`/`remote` 分类均为 `"modified"`——回归测试，防止有人改字符串。
#[test]
fn format_md_示例场景逐字匹配() {
    let base = base_present();
    let local = local_present(hash("local"));
    let remote = remote_present(hash("remote"));
    assert_eq!(local.classify(&base).as_str(), "modified");
    assert_eq!(remote.classify(&base).as_str(), "modified");
    let decision = decide(&base, &local, &remote);
    assert_eq!(decision.action.as_str(), "conflict");
    assert_eq!(decision.reason, "three_way_divergent");
}
