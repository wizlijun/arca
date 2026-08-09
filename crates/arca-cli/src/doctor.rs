//! `arca doctor`（M1d Task 7）：vault 一致性巡检 + 「本地存在但 hub 尚无
//! 副本」告警，外加一个既有 `Issue` 变体的呈现纪律。
//!
//! # 债一：`git clean -xdf` 风险的唯一缓解措施
//!
//! M1c 实测确认 `git clean -xdf`（以及 `-Xdf`）**真的会删掉受管二进制**——
//! 真删、不留 tombstone、找不回来（`.gitignore` 反选块让它们对 `git status`
//! 不可见，`-x` 恰恰专挑"被忽略的文件"下手）。项目决定接受这个风险不绕过
//! （摘出反选块会破坏它，后果是整个数据集进 git，更糟——见
//! `crates/arca-git/src/hooks.rs` 的 `TODO(M1)`）。缓解措施就是这里：检出
//! 「本地存在、但 hub 索引里完全没有这条记录」的文件，显著告警——用户在
//! 跑 `git clean` 前扫一眼就该看见。
//!
//! # 债二：`Issue::CheckIncomplete` 必须显式呈现
//!
//! `arca_git::tracking::check_vault` 的 [`Issue::CheckIncomplete`] 意味着
//! 「这项检查没跑成功」，**不是「检查通过」**。`doctor` 只是把 `check_vault`
//! 返回的每一条 `Issue`（含这一变体）原样纳入报告、原样打印——不单独过滤、
//! 不折叠成"没有其它问题就是干净"，它的 `Display` 本身已经把"本次结果不
//! 完整，不能当作库是干净的"说清楚（见 `tracking.rs`）。命令壳只需要保证
//! 把它当成与其它 `Issue` 同等严重（进同一份"有问题"清单、让退出码非零），
//! 不能因为它"看起来不像一个具体错误"就单独降级成安静。
//!
//! # 债三：`.gitignore` 反选块此前只挂了名字，从没真的断言过（评审 Important #1）
//!
//! CLAUDE.md 与本文件曾经的 doc comment 都写着"`arca doctor` 断言的是
//! `git check-ignore` 的实际结果，而非文本"，但从没有代码真的调用过
//! `Repo::check_ignore_no_index`（那是 M1c 专门为 doctor 加的能力，全仓库
//! 唯一的调用者是 `adopt.rs` 里的一个测试）。`Issue::AlreadyTracked` 只能
//! 抓到已经被 `git add` 过的二进制——那是损害发生**之后**。这里补上事前
//! 巡检：见 [`check_ignore_block`]。
//!
//! # 债四：`sync` 每次都重新生成清单，但清单本身可能已经漂移（评审 Important #4）
//!
//! `sync.rs` 现在每次收尾都会从最终基线重新生成 `.arca/manifest`，但已经
//! 用旧版本二进制跑过的既有数据集、或被人手工改过清单的数据集仍可能处于
//! 漂移状态——`doctor` 独立比对一遍，见 [`check_manifest`]。

use crate::{baseline, dataset, hub, scan, trash};
use arca_core::state::BaseState;
use arca_format::gitarca::Registry;
use arca_format::manifest::Manifest;
use arca_format::trace::NullSink;
use arca_git::repo::Repo;
use arca_git::tracking::{self, Issue};
use arca_store::root::StorageRoot;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// doctor 用来探测 `.gitignore` 反选块实际生效情况的探针文件名——不需要
/// 真的存在：`git check-ignore --no-index` 是纯规则匹配，不要求路径存在，
/// 借用这个相对位置去问"这里会不会被忽略"即可，不依赖数据集当前有没有
/// 任何真实的受管文件（空数据集也能测）。
const IGNORE_PROBE: &str = "__arca_doctor_ignore_probe__";

/// `.gitignore` 反选块的实测结果（评审 Important #1，见模块顶部「债三」）：
/// 用 [`arca_git::repo::Repo::check_ignore_no_index`] 直接问 `git
/// check-ignore` 三件事，任一不符即报告——`arca doctor` 断言的必须是这个
/// 命令的实际结果，不是"标记块的文本看起来还在"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoreIssue {
    /// 受管二进制没有被忽略——下次 `git add -A` 会把整个数据集提交进 git。
    ManagedNotIgnored { probe: String },
    /// `.arca/dataset.toml` 或 `.arca/manifest` 被忽略——协作者拿不到清单
    /// （反选块漏了 `!/…/.arca/` 那一行，或块被破坏成只剩 `/path/*`）。
    MetadataIgnored { path: String },
    /// `.arca/client/`（设备本地投影）没有被忽略——可能被误提交进 git。
    ClientNotIgnored { probe: String },
}

impl fmt::Display for IgnoreIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IgnoreIssue::ManagedNotIgnored { probe } => write!(
                f,
                "{probe:?} 未被 .gitignore 忽略——受管二进制会被 `git add -A` 提交进 git"
            ),
            IgnoreIssue::MetadataIgnored { path } => {
                write!(f, "{path:?} 被 .gitignore 忽略——协作者拿不到这份清单/配置")
            }
            IgnoreIssue::ClientNotIgnored { probe } => write!(
                f,
                "{probe:?} 未被 .gitignore 忽略——设备本地投影可能被误提交进 git"
            ),
        }
    }
}

/// 清单（`.arca/manifest`）与基线的一致性检查结果（评审 Important #4，
/// 见模块顶部「债四」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestIssue {
    /// 基线非空（此前已同步过），但 `.arca/manifest` 缺失。
    Missing,
    /// 清单文件存在但解析失败。
    Corrupt(String),
    /// 清单与基线记录的路径集合或内容不一致——`only_in_manifest`/
    /// `only_in_baseline` 各自列出只在一侧出现、或路径相同但哈希/大小不同
    /// 的路径（后一种情况会同时出现在两个列表里）。
    Drift {
        only_in_manifest: Vec<String>,
        only_in_baseline: Vec<String>,
    },
}

impl fmt::Display for ManifestIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestIssue::Missing => {
                write!(f, "基线非空但 .arca/manifest 缺失——协作者拿不到清单")
            }
            ManifestIssue::Corrupt(reason) => write!(f, ".arca/manifest 解析失败：{reason}"),
            ManifestIssue::Drift {
                only_in_manifest,
                only_in_baseline,
            } => write!(
                f,
                "清单与基线不一致——只在清单中：{only_in_manifest:?}，只在基线中：{only_in_baseline:?}"
            ),
        }
    }
}

