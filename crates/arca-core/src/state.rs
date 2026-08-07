//! 三态调和的输入词汇：`base`（基线）× `local`（本地实体）× `remote`（hub 状态）。
//!
//! spec §5.1：客户端只需三态对账 + CAS，不需要版本向量——本模块定义三态各自
//! 能取的原始形状（`Absent` / `Present` / `Tombstoned`），供 [`crate::reconcile::decide`]
//! 消费。
//!
//! **原始形状 vs. 分类词汇（两层，刻意分开）：**
//!
//! - 原始形状（[`BaseState`] / [`LocalState`] / [`RemoteState`] 各自的 `as_str`）
//!   只反映类型自身携带的信息，不看基线：`absent` / `present`（`RemoteState` 另有
//!   `tombstoned`）。这一层不需要外部上下文，能就地判断。
//! - 分类词汇（[`LocalState::classify`] / [`RemoteState::classify`]，各自返回
//!   [`LocalClass`] / [`RemoteClass`]）**相对基线**判断，是 `FORMAT.md` §10.3
//!   `reconcile.decide` 事件里 `local` / `remote` 字段实际使用的取值，也是
//!   决策表（[`crate::reconcile`]）人读版表格的行键。`unchanged` 意味着「与基线
//!   记录的哈希一致」，`modified` 意味着「存在但哈希与基线不同」，`added` 意味着
//!   「基线里没有但本地有」。
//!
//! `base` 只有两种原始形状且分类词汇与原始形状重合（`absent` / `present`），
//! 所以 [`BaseState`] 不需要单独的 `classify`。
//!
//! **`RemoteClass` 没有 `added`，这个不对称是有意的**：本地在基线缺失时新增，
//! 需要一个新词「`added`」把它与「基线存在但哈希变了」的 `modified` 区分开；
//! 但远端在基线缺失时新增，直接复用原始形状的词「`present`」即可表达同一件事
//! ——因为这种情况下没有基线可比，"存在" 本身就是全部信息，不需要另造词汇。
//! 参见 `crate::reconcile` 模块 doc 里的决策表，`absent|added|present` 与
//! `absent|absent|present` 两行分别踩中这条不对称的两侧。
//!
//! 参考 lazync：`client/src/nc_sync_engine.pas` 的三态判断。

use arca_chunk::hash::ContentHash;
use arca_format::model::{ItemId, VersionId};

/// 基线：客户端上一次对账时记下的、双方都曾确认过的状态（可抛弃投影，I9）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseState {
    /// 基线里没有这个 item——从未同步过，或已被双方共同确认删除并清basline。
    Absent,
    Present {
        item_id: ItemId,
        version_id: VersionId,
        hash: ContentHash,
        size: u64,
    },
}

impl BaseState {
    /// 原始形状：`absent` | `present`。`base` 只有这两种形状，
    /// 分类词汇与原始形状重合，故没有单独的 `classify`。
    pub fn as_str(&self) -> &'static str {
        match self {
            BaseState::Absent => "absent",
            BaseState::Present { .. } => "present",
        }
    }

    pub fn item_id(&self) -> Option<ItemId> {
        match self {
            BaseState::Absent => None,
            BaseState::Present { item_id, .. } => Some(*item_id),
        }
    }
}

/// 本地实体：当前扫描到的本地文件状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalState {
    Absent,
    Present { hash: ContentHash, size: u64 },
}

/// `local` 相对基线的分类词汇（`FORMAT.md` §10.3 `reconcile.decide` 的 `local` 字段取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalClass {
    /// 本地没有，基线也没有——或本地被删除了。
    Absent,
    /// 基线里没有，本地有——本地新增（含「删除后重建」，spec §4.1 视为新身份）。
    Added,
    /// 基线里有，本地也有，哈希与基线一致。
    Unchanged,
    /// 基线里有，本地也有，哈希与基线不同。
    Modified,
}

impl LocalClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            LocalClass::Absent => "absent",
            LocalClass::Added => "added",
            LocalClass::Unchanged => "unchanged",
            LocalClass::Modified => "modified",
        }
    }
}

