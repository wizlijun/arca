//! 占位符层的抽象边界（M3c Task 4，spec §3.1、§4.8）。
//!
//! ```text
//! 手动 CLI（基线，必须完整可用，无需任何 daemon）
//!   └─ arca-agentd（可选：自动同步）
//!        └─ 占位符层 arca-winfs / arca-macfs（可选：按需水化）
//! ```
//!
//! CLAUDE.md 那句「**占位符注册失败，必须退回全量物化**」在代码上的落点
//! 就是这个 trait：[`FullMaterialization`] 不是「测试替身」，它是
//! **Linux 与所有注册失败场景下的生产实现**，也是 CI 上唯一跑的那个。
//!
//! # 为什么现在就要这个边界
//!
//! M3d 的 CfAPI 与 M4 的 File Provider 是两套完全不同的 OS API。如果不先
//! 划出这条边界，`hydration` 的策略会被分别抄进两套回调里，然后各自漂移——
//! 「为什么 Windows 上不水化、Mac 上水化」将变成一个没人能回答的问题。
//!
//! 边界的位置也是有讲究的：trait 里**没有任何策略**，只有「把这个文件弄到
//! 本地」「把它变回占位符」「你支持什么」。判断全在 [`crate::hydration`]，
//! 两个平台共用。

// 与 `hydration` 同一条理由：本模块的公开项要等 M3d/M4 的占位符实现
// 接上才会有生产调用者。见 `hydration.rs` 顶部同一处说明。
#![allow(dead_code)]

use crate::hydration::{Decision, Intent};

/// 这个平台/这次注册到底支持什么。
///
/// **诚实地报告**是这个结构存在的全部意义：`arca status`、`bugreport`、
/// 以及策略引擎都要据此措辞。一个谎称支持占位符的实现，会让用户以为
/// 磁盘已经省下来了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// 能不能把文件变成占位符（dehydrate）。
    pub placeholders: bool,
    /// 能不能只取一段而不整文件水化（CfAPI 的 `FETCH_DATA` 区间响应）。
    /// **这一条决定了 §4.8 实现要求 2 能不能真正兑现**。
    pub ranged_fetch: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProviderError {
    /// 这个实现根本不支持这个操作——**不是失败，是能力边界**。
    /// 调用方据此降级，而不是重试。
    Unsupported {
        what: &'static str,
        why: String,
    },
    Io {
        path: String,
        reason: String,
    },
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Unsupported { what, why } => write!(f, "不支持{what}：{why}"),
            ProviderError::Io { path, reason } => write!(f, "{path}：{reason}"),
        }
    }
}

/// OS 占位符层。**实现里不许有策略**——该不该取、取多少，由
/// [`crate::hydration::decide`] 回答，两个平台共用同一段判断。
pub trait Provider {
    fn capabilities(&self) -> Capabilities;

    /// 按 `decision` 把内容弄到本地。返回**实际拉取的字节数**。
    ///
    /// 返回字节数而不是 `()` 是有意的：§6.3 第 7 条那条必过测试数的就是它。
    /// 如果一个实现「先读整个文件再切一段」返回 `FetchRange` 的长度，
    /// 测试会通过而真实故障模式原封不动——所以这里返回的必须是
    /// **真的从网络/hub 读了多少**。
    fn ensure_local(&mut self, path: &str, decision: Decision) -> Result<u64, ProviderError>;

    /// 把本地内容变回占位符，释放空间。
    fn evict(&mut self, path: &str) -> Result<(), ProviderError>;
}

/// **全量物化**：所有受管文件本来就完整躺在磁盘上。
///
/// 这是 spec §3.1 强制的降级路径，也是 Linux/CI 的一等形态（不是降级！——
/// CLAUDE.md：「Linux / CI 只用手动模式，是一等用户而非降级路径」）。
///
/// - `ensure_local` 是 no-op，恒返回 **0 字节**——内容已经在本地。
/// - `evict` **拒绝**。这一条值得写清楚：全量物化承诺的就是「本地永远有
///   完整数据」，一个会驱逐的全量物化是自相矛盾的。静默成功更糟——
///   调用方会以为空间释放了，而磁盘一个字节都没少（I5：宁可明确拒绝）。
#[derive(Debug, Default)]
pub struct FullMaterialization {
    /// 被拒绝的驱逐次数，供诊断（「这台机器上 LRU 一直在空转」）。
    refused_evictions: u64,
}

impl FullMaterialization {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn refused_evictions(&self) -> u64 {
        self.refused_evictions
    }
}

impl Provider for FullMaterialization {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            placeholders: false,
            ranged_fetch: false,
        }
    }

    fn ensure_local(&mut self, _path: &str, _decision: Decision) -> Result<u64, ProviderError> {
        // 全量物化下**任何** decision 的答案都一样：内容已经在本地，
        // 0 字节。连 `FetchFull` 也不例外——那份内容是 `arca sync` 早就
        // 下载好的，不是这里现拉的。
        Ok(0)
    }

    fn evict(&mut self, path: &str) -> Result<(), ProviderError> {
        self.refused_evictions += 1;
        Err(ProviderError::Unsupported {
            what: "驱逐（dehydrate）",
            why: format!(
                "{path}：本机运行在全量物化模式（没有 OS 占位符层）。\
                 全量物化承诺「本地永远有完整数据」，驱逐与它直接矛盾——\
                 这里明确拒绝而不是静默成功，否则调用方会以为空间释放了，\
                 而磁盘一个字节都没少"
            ),
        })
    }
}

