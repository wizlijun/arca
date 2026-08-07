//! 收敛性属性测试（proptest）——spec §11.2「收敛性属性测试：任意操作交错 +
//! 任意崩溃点，最终三态收敛，且无任何路径销毁数据（I3 作为可执行断言）」。
//!
//! 四条性质，各自的价值写在对应测试函数上。性质 2（I3）与性质 4（收敛）是
//! 本切片最有价值的两条产出——它们把 spec 里的两句承诺（「无任何路径销毁数据」
//! 「最终收敛」）变成机器每次提交都会检查的断言，而不是文档里的一句话承诺。
//!
//! `World`/`apply_decision`/`is_terminal`/`NON_DESTRUCTIVE` 定义在
//! `tests/common/mod.rs`，与 `tests/simulation.rs` 共用——两处都需要"决策应用到
//! 现实世界会发生什么"这同一份推导，理由见该文件顶部 doc comment。
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

mod common;

use arca_core::reconcile::{decide, Action};
use arca_core::state::{BaseState, LocalState, RemoteState};
use arca_format::model::ItemId;
use common::{apply_decision, hash_symbol, is_terminal, version_symbol, World, NON_DESTRUCTIVE};
use proptest::prelude::*;

const ITEM: u8 = 0xAB;

fn iid() -> ItemId {
    ItemId::from_bytes([ITEM; 16])
}

fn any_hash() -> impl Strategy<Value = arca_chunk::hash::ContentHash> {
    (0u8..3).prop_map(hash_symbol)
}

fn any_version() -> impl Strategy<Value = arca_format::model::VersionId> {
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
    /// 一份需要人工审查、写明理由的「非销毁」判别式白名单（`tests/common/mod.rs`
    /// 的 `NON_DESTRUCTIVE`，与 `tests/decision_table.rs` 同名断言共享同一份
    /// 理由），逐条核对**proptest 生成的、覆盖面远大于手写用例的输入**产出的
    /// `Action` 都落在白名单内。手写的 18/23 格只覆盖了"典型"的每一行；这条
    /// 测试用随机组合覆盖手写用例覆盖不到的角落，防止未来有人往 `decide` 里加
    /// 一条只在某个冷门三态组合下才触发的销毁路径。必须是属性测试而不是几个
    /// 例子的原因也在这——例子只能证明"这几个点没问题"，属性测试证明的是
    /// "这整片输入空间都没问题"。
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
    /// 模糊必停），`apply_decision` 对它们的模型是"原样不动"，再次 `decide`
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
        let next = apply_decision(&world, &decision, iid(), &mut next_version);
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
            world = apply_decision(&world, &decision, iid(), &mut next_version);
        }

        prop_assert!(
            converged,
            "{} 步内未收敛，决策序列={:?}, 最终 world={:?}",
            MAX_STEPS, history, world
        );
    }
}
