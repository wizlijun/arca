//! 角色声明：`<dataset>/.arca/client/role.toml`（M2d Task 1）。
//!
//! 一个数据集在**这台设备**上的存储角色——`server` 承诺永久保留一份完整
//! 副本，`client` 把本地内容当成可再生缓存、不承诺永久保留空间。M2d 后续
//! 任务（角色改变时的删除执行、副本数不足告警，见
//! `.superpowers/sdd/2026-08-09-m2d-roles-volumes/`）都读这个声明来判断
//! "删掉这份本地副本安不安全"，本模块只负责它的存储与语义，不参与任何
//! 决策——决策留给后续任务与 `arca-core`（本任务不碰 `arca-core`）。
//!
//! 字节级契约见 `FORMAT.md` §9.5；默认值与"为什么这不是可丢弃投影"的理由
//! 也写在那里（I10：格式先于代码，先改文档、再改实现）。
//!
//! # 落盘位置与 git 可见性
//!
//! 落在 `.arca/client/` 下——姊妹模块 [`crate::baseline`] 同样落在这里。这个
//! 目录整体被 `arca-git` 的 `.gitignore` 反选块排除在 git 追踪之外（见
//! `crates/arca-git/src/ignore_block.rs`），role.toml 因此不需要新增忽略
//! 规则；本模块的测试用 `arca_git::repo::Repo::check_ignore_no_index`
//! 实测这一点，不只看 `.gitignore` 的文本（CLAUDE.md「已知的高危处」）。
//!
//! # 与 [`crate::baseline`] 刻意不同的错误处理策略
//!
//! `baseline` 遇到"文件存在但内容读不懂"时**吸收**成一次重置（`Ok`，带
//! `ResetReason::Corrupt`）——基线是可抛弃投影，静默重置的后果只是多做一轮
//! 全量对账，无害。role.toml **不是**可抛弃投影：它是用户显式做出的持久
//! 策略声明（尤其是 `role = server`，一个"永久保留"的承诺）。如果 [`read`]
//! 对损坏内容也照抄"吸收成默认值"的策略，`server` 声明会在文件损坏的那一刻
//! 被悄悄降级成 `client`，而调用方毫无察觉——这正是 I5「绝不猜测」要拦住的
//! 场景，也是这个模块存在的理由（详细论证见 `FORMAT.md` §9.5 的例外条款）。
//! 因此：
//!
//! - 文件**不存在** → [`read`] 返回 `Ok(`[`Role::default()`]`)`——老仓库、
//!   刚 `register`/`adopt` 完、还没人显式选过角色的数据集，都是这个正常态；
//! - 文件**存在但内容非法**（TOML 解析失败、`schema` 高于本实现已知的最高
//!   版本、`role` 字段不是 `server`/`client` 之一）→ [`read`] 返回
//!   `Err(`[`RoleError::Invalid`]`)`，绝不尽力解析、绝不悄悄当默认值处理。
//!
//! # 范围边界：M2 阶段 `client` 角色不改变任何行为
//!
//! M2 阶段的 `client` 角色**纯粹是语义声明**："我把本地视为可再生缓存"，
//! 不代表任何行为上的精简——没有占位符、没有按需 hydration，仍然是全量
//! 物化（那是 M3 `arca-winfs`/`arca-macfs` 的范围）。今天设置 `client` 角色
//! 不会让任何文件从本地消失；它只是为 M2d 后续任务预先声明"这台设备将来
//! 是否可以被纳入安全删除的判断"。

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 本实现能写出、也能读懂的最高 `role.toml` schema 版本。
const MAX_SCHEMA: u32 = 1;

const CLIENT_DIR: &str = ".arca/client";
const ROLE_FILE: &str = "role.toml";

/// 数据集在本机的存储角色，见模块顶部 doc comment。
///
/// 未声明时的默认值是 [`Role::Client`]（保守：不主动承诺永久保留空间），
/// 理由见 `FORMAT.md` §9.5。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Client,
    Server,
}

