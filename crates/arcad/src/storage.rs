//! 库存储：每数据集一个存储根，可映射到不同物理卷（spec §4.2、§4.6）。
//!
//! - `files/` 平放 current（I1 逃生舱）；`.arca/` 旁路元数据；
//! - 启动 / 挂载变更时校验卷身份：`format.json` 的 `dataset_id` 必须与
//!   hub 配置及客户端绑定请求三方一致，不符 → 数据集离线（I11），
//!   **绝不触发删除对账**；
//! - 写入走 tmp → fsync → rename；chunks 引用计数变更走 `.txn` 事务日志。
//!
//! 参考 lazync：`server/src/nc_file_library.pas`。
//!
//! # 每请求重新打开存储根，不跨请求缓存（M2b Task 3）
//!
//! [`Dataset::open`] 每次调用都重新 [`StorageRoot::open`]——不持有一个跨请求
//! 存活的 `StorageRoot`。这正是 I11「挂载缺失即离线」在服务端的落地方式：
//! 存储根被卸载、卷被换掉这件事必须在**下一次请求**就能被发现并返回 503，
//! 而不是要等到进程重启才重新校验。`format.json` 只有几十字节，重新读它
//! 的开销可忽略不计——用这点开销换来「离线状态永远反映当下」，而不是
//! 进程生命周期内的一次性快照。
//!
//! # `write_lock`：为什么服务端需要一把 arca-cli 从来不需要的锁
//!
//! `arca-cli`（单进程、一次只跑一次 `sync()`）从不需要为
//! `LocalTransport::commit`/`tombstone` 加锁——`file://` 场景下不会有第二个
//! 进程与它争抢同一次调和。`arcad` 不同：一次 HTTP 请求就是一次独立的
//! `commit`/`tombstone` 调用，多个客户端可以对同一个数据集**并发**发起
//! 请求。`LocalTransport::commit` 内部是「读当前版本 → 比较 parent → 写入」
//! 三步（见 `arca-cli::transport::local` 的实现），`arca_store::atomic`
//! 只保证第三步本身落盘的原子性，不保证三步整体不被另一个并发请求打断——
//! 两个并发 `PUT` 都可能在第一步读到同一个「当前版本」、都通过第二步的
//! CAS 比较、都各自完成第三步的写入，其中一个会静默覆盖另一个，这正是
//! I4「一切写入走 CAS」要挡住的场景，只是威胁来自服务端内部的并发而不是
//! 客户端的无条件写。[`Dataset::write_lock`] 把这三步收窄成同一数据集内
//! 的临界区（串行执行），不同数据集之间互不影响（spec §4.3.2 独立故障域）。

use crate::config::HubConfig;
use arca_store::root::{MountError, StorageRoot};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::Notify;

/// 一个数据集在本机的挂载配置 + 写入序列化锁。
pub struct Dataset {
    pub id: String,
    pub root_path: PathBuf,
    /// 见模块顶部「`write_lock`」一节。`PUT`/`DELETE` 处理器必须在调用
    /// `LocalTransport::commit`/`tombstone` 之前持有它；`GET` 不需要（读取
    /// 本就不需要与写入互斥到这个粒度，`arca_store::atomic` 的 rename 本身
    /// 对并发读者是原子可见的）。
    pub write_lock: Mutex<()>,
    /// M2c Task 3：`GET .../changes` 的 longpoll 唤醒信号——`PUT`/`DELETE`/
    /// `PUT .../batch` 每次成功写入（真正落盘的 `CommitOutcome::Committed`，
    /// 不含 `Conflict`/`IdentityMismatch`）后调用
    /// [`Dataset::notify_changed`]，唤醒所有正挂起在这个数据集上的
    /// longpoll 请求——不必等它们各自的轮询间隔到期才发现新事件。
    ///
    /// 只是"加速发现"，不是唯一的正确性来源：longpoll 循环每次醒来都会
    /// 重新打开存储根、重新读一遍 journal（`api.rs::get_changes`），所以
    /// 即便通知丢失（例如变更来自另一个 `arcad` 进程或直接的 `file://`
    /// 写入，根本不会调用这个方法）、或在没有任何等待者时调用（`Notify`
    /// 不为此保留信号），挂起的请求最终仍会在下一次轮询周期里发现新事件
    /// ——`Notify` 只是让"同一个 arcad 实例内的 PUT 唤醒挂起的 GET"这条
    /// 路径不必等到轮询间隔，不是这个端点正确性的唯一保障。
    pub changes_notify: Notify,
}

