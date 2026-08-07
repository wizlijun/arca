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
//!   决策表（[`crate::reconcile`]）人读版表格的行键。
//!
//! **`local` 按内容哈希比较，`remote` 按版本号比较——这个不对称是有意的，
//! 且是修过一次真实 bug 的地方**：本地扫描没有版本号，只有内容，所以
//! `unchanged`/`modified` 只能靠哈希；但远端的权威标识是 `version_id`（CAS 的
//! If-Match 对象），`unchanged`/`modified` 必须按它判断。如果按哈希判断远端，
//! 「同一份内容被重新上传一次」会产生 `remote.hash == base.hash` 但
//! `remote.version_id != base.version_id`——分类成 `unchanged`，`decide` 端
//! 却拿着过期的 `base.version_id` 当 CAS parent 去提交，hub 以 412 拒绝，
//! 重新拉取后分类**仍然**是 `unchanged`，死循环无出口。`version_id` 一旦提交
//! 即不可变（I2：blob 不可变），同一个 `version_id` 必然对应同一个哈希，
//! 所以「`version_id` 相同」蕴含「哈希相同」，但反过来不成立——这正是死循环
//! 的根源，也是为什么只有 `version_id` 才是分类的权威依据。
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
    /// 基线里没有这个 item——从未同步过，或已被双方共同确认删除并清空基线记录。
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
    /// 相对基线分类——`FORMAT.md` §10.3 `local` 字段与决策表行键用的就是这个。
    ///
    /// 没有独立的、不看基线的 `as_str()`：`LocalState` 只有 `Absent`/`Present`
    /// 两种原始形状，若给它一个同名同签名的 `as_str()`，会吐出 `"present"`——
    /// 而 `"present"` 不在 `FORMAT.md` §10.3 `local` 字段的合法取值
    /// （`absent`/`unchanged`/`modified`/`added`）里，是一个悄悄摆在那里、
    /// 一用就错的 API。分类词汇由 `classify` 独占。
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
    /// 基线里有，远端也有，`version_id` 与基线一致——**按版本号判断，不是哈希**
    /// （见本模块顶部 doc comment：按哈希判断会在「同内容重新上传」时死循环）。
    Unchanged,
    /// 基线里有，远端也有，`version_id` 与基线不同。内容是否也变了要另外看
    /// 哈希——版本号变了不代表哈希变了（例如纯粹的版本推进）；这个区分由
    /// `crate::reconcile` 的决策表负责，`classify` 只按版本号判断「变没变」。
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
    /// 没有独立的、不看基线的 `as_str()`：与 `LocalState` 同一条纪律
    /// （见 [`LocalState::classify`] 的 doc comment）。`RemoteState` 这里更隐蔽——
    /// 它的原始形状恰好也叫 `present`，与 `remote` 字段的合法取值撞在一起，
    /// 一旦误用不会报错也不会看起来不对，只会悄悄传出一个语义错误的
    /// `remote:"present"`（基线存在时本应是 `unchanged`/`modified`）。
    /// 分类词汇由 `classify` 独占。
    pub fn item_id(&self) -> Option<ItemId> {
        match self {
            RemoteState::Absent => None,
            RemoteState::Present { item_id, .. } => Some(*item_id),
            RemoteState::Tombstoned { item_id, .. } => Some(*item_id),
        }
    }

    /// 相对基线分类——`FORMAT.md` §10.3 `remote` 字段与决策表行键用的就是这个。
    ///
    /// **按 `version_id` 判断，不是按哈希**（见本模块顶部 doc comment）：远端的
    /// 权威标识是 CAS 用的 `version_id`，按哈希判断会在「同内容重新上传产生新
    /// 版本」时误判为 `unchanged`，导致 `decide` 用过期 parent 提交、被 412
    /// 拒绝、再拉取仍误判——死循环无出口。
    pub fn classify(&self, base: &BaseState) -> RemoteClass {
        match (base, self) {
            (_, RemoteState::Absent) => RemoteClass::Absent,
            (_, RemoteState::Tombstoned { .. }) => RemoteClass::Tombstoned,
            (BaseState::Absent, RemoteState::Present { .. }) => RemoteClass::Present,
            (
                BaseState::Present {
                    version_id: base_version,
                    ..
                },
                RemoteState::Present { version_id, .. },
            ) => {
                if version_id == base_version {
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

    // --- 原始形状 as_str（只有 BaseState 有；LocalState/RemoteState 见其
    // impl 块顶部 doc comment——同名同签名的 as_str 会吐出非法的 trace 取值，
    // 已删除，分类词汇由 classify 独占）--------------------------------------

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

    // --- 分类词汇：字面量核对 FORMAT.md §10.3 的取值（不用表达式算出来再比，
    // 否则改了 as_str 实现本身，测试也跟着改，等于自证）---------------------

    /// `LocalClass::as_str()` 的每个变体逐字对照 `FORMAT.md` §10.3 `local` 字段
    /// 的合法取值。硬编码字符串——回归防线是防止有人把 `"added"` 悄悄改成
    /// `"new"` 之类，而所有基于「表达式算出来再比」的测试都不会发现。
    #[test]
    fn local_class_as_str_逐字匹配_format_md() {
        assert_eq!(LocalClass::Absent.as_str(), "absent");
        assert_eq!(LocalClass::Added.as_str(), "added");
        assert_eq!(LocalClass::Unchanged.as_str(), "unchanged");
        assert_eq!(LocalClass::Modified.as_str(), "modified");
    }

    /// 同上，`RemoteClass`。
    #[test]
    fn remote_class_as_str_逐字匹配_format_md() {
        assert_eq!(RemoteClass::Absent.as_str(), "absent");
        assert_eq!(RemoteClass::Present.as_str(), "present");
        assert_eq!(RemoteClass::Unchanged.as_str(), "unchanged");
        assert_eq!(RemoteClass::Modified.as_str(), "modified");
        assert_eq!(RemoteClass::Tombstoned.as_str(), "tombstoned");
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
    fn remote_classify_基线存在时按版本号比较_不是哈希() {
        let base = BaseState::Present {
            item_id: item_id(1),
            version_id: version_id(1),
            hash: hash(b"base"),
            size: 4,
        };
        assert_eq!(RemoteState::Absent.classify(&base), RemoteClass::Absent);
        // 版本号相同 → unchanged（哈希理应也相同，version_id 一旦提交即不可变）。
        assert_eq!(
            RemoteState::Present {
                item_id: item_id(1),
                version_id: version_id(1),
                hash: hash(b"base"),
                size: 4,
            }
            .classify(&base),
            RemoteClass::Unchanged
        );
        // 版本号不同、哈希也不同：正常的远端修改。
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
        // 回归测试：死循环的根源——同一份内容被重新上传产生了新版本号，
        // 哈希与基线一致，但 version_id 不同。按哈希判断会误判成 unchanged；
        // 必须判定为 modified，才能让 decide 走到「版本推进但内容未变」的
        // 零传输认领分支，而不是拿着过期 parent 反复提交被拒。
        assert_eq!(
            RemoteState::Present {
                item_id: item_id(1),
                version_id: version_id(2),
                hash: hash(b"base"),
                size: 4,
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