impl Role {
    /// 与 `role.toml` 里 `role` 字段的文本表示互为逆操作，见 [`Role::parse`]。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Client => "client",
        }
    }

    /// 解析 `role.toml` 里 `role` 字段的文本值。不认识的取值返回 `None`——
    /// 调用方（[`parse`] 私有函数、`arca role --set`）据此判断是"非法内容"
    /// 还是"合法但陌生的第三态"，本类型目前只有两态，二者等价，但保留
    /// `Option` 签名而非直接 panic，为未来可能的第三态留一条不 panic 的路。
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "server" => Some(Role::Server),
            "client" => Some(Role::Client),
            _ => None,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// [`read`]/[`write`] 的失败——与 [`crate::baseline::BaselineError`] 同一套
/// 纪律：区分"内容读不懂"与"IO 本身失败"，但两者在这里都必须让调用方停下来
/// （见模块顶部「与 baseline 刻意不同」一节），不像 baseline 那样把前者吸收
/// 成一次静默重置。
#[derive(Debug)]
pub enum RoleError {
    /// 文件存在，但内容不可信：TOML 解析失败、`schema` 高于本实现已知的
    /// 最高版本、或 `role` 字段不是 `server`/`client` 之一。
    Invalid { path: String, reason: String },
    /// 连"到底能不能读/写"这件事本身都不确定的真正 IO 故障（权限被拒、
    /// 路径某一级类型不对等）——非 `NotFound` 的 [`std::io::Error`]。
    Io { path: String, reason: String },
}

impl fmt::Display for RoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoleError::Invalid { path, reason } => {
                write!(f, "角色声明 {path} 无法识别，已停止（不猜测）：{reason}")
            }
            RoleError::Io { path, reason } => write!(f, "角色声明 {path} 读写失败：{reason}"),
        }
    }
}

impl std::error::Error for RoleError {}

fn io_err(path: &Path, e: io::Error) -> RoleError {
    RoleError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

fn role_path(dataset_root: &Path) -> PathBuf {
    dataset_root.join(CLIENT_DIR).join(ROLE_FILE)
}

#[derive(Serialize, Deserialize)]
struct RoleWire {
    schema: u32,
    role: String,
}

/// 读取 `<dataset_root>/.arca/client/role.toml`。
///
/// **文件缺失 = 默认角色，不是错误**；**内容非法 = `Err`，绝不尽力解析或
/// 悄悄当默认值处理**——完整理由见模块顶部 doc comment。
pub fn read(dataset_root: &Path) -> Result<Role, RoleError> {
    let path = role_path(dataset_root);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Role::default()),
        Err(e) => return Err(io_err(&path, e)),
    };
    parse(&text).map_err(|reason| RoleError::Invalid {
        path: path.display().to_string(),
        reason,
    })
}

/// 落盘到 `<dataset_root>/.arca/client/role.toml`。tmp → rename（同目录内，
/// rename 原子）——与 [`crate::baseline::Baseline::save`] 同一套纪律，但
/// role.toml 不是可抛弃投影这件事不影响这里的持久化机制本身：tmp→rename
/// 已经能保证"要么写入前的旧内容、要么写入后的新内容，不会读到半份"，
/// 这正是一份持久策略声明需要的最低保证。
pub fn write(dataset_root: &Path, role: Role) -> Result<(), RoleError> {
    let path = role_path(dataset_root);
    let dir = path
        .parent()
        .expect("role_path 总在 .arca/client 目录下，必有 parent");
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;

    let wire = RoleWire {
        schema: MAX_SCHEMA,
        role: role.as_str().to_string(),
    };
    let content = toml::to_string_pretty(&wire).map_err(|e| RoleError::Io {
        path: path.display().to_string(),
        reason: format!("序列化失败：{e}"),
    })?;

    let tmp_path = dir.join(format!("{ROLE_FILE}.tmp"));
    fs::write(&tmp_path, content.as_bytes()).map_err(|e| io_err(&tmp_path, e))?;
    fs::rename(&tmp_path, &path).map_err(|e| io_err(&path, e))?;
    Ok(())
}

