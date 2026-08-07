//! 测试专用公共小工具：三态世界快照 + 决策应用模型 + 符号哈希/版本号。
//!
//! 被 `tests/convergence.rs`（proptest 性质测试）与 `tests/simulation.rs`
//! （确定性模拟测试）共用——「决策应用到现实世界会发生什么」只值得推导一次，
//! 拆成两份测试文件各写一遍只会让两份悄悄长歪、互相不一致却没人发现。
//! 不是 `arca-core` 的公开 API，只在 `tests/` 内经 `mod common;` 可见。
//!
//! `NON_DESTRUCTIVE` 与 `apply_decision` 的推导理由见各自定义处的 doc comment；
//! 两份调用方（`convergence.rs` 性质 2/3/4、`simulation.rs` 的 I3 断言与
//! 崩溃注入）在各自文件里说明"为什么在这里用"，不在本文件重复。

#![allow(dead_code)] // 两个调用方各自只用到其中一部分

use arca_chunk::hash::ContentHash;
use arca_core::reconcile::{Action, Decision};
use arca_core::state::{BaseState, LocalState, RemoteState};
use arca_format::model::{ItemId, VersionId};

/// 符号哈希：内容以小整数命名，让"两处生成的哈希恰好相等"这种关键分支
/// （决策表里几乎每一条判定都靠哈希/版本号相等与否分叉）有足够高的概率出现，
/// 而不是像对 32 字节真随机哈希取值那样，两个独立生成的值几乎永远不相等。
pub fn hash_symbol(n: u8) -> ContentHash {
    ContentHash::from_bytes(format!("h{n}").as_bytes())
}

/// 符号版本号：与哈希符号相互独立的另一个小宇宙。
///
/// `n` 取 `u32`（不是 `u8`）：`fresh_version` 从 100 起跳累加，模拟里
/// `simulate(seed, 80, 24, 3)` 的理论上界是 80 + 3×24 = 152，`100 + 152 = 252`
/// 尚未溢出 `u8`，但压力跑（3000 种子）与后续调大步数的场景会真的顶到
/// `u8::MAX`——debug 下 panic，**release 下静默回绕成一个已经用过的版本号**，
/// 把"版本推进"误判成"版本没变"，这正是 `crate::state` 顶部记录的死循环
/// 隐患的测试版。`u32` 把这条余量从个位数拉到 40 亿级，实践中不会再顶到。
pub fn version_symbol(n: u32) -> VersionId {
    VersionId::new("20260805T093012Z", &format!("{:032x}", n as u128)).unwrap()
}

/// `Action::as_str()` 的非销毁白名单（I3 的可执行断言依据）。
///
/// - `noop`/`upload`/`download`/`adopt_baseline`：显然不涉及销毁；
/// - `delete_local`：只移除本地副本，权威副本仍在 hub trash 保留期内；
/// - `tombstone_remote`：写入的是墓碑记录，不是删除数据；
/// - `conflict`/`needs_human`：都是"停下，不擅自处置"。
///
/// 若将来有人给 `Action` 加一个真正销毁数据的变体，这份白名单不会自动放行它
/// ——除非有人把它加进来并在这里说明为什么安全，这正是这条检查要逼出来的
/// 审查动作（不是 `destroys_data()` 恒返回 `false` 那种自证式断言）。
pub const NON_DESTRUCTIVE: [&str; 8] = [
    "noop",
    "upload",
    "download",
    "adopt_baseline",
    "delete_local",
    "tombstone_remote",
    "conflict",
    "needs_human",
];

/// 三态世界的一份快照——某一个 item 当前的 `base`/`local`/`remote`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct World {
    pub base: BaseState,
    pub local: LocalState,
    pub remote: RemoteState,
}

/// `Action` 是否是"停下等人 / 走独立流程"的终态（I5 意义上正当的终点，
/// 不是还需要继续推进的中间态）。
pub fn is_terminal(action: &Action) -> bool {
    matches!(action, Action::Conflict { .. } | Action::NeedsHuman { .. })
}

