//! 三态调和决策表：穷举 18 格 + I3 可执行断言（无任何路径销毁数据）。
//!
//! 数据驱动：每一条 [`Case`] 对应 `reconcile` 模块 doc comment 里表格的一行。
//! 四行会按哈希再细分：`absent|added|present`、`present|unchanged|modified`、
//! `present|absent|modified` 各拆成两条 `Case`；`present|modified|modified`
//! 拆成三条（远端哈希与基线相同/不同两层判断，再嵌套本地与远端是否撞成同一
//! 内容——这一层三分支正是曾经漏掉 `remote_hash == base_hash` 检查、把
//! 「远端只是版本推进」误判进冲突的地方，回归测试见下方对应 `Case` 的注释）。
//! 所以下面共 23 条，覆盖全部 18 格。
//!
//! **每条 `Case` 断言的是完整的 `Action` 值（含 `parent`/`version_id`/`hash` 等
//! 携带字段），不是只看判别式**：`Action` 派生了 `PartialEq`，`assert_eq!` 直接
//! 结构比较。这是有教训的——CAS 的 `parent` 该取哪个版本号（基线版本还是远端
//! 当前版本）正是本文件要守住的最容易悄悄写错的地方。

use arca_chunk::hash::ContentHash;
use arca_core::reconcile::{decide, decide_traced, Action, Reason};
use arca_core::state::{BaseState, LocalClass, LocalState, RemoteClass, RemoteState};
use arca_format::model::{ItemId, VersionId};
use arca_format::trace::{EventKind, FieldValue, NullSink, VecSink};

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
/// 基线记录的版本号——`base_present()` 恒用它。
const BASE_VERSION: u8 = 0;
/// 「远端与基线同版本」场景用的版本号（就是 `BASE_VERSION`，起个名字避免裸写 0）。
const REMOTE_SAME_VERSION: u8 = BASE_VERSION;
/// 「远端版本已推进」场景用的版本号（内容是否也变了由哈希另外控制）。
const REMOTE_ADVANCED_VERSION: u8 = 2;
/// `base` 缺失时远端新增用的版本号——没有基线可比，随便取一个非零值即可。
const REMOTE_NEW_VERSION: u8 = 1;

fn base_absent() -> BaseState {
    BaseState::Absent
}

