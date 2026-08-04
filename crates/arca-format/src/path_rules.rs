//! 相对路径规则：规范化、禁用字符、跨平台等价性。
//!
//! 客户端与 hub 必须对路径规则跑同一段代码（两端共用纪律，spec §3）。
//! Tab 与换行禁止（清单分隔依赖此约束，§4.4.1）；大小写与 Unicode 规范化
//! 需明确定义（macOS NFD / Windows 大小写不敏感）。
//!
//! 参考 lazync：`shared/src/nc_path_rules.pas`（继承其规则集与边界处理）。
//!
//! TODO(M0)：规则集定义、规范化函数、校验函数、golden vectors。
