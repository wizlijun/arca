//! 三态对账状态机（本地实体 × 本地基线 × hub 状态）——同步的核心决策逻辑。
//!
//! hub-and-spoke + 线性历史（spec §5.1）：每个 item 的历史在 hub 上线性化，
//! 客户端只需三态对账 + CAS，不需要版本向量。
//!
//! 输入：本地扫描结果、基线（客户端投影）、hub journal 事件；
//! 输出一个 [`Decision`]（一个 [`Action`] + 一个 [`Reason`]）——**纯决策，不执行 IO**。
//! 上传/下载/CAS 提交由 arcad / arca-cli 等上层按 `Action` 执行。
//!
//! 手动模式与 agentd 自动模式正确性同源，只是触发时机不同（spec §5.2）。
//!
//! 参考 lazync：`client/src/nc_sync_engine.pas` 的三态调和决策表。
//!
//! # 决策表（本切片的全部价值所在）
//!
//! 行键是 [`crate::state::BaseState`] × [`crate::state::LocalState::classify`] ×
//! [`crate::state::RemoteState::classify`]（`FORMAT.md` §10.3 的 `base`/`local`/`remote`
//! 词汇）。18 格，覆盖全部合法组合——非法组合（例如 `base=absent` 时 `local=unchanged`）
//! 在类型层面就不可构造，见 `crate::state` 的分类逻辑。
//!
//! | base | local | remote | action | reason | 理由 |
//! | --- | --- | --- | --- | --- | --- |
//! | absent | absent | absent | `Noop` | `nothing_anywhere` | 无事发生 |
//! | absent | added | absent | `Upload{parent:None}` | `local_new` | 本地新增 → 上传，CAS 的 parent 为 None（仅创建） |
//! | absent | absent | present | `Download` | `remote_new` | 远端新增 → 下载 |
//! | absent | added | present | 哈希相同 → `AdoptBaseline`；否则 `Conflict` | `converged_independently` / `both_new_divergent` | **零传输认领**：两端各自产生了同一内容（例如同一张照片从两台设备导入）。spec §4.3 |
//! | present | unchanged | unchanged | `Noop` | `all_in_sync` | |
//! | present | modified | unchanged | `Upload{parent:Some(base.version)}` | `local_modified` | CAS 带父版本（I4） |
//! | present | unchanged | modified | `Download` | `remote_modified` | |
//! | present | modified | modified | 哈希相同 → `AdoptBaseline`；否则 `Conflict` | `converged_independently` / `three_way_divergent` | `three_way_divergent` 已被 `FORMAT.md` §10.1 示例钉死，逐字使用 |
//! | present | absent | unchanged | `TombstoneRemote` | `local_deleted` | 本地删除 → 传播为 tombstone（不是物理销毁，I3） |
//! | present | absent | modified | `Download` | `delete_vs_modify` | **本地删除撞上远端修改**：按 I3，删除绝不能赢——重新下载远端版本并报告 |
//! | present | unchanged | tombstoned | `DeleteLocal` | `remote_tombstoned` | 远端删除且本地无改动 → 移除本地副本 |
//! | present | modified | tombstoned | `Conflict` | `modify_vs_delete` | 本地有未同步修改 → 绝不删，升级为冲突副本（spec §5.3） |
//! | present | absent | tombstoned | `Noop` | `both_deleted` | 两端都删了，清基线即可 |
//! | present | absent | absent | `NeedsHuman` | `remote_vanished_without_tombstone` | **远端记录凭空消失**：基线说它存在过，远端却既无记录也无 tombstone。按 I5 停下，绝不推断成「远端删了」 |
//! | absent | absent | tombstoned | `Noop` | `tombstone_for_unknown_item` | 收到一个从没见过的 item 的 tombstone，无事可做 |
//! | absent | added | tombstoned | `Upload{parent:None}` | `local_new_over_tombstone` | 删除后重建 = 新身份（spec §4.1），按新增上传 |
//! | present | modified | absent | `NeedsHuman` | `remote_vanished_without_tombstone` | 同上，且本地还有未同步的修改，更不能猜 |
//! | present | unchanged | absent | `NeedsHuman` | `remote_vanished_without_tombstone` | 同上 |
//!
//! **两条贯穿全表的纪律：**
//!
//! 1. **没有任何一格的动作是「删除数据」**。[`Action::DeleteLocal`] 移除的是本地副本，
//!    权威副本在 hub 的 trash 保留期内；[`Action::TombstoneRemote`] 记的是墓碑不是销毁。
//!    物理销毁只经显式 `arca gc`。这就是 I3 在决策层的形态——
//!    [`tests`] 里没有，穷举测试在 `tests/decision_table.rs`，断言 `Action` 的判别式集合
//!    不含任何销毁语义的变体。
//! 2. **模糊必停**：`remote_vanished_without_tombstone` 三格宁可停下要人介入，
//!    也不推断成删除。这是 I5 最贵也最重要的一次应用。
//!
//! `decide` 本身按 `base` 先分两支、再按 `(local, remote)` 的原始形状（各自最多
//! 2/3 种）二级匹配——18 格里两格（`absent|added|present`、`present|modified|modified`）
//! 因为哈希相同/不同两种子结果，在代码里各自展开成一次 `if`，不增加匹配分支数，
//! 因此 Rust 的穷尽性检查恰好覆盖全部 18 格，不需要任何 `unreachable!()`。

