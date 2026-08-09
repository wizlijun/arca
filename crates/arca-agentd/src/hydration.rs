//! 分级驻留与水化调度（M3c，spec §4.8、§6.1）——防 hydration 风暴（头号体验风险）。
//!
//! spec §4.8 的设计立场一句话：
//!
//! > **占位符是给「大而冷」的文件用的，不是给所有文件用的。**
//!
//! 这一条把 iCloud/OneDrive 式体验的最大失败模式挡在门外：一篇内嵌 30 张图的
//! 笔记逐个读取 → 30 次阻塞式网络拉取；编辑器全库索引/备份/杀毒扫描 →
//! 全库水化，瞬间拉爆磁盘与带宽。
//!
//! # 本模块是纯函数，不做 IO
//!
//! 输入是「文件的元数据 + 访问意图 + 配置」，输出是「该不该取、取多少字节」。
//! 与 `arca-core` 的 sans-io 同一条理由：**两个 OS 实现（M3d 的 CfAPI、
//! M4 的 File Provider）必须跑同一段判断**，否则两个平台的行为必然分叉，
//! 而分叉之后没人能解释「为什么 Windows 上不水化、Mac 上水化」。
//!
//! # 本切片交付的是策略，不是占位符
//!
//! 没有 OS 占位符层，`client` 角色仍然全量物化，**磁盘一个字节都没省**。
//! 这里定的是「等占位符接上之后，谁该被驱逐、什么访问该拉多少」。

// 本模块的公开项在本切片里**还没有生产调用者**：策略与队列是给 M3d
// （Windows CfAPI）与 M4（macOS File Provider）两个占位符实现共用的，
// 而那两层还没落地。这不是 M2c 评审 I7 说的那种「零调用者的 API」——
// 那一条针对的是**已有实现却没人调**的接口；这里是**先定判断、后接 OS**，
// 顺序反过来会让策略散落进 CfAPI 回调、两个平台必然分叉（见模块文档）。
// 等 M3d 接上之后这个 allow 应当删掉。
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

/// 默认驻留阈值：8 MiB（spec §4.8 实现要求 1）。
///
/// 足以覆盖几乎全部内联图片与音频，使「笔记渲染永不等待网络」成为默认体验；
/// 同时 2 GB 的视频仍不占空间。
pub const DEFAULT_RESIDENT_MAX: u64 = 8 * 1024 * 1024;

/// 默认热度窗口：14 天。
pub const DEFAULT_HOT_DAYS: u32 = 14;

/// 默认水化并发上限。
///
/// 小是有意的：目标部署是家里的 NAS（spec §1.1），上行带宽通常是瓶颈。
/// 并发拉 16 个大文件不会更快，只会让**每一个**都变慢，同时把交互式请求
/// （用户刚点开的那个视频）排到后面。
pub const DEFAULT_MAX_INFLIGHT: usize = 4;

/// 访问意图——**整条防线的入口**。
///
/// spec §4.8 实现要求 2 的全部内容就是「别把前两种当成第三种」：
/// 索引器、缩略图服务、备份工具通常只 stat 或读文件头，而把它们当成
/// 「要整个文件」正是全库水化的成因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// stat / 列目录 / 读属性。**永远不需要内容。**
    Metadata,
    /// 读文件头：类型嗅探、缩略图、EXIF。只要前若干字节。
    Head { bytes: u64 },
    /// 真的要整个文件（用户双击打开、复制、上传）。
    Full,
}

/// 一个受管文件此刻的事实。**不含路径**——策略与「这是哪个文件」无关，
/// 把路径塞进来只会诱使实现按文件名做特判。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    pub size: u64,
    /// 内容此刻是否已经在本地。
    pub local: bool,
    /// 用户显式 pin 过——常驻，LRU 不得驱逐。
    pub pinned: bool,
    /// 距上次访问过去了多少天。用于热度判断。
    pub days_since_access: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub resident_max_bytes: u64,
    pub hot_days: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            resident_max_bytes: DEFAULT_RESIDENT_MAX,
            hot_days: DEFAULT_HOT_DAYS,
        }
    }
}