/// 解析 `role.toml` 全文本。任何一步失败都返回失败原因（人类可读，直接嵌进
/// [`RoleError::Invalid::reason`]），不做"哪怕某个字段读不懂也凑合返回"的
/// 宽松处理（I5、I10）。
fn parse(text: &str) -> Result<Role, String> {
    let wire: RoleWire = toml::from_str(text).map_err(|e| format!("TOML 解析失败：{e}"))?;
    if wire.schema > MAX_SCHEMA {
        return Err(format!(
            "role.toml 的 schema 版本 {} 高于本实现支持的 {MAX_SCHEMA}；请升级 arca",
            wire.schema
        ));
    }
    Role::parse(&wire.role)
        .ok_or_else(|| format!("role 字段 {:?} 不是 server/client 之一", wire.role))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_git::repo::Repo;
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

    #[test]
    fn 缺失文件返回默认角色client() {
        let dir = tempfile::tempdir().unwrap();
        let role = read(dir.path()).unwrap();
        assert_eq!(role, Role::Client);
    }

    #[test]
    fn 往返一致_server() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), Role::Server).unwrap();
        assert_eq!(read(dir.path()).unwrap(), Role::Server);
    }

    #[test]
    fn 往返一致_client() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), Role::Client).unwrap();
        assert_eq!(read(dir.path()).unwrap(), Role::Client);
    }

    #[test]
    fn 非法role字段报错而不是当默认值处理() {
        let dir = tempfile::tempdir().unwrap();
        let client_dir = dir.path().join(CLIENT_DIR);
        fs::create_dir_all(&client_dir).unwrap();
        fs::write(client_dir.join(ROLE_FILE), "schema = 1\nrole = \"peer\"\n").unwrap();

        let err = read(dir.path()).unwrap_err();
        match err {
            RoleError::Invalid { reason, .. } => {
                assert!(
                    reason.contains("peer"),
                    "原因应提及实际读到的取值：{reason}"
                )
            }
            other => panic!("应为 Invalid，实得 {other:?}"),
        }
    }

    #[test]
    fn 版本号高于已知时报错而不是当默认值处理() {
        let dir = tempfile::tempdir().unwrap();
        let client_dir = dir.path().join(CLIENT_DIR);
        fs::create_dir_all(&client_dir).unwrap();
        fs::write(
            client_dir.join(ROLE_FILE),
            "schema = 99\nrole = \"server\"\n",
        )
        .unwrap();

        let err = read(dir.path()).unwrap_err();
        match err {
            RoleError::Invalid { reason, .. } => {
                assert!(
                    reason.contains("99"),
                    "原因应提及实际读到的版本号：{reason}"
                )
            }
            other => panic!("应为 Invalid，实得 {other:?}"),
        }
    }

    #[test]
    fn 不是合法toml时报错而不是当默认值处理() {
        let dir = tempfile::tempdir().unwrap();
        let client_dir = dir.path().join(CLIENT_DIR);
        fs::create_dir_all(&client_dir).unwrap();
        fs::write(client_dir.join(ROLE_FILE), "不是合法toml{{{").unwrap();

        assert!(matches!(
            read(dir.path()).unwrap_err(),
            RoleError::Invalid { .. }
        ));
    }

    /// CLAUDE.md「已知的高危处」要求的实测，而不是只看 `.gitignore` 文本：
    /// `.arca/client/` 已被反选块整体排除在 git 追踪之外，role.toml 落在
    /// 这里理应不需要新增任何忽略规则——用
    /// `arca_git::repo::Repo::check_ignore_no_index` 实测这一点（模式取自
    /// `crates/arca-cli/src/adopt.rs:571` 与
    /// `crates/arca-cli/src/doctor.rs::check_ignore_block`）。
    #[test]
    fn client目录下的role_toml确实不被git追踪() {
        let dir = tempfile::tempdir().unwrap();
        建仓库(dir.path());
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        fs::write(
            dir.path().join(".gitignore"),
            arca_git::ignore_block::render(&["assets"]).unwrap(),
        )
        .unwrap();

        write(&dir.path().join("assets"), Role::Server).unwrap();

        let repo = Repo::open(dir.path()).unwrap();
        assert!(
            repo.check_ignore_no_index("assets/.arca/client/role.toml")
                .unwrap(),
            ".gitignore 反选规则必须匹配 role.toml 所在的 .arca/client/ 目录"
        );
    }
}