impl Dataset {
    /// 打开（重新验证）这个数据集当前的存储根——见模块顶部「每请求重新
    /// 打开」一节。`Err` 时调用方（`api.rs`）必须映射成 503，绝不当作
    /// 「这个数据集是空的」处理（I11）。
    pub fn open(&self) -> Result<StorageRoot, MountError> {
        StorageRoot::open(&self.root_path, Some(&self.id))
    }

    /// 见 [`Dataset::changes_notify`] 文档——写入成功后调用，唤醒全部当前
    /// 挂起的 longpoll 等待者。`notify_waiters`（不是 `notify_one`）：一次
    /// 写入可能与多个客户端各自挂起的 `GET .../changes` 相关，全部唤醒，
    /// 由它们各自重新判断"对我的游标而言是否真的有新事件"。
    pub fn notify_changed(&self) {
        self.changes_notify.notify_waiters();
    }
}

/// 全部已配置数据集的只读登记表，按 `dataset_id` 索引。
pub struct Registry {
    by_id: BTreeMap<String, Dataset>,
    /// M2c Task 3：longpoll 专属并发上限——独立于 `api.rs::MAX_CONCURRENT_REQUESTS`
    /// 那个全局请求并发上限，见 `api.rs::get_changes` 与
    /// `PROTOCOL.md` §1.2「`GET .../changes`：游标失效与 longpoll 的资源
    /// 上限」。跨全部数据集共享同一份配额（不是每数据集各自一份）——
    /// 一台 `arcad` 进程的总资源是共享的，隔离目标是"longpoll 不能耗尽
    /// 全局配额"，不是"每个数据集各自留一份配额"。
    pub longpoll_semaphore: tokio::sync::Semaphore,
}

/// 见 [`Registry::longpoll_semaphore`] 文档；数值本身在 `api.rs` 里还有一份
/// 供响应体/文档引用，这里作为唯一真相源。
pub const MAX_CONCURRENT_LONGPOLL: usize = 16;

impl Registry {
    /// 从 [`HubConfig`] 构建——不在这里做任何挂载检查（那是每请求 /
    /// `arcad --check` 各自的职责，见模块顶部「每请求重新打开」一节），
    /// 构建阶段只是把配置转成可查询的登记表。
    pub fn from_config(config: &HubConfig) -> Self {
        let by_id = config
            .datasets
            .iter()
            .map(|d| {
                (
                    d.id.clone(),
                    Dataset {
                        id: d.id.clone(),
                        root_path: d.path.clone(),
                        write_lock: Mutex::new(()),
                        changes_notify: Notify::new(),
                    },
                )
            })
            .collect();
        Registry {
            by_id,
            longpoll_semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_LONGPOLL),
        }
    }

    /// 按 `dataset_id` 查询——`None` 表示这个 id 根本没在 `hub.toml` 里配置过
    /// （与「配置了但挂载缺失」是两种不同的失败：前者是路由层面的「没有这个
    /// 数据集」，落 404；后者是 I11 的「数据集离线」，落 503——两者不能
    /// 折叠成同一个状态码，否则客户端没法区分「我打错了 dataset_id」与
    /// 「这个数据集我认识，但它现在不可用」）。
    pub fn get(&self, id: &str) -> Option<&Dataset> {
        self.by_id.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Dataset> {
        self.by_id.values()
    }
}

/// 单个数据集的挂载检查结果——`arcad --check`（运维排障用，只检查不起服务）
/// 与启动时的独立故障域日志共用这个形状。
pub struct CheckResult {
    pub dataset_id: String,
    pub path: PathBuf,
    /// `Ok(())` 表示健康可服务；`Err` 携带诊断文本（`MountError` 的
    /// `Display`），不是结构化错误——这里面向的是人读的运维输出，
    /// HTTP 层的结构化 `code` 映射在 `api.rs`。
    pub outcome: Result<(), String>,
}