/// 遍历一批文件、按意图决策并执行，返回**累计拉取字节数**。
///
/// §6.3 第 7 条那条必过测试的执行侧形态：它把「策略说不拉」与「实现真的
/// 没拉」串起来验——只验前者会漏掉「实现偷偷读了整个文件」这种情况。
pub fn walk<P: Provider>(
    provider: &mut P,
    files: &[(String, crate::hydration::FileFacts)],
    intent: Intent,
) -> Result<u64, ProviderError> {
    let mut total = 0u64;
    for (path, facts) in files {
        let d = crate::hydration::decide(facts, intent);
        total += provider.ensure_local(path, d)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hydration::FileFacts;

    /// 一个会**如实记账**的假占位符实现：`FetchFull` 真的记整个文件的字节，
    /// `FetchRange` 只记那一段。用它来验「策略 + 执行」串起来的效果——
    /// `FullMaterialization` 恒返回 0，单靠它验不出策略有没有问题。
    #[derive(Debug, Default)]
    struct 记账占位符 {
        sizes: std::collections::HashMap<String, u64>,
        evicted: Vec<String>,
    }

    impl Provider for 记账占位符 {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                placeholders: true,
                ranged_fetch: true,
            }
        }
        fn ensure_local(&mut self, path: &str, decision: Decision) -> Result<u64, ProviderError> {
            let size = self.sizes.get(path).copied().unwrap_or(0);
            Ok(decision.fetch_bytes(size))
        }
        fn evict(&mut self, path: &str) -> Result<(), ProviderError> {
            self.evicted.push(path.to_string());
            Ok(())
        }
    }

    fn 混合库(n: usize) -> Vec<(String, FileFacts)> {
        (0..n)
            .map(|i| {
                (
                    format!("f{i}.bin"),
                    FileFacts {
                        size: match i % 3 {
                            0 => 120 * 1024,
                            1 => 50 * 1024 * 1024,
                            _ => 900 * 1024 * 1024,
                        },
                        local: false,
                        pinned: false,
                        days_since_access: 0,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn 全量物化如实报告不支持占位符() {
        let p = FullMaterialization::new();
        let c = p.capabilities();
        assert!(!c.placeholders, "谎称支持会让用户以为磁盘省下来了");
        assert!(!c.ranged_fetch);
    }

    /// 驱逐**明确拒绝**，不是静默成功——静默成功会让调用方以为空间释放了。
    #[test]
    fn 全量物化拒绝驱逐并说明理由() {
        let mut p = FullMaterialization::new();
        match p.evict("big.mp4") {
            Err(ProviderError::Unsupported { what, why }) => {
                assert!(what.contains("驱逐"));
                assert!(why.contains("全量物化"), "{why}");
                assert!(why.contains("big.mp4"), "要点名是哪个文件：{why}");
            }
            other => panic!("必须明确拒绝，实得 {other:?}"),
        }
        assert_eq!(p.refused_evictions(), 1, "拒绝次数要留痕供诊断");
    }

    #[test]
    fn 全量物化下任何决策都不产生拉取() {
        let mut p = FullMaterialization::new();
        for d in [
            Decision::AlreadyLocal,
            Decision::NoFetch,
            Decision::FetchRange {
                start: 0,
                len: 4096,
            },
            Decision::FetchFull,
        ] {
            assert_eq!(p.ensure_local("x", d).unwrap(), 0, "{d:?}");
        }
    }

    /// **§6.3 第 7 条的执行侧形态。** 全库遍历（`Metadata`）的累计拉取
    /// 字节数为 0——而且这次是让**真的会记账的实现**去数，不是靠
    /// `FullMaterialization` 恒返回 0 蒙混过关。
    #[test]
    fn 全库索引在会记账的实现下也是零字节() {
        let files = 混合库(1000);
        let mut p = 记账占位符 {
            sizes: files.iter().map(|(k, f)| (k.clone(), f.size)).collect(),
            ..Default::default()
        };
        let total = walk(&mut p, &files, Intent::Metadata).unwrap();
        assert_eq!(total, 0, "spec §6.3 第 7 条：全库索引水化字节数必须为 0");
    }

    /// 反面：同一批文件、同样的遍历，但意图换成 `Full` → 字节数**必须暴涨**。
    /// 如果这条也是 0，说明记账实现本身失效，上一条证明不了任何事。
    #[test]
    fn 记账实现确实会记账否则上一条测试无意义() {
        let files = 混合库(10);
        let mut p = 记账占位符 {
            sizes: files.iter().map(|(k, f)| (k.clone(), f.size)).collect(),
            ..Default::default()
        };
        let total = walk(&mut p, &files, Intent::Full).unwrap();
        let expect: u64 = files.iter().map(|(_, f)| f.size).sum();
        assert_eq!(total, expect, "记账实现必须真的按字节计");
        assert!(total > 0);
    }

    /// 缩略图遍历：每个文件读前 4 KB，累计被界住——不是几百 GB。
    #[test]
    fn 缩略图遍历的累计字节被头部大小界住() {
        let files = 混合库(500);
        let mut p = 记账占位符 {
            sizes: files.iter().map(|(k, f)| (k.clone(), f.size)).collect(),
            ..Default::default()
        };
        let total = walk(&mut p, &files, Intent::Head { bytes: 4096 }).unwrap();
        assert!(
            total <= 4096 * 500,
            "缩略图遍历不该拉超过 4KB×N，实得 {total}"
        );
        assert!(total > 0, "总得读了点东西");
    }
}