/// 一次访问该怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 内容已在本地，什么都不用做。
    AlreadyLocal,
    /// **不需要取任何字节**——元数据已经足够回答这次访问。
    NoFetch,
    /// 只取一段。`len` 是**上限**：实现方读到文件尾即可停。
    FetchRange { start: u64, len: u64 },
    /// 取整个文件。
    FetchFull,
}

impl Decision {
    /// 这次决策会导致拉取多少字节——`§6.3` 第 7 条那条必过测试数的就是它。
    pub fn fetch_bytes(&self, size: u64) -> u64 {
        match self {
            Decision::AlreadyLocal | Decision::NoFetch => 0,
            Decision::FetchRange { start, len } => (*len).min(size.saturating_sub(*start)),
            Decision::FetchFull => size,
        }
    }
}

/// 一次访问该取多少字节。**纯函数**。
///
/// # 这里**没有** `Policy` 参数，这是有意的
///
/// 「这次访问取不取、取多少」只取决于**意图**与**内容在不在本地**——
/// 与 8 MiB 阈值无关。阈值管的是另一个问题：**谁该常驻、谁可以被驱逐**
/// （见 [`always_resident`] 与 [`may_evict`]）。
///
/// 把两者搅在一起是个很自然的错误：会写出「小文件就算没在本地也直接
/// FetchFull」这种看似合理的规则，而它恰好破坏了 §6.3 第 7 条——
/// 一次全库 stat 会把所有小文件拉下来，字节数不再是 0。
///
/// 穷举而不留 `_ =>` 兜底——与 `arca-core` 的 18 格决策表同一手法：
/// 新增一个 `Intent` 变体时，编译器会强迫下一个人在这里明确表态，
/// 而不是让它悄悄落进某个「其它情况」的分支。
pub fn decide(file: &FileFacts, intent: Intent) -> Decision {
    match intent {
        // **永远不取内容。** 这一行就是「全库索引水化字节数 = 0」的全部实现。
        // 元数据（大小、时间戳、存在性）在本地投影里本来就有，不需要网络。
        Intent::Metadata => {
            if file.local {
                Decision::AlreadyLocal
            } else {
                Decision::NoFetch
            }
        }
        Intent::Head { bytes } => {
            if file.local {
                return Decision::AlreadyLocal;
            }
            // 请求的头部已经覆盖整个文件时，取整个更划算也更简单——
            // 一个 3 KB 的文件被要求读前 4 KB，切成 range 没有意义。
            if bytes >= file.size {
                return Decision::FetchFull;
            }
            Decision::FetchRange {
                start: 0,
                len: bytes,
            }
        }
        Intent::Full => {
            if file.local {
                Decision::AlreadyLocal
            } else {
                Decision::FetchFull
            }
        }
    }
}

/// 这个文件是否**始终本地驻留、永不 dehydrate**（spec §4.8 的分级第一层）。
pub fn always_resident(file: &FileFacts, policy: &Policy) -> bool {
    file.pinned || file.size <= policy.resident_max_bytes
}

/// LRU 驱逐候选判断：**只有「大且冷且没 pin 且已在本地」的才可能被驱逐**。
///
/// 注意这里回答的是「**可以**驱逐吗」，不是「**应该**驱逐吗」——后者取决于
/// 缓存上限还剩多少，那是调用方的账。把两个问题混在一个函数里，会让
/// 「阈值内的小文件永不驱逐」这条硬约束依赖于调用方记得先问一次。
pub fn may_evict(file: &FileFacts, policy: &Policy) -> bool {
    file.local && !always_resident(file, policy) && file.days_since_access > policy.hot_days
}

// ---------------------------------------------------------------------------
// 水化队列：并发上限 + 批量合并（spec §4.8 实现要求 3）
// ---------------------------------------------------------------------------

/// 提交一次水化请求的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submit {
    /// 已受理，这是一次新的拉取。
    Accepted,
    /// 同一路径已经在队列里或正在拉——**合并掉了**，不产生第二次拉取。
    /// 一篇内嵌 30 张图的笔记会对同一批附件产生大量重复请求。
    Merged,
    /// 队列满。**明确拒绝而不是无限堆积**——M2b/M2c 的内存上限教训：
    /// 一个没有上限的队列在压力下会把内存吃光，而那时连报错都发不出去。
    Rejected,
}

