//! plumbing 命令（spec §3.2）——输出稳定、可脚本化，格式与退出码进 PROTOCOL.md §5：
//!
//! - `arca ls --json`：清单/状态枚举；
//! - `arca cat <hash>`：按哈希取内容；
//! - `arca resolve <path>`：路径 → 身份/版本；
//! - `arca state dump --json`：投影检视（SQLite 是二进制没关系，git 的 index 也是）。
//!
//! TODO(M1)。
