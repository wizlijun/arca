//! 数据集自描述 `<dataset>/.arca/dataset.toml`（spec §4.3）。
//!
//! 字段：`schema` · `dataset_id`（全局唯一，不可变）· `hub_instance_id`（认身份不认 URL）·
//! `public_base_url` / `url_style`（发布配置，§4.9）。
//!
//! 数据集随 `.arca/` 整体搬迁：身份、清单、目录卡、hub 归属全都跟着走。
//!
//! TODO(M0)：数据结构定义、TOML 解析/序列化。