/// 单个已在 `.gitarca` 登记、且本地也有 `dataset.toml` 的数据集的巡检结果。
/// （磁盘上完全缺失的数据集已经由 `check_vault` 的 `Issue::MissingDataset`
/// 覆盖，不在这里重复处理。）
#[derive(Debug)]
pub enum DatasetHealth {
    /// 存储根打开成功，本地扫描与远端读取都跑完了。
    Checked {
        path: String,
        /// 本地存在、但 hub 索引里完全没有这个路径记录的文件——
        /// `git clean -xdf` 会把它们永久删掉且无法找回（模块顶部「债一」）。
        local_only: Vec<String>,
        /// `.gitignore` 反选块的实测问题（模块顶部「债三」），空即通过。
        ignore_issues: Vec<IgnoreIssue>,
        /// 清单与基线不一致时的具体问题（模块顶部「债四」），`None` 即一致。
        manifest_issue: Option<ManifestIssue>,
        /// `.arca/trash/` 里逐条巡检出的损坏记录（评审 Minor）：`trash::list`
        /// 遇到第一条损坏的 `.meta` 就整体报错是对的（I5），但那会让整个
        /// 数据集的删除与 `restore --list` 永久失效，且不指出到底是哪一条
        /// 坏的——这里用 `trash::scan_issues` 逐条累积，点名具体的文件。
        trash_issues: Vec<trash::TrashIssue>,
        /// 评审 Minor #1：`<dataset>/.arca/client/trash/` 非空、但
        /// `<dataset>/.arca/client/role.toml` 缺失——`role::read` 把文件缺失
        /// 吸收成默认角色 `client`（这本身是正确设计，见 `role.rs` 模块顶部
        /// 「与 baseline 刻意不同的错误处理策略」），但 `trash/` 目录本身就是
        /// "这台设备曾经把这个数据集声明为 server"的物证——`server` 角色的
        /// `DeleteLocal` 才会往这里放东西（见 `sync.rs` 的
        /// `execute_delete_local`），`client` 角色从不写入这个目录。二者同时
        /// 出现，说明 `role.toml` 大概率是意外丢失（磁盘故障、误删、
        /// `.arca/client/` 整个投影被当成"可丢弃"清空——它其实不是，见
        /// `role.rs`），而不是用户主动降级；`doctor` 完全不认识角色，此前没
        /// 有任何代码路径检测这种不一致。不是数据丢失（hub 侧 trash 保留期
        /// 仍持有内容，见 `crate::trash`），但如果不提醒，设备下次收到删除
        /// 事件会真的按 `client` 角色移除本地副本。
        possible_lost_server_role: bool,
        /// M2e Task 1：`<dataset>/.arca/client/trash/` 的占用概况——
        /// **让它可见**。这个目录此前是纯只写的：`server` 角色每收到一次
        /// 远端删除就往里塞一份完整副本，没有任何诊断命令看得见它，用户
        /// 只会在某天发现磁盘满了才去翻。`None` 表示统计没能跑出来（记录
        /// 损坏，具体是哪一条见 `local_trash_issues`）——绝不用一个"看起来
        /// 很健康的 0"掩盖读不出来这件事（I5）。
        ///
        /// **非空不代表有问题**：它不参与 [`DoctorReport::is_clean`]，
        /// `server` 角色下回收站里有东西是完全正常的状态，命令壳只把它当
        /// 一条信息打出来，不影响退出码。
        local_trash_usage: Option<crate::local_trash::Usage>,
        /// 本地回收站里逐条巡检出的损坏记录（`local_trash::scan_issues`）
        /// ——与上面的 hub 侧 `trash_issues` 同一分工与同一严重性：一条读不
        /// 懂的 `.meta` 会让 `arca restore --local`/`arca gc --local` 对整个
        /// 数据集失效，必须点名具体文件。这一项**参与** `is_clean`。
        local_trash_issues: Vec<trash::TrashIssue>,
        /// M2e Task 3：hub 侧回收站（`.arca/trash/`）的巡检**没有跑**。
        /// `http(s)://` hub 下恒为真——`PROTOCOL.md` §1.2 没有"枚举回收站
        /// 全部记录并逐条校验"这个端点（`GET .../trash/{item_id}` 只回答
        /// 单个 item 是否可取回，是删除闸门用的，不是巡检面）。
        ///
        /// 单开一个字段而不是让 `trash_issues` 空着蒙混过去：空列表的意思
        /// 是"查过了，没问题"，跳过的意思是"没查"——把后者呈现成前者正是
        /// I5 禁止的那种静默降级（`DatasetHealth::Offline` 与
        /// `Checked{local_only: vec![]}` 必须分开，是同一条道理）。
        hub_trash_scan_skipped: bool,
    },
    /// 存储根打不开（I11：未挂载或卷身份不符），或 `http(s)://` hub 明确
    /// 回答"数据集离线"（503，`TransportError::Offline`）——数据集离线。
    /// **绝不能因此假装"本地没有未同步文件"**：那本该是 `local_only` 检查
    /// 要回答的问题，离线状态下这项检查根本没跑，必须与
    /// `Checked{local_only: vec![]}` 明确区分，不能静默退化成后者（I5、I11）。
    ///
    /// `reason` 从 `MountError` 放宽成 `String`（M2e Task 3）：离线现在有
    /// 两个来源（本地挂载失败 / 远端 503），它们没有共同的结构化类型，而
    /// 这个字段的唯一消费者是命令壳的一行 stderr 输出。
    Offline { path: String, reason: String },
    /// 扫描本地或读远端失败——真正的 IO/格式故障，与"检出了问题"是不同
    /// 性质的结果。
    CheckFailed { path: String, reason: String },
    /// 数据集本身解析不出存储根路径（评审 Important #3）——例如 hub 的
    /// `url` 用了 M2 才支持的 transport（`https://` 等）、或 `url` 为空。
    /// 这类失败**不被 `check_vault` 返回的任何 `Issue` 变体覆盖**（枚举
    /// 全部八个变体逐一核对过，没有一个与"hub URL 解析失败"相关），此前
    /// 用 `let Ok(..) = .. else { continue }` 静默跳过，doctor 会报告
    /// "零问题"，而同一状态下 `arca status`/`arca sync` 正确报错退出非零。
    /// 哪怕这里的失败与某条 `vault_issues` 语义重复（例如
    /// `NotRegistered`/`HubNotFound` 分别对应既有的
    /// `MissingDataset`/`UnknownHub`），重复报告也远好于"这个数据集在
    /// 报告里凭空消失"。
    ResolveFailed { path: String, reason: String },
}