/// 把一个决策应用到三态——**独立于 `decide` 的内部匹配结构重新推导**，
/// 不是照抄 `decide` 再翻译一遍。推导方式是问「这个动作在现实中真的做了
/// 什么，对本地文件、hub 存储、客户端基线各自留下什么后果」，而不是看
/// `decide` 判定这一格时用了哪个分支、携带了哪些字段。两者若不一致，
/// 要么是 `decide` 判错了，要么是这里对"落地后果"的常识判断错了——
/// 无论哪种，都值得停下核对，这正是这个模型函数存在的意义。
///
/// 各分支的现实推导：
/// - [`Action::Noop`]：无事发生，三态原样不变。
/// - [`Action::Upload`]：本地内容被推给 hub，hub 接受后分配一个新版本号；
///   本地内容本身不受影响；客户端基线现在应记录"双方都确认过的是这份内容、
///   这个新版本号"——`base` 对齐到 `(新版本号, local.hash)`，`remote` 也
///   对齐到同一份内容与版本号（hub 权威状态已更新）。
/// - [`Action::Download`]：把 hub 上这个版本的内容拉下来覆盖本地；`base` 与
///   `remote` 现在都指向这份内容——本地内容换成 `remote.hash`，`base` 对齐到
///   `(remote.version_id, remote.hash)`。
/// - [`Action::AdoptBaseline`]：**零传输**——本地文件和 hub 上的内容都不动
///   一个字节，动的只是客户端自己的记账：`base` 直接对齐到 `Action` 携带的
///   `(hash, version_id)`，`local`/`remote` 原样不变。
/// - [`Action::DeleteLocal`]：本地副本被移除；hub 那边本来就已经是
///   tombstone，本地跟上之后，这个 item 在客户端视角里已经没有"双方都确认过
///   的版本"需要追踪——基线记录清空（`base` 归 `Absent`）。
/// - [`Action::TombstoneRemote`]：本地的删除意图被提交为 hub 上一条新的
///   tombstone 记录（新版本号）；本地早已是 `Absent`（这个动作只在本地已删除
///   时触发，不是这里才让它变成 Absent）；基线同样清空，理由同 `DeleteLocal`。
/// - [`Action::Conflict`] / [`Action::NeedsHuman`]：**停在这里，不推进**
///   （I5：模糊必停；冲突走独立的落地流程，不在这个循环里自动前进）。
///   三态原样不变——这不是"什么都没发生"（那是 `Noop` 的含义），而是
///   "决策本身就是别再自动推进"。
///
/// `item_id` 由调用方给定，不是这里现造——真实系统里一个新 item 的身份
/// 由客户端在发起上传*之前*分配（spec I7：创建时分配，永不复用），
/// `decide` 本身不携带它（`Action::Upload`/`Download`/`AdoptBaseline` 都没有
/// `item_id` 字段，只有已经处于终态的 `DeleteLocal`/`TombstoneRemote`/
/// `Conflict`/`NeedsHuman` 才带）。
///
/// 对不该出现的组合（例如 `decide` 产出 `Upload` 但 `local` 却是 `Absent`）
/// 主动 `panic!`，而不是悄悄兜底——那种组合意味着 `decide` 与这个模型函数
/// 里至少有一个错了，装作没看见只会把 bug 埋得更深。
pub fn apply_decision(
    world: &World,
    decision: &Decision,
    item_id: ItemId,
    next_version: &mut u32,
) -> World {
    match &decision.action {
        Action::Noop => world.clone(),

        Action::Upload { parent } => {
            let (hash, size) = match &world.local {
                LocalState::Present { hash, size } => (*hash, *size),
                LocalState::Absent => panic!(
                    "模型契约被打破：decide 产出 Upload，但 local 是 Absent \
                     ——Upload 只应在本地存在内容时触发"
                ),
            };
            // 契约守卫（本切片的头号修复）：CAS 的 parent 必须取「远端当前
            // 版本」，绝不能是基线版本——`parent == None` 当且仅当 remote
            // 完全不认识这个 item（Absent 或 Tombstoned，仅创建没有父版本可
            // 比对）；否则 `parent` 必须精确等于 `remote` 当前的 version_id。
            // 这条断言就是「携带的版本号一律取远端当前版本、绝不取基线版本」
            // 这句话的可执行形态——`reconcile.rs` 一旦把某个 `Some(remote_version)`
            // 悄悄改回 `Some(base_version)`，这里必须炸。
            match (&world.remote, parent) {
                (RemoteState::Absent, None) | (RemoteState::Tombstoned { .. }, None) => {}
                (
                    RemoteState::Present {
                        version_id: remote_version,
                        ..
                    },
                    Some(p),
                ) if p == remote_version => {}
                _ => panic!(
                    "模型契约被打破：Upload 的 parent 必须是 None（remote 为 \
                     Absent/Tombstoned，仅创建）或 Some(remote 当前 version_id)\
                     ——实际 parent={parent:?}，remote={:?}",
                    world.remote
                ),
            }
            let version_id = fresh_version(next_version);
            World {
                base: BaseState::Present {
                    item_id,
                    version_id: version_id.clone(),
                    hash,
                    size,
                },
                local: world.local.clone(),
                remote: RemoteState::Present {
                    item_id,
                    version_id,
                    hash,
                    size,
                },
            }
        }

        Action::Download { version_id } => {
            let (hash, size, remote_version) = match &world.remote {
                RemoteState::Present {
                    hash,
                    size,
                    version_id,
                    ..
                } => (*hash, *size, version_id.clone()),
                other => panic!(
                    "模型契约被打破：decide 产出 Download，但 remote 不是 \
                     Present（实际是 {other:?}）"
                ),
            };
            // 契约守卫：Download 携带的 version_id 必须是 remote 当前版本
            // ——下载的内容永远是"新鲜的远端状态"，不是基线或别的什么版本。
            assert_eq!(
                version_id, &remote_version,
                "模型契约被打破：Download 的 version_id 必须等于 remote 当前 \
                 版本——实际 action.version_id={version_id:?}，\
                 remote.version_id={remote_version:?}"
            );
            let version_id = remote_version;
            World {
                base: BaseState::Present {
                    item_id,
                    version_id,
                    hash,
                    size,
                },
                local: LocalState::Present { hash, size },
                remote: world.remote.clone(),
            }
        }

        Action::AdoptBaseline { hash, version_id } => World {
            base: BaseState::Present {
                item_id,
                version_id: version_id.clone(),
                hash: *hash,
                size: 4,
            },
            local: world.local.clone(),
            remote: world.remote.clone(),
        },

        Action::DeleteLocal { .. } => World {
            base: BaseState::Absent,
            local: LocalState::Absent,
            remote: world.remote.clone(),
        },

        Action::TombstoneRemote { parent, .. } => {
            if world.local != LocalState::Absent {
                panic!(
                    "模型契约被打破：decide 产出 TombstoneRemote，但 local \
                     不是 Absent（实际是 {:?}）",
                    world.local
                );
            }
            // 契约守卫：同 Upload，parent 是 CAS 的 If-Match（I4），必须取
            // 远端当前版本——决策表里 TombstoneRemote 只在 remote 为 Present
            // 时产出（local_deleted），parent 必须精确等于它的 version_id。
            match &world.remote {
                RemoteState::Present {
                    version_id: remote_version,
                    ..
                } if parent == remote_version => {}
                _ => panic!(
                    "模型契约被打破：TombstoneRemote 的 parent 必须等于 remote \
                     当前 version_id（remote 应为 Present）——实际 parent={parent:?}，\
                     remote={:?}",
                    world.remote
                ),
            }
            World {
                base: BaseState::Absent,
                local: LocalState::Absent,
                remote: RemoteState::Tombstoned {
                    item_id,
                    version_id: fresh_version(next_version),
                },
            }
        }

        Action::Conflict { .. } | Action::NeedsHuman { .. } => world.clone(),
    }
}

/// 版本号发生器：现实中 hub 每接受一次新提交（`Upload`/`TombstoneRemote`）都会
/// 分配一个前所未见的版本号。测试用一个从符号版本号空间（通常是 0..3 一类
/// 小范围）之外取值的计数器模拟——从 100 起跳，永不与调用方生成的输入意外
/// 相等（一旦意外相等，会把"版本推进"误判成"版本没变"，污染下一轮 `decide`
/// 的判断，这正是 `arca_core::state` 顶部 doc comment 记录的那类死循环 bug
/// 的测试版）。
pub fn fresh_version(counter: &mut u32) -> VersionId {
    *counter += 1;
    version_symbol(100 + *counter)
}
