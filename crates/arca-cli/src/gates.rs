//! 删除传播的四道闸门（M2a tombstone 计划 Task 4，spec §5.3、§6，继承 lazync）。
//!
//! `arca_core::decide` 的决策表已经在格子 `present|unchanged|tombstoned` 里判定
//! `DeleteLocal` 是安全的——但那个判断发生在**调和时刻**，读的是扫描阶段拿到的
//! 一份快照；真正**执行**移除本地副本发生在稍后，中间存在一个窗口（同一次
//! `sync()` 调用内，一个大目录逐路径顺序执行；未来任何并发/异步执行路径这个
//! 窗口只会更宽）。决策表回答的是"按当时看到的状态，可以删"；闸门回答的是
//! "现在，真的可以删吗"——这是 I3（同步路径无销毁权）在执行侧的最后一道防线，
//! 也是本模块存在的全部理由：**决策与执行故意分层，闸门就是那道接缝**。
//!
//! 四道闸门（brief 明文顺序，逐条独立、任一不过则整体拒绝）：
//!
//! 1. **read_roots 范围**：要删的路径必须落在本次调和实际扫描过的范围内——
//!    没扫到就删，等于拿一份不完整的观察去销毁数据。当前实现只有单一存储根
//!    （`file://` 直连，M2b 之前没有多卷映射），实践中这道闸门在 `sync()` 里
//!    恒真（`DeleteLocal` 只在 local 被扫描判定为 `Unchanged` 时产生，意味着
//!    这个路径必然在 `scan_result.files` 里）；但闸门本身不能假设调用方永远
//!    这样接线，必须独立可验证——测试直接构造一个不含该路径的 `scanned_paths`
//!    来证明拦截逻辑本身是对的。
//! 2. **单点确认**：远端明确给出了这个 `item_id` 的 tombstone，不是"查不到
//!    记录"。`remote_vanished_without_tombstone` 那格已经在决策层挡住了模糊
//!    的"远端记录消失"，这里是独立的第二次确认——闸门不信任调用方一定传对了
//!    `remote_state`，要求它自己重新核对一遍。
//! 3. **基线一致性**：本地内容必须与基线记录的哈希一致，即"本地没有未同步
//!    的修改"。决策表用 `LocalClass`（扫描时的哈希）判过一次，**这道闸门在
//!    执行前重新读一次实际字节再判一次**——调和与执行之间的窗口内，文件可能
//!    被用户改动。这是四道里唯一需要真正做 IO 的一道，也是本任务最有价值的
//!    一条：它证明闸门不是决策的复读机，而是独立的、面向"现在"的二次核验。
//! 4. **保留期存在**：hub 的 `.arca/trash/` 里确实有这份**可取回**的内容。
//!    "可取回"不等于"`.meta`/`.data` 两个文件都 `symlink_metadata().is_ok()`"
//!    （评审 Critical #2）——0 字节的 `.data`、悬空符号链接、外部工具截断/
//!    替换过的内容都能通过那种检查，闸门却已经报告"放行"。这里改成打开
//!    `.data`、现场重算 BLAKE3（[`trash::content_hash`]），与 `.meta.hash`
//!    及 `ctx.base` 记录的期望哈希三方一致才放行——同一个 `item_id` 可能有
//!    多条历史 trash 记录（该路径曾被删除又恢复/重建又再次删除），只按
//!    `item_id` 取第一条会让一条**陈旧**记录为一条**缺失**记录背书，三方
//!    哈希核验顺带堵死这个口子（逐条候选都要现场核验，不是找到第一个
//!    `item_id` 匹配就停）。本切片不做保留期过期判断（那是 `arca restore`/
//!    `arca gc` 的范围，见 `trash.rs` 与
//!    `docs/superpowers/plans/2026-08-08-m2a-tombstone.md` Task 5），这里只
//!    确认"此刻确实可取回"这个当下的事实。
//!
//! **任一闸门不过 → 不删，把失败原因原样报给调用方**（I5：停下并可诊断）。
//! `sync.rs` 把闸门拒绝计入 `SyncReport::delete_blocked`，让退出码非零、
//! 运维能看到具体是哪一道拦下的——`GateFailure` 的每个变体逐条可区分，不折叠
//! 成一个笼统的"删除失败"。