use crate::state::{BaseState, LocalState, RemoteState};
use arca_chunk::hash::ContentHash;
use arca_format::model::{ItemId, VersionId};

/// 稳定的短标识，受 I10 约束——同一取值必须逐字出现在 `FORMAT.md` §10.3。
pub type Reason = &'static str;

/// 决策表选出的动作类别——**纯数据，不执行 IO**。
///
/// 判别式集合是 I3 的可执行断言依据：这里没有任何一个变体是「物理销毁数据」。
/// [`Action::DeleteLocal`] 只移除本地副本（权威副本仍在 hub trash 保留期内）；
/// [`Action::TombstoneRemote`] 记的是墓碑，不是销毁。物理销毁只经显式 `arca gc`
/// （不在这个类型里，也不可能由 `decide` 产出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 三方一致，无事可做。
    Noop,
    /// 上传本地内容。`parent` 为 CAS 的 If-Match（I4）；`None` 表示仅创建
    /// （基线不认识这个 item，没有父版本可比对）。
    Upload { parent: Option<VersionId> },
    /// 下载指定版本覆盖本地。
    Download { version_id: VersionId },
    /// 零传输认领：本地内容与远端内容哈希相同，直接把基线对齐到这个哈希，
    /// 不传输任何字节（spec §4.3）。
    AdoptBaseline { hash: ContentHash },
    /// 移除本地副本（不是物理销毁——权威副本仍在 hub trash 保留期内）。
    DeleteLocal { item_id: ItemId },
    /// 向 hub 提交 tombstone（不是物理销毁）。`parent` 为 CAS 的 If-Match（I4）。
    TombstoneRemote { item_id: ItemId, parent: VersionId },
    /// 结构化冲突：双方各自有独立、互不相同的修改。完整的冲突副本落地
    /// （命名、actor、时间戳）需要 sans-io 之外的上下文，属 M2 `conflict.rs`；
    /// 这里只标记「此 item 需要走冲突流程」。
    Conflict { item_id: ItemId },
    /// 状态模糊，停下等人（I5）。绝不在这里替用户做判断。
    NeedsHuman { item_id: ItemId },
}

impl Action {
    /// `FORMAT.md` §10.3 `reconcile.decide` 事件 `action` 字段的取值。
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Noop => "noop",
            Action::Upload { .. } => "upload",
            Action::Download { .. } => "download",
            Action::AdoptBaseline { .. } => "adopt_baseline",
            Action::DeleteLocal { .. } => "delete_local",
            Action::TombstoneRemote { .. } => "tombstone_remote",
            Action::Conflict { .. } => "conflict",
            Action::NeedsHuman { .. } => "needs_human",
        }
    }
}

/// 一次调和决策：选中的动作 + 稳定短标识的理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    pub reason: Reason,
}

impl Decision {
    fn new(action: Action, reason: Reason) -> Self {
        Decision { action, reason }
    }
}

/// 三态调和决策表——sans-io 纯函数，客户端与 hub 共用同一段代码。
///
/// 完整表见模块顶部 doc comment。这里的匹配结构是：先按 `base` 分两支
/// （`base` 只有两种原始形状），再按 `(local, remote)` 的原始形状二级匹配；
/// 需要哈希比较来在「一致」与「不一致」之间选择时用内部 `if`，不引入额外分支——
/// 于是 Rust 的穷尽性检查恰好覆盖全部合法组合，不需要 `unreachable!()`。
pub fn decide(base: &BaseState, local: &LocalState, remote: &RemoteState) -> Decision {
    match base {
        BaseState::Absent => decide_base_absent(local, remote),
        BaseState::Present {
            item_id,
            version_id: base_version,
            hash: base_hash,
            ..
        } => decide_base_present(*item_id, base_version, base_hash, local, remote),
    }
}