/// 水化队列。**不做 IO**：它只回答「这次请求该不该变成一次真实拉取」，
/// 真正的拉取由调用方执行完再调 [`Queue::finish`]。
#[derive(Debug)]
pub struct Queue {
    max_inflight: usize,
    capacity: usize,
    inflight: HashSet<String>,
    waiting: Vec<String>,
    /// 每个路径被合并掉的次数，供诊断（「这批请求里有多少是重复的」）。
    merged: HashMap<String, u32>,
}

impl Queue {
    pub fn new(max_inflight: usize, capacity: usize) -> Self {
        Self {
            max_inflight,
            capacity,
            inflight: HashSet::new(),
            waiting: Vec::new(),
            merged: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_INFLIGHT, 1024)
    }

    pub fn submit(&mut self, path: &str) -> Submit {
        if self.inflight.contains(path) || self.waiting.iter().any(|p| p == path) {
            *self.merged.entry(path.to_string()).or_insert(0) += 1;
            return Submit::Merged;
        }
        if self.inflight.len() + self.waiting.len() >= self.capacity {
            return Submit::Rejected;
        }
        if self.inflight.len() < self.max_inflight {
            self.inflight.insert(path.to_string());
        } else {
            self.waiting.push(path.to_string());
        }
        Submit::Accepted
    }

    /// 一次拉取完成。返回接下来该开始拉的那个路径（若队列里还有）。
    pub fn finish(&mut self, path: &str) -> Option<String> {
        self.inflight.remove(path);
        self.merged.remove(path);
        if self.inflight.len() < self.max_inflight && !self.waiting.is_empty() {
            let next = self.waiting.remove(0);
            self.inflight.insert(next.clone());
            return Some(next);
        }
        None
    }

    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    pub fn waiting_len(&self) -> usize {
        self.waiting.len()
    }

