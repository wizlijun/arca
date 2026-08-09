//! `arca checkout`：把受管二进制还原到**清单说的那个版本**
//! （spec §6.3 第 10 条、§4.4.1）。
//!
//! # 它解决的是 git 与 arca 之间那道天生的缝
//!
//! 清单（`<ds>/.arca/manifest`）在 git 里，受管二进制不在。所以
//! `git checkout` 到旧提交之后：**清单变回了旧版本，而工作区的二进制还停在
//! 新版本**。两边说的不是同一件事，而 git 对此一无所知。
//!
//! spec §6.3 第 10 条把「按当时清单还原对应版本」列为必过验收，这条命令
//! 就是它的执行侧。`arca status` 负责**发现**这道缝（不发现就是静默地
//! 让用户在一个错的工作区上继续干活），本模块负责**弥合**它。
//!
//! # 为什么不能让 `arca sync` 顺手做
//!
//! `sync` 的语义是「把本地与 hub 对齐」，而这里要的是「把本地与**清单**
//! 对齐」——清单可能指向一个既不是本地当前、也不是 hub 当前的**历史**版本。
//! 把两种语义塞进一个命令，用户就再也说不清 `arca sync` 到底会把文件变成
//! 什么样。所以另起一条命令，且**默认 dry-run**（与 `arca gc`、
//! `arca import lfs` 同一条纪律：会改写用户文件的命令，先让人看清单）。
//!
//! # 它会覆盖用户的工作区文件——所以有一道闸门
//!
//! 如果本地文件既不是清单说的那份、**也不是基线记录的那份**，说明用户在
//! 上次同步之后改过它，而这些改动**还没有被任何地方保存**。这时覆盖等于
//! 销毁未保存的工作。默认拒绝，`--force` 才越过——与删除传播的四道闸门
//! 同一条精神（I3：销毁只经显式动作）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use arca_chunk::hash::ContentHash;
use arca_core::state::BaseState;
use arca_format::manifest::Manifest;

use crate::baseline;
use crate::transport::Transport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 本地已经就是清单说的那份，什么都不用做。
    AlreadyMatches,
    /// 校验通过，可以还原（dry-run 时停在这里）。
    Ready {
        hash: String,
        size: u64,
    },
    /// 已还原。
    Restored {
        hash: String,
        size: u64,
    },
    Skipped(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// hub 上找不到清单说的那个哈希。
    ///
    /// **今天这几乎总是因为「历史版本的字节从未被保留过」**，而不是被清理了：
    /// hub 的 `files/` 只放当前版本（I1 逃生舱），历史版本本该落在
    /// `.arca/chunks/`，而**块的写入侧至今没有实现**。所以任何指向旧版本的
    /// 清单都还原不了。见本模块文档末尾。
    ContentUnavailable {
        hash: String,
    },
    /// **本地有未保存的改动。** 本地内容既不是清单说的那份、也不是基线
    /// 记录的那份——覆盖它等于销毁用户还没同步过的工作。
    LocalModified {
        baseline: Option<String>,
        actual: String,
    },
    Io {
        reason: String,
    },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::ContentUnavailable { hash } => write!(
                f,
                "hub 上没有 {hash} 这个版本的内容，无法还原。\
                 **今天这几乎总是因为历史版本的字节从未被保留过**：hub 的 `files/` \
                 只放当前版本（I1 逃生舱），历史版本本该落在 `.arca/chunks/`，\
                 而块的写入侧至今没有实现。也就是说，指向旧版本的清单目前一律\
                 还原不了——这不是你的操作有问题。（另外两种可能：这份清单来自\
                 另一个 hub，或者内容被 `arca gc` 清理过。）"
            ),
            SkipReason::LocalModified { baseline, actual } => write!(
                f,
                "本地内容（{actual}）既不是清单说的那份、也不是基线记录的那份（{}）\
                 ——你在上次同步之后改过它，而这些改动**还没有被任何地方保存**。\
                 已跳过：覆盖它等于销毁你没同步过的工作。先 `arca sync` 把改动存下来，\
                 或者确认要丢弃它们之后用 `--force`",
                baseline.as_deref().unwrap_or("无记录")
            ),
            SkipReason::Io { reason } => write!(f, "{reason}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// (数据集内相对路径, 结论)，按路径排序（确定性）。
    pub files: Vec<(String, Outcome)>,
}

impl Report {
    pub fn restored(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Restored { .. }))
    }
    pub fn ready(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Ready { .. }))
    }
    pub fn skipped(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Skipped(_)))
    }
    fn count(&self, f: impl Fn(&Outcome) -> bool) -> usize {
        self.files.iter().filter(|(_, o)| f(o)).count()
    }
}

