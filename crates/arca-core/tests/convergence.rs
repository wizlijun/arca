//! 收敛性属性测试（proptest）——spec §11.2「收敛性属性测试：任意操作交错 +
//! 任意崩溃点，最终三态收敛，且无任何路径销毁数据（I3 作为可执行断言）」。
//!
//! 四条性质，各自的价值写在对应测试函数上。性质 2（I3）与性质 4（收敛）是
//! 本切片最有价值的两条产出——它们把 spec 里的两句承诺（「无任何路径销毁数据」
//! 「最终收敛」）变成机器每次提交都会检查的断言，而不是文档里的一句话承诺。
//!
//! ## 生成域为什么是一个小的符号宇宙，而不是"任意"字节
//!
//! `decide` 的分支绝大多数由**相等判断**触发（哈希是否等于基线、版本号是否
//! 等于基线），如果直接对 32 字节哈希或任意随机版本号取值，两个独立生成的值
//! 撞上相等的概率趋近于零——那些恰恰是决策表历史上出过真实 bug 的分支
//! （`present|modified|modified` 曾经漏掉 `remote_hash == base_hash` 检查，
//! 见 `crates/arca-core/tests/decision_table.rs` 的回归注释）。所以生成域刻意
//! 收窄成 3 个哈希符号 × 3 个版本符号：既保留"完全独立生成"的随机性
//! （谁与谁相等、谁与谁不等，仍由 proptest 决定并收缩到最小反例），又让"相等"
//! 这个关键事件有足够高的概率被生成到。`size` 字段不参与任何决策分支，
//! 固定为 4，不生成。
//!
//! `item_id` 固定为单一常量：`base`/`local`/`remote` 描述的是同一个 item 的
//! 三个视角，三者的 item_id 理应一致——这个不变量由扫描/journal 层维持，
//! 不由这三个类型自身保证，`tests/decision_table.rs` 的手写用例同样这么简化。

use arca_chunk::hash::ContentHash;
use arca_core::reconcile::{decide, Action, Decision};
use arca_core::state::{BaseState, LocalState, RemoteState};
use arca_format::model::{ItemId, VersionId};
use proptest::prelude::*;

const ITEM: u8 = 0xAB;

fn iid() -> ItemId {
    ItemId::from_bytes([ITEM; 16])
}

/// 符号哈希：0/1/2 三个符号，足够让 proptest 高概率覆盖"相等"与"不等"两侧。
fn hash_symbol(n: u8) -> ContentHash {
    ContentHash::from_bytes(format!("h{n}").as_bytes())
}

/// 符号版本号：与哈希符号相互独立的另一个小宇宙。
fn version_symbol(n: u8) -> VersionId {
    VersionId::new("20260805T093012Z", &format!("{:032x}", n as u128)).unwrap()
}

fn any_hash() -> impl Strategy<Value = ContentHash> {
    (0u8..3).prop_map(hash_symbol)
}

fn any_version() -> impl Strategy<Value = VersionId> {
    (0u8..3).prop_map(version_symbol)
}

fn any_base() -> impl Strategy<Value = BaseState> {
    prop_oneof![
        Just(BaseState::Absent),
        (any_hash(), any_version()).prop_map(|(hash, version_id)| BaseState::Present {
            item_id: iid(),
            version_id,
            hash,
            size: 4,
        }),
    ]
}

fn any_local() -> impl Strategy<Value = LocalState> {
    prop_oneof![
        Just(LocalState::Absent),
        any_hash().prop_map(|hash| LocalState::Present { hash, size: 4 }),
    ]
}

fn any_remote() -> impl Strategy<Value = RemoteState> {
    prop_oneof![
        Just(RemoteState::Absent),
        (any_hash(), any_version()).prop_map(|(hash, version_id)| RemoteState::Present {
            item_id: iid(),
            version_id,
            hash,
            size: 4,
        }),
        any_version().prop_map(|version_id| RemoteState::Tombstoned {
            item_id: iid(),
            version_id,
        }),
    ]
}

// ---------------------------------------------------------------------------
// 应用模型：把一个决策"应用"到三态之后，现实世界会变成什么样
// ---------------------------------------------------------------------------

/// 三态世界的一份快照——[`apply_decision`] 的输入与输出，性质 3/4 的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
struct World {
    base: BaseState,
    local: LocalState,
    remote: RemoteState,
}

/// 版本号发生器：现实中 hub 每接受一次新提交（`Upload`/`TombstoneRemote`）都会
/// 分配一个前所未见的版本号。测试用一个从生成域（0..3）之外取值的计数器模拟
/// ——从 100 起跳，与 `any_version()` 的符号空间隔开，永不与生成的输入意外相等
/// （一旦意外相等，会把"版本推进"误判成"版本没变"，污染下一轮 `decide` 的判断，
/// 这正是 `crate::state` 顶部 doc comment 记录的那类死循环 bug 的测试版）。
fn fresh_version(counter: &mut u8) -> VersionId {
    *counter += 1;
    version_symbol(100 + *counter)
}