impl LocalState {
    /// 原始形状：`absent` | `present`。不看基线。
    pub fn as_str(&self) -> &'static str {
        match self {
            LocalState::Absent => "absent",
            LocalState::Present { .. } => "present",
        }
    }

    /// 相对基线分类——`FORMAT.md` §10.3 `local` 字段与决策表行键用的就是这个。
    pub fn classify(&self, base: &BaseState) -> LocalClass {
        match (base, self) {
            (_, LocalState::Absent) => LocalClass::Absent,
            (BaseState::Absent, LocalState::Present { .. }) => LocalClass::Added,
            (
                BaseState::Present {
                    hash: base_hash, ..
                },
                LocalState::Present { hash, .. },
            ) => {
                if hash == base_hash {
                    LocalClass::Unchanged
                } else {
                    LocalClass::Modified
                }
            }
        }
    }
}

/// hub 状态：journal / 库里对这个 item 的当前认知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteState {
    Absent,
    Present {
        item_id: ItemId,
        version_id: VersionId,
        hash: ContentHash,
        size: u64,
    },
    /// 远端已记录删除（tombstone）。数据仍在 hub trash 保留期内，
    /// 这不是物理销毁（I3）。
    Tombstoned {
        item_id: ItemId,
        version_id: VersionId,
    },
}

/// `remote` 相对基线的分类词汇（`FORMAT.md` §10.3 `reconcile.decide` 的 `remote` 字段取值）。
///
/// 没有 `Added`——见本模块顶部 doc comment「不对称」一节：基线缺失时远端新增
/// 直接复用原始形状的 `Present`（词面即 `present`），不需要另造词。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteClass {
    Absent,
    /// 基线缺失、远端有——远端新增，直接沿用原始形状的词面。
    Present,
    /// 基线里有，远端也有，哈希与基线一致。
    Unchanged,
    /// 基线里有，远端也有，哈希与基线不同。
    Modified,
    Tombstoned,
}

impl RemoteClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteClass::Absent => "absent",
            RemoteClass::Present => "present",
            RemoteClass::Unchanged => "unchanged",
            RemoteClass::Modified => "modified",
            RemoteClass::Tombstoned => "tombstoned",
        }
    }
}

