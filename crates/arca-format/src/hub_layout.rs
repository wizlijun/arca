//! hub 存储根磁盘布局（spec §4.2）：目录常量、`format.json` 卷身份标记。
//!
//! - `files/`：逃生舱（I1），当前版本永远完整平放；
//! - `.arca/{index,items,chunks,journal,trash,uploads,tmp,locks}/`：旁路元数据；
//! - `format.json`：格式版本 + `dataset_id`——卷身份标记（I11：挂载缺失即离线，
//!   绝不把未挂载的卷当空库）。
//!
//! vault 侧 `.arca/` 与 hub 侧 `.arca/` 结构不同，须可区分（§4.3，防误绑）。
//!
//! TODO(M0)：`format.json` 结构、布局常量、两种 `.arca/` 的判别函数。
