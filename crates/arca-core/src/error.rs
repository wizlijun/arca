//! 错误分类与「绝不猜测」处置（I5）。
//!
//! 原则：状态模糊 → 停下并可诊断，而不是尽力恢复。
//! 错误须区分：可重试（网络/锁竞争）、需人工介入（一致性冲突、孤儿数据集）、
//! 协议错误（CAS 412 → 结构化冲突，走 [`crate::conflict`]）。
//!
//! 分类的落地处已定：[`arca_format::trace::ErrorClass`]（`retryable` / `needs_human` /
//! `protocol` / `bug`），码表在 PROTOCOL.md §7。本模块的错误类型只需**映射**到它，
//! 不重新发明一套分类——同一套 `class` 同时出现在 trace 事件、HTTP 错误体与 `--json` 输出，
//! agent 只看 `class` 就知道该重试、该停下、还是该报 bug（FORMAT.md §10.4）。
//!
//! 参考 lazync：`shared/src/nc_errors.pas`、`shared/src/nc_error_codes.pas`。
//!
//! **本切片（M1b）只覆盖三态调和已经真实产出的终态**：[`crate::reconcile::Action::NeedsHuman`]
//! 与 [`crate::reconcile::Action::Conflict`]。`commit`/`conflict` 落地/`journal` 游标各自的
//! 错误随对应里程碑增补——不在这里预先造出当前没有任何生产代码会构造的变体（那类变体
//! 只会是摆设，也没有真实用例能验证分类对不对）。[`crate::reconcile::Decision::into_outcome`]
//! 是这层映射唯一的生产入口，`tests/decision_table.rs` 对 18 格逐一验证过。
//! **`CoreError::Conflict` 不代表 `into_outcome` 会对它返回 `Err`**——按
//! `PROTOCOL.md` §7，`class=protocol` 明确「不作为错误处理」，`into_outcome`
//! 把它包进 `Ok(Outcome::Conflict(..))`；这里仍然保留 `CoreError::Conflict`
//! 变体，是因为 `class()`/`code()`/`reason()` 这些访问器对冲突同样有用
//! （trace、`--json` 输出都要用），只是它不经 `Result::Err` 这条通道传播。

use arca_format::model::ItemId;
use arca_format::trace::ErrorClass;

/// arca-core 侧的错误类型：三态调和判定的终态，转成可传播的错误。
///
/// 判别式与 [`crate::reconcile::Action`] 里同样携带 `item_id` 的两个终态一一对应
/// （`Noop`/`Upload`/`Download`/`AdoptBaseline`/`DeleteLocal`/`TombstoneRemote` 都是
/// 「继续执行」的动作，不是错误，没有对应变体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreError {
    /// 对应 [`crate::reconcile::Action::NeedsHuman`]：状态模糊，停下等人（I5）——
    /// 例如基线说某个 item 存在过，远端却既无记录也无 tombstone
    /// （`reconcile.decide` 的 `remote_vanished_without_tombstone`）。
    /// agent 只看 [`Self::class`] 就知道要停下，不必理解 `reason` 的语义。
    NeedsHuman {
        item_id: ItemId,
        reason: &'static str,
    },
    /// 对应 [`crate::reconcile::Action::Conflict`]：结构化冲突，双方各自有独立、
    /// 互不相同的修改，走 M2 `crate::conflict` 的落地流程——**不是**要人立刻处理的
    /// 错误，是走既定流程，所以 `class` 是 `protocol` 而不是 `needs_human`。
    Conflict {
        item_id: ItemId,
        reason: &'static str,
    },
}

impl CoreError {
    /// agent 只看这个就知道该重试、该停下、还是该报 bug（PROTOCOL.md §7）。
    pub fn class(&self) -> ErrorClass {
        match self {
            CoreError::NeedsHuman { .. } => ErrorClass::NeedsHuman,
            CoreError::Conflict { .. } => ErrorClass::Protocol,
        }
    }

    /// 稳定的短码，PROTOCOL.md §7 登记，只增不改语义（I10）。
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::NeedsHuman { .. } => "reconcile.needs_human",
            CoreError::Conflict { .. } => "reconcile.conflict",
        }
    }

    pub fn item_id(&self) -> ItemId {
        match self {
            CoreError::NeedsHuman { item_id, .. } | CoreError::Conflict { item_id, .. } => *item_id,
        }
    }

    /// `reconcile.decide` 产出的稳定短标识（[`crate::reconcile::Reason`]）——
    /// 与触发这个错误的 `Decision::reason` 逐字相同,供诊断串联。
    pub fn reason(&self) -> &'static str {
        match self {
            CoreError::NeedsHuman { reason, .. } | CoreError::Conflict { reason, .. } => reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_id(byte: u8) -> ItemId {
        ItemId::from_bytes([byte; 16])
    }

    /// 每个变体的 `class()` 都被逐一断言——`NeedsHuman` 决策对应的错误必须是
    /// `needs_human`，agent 只看这个字段就知道要停下；`Conflict` 走结构化冲突
    /// 流程，不是要人立刻介入的错误，是 `protocol`。
    #[test]
    fn class_映射到_error_class_逐一核对() {
        assert_eq!(
            CoreError::NeedsHuman {
                item_id: item_id(1),
                reason: "remote_vanished_without_tombstone",
            }
            .class(),
            ErrorClass::NeedsHuman
        );
        assert_eq!(
            CoreError::Conflict {
                item_id: item_id(1),
                reason: "three_way_divergent",
            }
            .class(),
            ErrorClass::Protocol
        );
    }

    /// `code()` 字面量核对 PROTOCOL.md §7 登记的取值——硬编码，不用表达式算出来
    /// 再比（否则改了实现、测试也跟着改，等于自证）。
    #[test]
    fn code_逐字匹配_protocol_md() {
        assert_eq!(
            CoreError::NeedsHuman {
                item_id: item_id(1),
                reason: "x",
            }
            .code(),
            "reconcile.needs_human"
        );
        assert_eq!(
            CoreError::Conflict {
                item_id: item_id(1),
                reason: "x",
            }
            .code(),
            "reconcile.conflict"
        );
    }

    /// 码表互不重复——PROTOCOL.md §7 的 `code` 是跨 crate 共用的稳定标识，
    /// 撞码会让 agent 按错误的 `class` 处置。
    #[test]
    fn code_互不重复() {
        let variants = [
            CoreError::NeedsHuman {
                item_id: item_id(1),
                reason: "x",
            },
            CoreError::Conflict {
                item_id: item_id(1),
                reason: "x",
            },
        ];
        let mut codes: Vec<&str> = variants.iter().map(CoreError::code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), variants.len());
    }

    #[test]
    fn item_id_与_reason_原样透传() {
        let err = CoreError::NeedsHuman {
            item_id: item_id(7),
            reason: "remote_vanished_without_tombstone",
        };
        assert_eq!(err.item_id(), item_id(7));
        assert_eq!(err.reason(), "remote_vanished_without_tombstone");
    }
}