use crate::trash;
use arca_chunk::hash::ContentHash;
use arca_core::state::{BaseState, RemoteState};
use arca_format::model::ItemId;
use arca_store::root::StorageRoot;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// 闸门拒绝——逐条可区分（I5：运维看到"第几道拦下的"才知道该怎么办）。
#[derive(Debug)]
pub enum GateFailure {
    /// 第 1 道：路径不在本次调和实际扫描过的范围内。
    OutOfReadRoots { path: String },
    /// 第 2 道：远端状态不是对这个 `item_id` 的明确 tombstone——要么远端根本
    /// 不是 tombstone 状态，要么 tombstone 记录的是另一个 `item_id`。
    NotSinglePointConfirmed {
        path: String,
        item_id: ItemId,
        /// 闸门检查时实际看到的远端原始形状（`"absent"` / `"present"` /
        /// `"tombstoned_other_item"`），供诊断——不是 FORMAT.md 钉死的取值集合，
        /// 只在这条错误消息里使用。
        remote: &'static str,
    },
    /// 第 3 道：重新读取的本地字节与基线哈希不一致——调和之后、执行之前，
    /// 文件被改动过。
    BaselineDrift { path: String, reason: String },
    /// 第 4 道：hub 的 `.arca/trash/` 里找不到这个 `item_id` 对应、内容确实
    /// 存在的记录——权威副本一旦不可取回，移除本地副本就等于销毁。
    RetentionMissing { path: String, item_id: ItemId },
    /// 闸门检查本身发生的真实 IO 故障（权限等）——与"闸门判定不通过"是不同
    /// 性质的失败，不归入以上四种拒绝理由。
    Io { path: String, reason: String },
}

impl fmt::Display for GateFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateFailure::OutOfReadRoots { path } => write!(
                f,
                "{path}：不在本次调和实际扫描过的范围内，拒绝删除（第 1 道闸门）"
            ),
            GateFailure::NotSinglePointConfirmed {
                path,
                item_id,
                remote,
            } => write!(
                f,
                "{path}（item_id {}）：远端状态不是对这个 item 的明确 tombstone（实为 {remote}），\
                 拒绝删除（第 2 道闸门）",
                item_id.to_hex()
            ),
            GateFailure::BaselineDrift { path, reason } => {
                write!(f, "{path}：{reason}，拒绝删除（第 3 道闸门：基线一致性）")
            }
            GateFailure::RetentionMissing { path, item_id } => write!(
                f,
                "{path}（item_id {}）：hub 的 .arca/trash/ 里找不到可取回的内容，\
                 拒绝删除（第 4 道闸门：保留期存在）",
                item_id.to_hex()
            ),
            GateFailure::Io { path, reason } => write!(f, "{path}：闸门检查失败：{reason}"),
        }
    }
}

impl std::error::Error for GateFailure {}

/// 一次删除执行前闸门检查所需的全部上下文。刻意把它做成一个结构体而不是
/// 五六个位置参数——调用点（`sync.rs`）已经手握这些值，用具名字段比一长串
/// 同类型（`&str`/`ItemId`）参数排列更不容易在调用点传错顺序。
pub struct DeleteCheck<'a> {
    /// 待删除的逻辑路径。
    pub path: &'a str,
    /// 决策表给出的、待删除内容所属的 `item_id`。
    pub item_id: ItemId,
    /// 本次调和实际扫描到的本地路径集合（第 1 道闸门）。
    pub scanned_paths: &'a BTreeSet<String>,
    /// 决策时读到的远端状态——第 2 道闸门重新核对它确实是对 `item_id` 的
    /// tombstone。
    pub remote_state: &'a RemoteState,
    /// 数据集根目录——第 3 道闸门据此重新读取本地文件的当前字节。
    pub dataset_root: &'a Path,
    /// 基线记录的状态——第 3 道闸门据此比对哈希。
    pub base: &'a BaseState,
    /// hub 存储根——第 4 道闸门据此查询 `.arca/trash/`。
    pub root: &'a StorageRoot,
    /// 本次调和开始时读到的 `.arca/trash/` 全量快照（评审 Important #3）：
    /// 第 4 道闸门在同一次 `sync()` 里可能被调用成百上千次，每次都重新
    /// `read_dir` + 逐条解析整个回收站目录是 O(n·m)——`sync.rs` 在循环开始
    /// 前只读一次目录列表，这里只在内存里按 `item_id`/哈希过滤，不再重复
    /// `read_dir`；对通过预筛的候选，仍然逐条现场重算 `.data` 的哈希
    /// （C2 的安全性核心不能省，见 [`check_retention`]），省掉的只是目录
    /// 遍历本身。
    pub trash_entries: &'a [trash::TrashEntry],
}

