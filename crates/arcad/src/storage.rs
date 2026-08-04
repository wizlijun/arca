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
//! TODO(M2)：存储根管理、卷身份校验、原子写入原语。