impl RemoteState {
    /// 原始形状：`absent` | `present` | `tombstoned`。不看基线。
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteState::Absent => "absent",
            RemoteState::Present { .. } => "present",
            RemoteState::Tombstoned { .. } => "tombstoned",
        }
    }

    pub fn item_id(&self) -> Option<ItemId> {
        match self {
            RemoteState::Absent => None,
            RemoteState::Present { item_id, .. } => Some(*item_id),
            RemoteState::Tombstoned { item_id, .. } => Some(*item_id),
        }
    }

    /// 相对基线分类——`FORMAT.md` §10.3 `remote` 字段与决策表行键用的就是这个。
    pub fn classify(&self, base: &BaseState) -> RemoteClass {
        match (base, self) {
            (_, RemoteState::Absent) => RemoteClass::Absent,
            (_, RemoteState::Tombstoned { .. }) => RemoteClass::Tombstoned,
            (BaseState::Absent, RemoteState::Present { .. }) => RemoteClass::Present,
            (
                BaseState::Present {
                    hash: base_hash, ..
                },
                RemoteState::Present { hash, .. },
            ) => {
                if hash == base_hash {
                    RemoteClass::Unchanged
                } else {
                    RemoteClass::Modified
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_id(byte: u8) -> ItemId {
        ItemId::from_bytes([byte; 16])
    }

    fn version_id(seed: u8) -> VersionId {
        VersionId::new("20260805T093012Z", &format!("{:032x}", seed as u128)).unwrap()
    }

    fn hash(data: &[u8]) -> ContentHash {
        ContentHash::from_bytes(data)
    }

    // --- 原始形状 as_str ---------------------------------------------------

    #[test]
    fn base_state_原始形状取值() {
        assert_eq!(BaseState::Absent.as_str(), "absent");
        assert_eq!(
            BaseState::Present {
                item_id: item_id(1),
                version_id: version_id(1),
                hash: hash(b"a"),
                size: 1,
            }
            .as_str(),
            "present"
        );
    }

    #[test]
    fn local_state_原始形状取值() {
        assert_eq!(LocalState::Absent.as_str(), "absent");
        assert_eq!(
            LocalState::Present {
                hash: hash(b"a"),
                size: 1
            }
            .as_str(),
            "present"
        );
    }

    #[test]
    fn remote_state_原始形状取值() {
        assert_eq!(RemoteState::Absent.as_str(), "absent");
        assert_eq!(
            RemoteState::Present {
                item_id: item_id(1),
                version_id: version_id(1),
                hash: hash(b"a"),
                size: 1,
            }
            .as_str(),
            "present"
        );
        assert_eq!(
            RemoteState::Tombstoned {
                item_id: item_id(1),
                version_id: version_id(1),
            }
            .as_str(),
            "tombstoned"
        );
    }

    // --- 分类词汇 ------------------------------------------------------------

    #[test]
    fn local_classify_基线缺失时的两种取值() {
        let base = BaseState::Absent;
        assert_eq!(LocalState::Absent.classify(&base), LocalClass::Absent);
        assert_eq!(
            LocalState::Present {
                hash: hash(b"a"),
                size: 1
            }
            .classify(&base),
            LocalClass::Added
        );
    }

    #[test]
    fn local_classify_基线存在时按哈希比较() {
        let base = BaseState::Present {
            item_id: item_id(1),
            version_id: version_id(1),
            hash: hash(b"base"),
            size: 4,
        };
        assert_eq!(LocalState::Absent.classify(&base), LocalClass::Absent);
        assert_eq!(
            LocalState::Present {
                hash: hash(b"base"),
                size: 4
            }
            .classify(&base),
            LocalClass::Unchanged
        );
        assert_eq!(
            LocalState::Present {
                hash: hash(b"changed"),
                size: 7
            }
            .classify(&base),
            LocalClass::Modified
        );
    }

    #[test]
    fn remote_classify_基线缺失时复用_present_不新造_added() {
        let base = BaseState::Absent;
        assert_eq!(RemoteState::Absent.classify(&base), RemoteClass::Absent);
        assert_eq!(
            RemoteState::Present {
                item_id: item_id(1),
                version_id: version_id(1),
                hash: hash(b"a"),
                size: 1,
            }
            .classify(&base),
            RemoteClass::Present
        );
        assert_eq!(
            RemoteState::Tombstoned {
                item_id: item_id(1),
                version_id: version_id(1),
            }
            .classify(&base),
            RemoteClass::Tombstoned
        );
    }

    #[test]
    fn remote_classify_基线存在时按哈希比较() {
        let base = BaseState::Present {
            item_id: item_id(1),
            version_id: version_id(1),
            hash: hash(b"base"),
            size: 4,
        };
        assert_eq!(RemoteState::Absent.classify(&base), RemoteClass::Absent);
        assert_eq!(
            RemoteState::Present {
                item_id: item_id(1),
                version_id: version_id(2),
                hash: hash(b"base"),
                size: 4,
            }
            .classify(&base),
            RemoteClass::Unchanged
        );
        assert_eq!(
            RemoteState::Present {
                item_id: item_id(1),
                version_id: version_id(2),
                hash: hash(b"changed"),
                size: 7,
            }
            .classify(&base),
            RemoteClass::Modified
        );
        assert_eq!(
            RemoteState::Tombstoned {
                item_id: item_id(1),
                version_id: version_id(2),
            }
            .classify(&base),
            RemoteClass::Tombstoned
        );
    }

    #[test]
    fn item_id_取值优先取_base_其次_remote() {
        assert_eq!(BaseState::Absent.item_id(), None);
        assert_eq!(
            BaseState::Present {
                item_id: item_id(9),
                version_id: version_id(1),
                hash: hash(b"a"),
                size: 1,
            }
            .item_id(),
            Some(item_id(9))
        );
        assert_eq!(RemoteState::Absent.item_id(), None);
        assert_eq!(
            RemoteState::Tombstoned {
                item_id: item_id(3),
                version_id: version_id(1),
            }
            .item_id(),
            Some(item_id(3))
        );
    }
}