fn base_present() -> BaseState {
    BaseState::Present {
        item_id: iid(ITEM),
        version_id: vid(BASE_VERSION),
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

/// `version` 显式指定——三态调和按 `version_id` 判断远端「变没变」（不是哈希，
/// 见 `crate::state` 顶部 doc comment 的死循环教训），测试必须能独立控制
/// 版本号与哈希两个维度，不能像早期版本那样把版本号写死。
fn remote_present(h: ContentHash, version: u8) -> RemoteState {
    RemoteState::Present {
        item_id: iid(ITEM),
        version_id: vid(version),
        hash: h,
        size: 4,
    }
}

fn remote_tombstoned() -> RemoteState {
    RemoteState::Tombstoned {
        item_id: iid(ITEM),
        version_id: vid(REMOTE_NEW_VERSION),
    }
}

struct Case {
    label: &'static str,
    base: BaseState,
    local: LocalState,
    remote: RemoteState,
    expected_action: Action,
    expected_reason: Reason,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            label: "absent|absent|absent",
            base: base_absent(),
            local: local_absent(),
            remote: remote_absent(),
            expected_action: Action::Noop,
            expected_reason: "nothing_anywhere",
        },
        Case {
            label: "absent|added|absent",
            base: base_absent(),
            local: local_present(hash("local")),
            remote: remote_absent(),
            expected_action: Action::Upload { parent: None },
            expected_reason: "local_new",
        },
        Case {
            label: "absent|absent|present",
            base: base_absent(),
            local: local_absent(),
            remote: remote_present(hash("remote"), REMOTE_NEW_VERSION),
            expected_action: Action::Download {
                version_id: vid(REMOTE_NEW_VERSION),
            },
            expected_reason: "remote_new",
        },
        Case {
            label: "absent|added|present（哈希相同→零传输认领）",
            base: base_absent(),
            local: local_present(hash("same")),
            remote: remote_present(hash("same"), REMOTE_NEW_VERSION),
            expected_action: Action::AdoptBaseline {
                hash: hash("same"),
                version_id: vid(REMOTE_NEW_VERSION),
            },
            expected_reason: "converged_independently",
        },
        Case {
            label: "absent|added|present（哈希不同→冲突）",
            base: base_absent(),
            local: local_present(hash("local")),
            remote: remote_present(hash("remote"), REMOTE_NEW_VERSION),
            expected_action: Action::Conflict { item_id: iid(ITEM) },
            expected_reason: "both_new_divergent",
        },
        Case {
            label: "present|unchanged|unchanged",
            base: base_present(),
            local: local_present(hash("base")),
            remote: remote_present(hash("base"), REMOTE_SAME_VERSION),
            expected_action: Action::Noop,
            expected_reason: "all_in_sync",
        },
        Case {
            label: "present|modified|unchanged",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_present(hash("base"), REMOTE_SAME_VERSION),
            expected_action: Action::Upload {
                // parent 取远端当前版本，不是基线版本——此格两者数值相同，
                // 但来源必须是 remote（见 reconcile 模块 doc comment）。
                parent: Some(vid(REMOTE_SAME_VERSION)),
            },
            expected_reason: "local_modified",
        },
        Case {
            label: "present|unchanged|modified（哈希不同→下载）",
            base: base_present(),
            local: local_present(hash("base")),
            remote: remote_present(hash("remote"), REMOTE_ADVANCED_VERSION),
            expected_action: Action::Download {
                version_id: vid(REMOTE_ADVANCED_VERSION),
            },
            expected_reason: "remote_modified",
        },
        Case {
            label: "present|unchanged|modified（哈希相同→版本推进零传输认领）",
            base: base_present(),
            local: local_present(hash("base")),
            // 死循环场景：同内容重新上传，版本号推进但哈希与基线一致。
            remote: remote_present(hash("base"), REMOTE_ADVANCED_VERSION),
            expected_action: Action::AdoptBaseline {
                hash: hash("base"),
                version_id: vid(REMOTE_ADVANCED_VERSION),
            },
            expected_reason: "remote_version_advanced",
        },
        Case {
            // 回归测试：远端只是版本推进、内容其实没变（`remote.hash ==
            // base.hash`）——与 `present|modified|unchanged` 同构，本地的
            // 修改照常上传，不该被误判进冲突。这条 Case 曾经被现有测试集
            // 漏掉过（`present|modified|modified` 原来两条 Case 的 remote
            // 哈希都与 base 不同，恰好绕开了这一分支）。
            label: "present|modified|modified（远端哈希与基线相同→照常上传）",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_present(hash("base"), REMOTE_ADVANCED_VERSION),
            expected_action: Action::Upload {
                parent: Some(vid(REMOTE_ADVANCED_VERSION)),
            },
            expected_reason: "local_modified",
        },
        Case {
            label: "present|modified|modified（远端哈希与基线不同、与本地相同→零传输认领）",
            base: base_present(),
            local: local_present(hash("converged")),
            remote: remote_present(hash("converged"), REMOTE_ADVANCED_VERSION),
            expected_action: Action::AdoptBaseline {
                hash: hash("converged"),
                version_id: vid(REMOTE_ADVANCED_VERSION),
            },
            expected_reason: "converged_independently",
        },
        Case {
            label: "present|modified|modified（三方哈希互不相同→三方冲突）",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_present(hash("remote"), REMOTE_ADVANCED_VERSION),
            expected_action: Action::Conflict { item_id: iid(ITEM) },
            expected_reason: "three_way_divergent",
        },
        Case {
            label: "present|absent|unchanged",
            base: base_present(),
            local: local_absent(),
            remote: remote_present(hash("base"), REMOTE_SAME_VERSION),
            expected_action: Action::TombstoneRemote {
                item_id: iid(ITEM),
                parent: vid(REMOTE_SAME_VERSION),
            },
            expected_reason: "local_deleted",
        },
        Case {
            label: "present|absent|modified（版本推进但哈希相同→照常传播删除）",
            base: base_present(),
            local: local_absent(),
            remote: remote_present(hash("base"), REMOTE_ADVANCED_VERSION),
            expected_action: Action::TombstoneRemote {
                item_id: iid(ITEM),
                parent: vid(REMOTE_ADVANCED_VERSION),
            },
            expected_reason: "local_deleted",
        },
        Case {
            label: "present|absent|modified（哈希不同→delete_vs_modify）",
            base: base_present(),
            local: local_absent(),
            remote: remote_present(hash("remote"), REMOTE_ADVANCED_VERSION),
            expected_action: Action::Download {
                version_id: vid(REMOTE_ADVANCED_VERSION),
            },
            expected_reason: "delete_vs_modify",
        },
        Case {
            label: "present|unchanged|tombstoned",
            base: base_present(),
            local: local_present(hash("base")),
            remote: remote_tombstoned(),
            expected_action: Action::DeleteLocal { item_id: iid(ITEM) },
            expected_reason: "remote_tombstoned",
        },
        Case {
            label: "present|modified|tombstoned",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_tombstoned(),
            expected_action: Action::Conflict { item_id: iid(ITEM) },
            expected_reason: "modify_vs_delete",
        },
        Case {
            label: "present|absent|tombstoned",
            base: base_present(),
            local: local_absent(),
            remote: remote_tombstoned(),
            expected_action: Action::Noop,
            expected_reason: "both_deleted",
        },
        Case {
            label: "present|absent|absent",
            base: base_present(),
            local: local_absent(),
            remote: remote_absent(),
            expected_action: Action::NeedsHuman { item_id: iid(ITEM) },
            expected_reason: "remote_vanished_without_tombstone",
        },
        Case {
            label: "absent|absent|tombstoned",
            base: base_absent(),
            local: local_absent(),
            remote: remote_tombstoned(),
            expected_action: Action::Noop,
            expected_reason: "tombstone_for_unknown_item",
        },
        Case {
            label: "absent|added|tombstoned",
            base: base_absent(),
            local: local_present(hash("local")),
            remote: remote_tombstoned(),
            expected_action: Action::Upload { parent: None },
            expected_reason: "local_new_over_tombstone",
        },
        Case {
            label: "present|modified|absent",
            base: base_present(),
            local: local_present(hash("local")),
            remote: remote_absent(),
            expected_action: Action::NeedsHuman { item_id: iid(ITEM) },
            expected_reason: "remote_vanished_without_tombstone",
        },
        Case {
            label: "present|unchanged|absent",
            base: base_present(),
            local: local_present(hash("base")),
            remote: remote_absent(),
            expected_action: Action::NeedsHuman { item_id: iid(ITEM) },
            expected_reason: "remote_vanished_without_tombstone",
        },
    ]
}