#[derive(Debug, Default)]
pub struct DoctorReport {
    /// `arca_git::tracking::check_vault` 的原始输出，**原样收纳、不过滤**
    /// （含 `Issue::CheckIncomplete`，见模块顶部「债二」）。
    pub vault_issues: Vec<Issue>,
    pub datasets: Vec<DatasetHealth>,
}

impl DoctorReport {
    /// 是否完全干净：没有 vault 一致性问题、没有数据集离线/巡检失败/解析
    /// 失败，也没有任何数据集存在本地独有文件、`.gitignore` 反选块问题、
    /// 或清单漂移。
    pub fn is_clean(&self) -> bool {
        self.vault_issues.is_empty()
            && self.datasets.iter().all(|d| match d {
                DatasetHealth::Checked {
                    local_only,
                    ignore_issues,
                    manifest_issue,
                    trash_issues,
                    possible_lost_server_role,
                    local_trash_issues,
                    // `local_trash_usage` 刻意不参与：`server` 角色的本地
                    // 回收站非空是完全正常的状态，见该字段文档。
                    local_trash_usage: _,
                    ..
                } => {
                    local_only.is_empty()
                        && ignore_issues.is_empty()
                        && manifest_issue.is_none()
                        && trash_issues.is_empty()
                        && local_trash_issues.is_empty()
                        && !possible_lost_server_role
                }
                DatasetHealth::Offline { .. }
                | DatasetHealth::CheckFailed { .. }
                | DatasetHealth::ResolveFailed { .. } => false,
            })
    }

    /// 是否存在身份不明的数据集（I11）——命令壳据此把退出码提到 2
    /// （与 `arca fsck`/`arca sync` 的"2 = 身份不明"约定一致）。
    pub fn has_offline(&self) -> bool {
        self.datasets
            .iter()
            .any(|d| matches!(d, DatasetHealth::Offline { .. }))
    }
}

/// 对 `repo`/`registry` 描述的整个 vault 跑一次巡检。`root_override` 与
/// `arca sync --root` 同一语义（外置盘换挂载点场景），对 vault 下**所有**
/// 数据集统一生效——doctor 是全 vault 巡检，不是单数据集命令。
pub fn doctor(repo: &Repo, registry: &Registry, root_override: Option<&Path>) -> DoctorReport {
    let vault_issues = tracking::check_vault(repo, registry);
    let mut datasets = Vec::new();

    for entry in registry.datasets() {
        // 评审 Important #3：绝不静默跳过解析失败——见 `DatasetHealth::
        // ResolveFailed` 的 doc comment。
        let resolved = match dataset::resolve(repo.root(), &entry.path, root_override) {
            Ok(r) => r,
            Err(e) => {
                datasets.push(DatasetHealth::ResolveFailed {
                    path: entry.path.clone(),
                    reason: e.to_string(),
                });
                continue;
            }
        };

        // M2e Task 3：两种 hub 类型走同一套巡检——差别只在"远端状态从哪里
        // 读"与"hub 侧回收站能不能巡检"这两点上（见 `RemoteSource`）。
        // M2d 评审原话：「arcad 是 M2 的主线，而主健康检查命令对主 hub
        // 类型不工作」。
        let source = match remote_source(&resolved) {
            Ok(s) => s,
            Err(CheckFailure::Offline(reason)) => {
                datasets.push(DatasetHealth::Offline {
                    path: resolved.normalized_path,
                    reason,
                });
                continue;
            }
            // 证书 pin 不符**不是**"数据集离线"——那是"我不确定对面是不是
            // 你的 hub"，处置完全不同（离线是等卷挂回来，pin 不符是要人去
            // 查证）。报成 CheckFailed，不折进 Offline。
            Err(CheckFailure::Failed(reason)) => {
                datasets.push(DatasetHealth::CheckFailed {
                    path: resolved.normalized_path,
                    reason,
                });
                continue;
            }
        };

        let mut sink = NullSink;
        let health = match check_dataset(
            repo,
            &resolved.normalized_path,
            &resolved.dataset_dir,
            &source,
            &mut sink,
        ) {
            Ok(details) => DatasetHealth::Checked {
                path: resolved.normalized_path,
                local_only: details.local_only,
                ignore_issues: details.ignore_issues,
                manifest_issue: details.manifest_issue,
                trash_issues: details.trash_issues,
                possible_lost_server_role: details.possible_lost_server_role,
                local_trash_usage: details.local_trash_usage,
                local_trash_issues: details.local_trash_issues,
                hub_trash_scan_skipped: source.hub_trash_scan_skipped(),
            },
            Err(CheckFailure::Offline(reason)) => DatasetHealth::Offline {
                path: resolved.normalized_path,
                reason,
            },
            Err(CheckFailure::Failed(reason)) => DatasetHealth::CheckFailed {
                path: resolved.normalized_path,
                reason,
            },
        };
        datasets.push(health);
    }

    DoctorReport {
        vault_issues,
        datasets,
    }
}

/// doctor 需要向 hub 问的两件事的来源——`file://` 与 `http(s)://` 的唯一
/// 差别就在这里（M2e Task 3）。
///
/// 刻意不做成 `Box<dyn Transport>` 一把梭：本地这一侧 doctor 除了
/// `read_remote` 还要跑 `trash::scan_issues`（**逐条**巡检回收站记录，
/// 遇损坏不整体放弃），而 `Transport` trait 上没有、也不该有这个方法——
/// 它是磁盘级的诊断面，不是客户端与 hub 的协议面（`PROTOCOL.md` §1.2 里
/// 没有对应端点）。硬塞进 trait 会逼着 `http.rs` 实现一个只能返回"没查过"
/// 的方法，把"没查"伪装成"查过了没问题"。
enum RemoteSource {
    Local(StorageRoot),
    Http(crate::transport::http::HttpTransport),
}

/// [`check_dataset`] 内部的失败分流——`Offline` 必须一路传到
/// [`DatasetHealth::Offline`]，不能被折进 `CheckFailed`：前者是 I11 的
/// "身份不明/离线"（命令壳退出码 2），后者是"巡检本身出错"（退出码 1），
/// 两者的处置完全不同。
enum CheckFailure {
    Offline(String),
    Failed(String),
}