fn decide_base_absent(local: &LocalState, remote: &RemoteState) -> Decision {
    match (local, remote) {
        (LocalState::Absent, RemoteState::Absent) => {
            Decision::new(Action::Noop, "nothing_anywhere")
        }

        (LocalState::Present { .. }, RemoteState::Absent) => {
            Decision::new(Action::Upload { parent: None }, "local_new")
        }

        (LocalState::Absent, RemoteState::Present { version_id, .. }) => Decision::new(
            Action::Download {
                version_id: version_id.clone(),
            },
            "remote_new",
        ),

        // 两端各自新增：哈希相同则零传输认领，否则结构化冲突。
        (
            LocalState::Present {
                hash: local_hash, ..
            },
            RemoteState::Present {
                item_id,
                hash: remote_hash,
                ..
            },
        ) => {
            if local_hash == remote_hash {
                Decision::new(
                    Action::AdoptBaseline { hash: *local_hash },
                    "converged_independently",
                )
            } else {
                Decision::new(Action::Conflict { item_id: *item_id }, "both_new_divergent")
            }
        }

        (LocalState::Absent, RemoteState::Tombstoned { .. }) => {
            Decision::new(Action::Noop, "tombstone_for_unknown_item")
        }

        // 删除后重建 = 新身份（spec §4.1），按新增上传，不是「复活」远端的旧身份。
        (LocalState::Present { .. }, RemoteState::Tombstoned { .. }) => {
            Decision::new(Action::Upload { parent: None }, "local_new_over_tombstone")
        }
    }
}

fn decide_base_present(
    item_id: ItemId,
    base_version: &VersionId,
    base_hash: &ContentHash,
    local: &LocalState,
    remote: &RemoteState,
) -> Decision {
    match (local, remote) {
        // 远端记录凭空消失：基线说它存在过，远端却既无记录也无 tombstone。
        // 按 I5 停下，绝不推断成「远端删了」——本地是否有未同步修改都一样，
        // 三格（absent/unchanged/modified）结论相同，模糊面前不做区分。
        (LocalState::Absent, RemoteState::Absent) => Decision::new(
            Action::NeedsHuman { item_id },
            "remote_vanished_without_tombstone",
        ),
        (LocalState::Present { .. }, RemoteState::Absent) => Decision::new(
            Action::NeedsHuman { item_id },
            "remote_vanished_without_tombstone",
        ),

        // 本地删除撞上远端状态。
        (
            LocalState::Absent,
            RemoteState::Present {
                hash: remote_hash,
                version_id: remote_version,
                ..
            },
        ) => {
            if remote_hash == base_hash {
                Decision::new(
                    Action::TombstoneRemote {
                        item_id,
                        parent: base_version.clone(),
                    },
                    "local_deleted",
                )
            } else {
                // 删除绝不能赢（I3）：重新下载远端版本并报告，
                // 用户想删就再删一次，那是一次新的、明确的意图。
                Decision::new(
                    Action::Download {
                        version_id: remote_version.clone(),
                    },
                    "delete_vs_modify",
                )
            }
        }
        (LocalState::Absent, RemoteState::Tombstoned { .. }) => {
            Decision::new(Action::Noop, "both_deleted")
        }

        // 双方都还「在」：按各自与基线的哈希关系细分四种子情形。
        (
            LocalState::Present {
                hash: local_hash, ..
            },
            RemoteState::Present {
                hash: remote_hash,
                version_id: remote_version,
                ..
            },
        ) => {
            let local_unchanged = local_hash == base_hash;
            let remote_unchanged = remote_hash == base_hash;
            match (local_unchanged, remote_unchanged) {
                (true, true) => Decision::new(Action::Noop, "all_in_sync"),
                (false, true) => Decision::new(
                    Action::Upload {
                        parent: Some(base_version.clone()),
                    },
                    "local_modified",
                ),
                (true, false) => Decision::new(
                    Action::Download {
                        version_id: remote_version.clone(),
                    },
                    "remote_modified",
                ),
                (false, false) => {
                    if local_hash == remote_hash {
                        Decision::new(
                            Action::AdoptBaseline { hash: *local_hash },
                            "converged_independently",
                        )
                    } else {
                        Decision::new(Action::Conflict { item_id }, "three_way_divergent")
                    }
                }
            }
        }

        // 远端已删除：本地无改动才安全跟删；本地有改动绝不删，升级为冲突（spec §5.3）。
        (
            LocalState::Present {
                hash: local_hash, ..
            },
            RemoteState::Tombstoned { .. },
        ) => {
            if local_hash == base_hash {
                Decision::new(Action::DeleteLocal { item_id }, "remote_tombstoned")
            } else {
                Decision::new(Action::Conflict { item_id }, "modify_vs_delete")
            }
        }
    }
}