/// 每一格都断言**完整的** `Action`（含 `parent`/`version_id`/`hash` 字段），
/// 不是只看判别式——这是 I4（CAS 带父版本）唯一的覆盖点：如果 `local_modified`
/// 那格把 `Upload{parent: Some(...)}` 误写成 `Upload{parent: None}`（无条件创建、
/// 绕过 CAS），这条断言必须炸。
#[test]
fn 决策表_每一格都产出预期的完整动作与理由() {
    for case in cases() {
        let decision = decide(&case.base, &case.local, &case.remote);
        assert_eq!(
            decision.action, case.expected_action,
            "行 {} 的 action 不符",
            case.label
        );
        assert_eq!(
            decision.reason, case.expected_reason,
            "行 {} 的 reason 不符：得到 {:?}",
            case.label, decision.reason
        );
    }
}

/// 决策表覆盖的 18 个概念格（去重后应恰为 18 个不同的 label 前缀，
/// 哈希相同/不同子情形算同一格）。
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
        let kind = decision.action.as_str();
        assert!(
            NON_DESTRUCTIVE.contains(&kind),
            "行 {} 产出了不在非销毁白名单内的 action：{kind}",
            case.label
        );
    }
}

/// `Action::as_str()` 每个判别式逐字对照 `FORMAT.md` §10.3 `action` 字段的
/// 合法取值——硬编码字符串，不用表达式算出来再比（否则改了实现、测试也跟着
/// 改，等于自证）。
#[test]
fn action_as_str_逐字匹配_format_md() {
    assert_eq!(Action::Noop.as_str(), "noop");
    assert_eq!(Action::Upload { parent: None }.as_str(), "upload");
    assert_eq!(Action::Download { version_id: vid(0) }.as_str(), "download");
    assert_eq!(
        Action::AdoptBaseline {
            hash: hash("x"),
            version_id: vid(0),
        }
        .as_str(),
        "adopt_baseline"
    );
    assert_eq!(
        Action::DeleteLocal { item_id: iid(ITEM) }.as_str(),
        "delete_local"
    );
    assert_eq!(
        Action::TombstoneRemote {
            item_id: iid(ITEM),
            parent: vid(0),
        }
        .as_str(),
        "tombstone_remote"
    );
    assert_eq!(Action::Conflict { item_id: iid(ITEM) }.as_str(), "conflict");
    assert_eq!(
        Action::NeedsHuman { item_id: iid(ITEM) }.as_str(),
        "needs_human"
    );
}