/// 建立远端来源。`Err` 一律是 **I11 意义上的离线**（本地存储根未挂载/身份
/// 不符）——`http(s)://` 这一侧建立 `HttpTransport` 只是构造一个客户端，不
/// 发任何请求，因此永远成功；它的离线要等到真的 `read_remote()` 收到 503
/// 才知道（见 `RemoteSource::read_remote`）。
fn remote_source(resolved: &dataset::ResolvedDataset) -> Result<RemoteSource, CheckFailure> {
    match &resolved.target {
        dataset::HubTarget::Local(root_path) => {
            // I11：先按已知身份打开一次，身份不符/未挂载即离线。
            StorageRoot::open(root_path, Some(&resolved.cfg.dataset_id))
                .map(RemoteSource::Local)
                .map_err(|e| CheckFailure::Offline(e.to_string()))
        }
        dataset::HubTarget::Http { base_url, tls_pin } => {
            // M2e Task 4：`https://` 先按 spec §9 决定信任配置（配了 pin 就
            // 探测 + 比对指纹，不符即拒连）。pin 相关的失败**不是**"数据集
            // 离线"——它是"我不确定对面是不是你的 hub"，两者的处置完全不同：
            // 离线是等卷挂回来，pin 不符是要人去查证。所以走 `Err` 而不是
            // `Ok(Offline)`，由 doctor 报成 `CheckFailed`（I5）。
            let trust = crate::tls::decide_for_url(base_url, tls_pin.as_deref())
                .map_err(|e| CheckFailure::Failed(e.to_string()))?;
            // doctor 是一次性巡检，不携带 sid（它不产生 journal 事件，
            // sid 闭环对它没有意义）。
            Ok(RemoteSource::Http(
                crate::transport::http::HttpTransport::with_trust(
                    base_url,
                    &resolved.cfg.dataset_id,
                    None,
                    &trust,
                ),
            ))
        }
    }
}

impl RemoteSource {
    fn read_remote(&self) -> Result<BTreeMap<String, arca_core::state::RemoteState>, CheckFailure> {
        match self {
            RemoteSource::Local(root) => {
                hub::read_remote(root).map_err(|e| CheckFailure::Failed(e.to_string()))
            }
            RemoteSource::Http(t) => {
                use crate::transport::{Transport, TransportError};
                t.read_remote().map_err(|e| match e {
                    // I11：503 是"数据集离线"，绝不能当成"库是空的"，也不能
                    // 折成一句普通的巡检失败。
                    TransportError::Offline { .. } => CheckFailure::Offline(e.to_string()),
                    other => CheckFailure::Failed(other.to_string()),
                })
            }
        }
    }

    /// hub 侧回收站的逐条巡检——只有本地存储根能做，见 [`RemoteSource`]
    /// 的文档与 [`DatasetHealth::Checked::hub_trash_scan_skipped`]。
    fn scan_hub_trash(&self) -> Result<Vec<trash::TrashIssue>, CheckFailure> {
        match self {
            RemoteSource::Local(root) => {
                trash::scan_issues(root).map_err(|e| CheckFailure::Failed(e.to_string()))
            }
            RemoteSource::Http(_) => Ok(Vec::new()),
        }
    }

    fn hub_trash_scan_skipped(&self) -> bool {
        matches!(self, RemoteSource::Http(_))
    }
}

/// [`check_dataset`] 的产出：一个数据集"打开成功之后"三项独立巡检的结果。
struct CheckedDetails {
    local_only: Vec<String>,
    ignore_issues: Vec<IgnoreIssue>,
    manifest_issue: Option<ManifestIssue>,
    trash_issues: Vec<trash::TrashIssue>,
    possible_lost_server_role: bool,
    local_trash_usage: Option<crate::local_trash::Usage>,
    local_trash_issues: Vec<trash::TrashIssue>,
}

/// 扫描本地 + 读远端 +（评审新增）实测 `.gitignore` 反选块 + 比对清单与
/// 基线。任何一步真正的 IO/格式故障都整体报错（`Err(String)`），不是"检出
/// 了问题"那种正常但需要报告的结果。
fn check_dataset(
    repo: &Repo,
    normalized_path: &str,
    dataset_dir: &Path,
    source: &RemoteSource,
    sink: &mut dyn arca_format::trace::TraceSink,
) -> Result<CheckedDetails, CheckFailure> {
    let scan_result =
        scan::scan_dataset(dataset_dir, sink).map_err(|e| CheckFailure::Failed(e.to_string()))?;
    let remote = source.read_remote()?;
    let local_only = scan_result
        .files
        .keys()
        .filter(|p| !remote.contains_key(p.as_str()))
        .cloned()
        .collect();

    let ignore_issues = check_ignore_block(repo, normalized_path).map_err(CheckFailure::Failed)?;
    let manifest_issue = check_manifest(dataset_dir).map_err(CheckFailure::Failed)?;
    let trash_issues = source.scan_hub_trash()?;
    let possible_lost_server_role = check_lost_server_role(dataset_dir);
    let (local_trash_usage, local_trash_issues) =
        check_local_trash(dataset_dir).map_err(CheckFailure::Failed)?;

    Ok(CheckedDetails {
        local_only,
        ignore_issues,
        manifest_issue,
        trash_issues,
        possible_lost_server_role,
        local_trash_usage,
        local_trash_issues,
    })
}

/// M2e Task 1：巡检工作区侧本地回收站——占用概况 + 逐条损坏记录。
///
/// 两步的分工与 hub 侧 `trash::scan_issues`/`trash::list` 完全一致：先逐条
/// 巡检（不因一条坏记录整体放弃、点名具体文件），只有在**一条都不坏**时
/// 才去算占用（`local_trash::usage` 内部走 `list`，遇损坏记录会整体报错）。
/// 有损坏记录时 usage 给 `None`——绝不用一个"看起来很健康的 0"掩盖"这个
/// 目录此刻读不出来"（I5）。
///
/// 保留期判断用的是**当前墙上时钟**（`clock::now_rfc3339`）。这是 doctor
/// 里唯一一处读系统时钟的地方，刻意没有把 `now` 提升成 `doctor()` 的参数：
/// 它只影响 `Usage::expired` 这一个纯提示性的计数（"未来 `arca gc` 会把
/// 几条列进候选"），不参与 `is_clean`、不影响退出码、不驱动任何销毁——
/// 没有一条决策依赖它的确定性。
fn check_local_trash(
    dataset_dir: &Path,
) -> Result<(Option<crate::local_trash::Usage>, Vec<trash::TrashIssue>), String> {
    let issues = crate::local_trash::scan_issues(dataset_dir).map_err(|e| e.to_string())?;
    if !issues.is_empty() {
        return Ok((None, issues));
    }
    let usage = crate::local_trash::usage(
        dataset_dir,
        &crate::clock::now_rfc3339(),
        trash::DEFAULT_RETENTION_DAYS,
    )
    .map_err(|e| e.to_string())?;
    Ok((Some(usage), issues))
}