    pub fn merged_count(&self, path: &str) -> u32 {
        self.merged.get(path).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 小文件() -> FileFacts {
        FileFacts {
            size: 200 * 1024, // 200 KiB，典型内联图片
            local: false,
            pinned: false,
            days_since_access: 0,
        }
    }

    fn 大文件() -> FileFacts {
        FileFacts {
            size: 2 * 1024 * 1024 * 1024, // 2 GiB 视频
            local: false,
            pinned: false,
            days_since_access: 999,
        }
    }

    // -----------------------------------------------------------------
    // 决策表：Intent × 体积 × 是否已在本地，逐格断言
    // -----------------------------------------------------------------

    #[test]
    fn 元数据访问永远不取任何字节() {
        for mut f in [小文件(), 大文件()] {
            for local in [true, false] {
                f.local = local;
                let d = decide(&f, Intent::Metadata);
                assert_eq!(
                    d.fetch_bytes(f.size),
                    0,
                    "元数据访问绝不能产生任何拉取：{f:?} → {d:?}"
                );
            }
        }
    }

    /// 读文件头 → `FetchRange`，**不是** `FetchFull`。
    /// 「读了 4 KB 就把 2 GB 视频拉下来」正是 §4.8 要挡的故障模式。
    #[test]
    fn 读文件头只取一段而不是整个文件() {
        let f = 大文件();
        let d = decide(&f, Intent::Head { bytes: 4096 });
        assert_eq!(
            d,
            Decision::FetchRange {
                start: 0,
                len: 4096
            }
        );
        assert_eq!(d.fetch_bytes(f.size), 4096, "只该拉 4 KB");
    }

    /// 请求的头部已覆盖整个文件时取整个——切 range 没有意义，
    /// 而且会让实现多一次「读到尾了吗」的判断。
    #[test]
    fn 头部请求超过文件大小时直接取整个() {
        let f = FileFacts {
            size: 3000,
            ..小文件()
        };
        assert_eq!(
            decide(&f, Intent::Head { bytes: 4096 }),
            Decision::FetchFull
        );
    }

    #[test]
    fn 已在本地时任何意图都不再拉取() {
        let f = FileFacts {
            local: true,
            ..大文件()
        };
        for intent in [Intent::Metadata, Intent::Head { bytes: 4096 }, Intent::Full] {
            let d = decide(&f, intent);
            assert_eq!(d, Decision::AlreadyLocal, "{intent:?}");
            assert_eq!(d.fetch_bytes(f.size), 0);
        }
    }

    #[test]
    fn 完整读取大文件才取整个() {
        let f = 大文件();
        assert_eq!(decide(&f, Intent::Full), Decision::FetchFull);
        assert_eq!(decide(&f, Intent::Full).fetch_bytes(f.size), f.size);
    }

    // -----------------------------------------------------------------
    // 分级与驱逐
    // -----------------------------------------------------------------

    #[test]
    fn 阈值内的小文件始终驻留且永不可驱逐() {
        let p = Policy::default();
        let f = FileFacts {
            local: true,
            days_since_access: 9999,
            ..小文件()
        };
        assert!(always_resident(&f, &p));
        assert!(
            !may_evict(&f, &p),
            "阈值内的小文件即使一年没碰也不该被驱逐——这正是「笔记渲染零延迟」的保证"
        );
    }

    #[test]
    fn pin过的大文件永不可驱逐() {
        let p = Policy::default();
        let f = FileFacts {
            local: true,
            pinned: true,
            ..大文件()
        };
        assert!(always_resident(&f, &p));
        assert!(!may_evict(&f, &p));
    }

    #[test]
    fn 大且冷且未pin的才可能被驱逐() {
        let p = Policy::default();
        let 冷 = FileFacts {
            local: true,
            days_since_access: 30,
            ..大文件()
        };
        assert!(may_evict(&冷, &p));

        let 热 = FileFacts {
            days_since_access: 3,
            ..冷
        };
        assert!(
            !may_evict(&热, &p),
            "热度窗口内不驱逐（最近笔记的同目录附件）"
        );

        let 不在本地 = FileFacts {
            local: false, ..冷
        };
        assert!(!may_evict(&不在本地, &p), "本来就不在本地，无从驱逐");
    }

    /// 恰好等于阈值的文件算「始终驻留」——边界取闭区间，与 spec 的
    /// 「体积 ≤ 驻留阈值」字面一致。
    #[test]
    fn 恰好等于阈值算始终驻留() {
        let p = Policy::default();
        let f = FileFacts {
            size: p.resident_max_bytes,
            local: true,
            pinned: false,
            days_since_access: 9999,
        };
        assert!(always_resident(&f, &p));
        let 大一个字节 = FileFacts {
            size: p.resident_max_bytes + 1,
            ..f
        };
        assert!(!always_resident(&大一个字节, &p));
    }

    // -----------------------------------------------------------------
    // §6.3 第 7 条：全库索引水化字节数 = 0（必过）
    // -----------------------------------------------------------------

    /// 模拟 Obsidian 启动索引 / Spotlight / Windows Search / 备份 / 杀毒扫描
    /// 遍历全库：对每一个文件发一次 `Metadata`，断言**累计拉取字节为 0**。
    #[test]
    fn 全库索引水化字节数为零() {
        // 一个混合库：小图、中等 PDF、大视频、RAW，全部不在本地。
        let 库: Vec<FileFacts> = (0..1000)
            .map(|i| FileFacts {
                size: match i % 4 {
                    0 => 120 * 1024,
                    1 => 6 * 1024 * 1024,
                    2 => 800 * 1024 * 1024,
                    _ => 40 * 1024 * 1024,
                },
                local: false,
                pinned: i % 97 == 0,
                days_since_access: (i % 40) as u32,
            })
            .collect();

        let 总字节: u64 = 库
            .iter()
            .map(|f| decide(f, Intent::Metadata).fetch_bytes(f.size))
            .sum();
        assert_eq!(
            总字节, 0,
            "spec §6.3 第 7 条：全库索引/备份/杀毒遍历的水化字节数必须为 0"
        );
    }

    /// 缩略图服务遍历全库：每个文件读前 4 KB。断言**没有一个**变成整文件拉取，
    /// 且总字节数被前 4 KB 界住——而不是几百 GB。
    #[test]
    fn 缩略图遍历全库不会变成全量拉取() {
        let 库: Vec<FileFacts> = (0..500)
            .map(|i| FileFacts {
                size: 100 * 1024 * 1024 + i,
                local: false,
                pinned: false,
                days_since_access: 0,
            })
            .collect();

        let mut 总字节 = 0u64;
        for f in &库 {
            let d = decide(f, Intent::Head { bytes: 4096 });
            assert!(
                !matches!(d, Decision::FetchFull),
                "读 4 KB 头部绝不能变成整文件拉取：{f:?} → {d:?}"
            );
            总字节 += d.fetch_bytes(f.size);
        }
        assert_eq!(总字节, 4096 * 500, "总量应当被头部大小界住");
    }

    /// **反面断言。** 上面两条必过测试如果只看「决策」而策略被改坏了，
    /// 它们必须失败——不验反面的必过测试还是假绿（本项目已被同一形态咬过四次）。
    ///
    /// 这里直接构造「策略坏掉」的等价物：把 `Metadata` 当成 `Full` 去算，
    /// 断言累计字节**不是** 0。如果这条断言反而通过（即坏策略也算出 0），
    /// 说明上面那条测试的判据（`fetch_bytes`）本身就测不出东西。
    #[test]
    fn 坏掉的策略会让必过测试失败() {
        let 库: Vec<FileFacts> = (0..10)
            .map(|_| FileFacts {
                size: 100 * 1024 * 1024,
                local: false,
                pinned: false,
                days_since_access: 0,
            })
            .collect();
        // 坏策略：元数据访问也去拉整个文件。
        let 坏结果: u64 = 库
            .iter()
            .map(|f| decide(f, Intent::Full).fetch_bytes(f.size))
            .sum();
        assert!(
            坏结果 > 0,
            "判据本身失效了：把 Metadata 当成 Full 也算出 0 字节，\
             说明「全库索引水化字节数为零」那条测试证明不了任何事"
        );
    }

    // -----------------------------------------------------------------
    // 水化队列
    // -----------------------------------------------------------------

    /// 一篇内嵌 30 张图的笔记会对同一批附件产生大量重复请求——必须合并。
    #[test]
    fn 同一路径的重复请求被合并成一次拉取() {
        let mut q = Queue::with_defaults();
        assert_eq!(q.submit("video.mp4"), Submit::Accepted);
        for _ in 0..49 {
            assert_eq!(q.submit("video.mp4"), Submit::Merged);
        }
        assert_eq!(q.inflight_len(), 1, "50 次请求只该产生 1 次拉取");
        assert_eq!(q.merged_count("video.mp4"), 49);
    }

    #[test]
    fn 并发数不超过上限_其余排队() {
        let mut q = Queue::new(4, 100);
        for i in 0..10 {
            assert_eq!(q.submit(&format!("f{i}")), Submit::Accepted);
        }
        assert_eq!(q.inflight_len(), 4, "并发上限是 4");
        assert_eq!(q.waiting_len(), 6);

        // 完成一个 → 队列里下一个顶上，并发数仍是 4。
        let next = q.finish("f0").expect("应当有下一个顶上");
        assert!(next.starts_with('f'));
        assert_eq!(q.inflight_len(), 4);
        assert_eq!(q.waiting_len(), 5);
    }

    /// 队列满 → **明确拒绝**，不是无限堆积。M2b/M2c 的内存上限教训：
    /// 一个没有上限的队列在压力下会把内存吃光，而那时连报错都发不出去。
    #[test]
    fn 队列满时拒绝而不是无限堆积() {
        let mut q = Queue::new(2, 5);
        for i in 0..5 {
            assert_eq!(q.submit(&format!("f{i}")), Submit::Accepted);
        }
        assert_eq!(q.submit("f99"), Submit::Rejected);
        assert_eq!(
            q.inflight_len() + q.waiting_len(),
            5,
            "拒绝之后队列长度不该增长"
        );
    }

    #[test]
    fn 完成之后同一路径可以被重新提交() {
        let mut q = Queue::with_defaults();
        q.submit("a");
        q.finish("a");
        assert_eq!(
            q.submit("a"),
            Submit::Accepted,
            "上次已经拉完了，这是一次新的请求"
        );
    }

    #[test]
    fn 队列空时finish不panic也不凭空产生任务() {
        let mut q = Queue::with_defaults();
        assert_eq!(q.finish("从未提交过"), None);
        assert_eq!(q.inflight_len(), 0);
    }
}