/// `reason` 取值集合与 `FORMAT.md` §10.3 逐字一致；一些 reason 被多格共用
/// （`converged_independently`、`remote_vanished_without_tombstone`、
/// `local_deleted`）是刻意的，断言的是「集合」，不是「行数与 reason 数一一对应」。
#[test]
fn reason_取值集合与_format_md_一致() {
    let mut reasons: Vec<Reason> = cases().iter().map(|c| c.expected_reason).collect();
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
        "remote_version_advanced",
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
    let remote = remote_present(hash("remote"), REMOTE_ADVANCED_VERSION);
    assert_eq!(local.classify(&base), LocalClass::Modified);
    assert_eq!(remote.classify(&base), RemoteClass::Modified);
    assert_eq!(local.classify(&base).as_str(), "modified");
    assert_eq!(remote.classify(&base).as_str(), "modified");
    let decision = decide(&base, &local, &remote);
    assert_eq!(decision.action.as_str(), "conflict");
    assert_eq!(decision.reason, "three_way_divergent");
}

// ---------------------------------------------------------------------------
// decide_traced：trace 发射（`FORMAT.md` §10.3 `reconcile.decide` 的七个字段）
// ---------------------------------------------------------------------------

fn field_str(sink: &VecSink, key: &str) -> String {
    match sink.records()[0].field(key) {
        Some(FieldValue::Str(text)) => text.to_string(),
        other => panic!("字段 {key} 不是字符串：{other:?}"),
    }
}

/// 每种 `action` 至少一条：七个字段齐全（**恰好 7 个，不多不少**）、取值正确。
#[test]
fn decide_traced_七个字段齐全且取值正确() {
    for case in cases() {
        let mut sink = VecSink::new();
        let decision = decide_traced(
            &case.base,
            &case.local,
            &case.remote,
            "京都/鸭.png",
            42,
            &mut sink,
        );

        assert_eq!(
            sink.records().len(),
            1,
            "行 {} 应恰好发一条事件",
            case.label
        );
        let record = &sink.records()[0];
        assert_eq!(record.event, EventKind::ReconcileDecide);
        assert_eq!(record.t_abs_us, 42);
        // 恰好 7 个字段——多发一个第八字段（比如手滑加了 hash）不该被放过。
        assert_eq!(
            record.fields().len(),
            7,
            "行 {} 的字段数不是 7：{:?}",
            case.label,
            record.fields()
        );

        assert_eq!(field_str(&sink, "path"), "京都/鸭.png", "行 {}", case.label);
        assert_eq!(
            field_str(&sink, "base"),
            case.base.as_str(),
            "行 {}",
            case.label
        );
        assert_eq!(
            field_str(&sink, "local"),
            case.local.classify(&case.base).as_str(),
            "行 {}",
            case.label
        );
        assert_eq!(
            field_str(&sink, "remote"),
            case.remote.classify(&case.base).as_str(),
            "行 {}",
            case.label
        );
        assert_eq!(
            field_str(&sink, "action"),
            case.expected_action.as_str(),
            "行 {}",
            case.label
        );
        assert_eq!(
            field_str(&sink, "reason"),
            case.expected_reason,
            "行 {}",
            case.label
        );

        let expected_item_id = case
            .base
            .item_id()
            .or_else(|| case.remote.item_id())
            .map(|id| id.to_hex())
            .unwrap_or_default();
        assert_eq!(
            field_str(&sink, "item_id"),
            expected_item_id,
            "行 {}",
            case.label
        );

        // decide_traced 与 decide 对同一输入必须返回相同结果——trace 只是旁路。
        assert_eq!(decision, decide(&case.base, &case.local, &case.remote));
    }
}

/// `item_id` 在 base 与 remote 都没有时是**空字符串**，不是省略该字段
/// ——`Some("")` 与 `None` 是不同信号（M1a 已定的纪律，这里照办）。
#[test]
fn decide_traced_item_id_缺失时是空字符串而非省略字段() {
    let mut sink = VecSink::new();
    decide_traced(
        &base_absent(),
        &local_absent(),
        &remote_absent(),
        "x",
        0,
        &mut sink,
    );
    assert_eq!(
        sink.records()[0].field("item_id"),
        Some(&FieldValue::from(String::new()))
    );
}

/// `decide` 是 `decide_traced(..., &mut NullSink)` 的薄壳：同一输入下两者
/// 返回的 `Decision` 逐字段相同，且 `NullSink` 什么也不落。
#[test]
fn decide_是_decide_traced_加_null_sink_的薄壳() {
    let mut null = NullSink;
    for case in cases() {
        let via_traced = decide_traced(&case.base, &case.local, &case.remote, "", 0, &mut null);
        let via_plain = decide(&case.base, &case.local, &case.remote);
        assert_eq!(via_traced, via_plain, "行 {}", case.label);
    }
}