/// 按 `manifest` 把 `dataset_dir` 下的受管文件还原到清单说的版本。
///
/// `apply = false` 时只出清单，一个字节都不写。
/// `force = true` 时越过「本地有未保存改动」那道闸门。
pub fn checkout<T: Transport>(
    dataset_dir: &Path,
    manifest: &Manifest,
    transport: &T,
    apply: bool,
    force: bool,
) -> Report {
    let base: BTreeMap<String, Option<String>> = match baseline::load(dataset_dir) {
        Ok(loaded) => loaded
            .iter()
            .map(|(p, s)| {
                let h = match s {
                    BaseState::Present { hash, .. } => Some(hash.to_text()),
                    _ => None,
                };
                (p.clone(), h)
            })
            .collect(),
        // 基线读不出来 → 每条都当「基线无记录」处理。闸门因此更严
        // （任何与清单不符的本地内容都会被判成「有未保存改动」），
        // 保守的一侧正是我们要的。
        Err(_) => BTreeMap::new(),
    };

    let mut report = Report::default();
    for e in manifest.entries() {
        report.files.push((
            e.path.clone(),
            one(dataset_dir, e, &base, transport, apply, force),
        ));
    }
    report.files.sort_by(|a, b| a.0.cmp(&b.0));
    report
}

fn one<T: Transport>(
    dataset_dir: &Path,
    entry: &arca_format::manifest::ManifestEntry,
    base: &BTreeMap<String, Option<String>>,
    transport: &T,
    apply: bool,
    force: bool,
) -> Outcome {
    let target: PathBuf = dataset_dir.join(&entry.path);
    let want = entry.hash.to_text();

    // 本地此刻是什么。
    let local = std::fs::read(&target).ok();
    let local_hash = local.as_ref().map(|b| ContentHash::from_bytes(b).to_text());
    if local_hash.as_deref() == Some(want.as_str()) {
        return Outcome::AlreadyMatches;
    }

    // 闸门：本地内容既不是清单说的、也不是基线记录的 → 有未保存的改动。
    // 文件不存在不算「有改动」（没有东西会被销毁），照常还原。
    if !force {
        if let Some(actual) = &local_hash {
            let recorded = base.get(&entry.path).cloned().flatten();
            if recorded.as_deref() != Some(actual.as_str()) {
                return Outcome::Skipped(SkipReason::LocalModified {
                    baseline: recorded,
                    actual: actual.clone(),
                });
            }
        }
    }

    // 内容从 hub 按哈希取——历史版本的 blob 就是这么找的。
    let bytes = match transport.read_by_hash(entry.hash) {
        Ok(Some(b)) => b,
        Ok(None) => return Outcome::Skipped(SkipReason::ContentUnavailable { hash: want }),
        Err(e) => {
            return Outcome::Skipped(SkipReason::Io {
                reason: e.to_string(),
            })
        }
    };
    // hub 给回来的东西也要验——`read_by_hash` 是按哈希寻址的，但一份被
    // 损坏的存储仍可能返回错误的字节，而我们正要拿它覆盖用户的文件。
    let actual = ContentHash::from_bytes(&bytes).to_text();
    if actual != want {
        return Outcome::Skipped(SkipReason::Io {
            reason: format!("hub 返回的内容哈希是 {actual}，与清单要的 {want} 不符——拒绝写入"),
        });
    }

    if !apply {
        return Outcome::Ready {
            hash: want,
            size: entry.size,
        };
    }

    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Outcome::Skipped(SkipReason::Io {
                reason: format!("{}：{e}", parent.display()),
            });
        }
    }
    // tmp → rename：中途崩溃要么留下旧内容、要么留下新内容，绝不留半份。
    let tmp = target.with_extension("arca-checkout-tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        return Outcome::Skipped(SkipReason::Io {
            reason: format!("{}：{e}", tmp.display()),
        });
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Outcome::Skipped(SkipReason::Io {
            reason: format!("{}：{e}", target.display()),
        });
    }
    Outcome::Restored {
        hash: want,
        size: entry.size,
    }
}