/// 依次跑四道闸门，任一不过立即返回对应的 [`GateFailure`]，不继续往下检查——
/// 第一个拦下的理由就是最值得报告的理由，没有必要再花时间算出"如果第一道
/// 过了，后面还会不会挡"这种反事实。
pub fn check_delete(ctx: &DeleteCheck) -> Result<(), GateFailure> {
    check_read_roots(ctx)?;
    check_single_point_confirmation(ctx)?;
    check_baseline_consistency(ctx)?;
    check_retention(ctx)?;
    Ok(())
}

/// 第 1 道：read_roots 范围。
fn check_read_roots(ctx: &DeleteCheck) -> Result<(), GateFailure> {
    if ctx.scanned_paths.contains(ctx.path) {
        Ok(())
    } else {
        Err(GateFailure::OutOfReadRoots {
            path: ctx.path.to_string(),
        })
    }
}

/// 第 2 道：单点确认——远端必须明确是对这个 `item_id` 的 tombstone。
fn check_single_point_confirmation(ctx: &DeleteCheck) -> Result<(), GateFailure> {
    match ctx.remote_state {
        RemoteState::Tombstoned { item_id, .. } if *item_id == ctx.item_id => Ok(()),
        RemoteState::Tombstoned { .. } => Err(GateFailure::NotSinglePointConfirmed {
            path: ctx.path.to_string(),
            item_id: ctx.item_id,
            remote: "tombstoned_other_item",
        }),
        RemoteState::Absent => Err(GateFailure::NotSinglePointConfirmed {
            path: ctx.path.to_string(),
            item_id: ctx.item_id,
            remote: "absent",
        }),
        RemoteState::Present { .. } => Err(GateFailure::NotSinglePointConfirmed {
            path: ctx.path.to_string(),
            item_id: ctx.item_id,
            remote: "present",
        }),
    }
}

