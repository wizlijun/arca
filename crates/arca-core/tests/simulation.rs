//! 确定性模拟测试——spec §11.2「确定性模拟测试：sans-io 状态机 + 模拟时钟/
//! 网络/文件系统，随机事件序列 + 崩溃注入 + 种子可复现——Dropbox Nucleus 的
//! 核心教训」。
//!
//! 与 `tests/convergence.rs` 的 proptest 性质测试互补：性质测试对**单个**三态
//! 组合做穷举式的随机抽样；这里模拟的是**一串随时间推移、彼此交错的事件**
//! （本地改动 / 远端改动 / 本地删除 / 远端 tombstone / 调和尝试，调和尝试还有
//! 一定概率"崩溃"），更接近真实系统里 bug 出现的方式——单次调和永远正确，
//! 不代表一长串历史事件之后仍然正确。
//!
//! `World`/`apply_decision`/`is_terminal`/`NON_DESTRUCTIVE` 定义在
//! `tests/common/mod.rs`，与 `tests/convergence.rs` 共用（理由见该文件顶部）。

mod common;

use arca_chunk::hash::ContentHash;
use arca_core::reconcile::{decide, decide_traced, Action, Decision};
use arca_core::state::{BaseState, LocalState, RemoteState};
use arca_format::model::ItemId;
use arca_format::trace::{FieldValue, TraceRecord, VecSink};
use common::{
    apply_decision, fresh_version, hash_symbol, is_terminal, version_symbol, World, NON_DESTRUCTIVE,
};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// 模拟时钟：单调递增的 u64 微秒计数器。
//
// sans-io 约束下"时钟"就是这个——`decide_traced` 的 `t_abs_us` 由调用方注入，
// 函数内绝不读系统时钟，模拟测试才能逐字节复现 trace（spec §11.2）。
// ---------------------------------------------------------------------------

struct SimClock(u64);

impl SimClock {
    fn new() -> Self {
        SimClock(0)
    }

    /// 前进至少 1 微秒并返回新的绝对时间——每个事件都让时钟往前走一点，
    /// 模拟真实世界里"两件事不会在同一微秒发生"，同时保证严格单调。
    fn tick(&mut self) -> u64 {
        self.0 += 1000;
        self.0
    }
}

// ---------------------------------------------------------------------------
// 种子驱动的确定性伪随机数生成器（splitmix64）。
//
// 不引入 `rand` crate：arca-core 本身零重依赖（Cargo.toml 的纪律），测试也
// 没必要为了"随机数"多带一整个 crate 进依赖树；splitmix64 是十几行、公开发表
// 过的标准算法，同一 seed 在任何机器、任何时候都产生同一序列——种子可复现
// 天然满足，不需要额外做什么。
// ---------------------------------------------------------------------------

struct SimRng(u64);