/// 对登记表里的每个数据集做一次挂载检查（`StorageRoot::open`），不起任何
/// 服务、不改变任何状态——`arcad --check` 与启动时的一次性健康日志共用。
///
/// **一个根检查失败不影响其它根的检查**（spec §4.3.2 独立故障域）——
/// 这正是本函数返回 `Vec<CheckResult>` 而不是在第一个失败处 `Err` 提前
/// 返回的原因。
pub fn check_all(registry: &Registry) -> Vec<CheckResult> {
    registry
        .iter()
        .map(|d| CheckResult {
            dataset_id: d.id.clone(),
            path: d.root_path.clone(),
            outcome: d.open().map(|_| ()).map_err(|e| e.to_string()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatasetConfig;
    use arca_format::hub_layout::FormatJson;
    use std::fs;

    fn 造存储根(dir: &std::path::Path, dataset_id: &str) {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        let format = FormatJson {
            format: 1,
            dataset_id: dataset_id.to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    #[test]
    fn 健康根可以被打开() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let cfg = HubConfig {
            instance_id: "0".repeat(32),
            datasets: vec![DatasetConfig {
                id: id.to_string(),
                path: dir.path().to_path_buf(),
            }],
        };
        let registry = Registry::from_config(&cfg);
        assert!(registry.get(id).unwrap().open().is_ok());
    }

    #[test]
    fn 根被移走后打开失败但登记表本身不受影响() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let cfg = HubConfig {
            instance_id: "0".repeat(32),
            datasets: vec![DatasetConfig {
                id: id.to_string(),
                path: dir.path().to_path_buf(),
            }],
        };
        let registry = Registry::from_config(&cfg);
        assert!(registry.get(id).unwrap().open().is_ok());

        // 模拟卷被卸载：整个目录消失。
        fs::remove_dir_all(dir.path()).unwrap();
        assert!(matches!(
            registry.get(id).unwrap().open(),
            Err(MountError::Absent { .. })
        ));
    }

    #[test]
    fn 未知_dataset_id_返回_none() {
        let cfg = HubConfig {
            instance_id: "0".repeat(32),
            datasets: vec![],
        };
        let registry = Registry::from_config(&cfg);
        assert!(registry.get("9c41000000000000000000000000abcd").is_none());
    }

    #[test]
    fn check_all_中一个根故障不影响其它根() {
        let healthy_dir = tempfile::tempdir().unwrap();
        let healthy_id = "9c41000000000000000000000000abcd";
        造存储根(healthy_dir.path(), healthy_id);

        let broken_dir = tempfile::tempdir().unwrap();
        let broken_id = "a1b2000000000000000000000000c3d4";
        // 不造存储根——broken_dir 下没有 format.json，等价于卷未挂载。

        let cfg = HubConfig {
            instance_id: "0".repeat(32),
            datasets: vec![
                DatasetConfig {
                    id: healthy_id.to_string(),
                    path: healthy_dir.path().to_path_buf(),
                },
                DatasetConfig {
                    id: broken_id.to_string(),
                    path: broken_dir.path().to_path_buf(),
                },
            ],
        };
        let registry = Registry::from_config(&cfg);
        let results = check_all(&registry);
        assert_eq!(results.len(), 2);

        let healthy = results.iter().find(|r| r.dataset_id == healthy_id).unwrap();
        assert!(healthy.outcome.is_ok());
        let broken = results.iter().find(|r| r.dataset_id == broken_id).unwrap();
        assert!(broken.outcome.is_err());
    }

    #[test]
    fn 身份不符时打开失败() {
        let dir = tempfile::tempdir().unwrap();
        let actual_id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), actual_id);
        let expected_id = "a1b2000000000000000000000000c3d4";
        let cfg = HubConfig {
            instance_id: "0".repeat(32),
            datasets: vec![DatasetConfig {
                id: expected_id.to_string(),
                path: dir.path().to_path_buf(),
            }],
        };
        let registry = Registry::from_config(&cfg);
        assert!(matches!(
            registry.get(expected_id).unwrap().open(),
            Err(MountError::IdentityMismatch { .. })
        ));
    }
}