/// 评审 Minor #1：`<dataset>/.arca/client/trash/` 非空、但
/// `<dataset>/.arca/client/role.toml` 缺失——见 `DatasetHealth::Checked::
/// possible_lost_server_role` 文档的完整论证。这里直接拼路径而不是复用
/// `role`/`local_trash` 模块的私有常量：两个模块都没有导出它们，doctor 只
/// 关心磁盘上这两个相对路径是否存在，不需要走它们各自的读写语义（尤其是
/// `role::read` 会把"缺失"吸收成默认值——doctor 恰恰要分辨的正是"缺失"
/// 这件事本身，不能吸收掉）。
fn check_lost_server_role(dataset_dir: &Path) -> bool {
    let role_file = dataset_dir.join(".arca/client/role.toml");
    if role_file.exists() {
        return false;
    }
    let trash_dir = dataset_dir.join(".arca/client/trash");
    fs::read_dir(&trash_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// 实测 `.gitignore` 反选块（评审 Important #1，模块顶部「债三」）：断言
/// `git check-ignore --no-index` 的**实际结果**，不看标记块文本是否"看起来
/// 还在"。三条断言对应 CLAUDE.md 点名的三种后果：
/// 1. 受管二进制必须被忽略（否则整个数据集会被误提交进 git）；
/// 2. `.arca/dataset.toml`/`.arca/manifest` 绝不能被忽略（否则协作者拿不到
///    清单）；
/// 3. `.arca/client/`（设备本地投影）必须被忽略。
fn check_ignore_block(repo: &Repo, normalized_path: &str) -> Result<Vec<IgnoreIssue>, String> {
    let mut issues = Vec::new();

    let managed_probe = format!("{normalized_path}/{IGNORE_PROBE}");
    match repo.check_ignore_no_index(&managed_probe) {
        Ok(true) => {}
        Ok(false) => issues.push(IgnoreIssue::ManagedNotIgnored {
            probe: managed_probe,
        }),
        Err(e) => return Err(e.to_string()),
    }

    for meta in ["dataset.toml", "manifest"] {
        let path = format!("{normalized_path}/.arca/{meta}");
        match repo.check_ignore_no_index(&path) {
            Ok(false) => {}
            Ok(true) => issues.push(IgnoreIssue::MetadataIgnored { path }),
            Err(e) => return Err(e.to_string()),
        }
    }

    let client_probe = format!("{normalized_path}/.arca/client/{IGNORE_PROBE}");
    match repo.check_ignore_no_index(&client_probe) {
        Ok(true) => {}
        Ok(false) => issues.push(IgnoreIssue::ClientNotIgnored {
            probe: client_probe,
        }),
        Err(e) => return Err(e.to_string()),
    }

    Ok(issues)
}

/// 比对 `.arca/manifest` 与当前基线（评审 Important #4，模块顶部「债四」）。
/// 基线是本地投影（I9：可抛弃），清单是从它渲染出的、进 git 的行式镜像——
/// 两者理应逐路径一致；`sync` 现在每次收尾都会重新生成清单，这里独立巡检
/// 一遍以覆盖"用旧版本二进制同步过""清单被人手工改过"这两类既有漂移。
fn check_manifest(dataset_dir: &Path) -> Result<Option<ManifestIssue>, String> {
    let loaded = baseline::load(dataset_dir).map_err(|e| e.to_string())?;
    let mut expected: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for (path, state) in loaded.iter() {
        if let BaseState::Present { hash, size, .. } = state {
            expected.insert(path.clone(), (hash.to_text(), *size));
        }
    }

    let manifest_path = dataset_dir.join(".arca").join("manifest");
    let text = match fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(if expected.is_empty() {
                // 从未同步过（基线为空）——清单本就不该存在，不是漂移。
                None
            } else {
                Some(ManifestIssue::Missing)
            });
        }
        Err(e) => return Err(format!("读取 {} 失败：{e}", manifest_path.display())),
    };
    let manifest = match Manifest::parse(&text) {
        Ok(m) => m,
        Err(e) => return Ok(Some(ManifestIssue::Corrupt(e.to_string()))),
    };

    let mut actual: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for entry in manifest.entries() {
        actual.insert(entry.path.clone(), (entry.hash.to_text(), entry.size));
    }

    if actual == expected {
        return Ok(None);
    }

    let mut only_in_manifest: Vec<String> = actual
        .keys()
        .filter(|p| !expected.contains_key(p.as_str()))
        .cloned()
        .collect();
    let mut only_in_baseline: Vec<String> = expected
        .keys()
        .filter(|p| !actual.contains_key(p.as_str()))
        .cloned()
        .collect();
    // 路径在两侧都有、但哈希/大小不同——同一个路径的两种取值互相冲突，
    // 双侧都列出，不细分是"清单落后"还是"基线落后"。
    for (path, expected_value) in &expected {
        if let Some(actual_value) = actual.get(path) {
            if actual_value != expected_value {
                only_in_manifest.push(path.clone());
                only_in_baseline.push(path.clone());
            }
        }
    }
    only_in_manifest.sort();
    only_in_manifest.dedup();
    only_in_baseline.sort();
    only_in_baseline.dedup();

    Ok(Some(ManifestIssue::Drift {
        only_in_manifest,
        only_in_baseline,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::register::{self, RegisterOptions};
    use crate::sync;
    use crate::vault::GITARCA_FILE;
    use arca_format::model::Actor;
    use arca_format::trace::NullSink;
    use std::fs;
    use std::process::Command;

    fn 建仓库(dir: &Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .expect("需要可用的 git")
                .success();
            assert!(ok, "git {args:?} 失败");
        }
    }

    fn actor() -> Actor {
        Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        }
    }

    /// 引导一个已 register 数据集对应的存储根（不跑 sync），供只关心
    /// "存储根已打开、不涉及内容是否同步"的巡检测试（`.gitignore`/清单
    /// 漂移等）复用，避免每条测试都重复这一段样板。
    fn 引导存储根(vault_dir: &Path) {
        let vault = crate::vault::open(vault_dir).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text = fs::read_to_string(vault_dir.join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        arca_store::root::StorageRoot::create(&root_path, &cfg.dataset_id, "2026-08-08T09:00:00Z")
            .unwrap();
    }

    /// 建一个已 register 的数据集，返回 (vault_dir, store_dir)。
    fn 建已登记的数据集(files: &[(&str, &[u8])]) -> (tempfile::TempDir, tempfile::TempDir) {
        let vault_dir = tempfile::tempdir().unwrap();
        建仓库(vault_dir.path());
        fs::write(vault_dir.path().join(GITARCA_FILE), "schema = 1\n").unwrap();
        fs::create_dir_all(vault_dir.path().join("assets")).unwrap();
        for (rel, content) in files {
            let full = vault_dir.path().join("assets").join(rel);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
        }

        let store_dir = tempfile::tempdir().unwrap();
        let root_path = store_dir.path().join("root");
        register::register(
            vault_dir.path(),
            RegisterOptions {
                path: "assets",
                hub_name: "home",
                hub_instance_id: None,
                hub_url: Some(&format!("file://{}", root_path.display())),
                root_hint: None,
                dataset_id: None,
            },
        )
        .unwrap();

        (vault_dir, store_dir)
    }

    #[test]
    fn 干净的vault且已同步的数据集没有任何问题() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);

        // 引导存储根并把文件同步上去（doctor 不负责引导，需要一个已存在的
        // 存储根——与 adopt/sync 分工一致）。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text =
            fs::read_to_string(vault_dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        let store_root = arca_store::root::StorageRoot::create(
            &root_path,
            &cfg.dataset_id,
            "2026-08-08T09:00:00Z",
        )
        .unwrap();
        let mut sink = NullSink;
        sync::sync(
            &vault_dir.path().join("assets"),
            &store_root,
            &actor(),
            &mut sink,
        )
        .unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        assert!(report.vault_issues.is_empty());
        assert!(report.is_clean(), "{report:?}");
        assert!(!report.has_offline());
    }

    #[test]
    fn 未同步的本地文件被检出为local_only() {
        let (vault_dir, _store_dir) =
            建已登记的数据集(&[("never-synced.bin", b"precious")]);

        // 只引导存储根（相当于 adopt 之前），不跑 sync——模拟"数据集已登记，
        // 但本地文件从未上传过一次"的场景，正是 `git clean -xdf` 会造成
        // 数据丢失的那种状态。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text =
            fs::read_to_string(vault_dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        arca_store::root::StorageRoot::create(&root_path, &cfg.dataset_id, "2026-08-08T09:00:00Z")
            .unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        assert_eq!(report.datasets.len(), 1);
        match &report.datasets[0] {
            DatasetHealth::Checked {
                path, local_only, ..
            } => {
                assert_eq!(path, "assets");
                assert_eq!(local_only, &vec!["never-synced.bin".to_string()]);
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(!report.is_clean());
    }

    #[test]
    fn 未引导的存储根报告offline而不是假装干净() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        // 存储根从未被 create 过——挂载点缺失（I11）。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let report = doctor(&vault.repo, &vault.registry, None);

        assert_eq!(report.datasets.len(), 1);
        assert!(matches!(report.datasets[0], DatasetHealth::Offline { .. }));
        assert!(report.has_offline());
        assert!(!report.is_clean(), "离线数据集绝不能被判定为干净");
    }

    #[test]
    fn check_incomplete会体现在vault_issues里且被视为不干净() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        // 先在 .git 还在时打开 vault（Repo::open 需要它），再破坏 .git——
        // 让 check_vault 内部后续的 git 调用失败，产出 CheckIncomplete。
        // 与 tracking.rs 里同名场景的测试同一顺序。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        fs::remove_dir_all(vault_dir.path().join(".git")).unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        assert!(
            report
                .vault_issues
                .iter()
                .any(|i| matches!(i, Issue::CheckIncomplete { .. })),
            "{:?}",
            report.vault_issues
        );
        assert!(!report.is_clean());
    }

    /// 评审 Important #1 的核心复现测试：`.gitignore` 被清空后，受管二进制
    /// 不再被反选块忽略——下次 `git add -A` 会把整个数据集提交进 git。
    /// `doctor` 此前完全没有代码调用 `check_ignore_no_index`，这里驱动的
    /// 正是那条从未被覆盖的路径（模块顶部「债三」）。
    #[test]
    fn gitignore被清空后doctor检出受管二进制未被忽略() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        引导存储根(vault_dir.path());
        fs::write(vault_dir.path().join(".gitignore"), "").unwrap();

        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let report = doctor(&vault.repo, &vault.registry, None);

        assert_eq!(report.datasets.len(), 1);
        match &report.datasets[0] {
            DatasetHealth::Checked { ignore_issues, .. } => {
                assert!(
                    ignore_issues
                        .iter()
                        .any(|i| matches!(i, IgnoreIssue::ManagedNotIgnored { .. })),
                    "{ignore_issues:?}"
                );
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(
            !report.is_clean(),
            "受管二进制未被忽略必须让 doctor 判定为不干净"
        );
    }

    /// 评审 Important #1：`.gitignore` 只有 `/assets/*`、没有反选行时，
    /// `.arca/dataset.toml`/`.arca/manifest` 会被一并忽略——协作者永远拿
    /// 不到这份清单/配置。这是反选块被破坏成"只剩前半段"的典型样子。
    #[test]
    fn gitignore缺反选行时doctor检出元数据被忽略() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        引导存储根(vault_dir.path());
        fs::write(vault_dir.path().join(".gitignore"), "/assets/*\n").unwrap();

        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let report = doctor(&vault.repo, &vault.registry, None);

        match &report.datasets[0] {
            DatasetHealth::Checked { ignore_issues, .. } => {
                assert!(
                    ignore_issues.iter().any(|i| matches!(
                        i,
                        IgnoreIssue::MetadataIgnored { path } if path.ends_with("manifest")
                    )),
                    "{ignore_issues:?}"
                );
                assert!(
                    ignore_issues.iter().any(|i| matches!(
                        i,
                        IgnoreIssue::MetadataIgnored { path } if path.ends_with("dataset.toml")
                    )),
                    "{ignore_issues:?}"
                );
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(!report.is_clean());
    }

    /// 评审 Important #3 的核心复现测试：hub 的 `url` 用了本二进制读不懂的
    /// transport（这里用 `ftp://`——`https://` 自 M2e Task 4 起已经受支持，
    /// 不再是"不认识的 transport"了）。`register` 本身会拒绝这种 url（见
    /// `register.rs` 的 `未知transport仍被拒绝`），但既有 `.gitarca` 完全
    /// 可能是手工改过、或来自更新版本客户端写入的。
    /// `doctor` 此前用 `let Ok(resolved) = .. else { continue }` 把这种
    /// 数据集整个静默跳过，报告"零问题"；同一状态下 `arca status` 正确
    /// 报错退出非零——这个测试钉住两者必须一致。
    #[test]
    fn hub_url不支持的transport时doctor报resolvefailed而不是静默跳过() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        // 绕开 register 的校验，直接手工把 .gitarca 里的 url 改成 M1 不
        // 支持的 transport——模拟"既有 .gitarca 被手工改过/来自未来版本"。
        let gitarca_path = vault_dir.path().join(crate::vault::GITARCA_FILE);
        let text = fs::read_to_string(&gitarca_path).unwrap();
        let patched = text.replace("file://", "ftp://");
        assert!(patched.contains("ftp://"), "测试前置条件：替换应生效");
        fs::write(&gitarca_path, patched).unwrap();

        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let report = doctor(&vault.repo, &vault.registry, None);

        assert_eq!(
            report.datasets.len(),
            1,
            "解析失败的数据集绝不能从报告里凭空消失"
        );
        assert!(
            matches!(report.datasets[0], DatasetHealth::ResolveFailed { .. }),
            "{:?}",
            report.datasets[0]
        );
        assert!(!report.is_clean());
    }

    /// 评审 Important #4 的核心复现测试：`.arca/manifest` 与基线不一致
    /// （清单被人手工改过，或用旧版本二进制同步过遗留的漂移）——`sync`
    /// 现在每次收尾都会重新生成清单，但已经存在的漂移状态需要 `doctor`
    /// 独立巡检出来，不能指望"下次 sync 会自愈"掩盖过去。
    #[test]
    fn 清单与基线不一致时doctor检出漂移() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);

        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text =
            fs::read_to_string(vault_dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        let store_root = arca_store::root::StorageRoot::create(
            &root_path,
            &cfg.dataset_id,
            "2026-08-08T09:00:00Z",
        )
        .unwrap();
        let mut sink = NullSink;
        sync::sync(
            &vault_dir.path().join("assets"),
            &store_root,
            &actor(),
            &mut sink,
        )
        .unwrap();
        assert!(
            vault_dir.path().join("assets/.arca/manifest").is_file(),
            "测试前置条件：sync 应该已经生成清单"
        );

        // 手工弄脏清单：清空条目，只留头部——模拟"清单被人手工改过"或
        // "旧版本二进制遗留的漂移"，基线仍然记着 a.txt，清单却什么都没有。
        fs::write(
            vault_dir.path().join("assets/.arca/manifest"),
            "#%arca-manifest v1\n",
        )
        .unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        match &report.datasets[0] {
            DatasetHealth::Checked { manifest_issue, .. } => {
                assert!(manifest_issue.is_some(), "应检出清单与基线的漂移");
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(!report.is_clean());
    }

    /// 评审 Minor 的复现测试：`.arca/trash/` 里一条损坏的 `.meta` 记录
    /// 此前完全没有代码路径能点名——`trash::list()` 遇到它就整体报错，
    /// 让整个数据集的删除与 `restore --list` 永久失效，但没有任何诊断指出
    /// 到底是哪个文件坏的。`doctor` 现在用 `trash::scan_issues` 单独巡检，
    /// 必须能点名具体是哪个 `trash_id`。
    #[test]
    fn trash里损坏的meta被doctor点名具体文件() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);

        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text =
            fs::read_to_string(vault_dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        let store_root = arca_store::root::StorageRoot::create(
            &root_path,
            &cfg.dataset_id,
            "2026-08-08T09:00:00Z",
        )
        .unwrap();
        let mut sink = NullSink;
        sync::sync(
            &vault_dir.path().join("assets"),
            &store_root,
            &actor(),
            &mut sink,
        )
        .unwrap();

        // 手工放一个文件名合法、内容损坏的 .meta（与 trash.rs 同名测试同一
        // 手法）——不需要真的先删除任何文件，doctor 的巡检面向的是磁盘上
        // 已经存在的记录，与它们是怎么来的无关。
        let phantom_id = crate::trash::TrashId::parse(&"a".repeat(32)).unwrap();
        fs::write(
            root_path.join(format!(".arca/trash/{phantom_id}.meta")),
            "不是合法json",
        )
        .unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        match &report.datasets[0] {
            DatasetHealth::Checked { trash_issues, .. } => {
                assert_eq!(trash_issues.len(), 1, "{trash_issues:?}");
                assert!(
                    trash_issues[0].file_name.contains(&phantom_id.to_string()),
                    "应点名具体的 trash_id：{:?}",
                    trash_issues[0]
                );
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(
            !report.is_clean(),
            "损坏的 trash 记录必须让 doctor 判定为不干净"
        );
    }

    /// 评审 Minor #1 的核心复现测试：本地回收站（`.arca/client/trash/`）
    /// 非空、但角色声明（`.arca/client/role.toml`）缺失——大概率是
    /// `role.toml` 意外丢失（不是用户主动把 server 降级成 client，那种情况
    /// 不会往 trash/ 里留东西）。`doctor` 此前完全不认识角色，这里驱动的是
    /// 从未被覆盖的检测路径。
    #[test]
    fn 本地回收站非空但role_toml缺失时doctor检出可能丢失的server声明() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        引导存储根(vault_dir.path());

        // 手工模拟"本地回收站里留有条目，但 role.toml 不在了"——不需要真的
        // 跑一轮 server 角色的 DeleteLocal，doctor 的检测只看这两个路径此刻
        // 是否存在，与它们是怎么来的无关（与
        // `trash里损坏的meta被doctor点名具体文件` 同一手法）。
        let local_trash_dir = vault_dir.path().join("assets/.arca/client/trash");
        fs::create_dir_all(&local_trash_dir).unwrap();
        fs::write(local_trash_dir.join("deadbeef.data"), "曾经存在过的内容").unwrap();
        // role.toml 故意不写——模拟"从未写过，或写过之后丢了"。

        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let report = doctor(&vault.repo, &vault.registry, None);

        assert_eq!(report.datasets.len(), 1);
        match &report.datasets[0] {
            DatasetHealth::Checked {
                possible_lost_server_role,
                ..
            } => {
                assert!(
                    *possible_lost_server_role,
                    "本地回收站非空但 role.toml 缺失，应被检出为可能丢失了 server 声明"
                );
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(
            !report.is_clean(),
            "可能丢失的 server 声明必须让 doctor 判定为不干净"
        );
    }

    /// 反例：本地回收站不存在（正常的 client 角色数据集，从没触发过
    /// server 角色的 DeleteLocal）——`role.toml` 缺失本身完全正常（默认
    /// 角色就是 client，见 `role.rs`），不该被误报成"可能丢失了声明"。用
    /// "干净的vault且已同步的数据集没有任何问题" 同一套已同步、无 local_only
    /// 噪音的前置条件，这样能顺带断言 `report.is_clean()`——只要
    /// `possible_lost_server_role` 被误报成 `true`，这个断言就会失败。
    #[test]
    fn 本地回收站为空时role_toml缺失不被误报() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);

        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text =
            fs::read_to_string(vault_dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        let store_root = arca_store::root::StorageRoot::create(
            &root_path,
            &cfg.dataset_id,
            "2026-08-09T09:00:00Z",
        )
        .unwrap();
        let mut sink = NullSink;
        sync::sync(
            &vault_dir.path().join("assets"),
            &store_root,
            &actor(),
            &mut sink,
        )
        .unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        match &report.datasets[0] {
            DatasetHealth::Checked {
                possible_lost_server_role,
                ..
            } => {
                assert!(
                    !possible_lost_server_role,
                    "没有本地回收站条目时不该误报丢失了 server 声明"
                );
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(report.is_clean(), "{report:?}");
    }

    // -----------------------------------------------------------------
    // M2e Task 1：本地回收站的可见性
    // -----------------------------------------------------------------

    /// 「让它可见」：本地回收站非空时，doctor 必须报出条目数、实际占用与
    /// 最老一条——这个目录此前是纯只写的，没有任何诊断命令看得见它。
    /// 同时断言它**不影响** `is_clean()`：`server` 角色下这里有东西是完全
    /// 正常的状态，不是一个"问题"。
    #[test]
    fn doctor报告本地回收站的占用与最老条目且不因此判定为不干净() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        let dataset_dir = vault_dir.path().join("assets");

        // 用真实的 `local_trash::move_to_trash` 造两条记录（不手工拼字节，
        // 这样格式与生产路径一定一致）。
        for (rel, content, at) in [
            ("old.bin", &b"1234"[..], "2026-01-02T00:00:00Z"),
            ("new.bin", &b"56"[..], "2026-08-08T00:00:00Z"),
        ] {
            let src = dataset_dir.join(rel);
            fs::write(&src, content).unwrap();
            crate::local_trash::move_to_trash(
                &dataset_dir,
                &src,
                rel,
                arca_format::model::ItemId::from_bytes([0x5a; 16]),
                at,
            )
            .unwrap()
            .unwrap();
        }

        // 同步 a.txt，让这个数据集在其它维度上是干净的——这样
        // `is_clean()` 的断言只可能被本地回收站这一项破坏。
        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let entry = &vault.registry.datasets()[0];
        let hub = vault.registry.hub(&entry.hub).unwrap();
        let root_path = crate::vault::resolve_hub_root(hub, None).unwrap();
        let cfg_text =
            fs::read_to_string(vault_dir.path().join("assets/.arca/dataset.toml")).unwrap();
        let cfg = arca_format::dataset::DatasetConfig::parse(&cfg_text).unwrap();
        let store_root = arca_store::root::StorageRoot::create(
            &root_path,
            &cfg.dataset_id,
            "2026-08-09T09:00:00Z",
        )
        .unwrap();
        let mut sink = NullSink;
        sync::sync(&dataset_dir, &store_root, &actor(), &mut sink).unwrap();
        // 角色声明也写上，否则会触发 `possible_lost_server_role`（回收站
        // 非空 + role.toml 缺失）那条独立的告警，干扰 is_clean 断言。
        crate::role::write(&dataset_dir, crate::role::Role::Server).unwrap();

        let report = doctor(&vault.repo, &vault.registry, None);
        match &report.datasets[0] {
            DatasetHealth::Checked {
                local_trash_usage,
                local_trash_issues,
                ..
            } => {
                let usage = local_trash_usage
                    .as_ref()
                    .expect("记录健康时占用统计必须能算出来");
                assert_eq!(usage.entries, 2);
                assert_eq!(usage.bytes, 6, "应是两份 .data 的实际字节数之和");
                assert_eq!(
                    usage.oldest_deleted_at.as_deref(),
                    Some("2026-01-02T00:00:00Z")
                );
                assert!(local_trash_issues.is_empty());
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(
            report.is_clean(),
            "本地回收站非空是 server 角色的正常状态，绝不能因此判定为不干净：{report:?}"
        );
    }

    /// 损坏的本地回收站 `.meta` 必须被**点名**（哪个文件），且占用统计给
    /// `None` 而不是一个"看起来很健康的 0"——后者会让用户以为回收站是空的。
    #[test]
    fn 本地回收站里损坏的meta被doctor点名且占用统计不伪造成零() {
        let (vault_dir, _store_dir) = 建已登记的数据集(&[("a.txt", b"hello")]);
        引导存储根(vault_dir.path());

        let local_trash_dir = vault_dir.path().join("assets/.arca/client/trash");
        fs::create_dir_all(&local_trash_dir).unwrap();
        let phantom = crate::trash::TrashId::parse(&"b".repeat(32)).unwrap();
        fs::write(
            local_trash_dir.join(format!("{phantom}.meta")),
            "不是合法json",
        )
        .unwrap();

        let vault = crate::vault::open(vault_dir.path()).unwrap();
        let report = doctor(&vault.repo, &vault.registry, None);
        match &report.datasets[0] {
            DatasetHealth::Checked {
                local_trash_usage,
                local_trash_issues,
                ..
            } => {
                assert_eq!(local_trash_issues.len(), 1, "{local_trash_issues:?}");
                assert!(
                    local_trash_issues[0]
                        .file_name
                        .contains(&phantom.to_string()),
                    "应点名具体的 trash_id：{:?}",
                    local_trash_issues[0]
                );
                assert!(
                    local_trash_usage.is_none(),
                    "记录读不出来时绝不能伪造一个 0 占用"
                );
            }
            other => panic!("应为 Checked，实得 {other:?}"),
        }
        assert!(
            !report.is_clean(),
            "损坏的本地回收站记录必须让 doctor 判定为不干净"
        );
    }
}
