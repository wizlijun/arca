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
use common::{apply_decision, fresh_version, hash_symbol, is_terminal, World, NON_DESTRUCTIVE};
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

/// 一次模拟里发生的一个事件——本地改动 / 本地删除 / 远端改动 / 远端 tombstone /
/// 调和尝试（`crashed` 记录这次尝试是否在"决策已产出但尚未应用"的点被崩溃打断）。
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
        version_num: u8,
    },
    RemoteTombstone {
        path: &'static str,
        version_num: u8,
    },
    Reconcile {
        path: &'static str,
        crashed: bool,
    },
}

/// 一次完整模拟的产出：生成的事件序列、`decide_traced` 落下的完整 trace、
/// 每次调和实际拿到的决策（与 trace 逐条对应，供「trace 不漏事件」的断言用）、
/// 以及每条路径是否在结算阶段收敛。
struct SimRun {
    events: Vec<SimEvent>,
    trace: Vec<TraceRecord>,
    decisions: Vec<(&'static str, Decision)>,
    converged: HashMap<&'static str, bool>,
}

/// 跑一次确定性模拟：`churn_steps` 步随机交错的本地/远端变更与调和尝试
/// （调和尝试里，非终态决策有 `1/crash_denom` 的概率"崩溃"——决策已经产出并
/// 落进 trace，但跳过应用，模拟"决策做完、还没来得及落地进程就没了"）；
/// 之后进入结算阶段，对每条路径反复调和（同样可能崩溃）直到收敛或用完
/// `settle_bound` 步预算。
///
/// 纯函数：只依赖 `seed` 与三个步数参数，不读任何外部状态（系统时钟、环境
/// 变量都不碰）——种子可复现的前提就是这个函数本身是确定性的。
fn simulate(seed: u64, churn_steps: u32, settle_bound: u32, crash_denom: u64) -> SimRun {
    let mut rng = SimRng::new(seed);
    let mut clock = SimClock::new();
    let mut sink = VecSink::new();
    let mut events = Vec::new();
    let mut decisions = Vec::new();
    let mut next_version = 0u8;

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
        let kind = rng.next_below(5);
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

/// 一次调和尝试：`decide_traced` 产出决策并落 trace；若是终态
/// （`Conflict`/`NeedsHuman`，I5：模糊必停）则不应用，也不掷崩溃骰子——终态
/// 本来就不该被"应用"。否则以 `1/crash_denom` 的概率崩溃（决策已产出但跳过
/// 应用，模拟进程在这个点消失）；不崩溃则调用 `apply_decision` 把决策落地。
#[allow(clippy::too_many_arguments)]
fn attempt_reconcile(
    path: &'static str,
    store: &mut HashMap<&'static str, World>,
    sink: &mut VecSink,
    clock: &mut SimClock,
    rng: &mut SimRng,
    next_version: &mut u8,
    crash_denom: u64,
    events: &mut Vec<SimEvent>,
    decisions: &mut Vec<(&'static str, Decision)>,
) {
    let world = store[path].clone();
    let t = clock.tick();
    let decision = decide_traced(&world.base, &world.local, &world.remote, path, t, sink);
    decisions.push((path, decision.clone()));

    if is_terminal(&decision.action) {
        events.push(SimEvent::Reconcile {
            path,
            crashed: false,
        });
        return;
    }

    let crashed = rng.next_below(crash_denom) == 0;
    events.push(SimEvent::Reconcile { path, crashed });
    if !crashed {
        let applied = apply_decision(&world, &decision, item_id_for(path), next_version);
        store.insert(path, applied);
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
/// 调用 `decide_traced` 的次数一一对应，且每条记录的 `path`/`action`/`reason`
/// 字段与对应的 `Decision` 逐字相同——漏了一条，事故现场就少一条线索。
#[test]
fn trace事件序列与实际决策一一对应() {
    let run = simulate(0x1234_5678, 60, 20, 3);

    assert_eq!(
        run.trace.len(),
        run.decisions.len(),
        "trace 记录数与 decide_traced 调用次数不一致——事件序列={:?}",
        run.events
    );

    for (record, (path, decision)) in run.trace.iter().zip(run.decisions.iter()) {
        assert_eq!(record.event, arca_format::trace::EventKind::ReconcileDecide);
        assert_eq!(field_str(record, "path"), *path);
        assert_eq!(field_str(record, "action"), decision.action.as_str());
        assert_eq!(field_str(record, "reason"), decision.reason);
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
    let mut next_version = 0u8;
    let applied = apply_decision(&world, &after_restart, item_id, &mut next_version);
    let settled = decide(&applied.base, &applied.local, &applied.remote);
    assert_eq!(settled.action, Action::Noop);

    // 全程两条 trace 记录都不是销毁语义的动作。
    for record in sink.records() {
        let action = field_str(record, "action");
        assert!(NON_DESTRUCTIVE.contains(&action.as_str()));
    }
}