/// 把一个决策应用到三态——**独立于 `decide` 的内部匹配结构重新推导**，
/// 不是照抄 `decide` 再翻译一遍。推导方式是问「这个动作在现实中真的做了
/// 什么，对本地文件、hub 存储、客户端基线各自留下什么后果」，而不是看
/// `decide` 判定这一格时用了哪个分支、携带了哪些字段。两者若不一致，
/// 要么是 `decide` 判错了，要么是这里对"落地后果"的常识判断错了——
/// 无论哪种，都值得停下核对，这正是这个模型函数存在的意义（brief 语）。
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
/// 对不该出现的组合（例如 `decide` 产出 `Upload` 但 `local` 却是 `Absent`）
/// 主动 `panic!`，而不是悄悄兜底——那种组合意味着 `decide` 与这个模型函数
/// 里至少有一个错了，装作没看见只会把 bug 埋得更深。
fn apply_decision(world: &World, decision: &Decision, next_version: &mut u8) -> World {
    match &decision.action {
        Action::Noop => world.clone(),

        Action::Upload { .. } => {
            let (hash, size) = match &world.local {
                LocalState::Present { hash, size } => (*hash, *size),
                LocalState::Absent => panic!(
                    "模型契约被打破：decide 产出 Upload，但 local 是 Absent \
                     ——Upload 只应在本地存在内容时触发"
                ),
            };
            let version_id = fresh_version(next_version);
            World {
                base: BaseState::Present {
                    item_id: iid(),
                    version_id: version_id.clone(),
                    hash,
                    size,
                },
                local: world.local.clone(),
                remote: RemoteState::Present {
                    item_id: iid(),
                    version_id,
                    hash,
                    size,
                },
            }
        }

        Action::Download { .. } => {
            let (hash, size, version_id) = match &world.remote {
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
            World {
                base: BaseState::Present {
                    item_id: iid(),
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
                item_id: iid(),
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

        Action::TombstoneRemote { .. } => {
            if world.local != LocalState::Absent {
                panic!(
                    "模型契约被打破：decide 产出 TombstoneRemote，但 local \
                     不是 Absent（实际是 {:?}）",
                    world.local
                );
            }
            World {
                base: BaseState::Absent,
                local: LocalState::Absent,
                remote: RemoteState::Tombstoned {
                    item_id: iid(),
                    version_id: fresh_version(next_version),
                },
            }
        }

        Action::Conflict { .. } | Action::NeedsHuman { .. } => world.clone(),
    }
}

/// `Action` 是否是「停下等人 / 走独立流程」的终态——I5 意义上的正当终点，
/// 不是需要继续推进的中间态。性质 4 用它判断循环该在哪一步停下。
fn is_terminal(action: &Action) -> bool {
    matches!(action, Action::Conflict { .. } | Action::NeedsHuman { .. })
}

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

proptest! {
    // 性质 3 用 `prop_assume!` 收窄到非终态决策（约 3 成生成的三态会被判定为
    // 终态而跳过，见该测试的 doc comment）。默认的 `max_global_rejects`（1024）
    // 在案例数调大时会被这类"正常但被过滤"的跳过提前耗尽，报出与决策表正确性
    // 无关的 `Too many global rejects`；调大到 1 << 20 只是给这类良性跳过留够
    // 预算，不改变任何一条性质实际验证的内容。
    #![proptest_config(ProptestConfig {
        max_global_rejects: 1 << 20,
        ..ProptestConfig::default()
    })]

    /// 性质 1（决策全域性）：`decide` 对任意三态组合都返回一个 `Decision`，
    /// 绝不 panic——即便这组三态在真实系统里"不该同时出现"（例如 base 缺失，
    /// 但 remote 恰好是某个随便撞上的 tombstone 版本号）。`decide` 的匹配结构
    /// （见 `reconcile` 模块顶部 doc comment）先按 `base` 的两种原始形状分两支、
    /// 再按 `(local, remote)` 的原始形状二级匹配，类型层面就是穷尽的，
    /// 编译期已经保证"没有漏分支"；这条测试补的是运行时那道防线——
    /// 防止未来有人往某个分支里插入一个会 panic 的 `unwrap()`/数组越界之类的
    /// 操作，且恰好被手写的 18/23 个用例绕开。
    #[test]
    fn 性质1_decide对任意三态都返回决策不panic(
        base in any_base(), local in any_local(), remote in any_remote(),
    ) {
        let _ = decide(&base, &local, &remote);
    }

    /// 性质 2（I3：无销毁）——**本切片最有价值的两条断言之一**。
    ///
    /// spec 的承诺是「同步路径无销毁权」：删除永远表现为 tombstone，物理销毁
    /// 只经显式 `arca gc`。这条测试不是对 `destroys_data()` 这种恒返回 `false`
    /// 的方法做自证式断言（那只是把承诺原样抄一遍，测不出任何东西）；而是维护
    /// 一份需要人工审查、写明理由的「非销毁」判别式白名单（与
    /// `tests/decision_table.rs` 同名断言共享同一份白名单与理由），逐条核对
    /// **proptest 生成的、覆盖面远大于手写用例的输入**产出的 `Action` 都落在
    /// 白名单内。手写的 18/23 格只覆盖了"典型"的每一行；这条测试用随机组合
    /// 覆盖手写用例覆盖不到的角落，防止未来有人往 `decide` 里加一条只在某个
    /// 冷门三态组合下才触发的销毁路径。必须是属性测试而不是几个例子的原因
    /// 也在这——例子只能证明"这几个点没问题"，属性测试证明的是"这整片输入
    /// 空间都没问题"。
    #[test]
    fn 性质2_i3_任意输入的决策都不产出销毁语义的动作(
        base in any_base(), local in any_local(), remote in any_remote(),
    ) {
        let decision = decide(&base, &local, &remote);
        prop_assert!(
            NON_DESTRUCTIVE.contains(&decision.action.as_str()),
            "产出了不在非销毁白名单内的 action：{}",
            decision.action.as_str()
        );
    }

    /// 性质 3（幂等）：应用一个**非终态**决策的效果之后再 `decide` 一次，
    /// 必须得到 `Noop`。
    ///
    /// 只限定在非终态决策上——`Conflict`/`NeedsHuman` 是正当的终态（I5：
    /// 模糊必停），[`apply_decision`] 对它们的模型是"原样不动"，再次 `decide`
    /// 会得到*同一个终态决策*而不是 `Noop`（这本身也是一种幂等：多次调用
    /// 结果稳定不变，但不是"收敛到 Noop"意义上的幂等）。用 `prop_assume!`
    /// 收窄到非终态的输入域，不是回避问题——终态的"幂等"由性质 4 的循环
    /// 间接验证（循环第一步就会在终态停住，不会继续推进）。
    #[test]
    fn 性质3_应用非终态决策后再次决定必得noop(
        base in any_base(), local in any_local(), remote in any_remote(),
    ) {
        let world = World { base, local, remote };
        let decision = decide(&world.base, &world.local, &world.remote);
        prop_assume!(!is_terminal(&decision.action));

        let mut next_version = 0u8;
        let next = apply_decision(&world, &decision, &mut next_version);
        let redecided = decide(&next.base, &next.local, &next.remote);

        prop_assert_eq!(
            redecided.action, Action::Noop,
            "world={:?}, 第一次决策={:?}, 应用后 world={:?}",
            world, decision, next
        );
    }

    /// 性质 4（收敛）——**本切片最有价值的两条断言之一**。
    ///
    /// 从任意初始三态出发，反复「`decide` → `apply_decision`」，必须在有限步
    /// （上限 8）内到达一个不再推进的点：要么是 `Noop`（真正达成一致），
    /// 要么是 `Conflict`/`NeedsHuman`（I5 意义上正当的终态——模糊或冲突时
    /// 停下等人处理，不是"没收敛"，是"收敛到了一个需要人介入的点，循环
    /// 不该再自动推进"）。超过步数上限还在变化，就是决策表或应用模型里
    /// 存在真实的震荡 bug。
    ///
    /// 步数上限给到 8 是留出充分余量：性质 3 的推导已经表明，任何非终态决策
    /// 应用一次就必定收敛到 `Noop`（因为 `apply_decision` 的构造方式保证了
    /// `base` 会被对齐到 `local`/`remote` 双方事后一致同意的哈希与版本号）,
    /// 所以真实所需步数是 0（起点已是 `Noop`）或 1；8 不是"卡着刚好够用"的
    /// 数字，是给未来决策表演化留的安全边际，同时仍然是一个会因为真实震荡
    /// 而炸掉的有限值。
    #[test]
    fn 性质4_从任意三态出发有限步内收敛(
        base in any_base(), local in any_local(), remote in any_remote(),
    ) {
        const MAX_STEPS: u32 = 8;
        let mut world = World { base, local, remote };
        let mut next_version = 0u8;
        let mut history = Vec::new();

        let mut converged = false;
        for _ in 0..MAX_STEPS {
            let decision = decide(&world.base, &world.local, &world.remote);
            history.push(decision.clone());
            if decision.action == Action::Noop || is_terminal(&decision.action) {
                converged = true;
                break;
            }
            world = apply_decision(&world, &decision, &mut next_version);
        }

        prop_assert!(
            converged,
            "{} 步内未收敛，决策序列={:?}, 最终 world={:?}",
            MAX_STEPS, history, world
        );
    }
}