impl SimRng {
    fn new(seed: u64) -> Self {
        SimRng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[0, bound)` 均匀取值——`bound` 全程是个位数，`%` 引入的偏差可忽略。
    fn next_below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

// ---------------------------------------------------------------------------
// 模拟世界：固定的三条路径，每条各自维护一份三态
// ---------------------------------------------------------------------------

const PATHS: [&str; 3] = ["a.bin", "b.bin", "c.bin"];

fn item_id_for(path: &str) -> ItemId {
    let digest = ContentHash::from_bytes(path.as_bytes());
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&digest.as_bytes()[..16]);
    ItemId::from_bytes(buf)
}

/// 一次调和尝试落地时的崩溃结局——见 [`SimEvent::Reconcile`] 与
/// `attempt_reconcile`。两种崩溃模式对应 I9 真正在意的两类"进程消失"：
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashOutcome {
    /// 没崩溃：`apply_decision` 正常落地。
    None,
    /// **完全崩溃**：决策已产出并落进 trace，但整个应用被跳过，三态原封
    /// 不动——验证的是"重试安全"：重启后重新 `decide` 得到同一个决策。
    Full,
    /// **半边崩溃**：只应用 `Upload`/`TombstoneRemote` 的远端半边（hub 已
    /// 接受提交，`remote` 落了新版本），但客户端在写本地基线前死掉，`base`
    /// 保留旧值。这是 M1d/M2 里真实存在的窗口——持久侧（hub）落了，
    /// 可抛弃投影（客户端基线，I9）没落。只对 `Upload`/`TombstoneRemote`
    /// 有意义（只有这两个动作会让 hub 与客户端基线出现"半边更新"的分歧）。
    HalfRemoteOnly,
}

/// 一次模拟里发生的一个事件——本地改动 / 本地删除 / 远端改动 / 远端 tombstone /
/// 远端记录消失 / 调和尝试（`crash` 记录这次尝试的崩溃结局，见 [`CrashOutcome`]）。
/// 只存数据，不存派生量——同一份 `SimEvent` 序列配合同一个 seed 必须能重放出
/// 完全相同的后续状态。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SimEvent {
    LocalChange {
        path: &'static str,
        hash_symbol: u8,
    },
    LocalDelete {
        path: &'static str,
    },
    RemoteChange {
        path: &'static str,
        hash_symbol: u8,
        version_num: u32,
    },
    RemoteTombstone {
        path: &'static str,
        version_num: u32,
    },
    /// 远端记录凭空消失（journal 被截断 / 存储根被换掉）——不产出 tombstone，
    /// 直接把 `remote` 打回 `Absent`。这是让模拟能走到 `present|*|absent`
    /// 三格（`remote_vanished_without_tombstone`，I5 最贵的一次应用）唯一的
    /// 入口：`decide` 本身只会把 `remote` 从 `Present` 推进到 `Tombstoned`，
    /// 永不产出"退回 Absent"，这类状态只能靠外部事件注入。
    RemoteVanish {
        path: &'static str,
    },
    Reconcile {
        path: &'static str,
        crash: CrashOutcome,
    },
}

/// 一次调和尝试实际拿到的决策，连同产出它时的三态分类词汇——供「trace 不漏
/// 事件」的断言核对全部七个字段（`path`/`base`/`local`/`remote`/`action`/
/// `reason`/`item_id`），不只是 `path`/`action`/`reason` 三个。
struct RecordedDecision {
    path: &'static str,
    base: &'static str,
    local: &'static str,
    remote: &'static str,
    item_id: String,
    decision: Decision,
}

/// 一次完整模拟的产出：生成的事件序列、`decide_traced` 落下的完整 trace、
/// 每次调和实际拿到的决策（与 trace 逐条对应，供「trace 不漏事件」的断言用）、
/// 以及每条路径是否在结算阶段收敛。
struct SimRun {
    events: Vec<SimEvent>,
    trace: Vec<TraceRecord>,
    decisions: Vec<RecordedDecision>,
    converged: HashMap<&'static str, bool>,
}

/// 跑一次确定性模拟：`churn_steps` 步随机交错的本地/远端变更与调和尝试
/// （调和尝试里，非终态决策有概率"崩溃"——见 [`CrashOutcome`]：完全崩溃或
/// 只应用远端半边，决策已经产出并落进 trace，但应用被跳过/半跳过，模拟
/// "决策做完、还没来得及（完全）落地进程就没了"）；之后进入结算阶段，对每条
/// 路径反复调和（同样可能崩溃）直到收敛或用完 `settle_bound` 步预算。
///
/// 纯函数：只依赖 `seed` 与三个步数参数，不读任何外部状态（系统时钟、环境
/// 变量都不碰）——种子可复现的前提就是这个函数本身是确定性的。
fn simulate(seed: u64, churn_steps: u32, settle_bound: u32, crash_denom: u64) -> SimRun {
    let mut rng = SimRng::new(seed);
    let mut clock = SimClock::new();
    let mut sink = VecSink::new();
    let mut events = Vec::new();
    let mut decisions = Vec::new();
    let mut next_version = 0u32;

    let mut store: HashMap<&'static str, World> = PATHS
        .iter()
        .map(|&path| {
            (
                path,
                World {
                    base: BaseState::Absent,
                    local: LocalState::Absent,
                    remote: RemoteState::Absent,
                },
            )
        })
        .collect();

    // 阶段一：churn——随机交错的本地/远端变更 + 偶发崩溃的调和尝试。
    for _ in 0..churn_steps {
        let path = PATHS[rng.next_below(PATHS.len() as u64) as usize];
        let kind = rng.next_below(6);
        match kind {
            0 => {
                let h = rng.next_below(3) as u8;
                events.push(SimEvent::LocalChange {
                    path,
                    hash_symbol: h,
                });
                store.get_mut(path).unwrap().local = LocalState::Present {
                    hash: hash_symbol(h),
                    size: 4,
                };
            }
            1 => {
                events.push(SimEvent::LocalDelete { path });
                store.get_mut(path).unwrap().local = LocalState::Absent;
            }
            2 => {
                let h = rng.next_below(3) as u8;
                let version_id = fresh_version(&mut next_version);
                events.push(SimEvent::RemoteChange {
                    path,
                    hash_symbol: h,
                    version_num: next_version,
                });
                store.get_mut(path).unwrap().remote = RemoteState::Present {
                    item_id: item_id_for(path),
                    version_id,
                    hash: hash_symbol(h),
                    size: 4,
                };
            }
            3 => {
                let version_id = fresh_version(&mut next_version);
                events.push(SimEvent::RemoteTombstone {
                    path,
                    version_num: next_version,
                });
                store.get_mut(path).unwrap().remote = RemoteState::Tombstoned {
                    item_id: item_id_for(path),
                    version_id,
                };
            }
            4 => {
                // 远端记录凭空消失（journal 截断 / 存储根被换掉）——
                // 唯一能把 remote 打回 Absent 的事件，见 SimEvent::RemoteVanish
                // 的 doc comment：decide 本身绝不会产出这个方向的转移。
                events.push(SimEvent::RemoteVanish { path });
                store.get_mut(path).unwrap().remote = RemoteState::Absent;
            }
            _ => attempt_reconcile(
                path,
                &mut store,
                &mut sink,
                &mut clock,
                &mut rng,
                &mut next_version,
                crash_denom,
                &mut events,
                &mut decisions,
            ),
        }
    }

    // 阶段二：结算——不再产生新的外部变更，只反复调和到收敛，验证崩溃注入
    // 不妨碍最终收敛（每条路径独立计入 settle_bound 步预算）。
    let mut converged = HashMap::new();
    for &path in PATHS.iter() {
        let mut reached = false;
        for _ in 0..settle_bound {
            let decision = decide(&store[path].base, &store[path].local, &store[path].remote);
            if decision.action == Action::Noop || is_terminal(&decision.action) {
                reached = true;
                break;
            }
            attempt_reconcile(
                path,
                &mut store,
                &mut sink,
                &mut clock,
                &mut rng,
                &mut next_version,
                crash_denom,
                &mut events,
                &mut decisions,
            );
        }
        converged.insert(path, reached);
    }

    SimRun {
        events,
        trace: sink.records().to_vec(),
        decisions,
        converged,
    }
}

/// 半边崩溃的应用模型：只应用 [`Action::Upload`]/[`Action::TombstoneRemote`]
/// 的远端半边——`apply_decision` 算出的落地结果里，`remote`（hub 权威状态）
/// 照常对齐，但 `base`（客户端可抛弃投影，I9）保留旧值，模拟"hub 已接受
/// 提交，客户端在写基线前死掉"这段真实存在的窗口。除此之外与完全应用没有
/// 区别——`local` 的推导同样来自 `apply_decision`，这条函数不重新发明一遍
/// 落地逻辑，只是应用完之后把 `base` 字段换回崩溃前的旧值。
fn apply_remote_half(
    world: &World,
    decision: &Decision,
    item_id: ItemId,
    next_version: &mut u32,
) -> World {
    let fully_applied = apply_decision(world, decision, item_id, next_version);
    World {
        base: world.base.clone(),
        ..fully_applied
    }
}

/// 一次调和尝试：`decide_traced` 产出决策并落 trace；若是终态
/// （`Conflict`/`NeedsHuman`，I5：模糊必停）则不应用，也不掷崩溃骰子——终态
/// 本来就不该被"应用"。否则掷两次骰子决定崩溃结局（[`CrashOutcome`]）：
/// 先以 `1/crash_denom` 的概率完全崩溃（决策已产出但整个应用被跳过）；
/// 若没有，且动作是 `Upload`/`TombstoneRemote`，再以 `1/crash_denom` 的概率
/// 半边崩溃（只落远端，`base` 保留旧值，见 [`apply_remote_half`]）；
/// 否则正常应用。
#[allow(clippy::too_many_arguments)]
fn attempt_reconcile(
    path: &'static str,
    store: &mut HashMap<&'static str, World>,
    sink: &mut VecSink,
    clock: &mut SimClock,
    rng: &mut SimRng,
    next_version: &mut u32,
    crash_denom: u64,
    events: &mut Vec<SimEvent>,
    decisions: &mut Vec<RecordedDecision>,
) {
    let world = store[path].clone();
    let t = clock.tick();
    let decision = decide_traced(&world.base, &world.local, &world.remote, path, t, sink);
    decisions.push(RecordedDecision {
        path,
        base: world.base.as_str(),
        local: world.local.classify(&world.base).as_str(),
        remote: world.remote.classify(&world.base).as_str(),
        item_id: world
            .base
            .item_id()
            .or_else(|| world.remote.item_id())
            .map(|id| id.to_hex())
            .unwrap_or_default(),
        decision: decision.clone(),
    });

    if is_terminal(&decision.action) {
        events.push(SimEvent::Reconcile {
            path,
            crash: CrashOutcome::None,
        });
        return;
    }

    let full_crash = rng.next_below(crash_denom) == 0;
    let half_eligible = matches!(
        decision.action,
        Action::Upload { .. } | Action::TombstoneRemote { .. }
    );
    let crash = if full_crash {
        CrashOutcome::Full
    } else if half_eligible && rng.next_below(crash_denom) == 0 {
        CrashOutcome::HalfRemoteOnly
    } else {
        CrashOutcome::None
    };
    events.push(SimEvent::Reconcile { path, crash });

    match crash {
        CrashOutcome::None => {
            let applied = apply_decision(&world, &decision, item_id_for(path), next_version);
            store.insert(path, applied);
        }
        CrashOutcome::Full => {}
        CrashOutcome::HalfRemoteOnly => {
            let applied = apply_remote_half(&world, &decision, item_id_for(path), next_version);
            store.insert(path, applied);
        }
    }
}

fn field_str(record: &TraceRecord, key: &str) -> String {
    match record.field(key) {
        Some(FieldValue::Str(text)) => text.to_string(),
        other => panic!("字段 {key} 不是字符串：{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// 种子可复现：同一 seed 跑两次，事件序列与 trace 必须逐条相同。
///
/// 这是 spec §11.2「种子可复现」的字面要求——不是"大体相似"，是逐条相等。
/// 失败时把 seed 打进断言消息：模拟测试一旦在 CI 上失败，第一件要做的事就是
/// 用同一个 seed 在本地重跑，`assert_eq!` 的消息里没有 seed，调试就要先去翻
/// 测试代码找 seed 是多少，白白多一轮。
#[test]
fn 种子可复现_同一种子两次运行事件序列与trace逐条相同() {
    let seed = 0x5EED_C0DE_u64;
    let first = simulate(seed, 60, 20, 3);
    let second = simulate(seed, 60, 20, 3);

    assert_eq!(
        first.events, second.events,
        "seed={seed:#x}：同一种子两次运行的事件序列不一致"
    );
    assert_eq!(
        first.trace, second.trace,
        "seed={seed:#x}：同一种子两次运行的 trace 不一致"
    );
}

/// 性质（I3，跨整段模拟历史）：任意 churn + 崩溃注入 + 结算的全过程中，
/// `VecSink` 落下的每一条 `reconcile.decide` 记录都不产出销毁语义的动作。
///
/// 与 `tests/convergence.rs` 性质 2 的区别：那条测试检查的是"单个随机三态
/// 组合"，这里检查的是"一长串真实发生过的、由随机事件序列一步步演化出来的
/// 三态历史"——覆盖的是"多步交错之后才会出现的状态"，例如某条路径经历了
/// 本地改动、远端又被删除、还没来得及调和又被再次改动之后的三态，这类状态
/// proptest 的单步生成域覆盖不到（除非它恰好生成出同样的组合）。
#[test]
fn i3_模拟全程的每条trace都不产出销毁语义的动作() {
    for seed in 0..40u64 {
        let run = simulate(seed, 80, 24, 3);
        for record in &run.trace {
            let action = field_str(record, "action");
            assert!(
                NON_DESTRUCTIVE.contains(&action.as_str()),
                "seed={seed}：产出了不在非销毁白名单内的 action：{action}"
            );
        }
    }
}

/// trace 不能漏事件：`VecSink` 收到的 `reconcile.decide` 记录数必须与实际
/// 调用 `decide_traced` 的次数一一对应，且每条记录的全部七个字段
/// （`path`/`base`/`local`/`remote`/`action`/`reason`/`item_id`）与产出它的
/// 那次调和逐字相同——漏了一条，或某个字段悄悄不对，事故现场就少一条线索。
/// 只查 `path`/`action`/`reason` 三个字段曾经是本切片的覆盖缺口
/// （`tests/decision_table.rs` 对单步用例查了全部 7 个，模拟测试作为
/// 多步演化出的状态的补充，理应同一标准）。
#[test]
fn trace事件序列与实际决策一一对应() {
    let run = simulate(0x1234_5678, 60, 20, 3);

    assert_eq!(
        run.trace.len(),
        run.decisions.len(),
        "trace 记录数与 decide_traced 调用次数不一致——事件序列={:?}",
        run.events
    );

    for (record, recorded) in run.trace.iter().zip(run.decisions.iter()) {
        assert_eq!(record.event, arca_format::trace::EventKind::ReconcileDecide);
        assert_eq!(field_str(record, "path"), recorded.path);
        assert_eq!(
            field_str(record, "base"),
            recorded.base,
            "path={}",
            recorded.path
        );
        assert_eq!(
            field_str(record, "local"),
            recorded.local,
            "path={}",
            recorded.path
        );
        assert_eq!(
            field_str(record, "remote"),
            recorded.remote,
            "path={}",
            recorded.path
        );
        assert_eq!(
            field_str(record, "item_id"),
            recorded.item_id,
            "path={}",
            recorded.path
        );
        assert_eq!(
            field_str(record, "action"),
            recorded.decision.action.as_str()
        );
        assert_eq!(field_str(record, "reason"), recorded.decision.reason);
    }
}

/// 崩溃注入下仍在有限步内收敛：churn 阶段结束、不再有外部变更之后，即便
/// 结算阶段的每次调和仍有 1/3 概率崩溃（决策产出但跳过应用），每条路径也必须
/// 在 `settle_bound` 步预算内到达 `Noop` 或正当终态（`Conflict`/`NeedsHuman`，
/// I5）。这条测试验证的是"崩溃后重启重新 decide、再重试"这个模式本身是安全
/// 的——如果 `decide` 不是幂等的，或者应用模型有副作用会累积成震荡，崩溃反复
/// 打断重试会让某条路径永远到不了收敛，这条测试会因为 `converged` 里出现
/// `false` 而失败。
#[test]
fn 性质_崩溃注入下有限步内仍收敛() {
    for seed in 0..40u64 {
        let run = simulate(seed, 80, 24, 3);
        for &path in PATHS.iter() {
            assert!(
                run.converged[path],
                "seed={seed}, path={path}：结算阶段预算耗尽仍未收敛，事件序列={:?}",
                run.events
            );
        }
    }
}

/// 崩溃注入的最小可读案例——不依赖种子，直接摆一个具体场景，把"决策已产出但
/// 尚未应用时崩溃、重启后重新 decide"这句话字面地跑一遍：
/// 1. 本地新增一个文件，第一次 `decide_traced` 产出 `Upload`；
/// 2. "崩溃"——不调用 `apply_decision`，三态原封不动；
/// 3. "重启"后重新 `decide_traced`，必须得到与崩溃前逐字段相同的决策
///    （`decide` 是 sans-io 纯函数，同输入必同输出，这正是崩溃安全的根基：
///    重启不需要任何特殊的"恢复逻辑"，重新跑一次调和就自然得到正确答案）；
/// 4. 这一次真正应用，随后收敛到 `Noop`；
/// 5. 全程两条 trace 记录都不是销毁语义的动作。
#[test]
fn 崩溃发生在决策已产出但未应用时_重启后重新决定并最终收敛() {
    let world = World {
        base: BaseState::Absent,
        local: LocalState::Present {
            hash: hash_symbol(1),
            size: 4,
        },
        remote: RemoteState::Absent,
    };
    let mut sink = VecSink::new();
    let mut clock = SimClock::new();
    let item_id = item_id_for("crash-demo.bin");

    // 决策产出，随即"崩溃"：不应用，world 原封不动。
    let before_crash = decide_traced(
        &world.base,
        &world.local,
        &world.remote,
        "crash-demo.bin",
        clock.tick(),
        &mut sink,
    );
    assert_eq!(before_crash.action, Action::Upload { parent: None });

    // 重启：world 没变过，重新决定必须得到完全相同的决策。
    let after_restart = decide_traced(
        &world.base,
        &world.local,
        &world.remote,
        "crash-demo.bin",
        clock.tick(),
        &mut sink,
    );
    assert_eq!(
        after_restart, before_crash,
        "重启后重新 decide 必须产出与崩溃前相同的决策——decide 是 sans-io 纯函数"
    );

    // 这一次真正应用，收敛到 Noop。
    let mut next_version = 0u32;
    let applied = apply_decision(&world, &after_restart, item_id, &mut next_version);
    let settled = decide(&applied.base, &applied.local, &applied.remote);
    assert_eq!(settled.action, Action::Noop);

    // 全程两条 trace 记录都不是销毁语义的动作。
    for record in sink.records() {
        let action = field_str(record, "action");
        assert!(NON_DESTRUCTIVE.contains(&action.as_str()));
    }
}

/// 半边崩溃的最小可读案例——不依赖种子，字面地把 [`CrashOutcome::HalfRemoteOnly`]
/// 的 doc comment 描述的场景跑一遍："hub 已接受 Upload，客户端在写基线前死掉"：
/// 1. 本地把内容改成 X，`decide` 产出 `Upload{parent: Some(remote 当前版本)}`；
/// 2. 半边崩溃——`apply_remote_half`：`remote` 落到新版本、新哈希 X（hub 已接受），
///    但 `base` 保留旧值（客户端没来得及写基线）；
/// 3. 重启后重新 `decide`：这时 `base` 是旧的、`local` 仍是 X、`remote` 是
///    新版本 + 哈希 X——恰好落进 `present|modified|modified` 格、
///    `remote_hash != base_hash && local_hash == remote_hash` 这个子情形，
///    产出 `AdoptBaseline`/`converged_independently`（零传输认领，不会重新
///    上传一遍已经在 hub 上的内容）；
/// 4. 应用后收敛到 `Noop`。
#[test]
fn 崩溃只落远端半边_基线未更新_重启后仍收敛到零传输认领() {
    let base_hash = hash_symbol(0);
    let new_hash = hash_symbol(1);
    let base_version = version_symbol(0);
    let item_id = item_id_for("half-crash-demo.bin");

    let world = World {
        base: BaseState::Present {
            item_id,
            version_id: base_version.clone(),
            hash: base_hash,
            size: 4,
        },
        local: LocalState::Present {
            hash: new_hash,
            size: 4,
        },
        remote: RemoteState::Present {
            item_id,
            version_id: base_version,
            hash: base_hash,
            size: 4,
        },
    };

    let mut sink = VecSink::new();
    let mut clock = SimClock::new();
    let mut next_version = 200u32; // 与 base_version 的符号空间不重叠。

    let decision = decide_traced(
        &world.base,
        &world.local,
        &world.remote,
        "half-crash-demo.bin",
        clock.tick(),
        &mut sink,
    );
    assert!(
        matches!(decision.action, Action::Upload { parent: Some(_) }),
        "前置条件：present|modified|unchanged 应产出带 parent 的 Upload，实际是 {:?}",
        decision.action
    );

    // 半边崩溃：hub 落了新版本，客户端基线没更新。
    let after_half_crash = apply_remote_half(&world, &decision, item_id, &mut next_version);
    assert_eq!(
        after_half_crash.base, world.base,
        "半边崩溃后 base 必须保持崩溃前的旧值"
    );
    assert_ne!(
        after_half_crash.remote, world.remote,
        "半边崩溃后 remote 必须已经落到 hub 接受的新版本"
    );

    // 重启后重新决定：应落进零传输认领，而不是重新上传或冲突。
    let after_restart = decide_traced(
        &after_half_crash.base,
        &after_half_crash.local,
        &after_half_crash.remote,
        "half-crash-demo.bin",
        clock.tick(),
        &mut sink,
    );
    assert_eq!(after_restart.reason, "converged_independently");
    assert!(matches!(after_restart.action, Action::AdoptBaseline { .. }));

    let settled_world = apply_decision(
        &after_half_crash,
        &after_restart,
        item_id,
        &mut next_version,
    );
    let settled = decide(
        &settled_world.base,
        &settled_world.local,
        &settled_world.remote,
    );
    assert_eq!(settled.action, Action::Noop);

    for record in sink.records() {
        let action = field_str(record, "action");
        assert!(NON_DESTRUCTIVE.contains(&action.as_str()));
    }
}

/// 崩溃注入真的覆盖了两种模式：在随机模拟里核对 40 个种子的事件流，
/// 完全崩溃（[`CrashOutcome::Full`]）与半边崩溃（[`CrashOutcome::HalfRemoteOnly`]）
/// 都必须至少发生过一次——否则第 2 条修复只是加了个从未被走到的死分支，
/// 跟没加一样。
#[test]
fn 崩溃注入覆盖两种模式_完全崩溃与半边崩溃都被真实触发() {
    let mut saw_full = false;
    let mut saw_half = false;
    for seed in 0..40u64 {
        let run = simulate(seed, 80, 24, 3);
        for event in &run.events {
            if let SimEvent::Reconcile { crash, .. } = event {
                match crash {
                    CrashOutcome::Full => saw_full = true,
                    CrashOutcome::HalfRemoteOnly => saw_half = true,
                    CrashOutcome::None => {}
                }
            }
        }
    }
    assert!(saw_full, "40 个种子里从未触发过完整崩溃");
    assert!(
        saw_half,
        "40 个种子里从未触发过远端半边崩溃——新崩溃模式未被真正走到"
    );
}

/// `RemoteVanish` 事件让模拟真的能走到 I5 最贵的那三格：`present|absent|absent`、
/// `present|unchanged|absent`、`present|modified|absent`（决策表里统一产出
/// `remote_vanished_without_tombstone`，按 local 的三种分类拆成三格）。在这条
/// 修复之前，`remote` 只会 `Present → Tombstoned`，永不回到 `Absent`，这三格
/// 在模拟测试里一次都没被覆盖过——只有 `decision_table.rs` 与 proptest 打到过。
#[test]
fn remote_vanish事件让模拟覆盖到全部三格_remote_vanished_without_tombstone() {
    use std::collections::HashSet;
    let mut seen_locals: HashSet<String> = HashSet::new();
    for seed in 0..40u64 {
        let run = simulate(seed, 80, 24, 3);
        for record in &run.trace {
            if field_str(record, "reason") == "remote_vanished_without_tombstone" {
                seen_locals.insert(field_str(record, "local"));
            }
        }
    }
    let expected: HashSet<String> = ["absent", "unchanged", "modified"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(
        seen_locals, expected,
        "三格 present|{{absent,unchanged,modified}}|absent 应全部被模拟覆盖到，\
         实际覆盖 local 取值={seen_locals:?}"
    );
}

/// 种子可复现测试只断言"同种子同结果"，一个完全忽略 seed 的 `simulate` 也能
/// 通过那条测试——这里补一条反向断言，钉住 seed 真的在驱动事件序列，不是
/// 摆设参数。
#[test]
fn 不同种子产生不同的事件序列() {
    let a = simulate(1, 60, 20, 3);
    let b = simulate(2, 60, 20, 3);
    assert_ne!(
        a.events, b.events,
        "种子 1 与种子 2 产生了完全相同的事件序列——seed 参数疑似没有真正驱动随机性"
    );
}