/// 第 3 道：基线一致性——重新读一次实际字节，与基线哈希比对。
///
/// 本地文件此刻已经不存在也算通过：不管是用户手动删了、还是别的路径已经
/// 处理过，"现在没有未同步的修改"这件事本身是成立的，交给调用方的
/// `fs::remove_file` 去幂等地处理"已经不在"（它本就把 `NotFound` 当无操作）。
fn check_baseline_consistency(ctx: &DeleteCheck) -> Result<(), GateFailure> {
    let expected_hash = match ctx.base {
        BaseState::Present { hash, .. } => *hash,
        BaseState::Absent => {
            // 结构上不应该出现：DeleteLocal 只在 `base=Present` 的格子产生
            // （见 arca_core::reconcile 决策表）。防御性拒绝而不是 panic（I5）。
            return Err(GateFailure::BaselineDrift {
                path: ctx.path.to_string(),
                reason: "内部不变量被破坏：DeleteLocal 的执行前提是基线存在".to_string(),
            });
        }
    };

    let local_path = ctx.dataset_root.join(crate::sync::to_native(ctx.path));
    match fs::read(&local_path) {
        Ok(bytes) => {
            let hash = ContentHash::from_bytes(&bytes);
            if hash == expected_hash {
                Ok(())
            } else {
                Err(GateFailure::BaselineDrift {
                    path: ctx.path.to_string(),
                    reason: "本地内容自调和以来已被修改，与基线哈希不一致".to_string(),
                })
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(GateFailure::Io {
            path: local_path.display().to_string(),
            reason: e.to_string(),
        }),
    }
}

/// 第 4 道：保留期存在——hub 的 `.arca/trash/` 里确实有这个 `item_id` 对应、
/// 内容也在场的记录（评审 Critical #2：三方哈希核验，见模块顶部文档）。
fn check_retention(ctx: &DeleteCheck) -> Result<(), GateFailure> {
    // `ctx.base` 到这里必然是 `Present`——`check_baseline_consistency` 已经在
    // 它之前跑过，`Absent` 分支在那一步就已经拒绝。这里的 `Absent` 分支只是
    // 防御性兜底（I5：绝不假设调用顺序不会变），不是正常可达路径。
    let expected_hash = match ctx.base {
        BaseState::Present { hash, .. } => *hash,
        BaseState::Absent => {
            return Err(GateFailure::RetentionMissing {
                path: ctx.path.to_string(),
                item_id: ctx.item_id,
            });
        }
    };

    // 目录列表来自调用方在循环开始前读好的快照（评审 Important #3），这里
    // 不再重复 `read_dir` 整个 `.arca/trash/`——`.meta.hash` 先做一次快速
    // 预筛（同一 item_id 可能有多条历史记录，见模块顶部文档），再逐条现场
    // 重算 `.data` 的哈希——只信 `.meta` 记录的哈希不够：`.meta` 说的是
    // "移入时刻"的内容，`.data` 此刻可能已经被外部工具截断/替换。任一候选
    // 三方一致（`ctx.base` 的期望哈希 = `.meta.hash` = 现场重算的哈希）即
    // 放行；一条候选核验失败不放弃，继续看下一条，绝不让一条陈旧/损坏的
    // 记录为另一条缺失的记录背书。
    let recoverable = ctx
        .trash_entries
        .iter()
        .filter(|e| e.meta.item_id == ctx.item_id && e.meta.hash == expected_hash)
        .any(|entry| matches!(trash::content_hash(ctx.root, entry.trash_id), Ok(h) if h == expected_hash));

    if recoverable {
        Ok(())
    } else {
        Err(GateFailure::RetentionMissing {
            path: ctx.path.to_string(),
            item_id: ctx.item_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::hub_layout::FormatJson;
    use arca_format::model::VersionId;
    use std::fs;

    fn 造存储根(dir: &Path) {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        fs::create_dir_all(dir.join(".arca/trash")).unwrap();
        let format = FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    fn open(dir: &Path) -> StorageRoot {
        StorageRoot::open(dir, None).unwrap()
    }

    fn version_id() -> VersionId {
        VersionId::new("20260808T090000Z", &"1".repeat(32)).unwrap()
    }

    /// 造出一个"四道全过"的完整场景：本地文件存在且与基线一致、远端确实
    /// 是对同一 item_id 的 tombstone、trash 里确实有对应内容、路径在扫描
    /// 范围内。各条"该拦住"的测试从这个基线出发，只破坏其中一道。
    struct Scene {
        _dir: tempfile::TempDir,
        dataset_root: std::path::PathBuf,
        root: StorageRoot,
        item_id: ItemId,
        base: BaseState,
        remote_state: RemoteState,
        scanned_paths: BTreeSet<String>,
    }

    fn 搭建全过场景() -> Scene {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let dataset_root = dir.path().join("dataset");
        fs::create_dir_all(&dataset_root).unwrap();
        fs::write(dataset_root.join("a.png"), b"content").unwrap();

        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x3f; 16]);
        let hash = ContentHash::from_bytes(b"content");

        // hub 侧：内容已经移进 trash（tombstone 执行的一部分，Task 3 已交付）。
        fs::write(dir.path().join("files/a.png"), b"content").unwrap();
        crate::trash::move_to_trash(&root, "a.png", item_id, "2026-08-08T09:10:00Z").unwrap();

        let base = BaseState::Present {
            item_id,
            version_id: version_id(),
            hash,
            size: 7,
        };
        let remote_state = RemoteState::Tombstoned {
            item_id,
            version_id: version_id(),
        };
        let mut scanned_paths = BTreeSet::new();
        scanned_paths.insert("a.png".to_string());

        Scene {
            _dir: dir,
            dataset_root,
            root,
            item_id,
            base,
            remote_state,
            scanned_paths,
        }
    }

    impl Scene {
        fn check(&self) -> Result<(), GateFailure> {
            // 测试里不追求性能，每次 check() 都重新读一遍 trash 快照即可——
            // 生产代码（`sync.rs`）才是真正只读一次的调用方。
            let trash_entries = crate::trash::list(&self.root).unwrap();
            check_delete(&DeleteCheck {
                path: "a.png",
                item_id: self.item_id,
                scanned_paths: &self.scanned_paths,
                remote_state: &self.remote_state,
                dataset_root: &self.dataset_root,
                base: &self.base,
                root: &self.root,
                trash_entries: &trash_entries,
            })
        }
    }

    #[test]
    fn 四道全过时放行删除() {
        let scene = 搭建全过场景();
        assert!(scene.check().is_ok());
    }

    #[test]
    fn 第1道_路径不在扫描范围内则拦住() {
        let mut scene = 搭建全过场景();
        scene.scanned_paths.clear(); // 模拟"没扫到这个路径"
        let err = scene.check().unwrap_err();
        assert!(
            matches!(err, GateFailure::OutOfReadRoots { .. }),
            "实得 {err:?}"
        );
    }

    #[test]
    fn 第2道_远端不是明确的tombstone则拦住() {
        let mut scene = 搭建全过场景();
        // 远端"查不到记录"——决策层本该已经挡住（remote_vanished_without_tombstone），
        // 闸门是独立的第二次确认，不信任调用方一定传对了。
        scene.remote_state = RemoteState::Absent;
        let err = scene.check().unwrap_err();
        match err {
            GateFailure::NotSinglePointConfirmed { remote, .. } => assert_eq!(remote, "absent"),
            other => panic!("应为 NotSinglePointConfirmed，实得 {other:?}"),
        }
    }

    #[test]
    fn 第2道_tombstone记录的是另一个item时拦住() {
        let mut scene = 搭建全过场景();
        scene.remote_state = RemoteState::Tombstoned {
            item_id: ItemId::from_bytes([0x99; 16]), // 另一个 item
            version_id: version_id(),
        };
        let err = scene.check().unwrap_err();
        match err {
            GateFailure::NotSinglePointConfirmed { remote, .. } => {
                assert_eq!(remote, "tombstoned_other_item")
            }
            other => panic!("应为 NotSinglePointConfirmed，实得 {other:?}"),
        }
    }

    /// 第 3 道的核心价值：构造真实的竞态——先用真实的 `decide()` 拿到一条
    /// `DeleteLocal` 决策（证明"调和时刻"确实判定可以删），然后在决策之后、
    /// 执行之前修改本地文件内容，断言闸门重新读字节后拦住了删除。这条测试
    /// 证明闸门不是决策的复读机：它面向"现在"，独立于"当时决策时看到了什么"。
    #[test]
    fn 第3道_调和后执行前文件被改则拦住_真实竞态() {
        let scene = 搭建全过场景();

        // 先真实走一遍决策，证明"调和时刻"确实判定为 DeleteLocal。
        let local_at_decide_time = arca_core::state::LocalState::Present {
            hash: ContentHash::from_bytes(b"content"),
            size: 7,
        };
        let decision =
            arca_core::reconcile::decide(&scene.base, &local_at_decide_time, &scene.remote_state);
        match decision.action {
            arca_core::reconcile::Action::DeleteLocal { item_id } => {
                assert_eq!(item_id, scene.item_id)
            }
            other => panic!("测试前置条件不成立：应为 DeleteLocal，实得 {other:?}"),
        }

        // 竞态窗口：决策已经做出，执行之前，文件被改了（例如用户此刻正在
        // 编辑这份"即将被判定为已同步、可以安全删除"的文件）。
        fs::write(
            scene.dataset_root.join("a.png"),
            "这是新内容，决策时还不存在".as_bytes(),
        )
        .unwrap();

        let err = scene.check().unwrap_err();
        assert!(
            matches!(err, GateFailure::BaselineDrift { .. }),
            "实得 {err:?}"
        );

        // 闸门拒绝时绝不能动本地文件——内容必须是竞态写入的那份，不是原文件、
        // 更不能被删除。
        assert_eq!(
            fs::read(scene.dataset_root.join("a.png")).unwrap(),
            "这是新内容，决策时还不存在".as_bytes()
        );
    }

    #[test]
    fn 第3道_本地文件已经不存在时视为通过() {
        let scene = 搭建全过场景();
        fs::remove_file(scene.dataset_root.join("a.png")).unwrap();
        assert!(scene.check().is_ok(), "本地已经不在，无需再删，应视为通过");
    }

    #[test]
    fn 第4道_trash里没有对应内容时拦住() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let dataset_root = dir.path().join("dataset");
        fs::create_dir_all(&dataset_root).unwrap();
        fs::write(dataset_root.join("a.png"), b"content").unwrap();
        // 刻意不调用 move_to_trash——hub 侧从未真正执行过 tombstone 的内容
        // 移动，只是（假设地）声称远端已经 tombstone。

        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x3f; 16]);
        let base = BaseState::Present {
            item_id,
            version_id: version_id(),
            hash: ContentHash::from_bytes(b"content"),
            size: 7,
        };
        let remote_state = RemoteState::Tombstoned {
            item_id,
            version_id: version_id(),
        };
        let mut scanned_paths = BTreeSet::new();
        scanned_paths.insert("a.png".to_string());
        let trash_entries = crate::trash::list(&root).unwrap();

        let err = check_delete(&DeleteCheck {
            path: "a.png",
            item_id,
            scanned_paths: &scanned_paths,
            remote_state: &remote_state,
            dataset_root: &dataset_root,
            base: &base,
            root: &root,
            trash_entries: &trash_entries,
        })
        .unwrap_err();
        assert!(
            matches!(err, GateFailure::RetentionMissing { .. }),
            "实得 {err:?}"
        );
    }

    /// 评审 Critical #2 实机复现之一：`.data` 被截成 0 字节（ENOSPC 下的部分
    /// 拷贝、位腐都会造出这个状态）——旧实现只用 `symlink_metadata().is_ok()`
    /// 判断"存在"，0 字节文件照样通过；修复后必须重新打开、重算哈希，与
    /// `.meta.hash`/`ctx.base` 的期望哈希不一致就拦住。
    #[test]
    fn 第4道_trash的data被截成0字节时拦住_评审critical2实机复现() {
        let scene = 搭建全过场景();
        assert!(scene.check().is_ok(), "测试前置条件：改坏之前应先能全过");

        let entries = crate::trash::list(&scene.root).unwrap();
        let entry = entries
            .iter()
            .find(|e| e.meta.item_id == scene.item_id)
            .expect("测试前置条件：应能找到对应的 trash 记录");
        let data_path = scene
            ._dir
            .path()
            .join(format!(".arca/trash/{}.data", entry.trash_id));
        fs::write(&data_path, b"").unwrap();

        let err = scene.check().unwrap_err();
        assert!(
            matches!(err, GateFailure::RetentionMissing { .. }),
            "0 字节的 .data 不应被当作可取回，实得 {err:?}"
        );
    }

    /// 评审 Critical #2 实机复现之二：`.data` 被换成悬空符号链接（hub 常年
    /// 放在外置盘/网盘同步目录/备份还原出来的副本上，rsync 出来的悬空链接
    /// 是真实会出现的状态）——旧实现的 `symlink_metadata` 不跟随链接，"链接
    /// 本身存在"就判定通过；修复后 `content_hash` 用 `fs::read` 跟随链接，
    /// 悬空链接读出 `NotFound`，闸门必须拦住而不是放行后让紧随其后的
    /// `arca restore` 才发现"其实找不到"。
    #[test]
    #[cfg(unix)]
    fn 第4道_trash的data换成悬空符号链接时拦住_评审critical2实机复现() {
        use std::os::unix::fs::symlink;

        let scene = 搭建全过场景();
        assert!(scene.check().is_ok(), "测试前置条件：改坏之前应先能全过");

        let entries = crate::trash::list(&scene.root).unwrap();
        let entry = entries
            .iter()
            .find(|e| e.meta.item_id == scene.item_id)
            .expect("测试前置条件：应能找到对应的 trash 记录");
        let data_path = scene
            ._dir
            .path()
            .join(format!(".arca/trash/{}.data", entry.trash_id));
        fs::remove_file(&data_path).unwrap();
        symlink(scene._dir.path().join("不存在的目标"), &data_path).unwrap();

        let err = scene.check().unwrap_err();
        assert!(
            matches!(err, GateFailure::RetentionMissing { .. }),
            "悬空符号链接不应被当作可取回，实得 {err:?}"
        );
    }

    /// 评审 Critical #2 指出的连带问题：旧实现 `entries.iter().find(|e|
    /// e.meta.item_id == ctx.item_id)` 只按 `item_id` 取第一条——同一个
    /// item 若有多条历史 trash 记录（该 item 曾被删除又重建又再次删除，
    /// 从未 `arca gc`），一条**陈旧**记录（内容与本次要保护的版本不同）
    /// 足以为一条**缺失**记录（本该匹配、但 `.data` 已损坏）背书，`list()`
    /// 按 `trash_id`（随机十六进制）排序，谁排在前全看运气。三方哈希核验
    /// 逐条候选现场核验，从根上堵死这个口子。
    #[test]
    fn 第4道_陈旧的历史记录不能为缺失的当前记录背书_评审critical2实机复现() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let dataset_root = dir.path().join("dataset");
        fs::create_dir_all(&dataset_root).unwrap();
        fs::write(dataset_root.join("a.png"), b"content").unwrap();

        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x3f; 16]);

        // 陈旧记录：同一 item_id 更早一次删除留下的历史 trash 记录，内容与
        // "现在"要保护的版本不同。
        fs::write(dir.path().join("files/old.png"), b"old stale content").unwrap();
        crate::trash::move_to_trash(&root, "old.png", item_id, "t1").unwrap();

        // "现在"这条记录：内容本应与本次删除要保护的版本一致，但它的
        // `.data` 随后被截断——这才是本次删除真正应该核验、也确实核验失败
        // 的那一条。
        fs::write(dir.path().join("files/a.png"), b"content").unwrap();
        let current_id = crate::trash::move_to_trash(&root, "a.png", item_id, "t2").unwrap();
        fs::write(
            dir.path().join(format!(".arca/trash/{current_id}.data")),
            b"",
        )
        .unwrap();

        let base = BaseState::Present {
            item_id,
            version_id: version_id(),
            hash: ContentHash::from_bytes(b"content"),
            size: 7,
        };
        let remote_state = RemoteState::Tombstoned {
            item_id,
            version_id: version_id(),
        };
        let mut scanned_paths = BTreeSet::new();
        scanned_paths.insert("a.png".to_string());
        let trash_entries = crate::trash::list(&root).unwrap();

        let err = check_delete(&DeleteCheck {
            path: "a.png",
            item_id,
            scanned_paths: &scanned_paths,
            remote_state: &remote_state,
            dataset_root: &dataset_root,
            base: &base,
            root: &root,
            trash_entries: &trash_entries,
        })
        .unwrap_err();
        assert!(
            matches!(err, GateFailure::RetentionMissing { .. }),
            "陈旧记录不应为损坏的当前记录背书，实得 {err:?}"
        );
    }

    /// 闸门检查顺序：第 1 道最先失败时，不应该继续往下检查到后面几道——
    /// 用一个"连第 2/3/4 道也会失败"的场景验证返回的确实是第 1 道的错误。
    #[test]
    fn 多道同时失败时报第一个失败的闸门() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let dataset_root = dir.path().join("dataset");
        fs::create_dir_all(&dataset_root).unwrap();
        // 本地文件也不写，trash 也不写，remote 也给 Absent——只有第 1 道
        // （不在扫描范围）会被优先报告。
        let root = open(dir.path());
        let item_id = ItemId::from_bytes([0x3f; 16]);
        let base = BaseState::Present {
            item_id,
            version_id: version_id(),
            hash: ContentHash::from_bytes(b"content"),
            size: 7,
        };
        let remote_state = RemoteState::Absent;
        let scanned_paths = BTreeSet::new();
        let trash_entries = crate::trash::list(&root).unwrap();

        let err = check_delete(&DeleteCheck {
            path: "a.png",
            item_id,
            scanned_paths: &scanned_paths,
            remote_state: &remote_state,
            dataset_root: &dataset_root,
            base: &base,
            root: &root,
            trash_entries: &trash_entries,
        })
        .unwrap_err();
        assert!(
            matches!(err, GateFailure::OutOfReadRoots { .. }),
            "实得 {err:?}"
        );
    }
}
