//! [`HttpTransport`]：[`super::Transport`] 的 `http://` 实现（M2c Task 5，
//! `docs/superpowers/plans/2026-08-08-m2c-journal-longpoll.md`）。
//!
//! # 为什么是 `ureq`，不是 `reqwest`
//!
//! spec §3.1「客户端零常驻」是形态约束：`arca-cli` 是一次性进程，不能为了发
//! 几个 HTTP 请求就启动一个 tokio 运行时（M2c Global Constraints：「若选的
//! HTTP 库强制异步，停下报告，那需要换库不是妥协」）。`reqwest` 即便用它的
//! `blocking` feature，内部仍然会起一个隐藏的 tokio runtime（它的阻塞客户端
//! 是在异步客户端外面包一层同步门面，运行时本身并没有消失，只是不需要调用方
//! 自己管理）；`ureq` 不是这样——它从骨子里就是同步实现，直接在 `std::net`
//! 的 `TcpStream` 上收发字节，**不链接任何异步运行时**，`cargo tree -p
//! arca-cli -i tokio` 报"找不到这个包"能直接证明这一点（不是"tokio 被裁剪掉
//! 了某些 feature"，是这棵依赖树里压根不存在 tokio）。
//!
//! `default-features = false`：默认 feature 会带上 `rustls`（给 `https://`
//! 用），这一版只用明文 `http://` 在本机测试（TLS 是 M2e 的部署问题，见
//! `PROTOCOL.md` §1.2 顶部按语），不需要为一个用不到的 TLS 栈多背依赖。
//!
//! # 网络故障 vs 协议错误：两种不同的失败，两种不同的处置
//!
//! 见 [`super::TransportError::class`] 的文档——这个方法就是本模块存在的
//! 意义之一：`agent`（或任何调用方）只要看 `class()`，不需要理解每个
//! `TransportError` 变体的具体含义，就知道该退避重试（[`TransportError::Network`]）
//! 还是该停下报告给人（[`TransportError::Offline`]，I11：数据集离线不是
//! "这次请求恰好失败了"）还是该去看代码（[`TransportError::Protocol`]）。
//! `412`/`409` 从不走这条路径——它们是 [`CommitOutcome::Conflict`]/
//! [`CommitOutcome::IdentityMismatch`]，`Ok` 的变体，不是 `Err`（与
//! `local.rs` 完全一致的形状，见 `transport/mod.rs::CommitOutcome` 文档）。
//!
//! # 流式读：`read_content_into` 真的不整份缓冲
//!
//! 与服务端 C2 修复（`arcad/src/api.rs::put_file`，600MB PUT 曾让 RSS 涨到
//! 1.86GB）同一条纪律的客户端镜像——`ureq::Body::as_reader()` 返回一个
//! `impl Read`，直接从 TCP 连接上边读边写，`io::copy` 用固定大小的栈上缓冲
//! 搬运，不整份读进 `Vec<u8>` 再写出去。

use super::{
    BatchOutcome, CommitOutcome, CommitRequest, Recoverable, RenameRequest, TombstoneRequest,
    Transport, TransportError,
};
use arca_chunk::hash::ContentHash;
use arca_core::state::RemoteState;
use arca_format::model::{ItemId, VersionId};
use arca_format::trace::Sid;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::time::Duration;
use ureq::http::StatusCode;
use ureq::Body;

/// 单次请求的默认超时——连接 + 收发 + 等响应整体不超过这个时长。`arcad`
/// 自己的 `REQUEST_TIMEOUT`（`arcad/src/storage.rs`）是 300 秒（给大文件
/// PUT 留够时间），客户端这一侧钉一个更宽松但仍然有限的值，避免一次连不上
/// 的请求把 `arca sync` 挂到天荒地老——真正连不上时应该较快地报
/// [`TransportError::Network`]，让调用方决定要不要退避重试，而不是一直等。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// GET 响应体在内存中缓冲的上限——**评审 I2/I3**：`ureq::Body::read_json`/
/// `read_to_vec` 默认把响应体截断在 10MB（`ureq` 内部常量 `MAX_BODY_SIZE`），
/// 这个默认值此前从未被本模块显式覆盖过：`GET .../state` 每条约 249 字节，
/// 约 42,000 个文件（"个人照片库"完全在范围内）就会撞上这个上限——而且
/// 触发方式是把 `ureq::Error::BodyExceedsLimit` 悄悄折进
/// `TransportError::Protocol`（`class=Bug`，"去看代码"），真实原因却是"库
/// 太大了"，诊断信息完全误导；`read_by_hash`/`read_range` 同样受这个默认值
/// 拖累，而它们恰恰是给大文件用的原语（`arca cat <hash>`、Range 续传）。
/// 与服务端 `arcad::api::MAX_BODY_BYTES`（单次 `PUT` 请求体上限）取同一个
/// 量级：GET 这一侧允许缓冲的响应体理应能装下服务端愿意接受的最大一次
/// 写入，两端不能有一侧比另一侧窄。`file://` 的 `LocalTransport` 没有这个
/// 限制（本地文件系统读取不经过这层缓冲上限），这正是评审点名的"同一个
/// 数据集 file:// 能同步、http:// 不能"分叉的根源。
const MAX_RESPONSE_BODY_BYTES: u64 = 256 * 1024 * 1024;

/// 路径分段的百分号编码字符集：字母数字与 `-_.~`（RFC 3986 未预留字符）
/// 之外全部编码——`/` 本身是分隔符，不在这个字符集里编码，由调用方在分段
/// 之间保留（`PROTOCOL.md` §1.2「通用约定」：「`/` 是路径分隔符本身、不
/// 编码，其余字符……按 RFC 3986 百分号编码」）。
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| utf8_percent_encode(seg, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// `http://` 传输：包一个复用连接池的 [`ureq::Agent`] + 目标数据集坐标。
///
/// `base_url` 不含末尾 `/`（构造时裁剪，见 [`HttpTransport::new`]）；
/// `dataset_id` 是 32 位小写十六进制（`arca_format::hub_layout::FormatJson`
/// 同一编码纪律）。`sid`：本次会话的 trace sid（M2c Task 4 sid 闭环），
/// `None` 时不发 `Arca-Session` 头（PROTOCOL.md §1.2：缺失是正常情形，
/// 不是错误）。
pub struct HttpTransport {
    agent: ureq::Agent,
    base_url: String,
    dataset_id: String,
    sid: Option<Sid>,
}

impl HttpTransport {
    /// 明文 `http://`（或"信任配置由调用方另行决定"）的构造。`https://`
    /// 请用 [`HttpTransport::with_trust`]——它才会把 pin 过的证书装进
    /// TLS 配置。
    pub fn new(base_url: &str, dataset_id: &str, sid: Option<Sid>) -> Self {
        Self::build(base_url, dataset_id, sid, None)
    }

    /// 带 TLS 信任配置的构造（M2e Task 4，spec §9）——`trust` 由
    /// [`crate::tls::decide`] 产出：
    ///
    /// - [`crate::tls::Trust::PublicRoots`]：走 ureq 默认的公共根
    ///   （WebPki）。公网签发的证书静默通过；自签名握手失败，由命令壳用
    ///   [`crate::tls::explain_handshake_failure`] 补上"你需要 pin"的诊断。
    /// - [`crate::tls::Trust::PinnedCert`]：**只信任这一张证书**。注意这
    ///   仍然是完整的 rustls 校验（链、有效期、SAN/主机名全查），只是信任
    ///   锚点换成了这一张——不是"关掉校验再自己比一次哈希"。
    pub fn with_trust(
        base_url: &str,
        dataset_id: &str,
        sid: Option<Sid>,
        trust: &crate::tls::Trust,
    ) -> Self {
        let tls = match trust {
            crate::tls::Trust::PublicRoots => None,
            crate::tls::Trust::PinnedCert(der) => {
                let cert = ureq::tls::Certificate::from_der(der).to_owned();
                Some(
                    ureq::tls::TlsConfig::builder()
                        .root_certs(ureq::tls::RootCerts::new_with_certs(&[cert]))
                        .build(),
                )
            }
        };
        Self::build(base_url, dataset_id, sid, tls)
    }

    fn build(
        base_url: &str,
        dataset_id: &str,
        sid: Option<Sid>,
        tls: Option<ureq::tls::TlsConfig>,
    ) -> Self {
        // `http_status_as_error(false)`：4xx/5xx 也要拿到 `Ok(Response)`——
        // 本模块要读它们的结构化响应体（412 的 base/theirs/yours、409 的
        // claimed/actual_item_id、503 的 code/message），不能让 ureq 提前
        // 把它们折叠成一个不带响应体的 `Err`。真正的 `Err` 因此只代表"这次
        // 请求压根没有走完"（连不上/超时/协议解析失败等），与 [`TransportError::class`]
        // 的分类原则完全对齐。
        let builder = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(DEFAULT_TIMEOUT));
        let builder = match tls {
            Some(t) => builder.tls_config(t),
            None => builder,
        };
        Self {
            agent: ureq::Agent::new_with_config(builder.build()),
            base_url: base_url.trim_end_matches('/').to_string(),
            dataset_id: dataset_id.to_string(),
            sid,
        }
    }

    fn dataset_url(&self, suffix: &str) -> String {
        format!(
            "{}/v1/datasets/{}{}",
            self.base_url, self.dataset_id, suffix
        )
    }

    fn file_url(&self, path: &str) -> String {
        self.dataset_url(&format!("/files/{}", encode_path(path)))
    }

    /// `Arca-Session` 头——M2c Task 4 sid 闭环：本次会话的 trace sid 原样
    /// 放进这个头，服务端校验格式后记入 journal 事件的 `actor.session`
    /// （`PROTOCOL.md` §1.2/§5.2）。`sid` 本身来自
    /// [`Sid::parse`]/[`Sid::new`]，构造时已经保证格式合法，这里不需要
    /// 再校验一遍——服务端仍然会独立校验（HTTP 是不可信输入的入口，不能
    /// 只信"我们自己的客户端不会发错"）。
    fn with_session<T>(&self, rb: ureq::RequestBuilder<T>) -> ureq::RequestBuilder<T> {
        match &self.sid {
            Some(sid) => rb.header("Arca-Session", sid.as_str()),
            None => rb,
        }
    }

    /// 发送请求、拿到响应——把 `ureq::Error`（这次请求压根没走完）翻译成
    /// [`TransportError::Network`]/[`TransportError::Protocol`]，与 4xx/5xx
    /// 这类"走完了、但服务端说不"的响应分开（那些由各调用点自己按响应体
    /// 结构翻译，见模块文档「网络故障 vs 协议错误」）。
    fn map_send_error(e: ureq::Error) -> TransportError {
        use ureq::Error;
        match e {
            // 真正的网络层瞬时故障：连不上、DNS 解析失败、连接被对端重置、
            // 各类超时、底层协议帧解析失败（服务端异常关闭连接等）——退避
            // 重试往往就好了。
            Error::Io(io_err) => TransportError::Network {
                reason: io_err.to_string(),
            },
            Error::Timeout(t) => TransportError::Network {
                reason: format!("请求超时：{t:?}"),
            },
            Error::HostNotFound => TransportError::Network {
                reason: "无法解析主机名".to_string(),
            },
            Error::ConnectionFailed => TransportError::Network {
                reason: "连接失败".to_string(),
            },
            Error::Protocol(p) => TransportError::Network {
                reason: format!("HTTP 协议层错误：{p}"),
            },
            // 其余（`BadUri`/`Http`/`TooManyRedirects`/`RedirectFailed`/
            // `BodyExceedsLimit` 等）都源自客户端自己构造的请求不合规、或
            // 服务端行为超出协议约定——不是"这次网络不稳"，是需要有人看
            // 代码的情形（`class()` 为 `Bug`）。
            other => TransportError::Protocol {
                message: format!("{other}"),
            },
        }
    }

    /// 503（数据集离线，I11）的统一识别——`PROTOCOL.md` §1.2：任何端点都
    /// 可能因为存储根未挂载/身份不符而返回 503，响应体 `{"code":"mount.absent"
    /// 或"mount.identity_mismatch","message":"..."}`。所有调用点先过这一关，
    /// 不需要在每个端点各自重复判断。
    fn check_offline(status: StatusCode, body: &mut Body) -> Option<TransportError> {
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return None;
        }
        let message = body
            .as_reader()
            .read_to_string_lossy()
            .unwrap_or_else(|| "数据集离线（响应体读取失败）".to_string());
        Some(TransportError::Offline { message })
    }
}

/// [`ureq::BodyReader`] 没有直接的"读成字符串、读不出来就给个占位符"方法
/// （`read_to_string` 要求合法 UTF-8 才成功）——503 的响应体理应总是我们
/// 自己的 JSON，但读取本身仍可能因为连接中途断开而失败，这时候不能让"读
/// 错误消息"这件事本身又抛出一个新错误，吞掉原始的离线信号。
trait ReadLossy {
    fn read_to_string_lossy(&mut self) -> Option<String>;
}

impl ReadLossy for ureq::BodyReader<'_> {
    fn read_to_string_lossy(&mut self) -> Option<String> {
        let mut buf = Vec::new();
        self.read_to_end(&mut buf).ok()?;
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

// ---------------------------------------------------------------------------
// 响应体解析：与 `arcad/src/api.rs` 的序列化形状逐一对应
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StateEntryWire {
    path: String,
    item_id: String,
    version_id: String,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    state: String,
}

fn parse_version_id(text: &str) -> Option<VersionId> {
    let (timestamp, random) = text.split_once('-')?;
    VersionId::new(timestamp, random).ok()
}

fn state_entry_to_remote(wire: StateEntryWire) -> Option<(String, RemoteState)> {
    let item_id = ItemId::parse(&wire.item_id).ok()?;
    let version_id = parse_version_id(&wire.version_id)?;
    let state = match wire.state.as_str() {
        "present" => RemoteState::Present {
            item_id,
            version_id,
            hash: ContentHash::parse(wire.hash.as_deref()?).ok()?,
            size: wire.size?,
        },
        "tombstoned" => RemoteState::Tombstoned {
            item_id,
            version_id,
        },
        _ => return None,
    };
    Some((wire.path, state))
}

/// `412`/`409` 结构化响应体里的 `theirs` 字段 → [`RemoteState`]——与
/// `arcad::api::theirs_json` 序列化的形状逐一对应（`null` → `Absent`，
/// `{"tombstoned":true,...}` → `Tombstoned`，否则 → `Present`）。
fn parse_theirs(value: &serde_json::Value) -> Option<RemoteState> {
    if value.is_null() {
        return Some(RemoteState::Absent);
    }
    let item_id = ItemId::parse(value.get("item_id")?.as_str()?).ok()?;
    let version_id = parse_version_id(value.get("version_id")?.as_str()?)?;
    if value.get("tombstoned").and_then(|v| v.as_bool()) == Some(true) {
        return Some(RemoteState::Tombstoned {
            item_id,
            version_id,
        });
    }
    let hash = ContentHash::parse(value.get("hash")?.as_str()?).ok()?;
    let size = value.get("size")?.as_u64()?;
    Some(RemoteState::Present {
        item_id,
        version_id,
        hash,
        size,
    })
}

/// 412 响应体解析失败——响应体形状与协议不符（服务端/客户端版本不一致，
/// 或响应体压根不是合法 JSON），走 [`TransportError::Protocol`]（`Bug`），
/// 不是网络故障也不是数据集离线，需要有人去看代码/版本匹配。
fn conflict_outcome(
    body: &serde_json::Value,
    expected_parent: Option<VersionId>,
) -> Result<CommitOutcome, TransportError> {
    let theirs =
        body.get("theirs")
            .and_then(parse_theirs)
            .ok_or_else(|| TransportError::Protocol {
                message: format!("412 响应体 theirs 字段解析失败：{body}"),
            })?;
    Ok(CommitOutcome::Conflict {
        expected_parent,
        actual: theirs,
    })
}

fn identity_mismatch_outcome(body: &serde_json::Value) -> Result<CommitOutcome, TransportError> {
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| TransportError::Protocol {
            message: format!("409 响应体 path 字段缺失：{body}"),
        })?
        .to_string();
    let claimed_item_id = body
        .get("claimed_item_id")
        .and_then(|v| v.as_str())
        .and_then(|s| ItemId::parse(s).ok())
        .ok_or_else(|| TransportError::Protocol {
            message: format!("409 响应体 claimed_item_id 字段解析失败：{body}"),
        })?;
    let actual_item_id = body
        .get("actual_item_id")
        .and_then(|v| v.as_str())
        .and_then(|s| ItemId::parse(s).ok());
    Ok(CommitOutcome::IdentityMismatch {
        path,
        claimed_item_id,
        actual_item_id,
    })
}

/// 响应体读取失败的翻译——**评审 I2/I3**：先识别"超过本地缓冲上限"这一种
/// （[`ureq::Error::BodyExceedsLimit`]，`.into_io()`/`From<io::Error>` 往返
/// 无损，见 `ureq::body::limit::LimitReader` 的实现），映射为
/// [`TransportError::Io`]（`class=NeedsHuman`：需要人评估这个数据集的规模
/// 是否超出了这个操作当前能处理的范围，不是网络抖动——退避重试不会让
/// 响应变小；也不是代码 bug——请求本身完全合规，只是响应太大）；其余读取
/// 失败（连接中途断开、JSON 语法错误等）保持原有的 `Protocol`
/// （`class=Bug`）不变，那些确实是"这次交互不符合协议契约"。
fn body_read_error(context: &str, e: ureq::Error) -> TransportError {
    match e {
        ureq::Error::BodyExceedsLimit(limit) => TransportError::Io {
            path: context.to_string(),
            reason: format!(
                "响应体超过本地缓冲上限 {limit} 字节——数据集/文件的规模超出了这个操作\
                 当前能处理的范围，不是网络故障也不是协议错误（评审 I2/I3）"
            ),
        },
        other => TransportError::Protocol {
            message: format!("{context} 响应体读取失败：{other}"),
        },
    }
}

fn read_json_body(
    resp: &mut ureq::http::Response<Body>,
) -> Result<serde_json::Value, TransportError> {
    resp.body_mut()
        .read_to_vec()
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| TransportError::Protocol {
            message: "响应体不是合法 JSON".to_string(),
        })
}

impl Transport for HttpTransport {
    fn read_remote(&self) -> Result<BTreeMap<String, RemoteState>, TransportError> {
        let mut resp = self
            .with_session(self.agent.get(self.dataset_url("/state")))
            .call()
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        if status != StatusCode::OK {
            return Err(TransportError::Protocol {
                message: format!("GET .../state 返回意外状态码 {status}"),
            });
        }
        // 评审 I2：把 ureq 默认的 10MB 上限提到 `MAX_RESPONSE_BODY_BYTES`，
        // 并把"超过上限"与其它读取失败区分开（`body_read_error` 文档）。
        let entries: Vec<StateEntryWire> = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BODY_BYTES)
            .read_json()
            .map_err(|e| body_read_error("GET .../state", e))?;
        let mut out = BTreeMap::new();
        for entry in entries {
            let path_for_error = entry.path.clone();
            let (path, state) =
                state_entry_to_remote(entry).ok_or_else(|| TransportError::Protocol {
                    message: format!("GET .../state 条目 {path_for_error:?} 形状不合法"),
                })?;
            out.insert(path, state);
        }
        Ok(out)
    }

    fn list(&self) -> Result<Vec<String>, TransportError> {
        Ok(self.read_remote()?.into_keys().collect())
    }

    fn read_content(&self, path: &str) -> Result<Vec<u8>, TransportError> {
        let mut buf = Vec::new();
        self.read_content_into(path, &mut buf)?;
        Ok(buf)
    }

    fn read_content_into(&self, path: &str, out: &mut dyn Write) -> Result<u64, TransportError> {
        let mut resp = self
            .with_session(self.agent.get(self.file_url(path)))
            .call()
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        if status == StatusCode::NOT_FOUND {
            return Err(TransportError::Io {
                path: path.to_string(),
                reason: "远端没有这个路径的内容（404）".to_string(),
            });
        }
        if status != StatusCode::OK {
            return Err(TransportError::Protocol {
                message: format!("GET .../files/{path} 返回意外状态码 {status}"),
            });
        }
        // 流式：`BodyReader` 直接从连接上读，`io::copy` 用固定大小的栈上
        // 缓冲搬运，不整份读进内存（模块文档「流式读」一节）。
        let mut reader = resp.body_mut().as_reader();
        io::copy(&mut reader, out).map_err(|e| TransportError::Io {
            path: path.to_string(),
            reason: e.to_string(),
        })
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>, TransportError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = start + len - 1;
        let mut resp = self
            .with_session(self.agent.get(self.file_url(path)))
            .header("range", format!("bytes={start}-{end}"))
            .call()
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(TransportError::Io {
                path: path.to_string(),
                reason: format!(
                    "Range 请求 [{start}, {}) 未获得 206（实得 {status}）",
                    start + len
                ),
            });
        }
        // 评审 I3：把 ureq 默认的 10MB 上限提到 `MAX_RESPONSE_BODY_BYTES`
        // ——`read_range` 恰恰是给大文件续传用的原语，不能比 `file://` 那侧
        // 先天窄一大截。
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BODY_BYTES)
            .read_to_vec()
            .map_err(|e| body_read_error(path, e))?;
        // 评审 I4：服务端按 RFC 9110 把越界的 `end` 钳到文件末尾、正常回
        // 206（服务端行为本身没错），但这意味着响应体可能比请求的 `len`
        // 短——`local.rs::read_range` 对越界请求报 `Io`（`read_exact` 读不满
        // 就失败），`http.rs` 此前从未校验返回长度是否等于请求的 `len`，
        // 两侧因此在"越界 Range"上行为不一致：同一份 2 字节文件请求 100
        // 万字节，`file://` 报错、`http://` 静默返回 2 字节——续传场景下
        // 一次静默短读就是一个被截断的文件，绝不能悄悄放过（trait 文档
        // 明写"越界应映射为 `Io`"）。
        if bytes.len() as u64 != len {
            return Err(TransportError::Io {
                path: path.to_string(),
                reason: format!(
                    "Range 请求 [{start}, {}) 期望 {len} 字节，实得 {}——响应被截断\
                     （评审 I4：服务端把越界的 end 钳到文件末尾，客户端必须校验\
                     实际返回长度，不能静默接受短读）",
                    start + len,
                    bytes.len()
                ),
            });
        }
        Ok(bytes)
    }

    fn read_by_hash(&self, hash: ContentHash) -> Result<Option<Vec<u8>>, TransportError> {
        let mut resp = self
            .with_session(
                self.agent
                    .get(self.dataset_url(&format!("/blobs/{}", encode_path(&hash.to_text())))),
            )
            .call()
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        match status {
            // 评审 I3：同一处上限提升——`read_by_hash` 是 `arca cat <hash>`
            // 的传输层原语，天然面向"想要拿到某个哈希对应的完整内容"，
            // 不能被 ureq 的默认 10MB 拦在半路。
            StatusCode::OK => resp
                .body_mut()
                .with_config()
                .limit(MAX_RESPONSE_BODY_BYTES)
                .read_to_vec()
                .map(Some)
                .map_err(|e| body_read_error(&format!("GET .../blobs/{}", hash.to_text()), e)),
            StatusCode::NOT_FOUND => Ok(None),
            other => Err(TransportError::Protocol {
                message: format!("GET .../blobs/{} 返回意外状态码 {other}", hash.to_text()),
            }),
        }
    }

    fn commit(&self, req: &CommitRequest) -> Result<CommitOutcome, TransportError> {
        let rb = self.with_session(self.agent.put(self.file_url(&req.path)));
        let rb = match &req.parent {
            Some(v) => rb.header("if-match", v.as_str()),
            None => rb.header("if-none-match", "*"),
        };
        let mut resp = rb
            .header("arca-item-id", req.item_id.to_hex())
            .header("arca-version-id", req.version_id.as_str())
            .header("arca-mtime", &req.mtime)
            .content_type("application/octet-stream")
            .send(req.bytes.as_slice())
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        match status {
            StatusCode::CREATED | StatusCode::OK => Ok(CommitOutcome::Committed {
                item_id: req.item_id,
                version_id: req.version_id.clone(),
            }),
            StatusCode::PRECONDITION_FAILED => {
                let body = read_json_body(&mut resp)?;
                conflict_outcome(&body, req.parent.clone())
            }
            StatusCode::CONFLICT => {
                let body = read_json_body(&mut resp)?;
                identity_mismatch_outcome(&body)
            }
            other => Err(TransportError::Protocol {
                message: format!("PUT .../files/{} 返回意外状态码 {other}", req.path),
            }),
        }
    }

    fn commit_batch(&self, reqs: &[CommitRequest]) -> Result<BatchOutcome, TransportError> {
        if reqs.is_empty() {
            return Ok(BatchOutcome::Committed(Vec::new()));
        }
        use base64::Engine;
        let entries: Vec<serde_json::Value> = reqs
            .iter()
            .map(|r| {
                json!({
                    "path": r.path,
                    "item_id": r.item_id.to_hex(),
                    "version_id": r.version_id.as_str(),
                    "parent": r.parent.as_ref().map(|v| v.as_str()),
                    "mtime": r.mtime,
                    "content_base64": base64::engine::general_purpose::STANDARD.encode(&r.bytes),
                })
            })
            .collect();
        let mut resp = self
            .with_session(self.agent.put(self.dataset_url("/batch")))
            .content_type("application/json")
            .send_json(serde_json::Value::Array(entries))
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        match status {
            StatusCode::OK => {
                let results: Vec<(ItemId, VersionId)> = reqs
                    .iter()
                    .map(|r| (r.item_id, r.version_id.clone()))
                    .collect();
                Ok(BatchOutcome::Committed(results))
            }
            StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT | StatusCode::BAD_REQUEST => {
                let body = read_json_body(&mut resp)?;
                let index = body.get("index").and_then(|v| v.as_u64()).ok_or_else(|| {
                    TransportError::Protocol {
                        message: format!("批量提交失败响应缺少 index：{body}"),
                    }
                })? as usize;
                let outcome = match status {
                    StatusCode::PRECONDITION_FAILED => {
                        let req = reqs.get(index).ok_or_else(|| TransportError::Protocol {
                            message: format!("批量提交响应的 index {index} 超出请求条目数"),
                        })?;
                        conflict_outcome(&body, req.parent.clone())?
                    }
                    StatusCode::CONFLICT => identity_mismatch_outcome(&body)?,
                    _ => {
                        return Err(TransportError::Protocol {
                            message: format!("批量提交请求本身不合法：{body}"),
                        })
                    }
                };
                Ok(BatchOutcome::Rejected { index, outcome })
            }
            other => Err(TransportError::Protocol {
                message: format!("PUT .../batch 返回意外状态码 {other}"),
            }),
        }
    }

    fn tombstone(&self, req: &TombstoneRequest) -> Result<CommitOutcome, TransportError> {
        let mut resp = self
            .with_session(self.agent.delete(self.file_url(&req.path)))
            .header("if-match", req.parent.as_str())
            .header("arca-item-id", req.item_id.to_hex())
            .call()
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        match status {
            StatusCode::NO_CONTENT => Ok(CommitOutcome::Committed {
                item_id: req.item_id,
                version_id: req.parent.clone(),
            }),
            StatusCode::PRECONDITION_FAILED => {
                let body = read_json_body(&mut resp)?;
                conflict_outcome(&body, Some(req.parent.clone()))
            }
            StatusCode::CONFLICT => {
                let body = read_json_body(&mut resp)?;
                identity_mismatch_outcome(&body)
            }
            StatusCode::NOT_FOUND => Err(TransportError::Io {
                path: req.path.clone(),
                reason: "远端此刻没有这个路径，无事可删（404）".to_string(),
            }),
            other => Err(TransportError::Protocol {
                message: format!("DELETE .../files/{} 返回意外状态码 {other}", req.path),
            }),
        }
    }

    fn rename(&self, req: &RenameRequest) -> Result<CommitOutcome, TransportError> {
        let body = json!({
            "from": req.old_path,
            "to": req.new_path,
            "item_id": req.item_id.to_hex(),
            "parent": req.parent.as_str(),
        });
        let mut resp = self
            .with_session(self.agent.post(self.dataset_url("/rename")))
            .content_type("application/json")
            .send_json(body)
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        match status {
            StatusCode::OK => Ok(CommitOutcome::Committed {
                item_id: req.item_id,
                version_id: req.parent.clone(),
            }),
            StatusCode::PRECONDITION_FAILED => {
                let body = read_json_body(&mut resp)?;
                conflict_outcome(&body, Some(req.parent.clone()))
            }
            StatusCode::CONFLICT => {
                let body = read_json_body(&mut resp)?;
                identity_mismatch_outcome(&body)
            }
            other => Err(TransportError::Protocol {
                message: format!(
                    "POST .../rename（{} -> {}）返回意外状态码 {other}",
                    req.old_path, req.new_path
                ),
            }),
        }
    }

    fn recoverable(
        &self,
        item_id: ItemId,
        expected_hash: ContentHash,
    ) -> Result<Option<Recoverable>, TransportError> {
        let hex = expected_hash.to_text();
        let hex = hex.strip_prefix("blake3:").unwrap_or(&hex);
        let mut resp = self
            .with_session(
                self.agent
                    .get(self.dataset_url(&format!("/trash/{}", item_id.to_hex()))),
            )
            .query("hash", hex)
            .call()
            .map_err(Self::map_send_error)?;
        let status = resp.status();
        if let Some(e) = Self::check_offline(status, resp.body_mut()) {
            return Err(e);
        }
        match status {
            StatusCode::OK => {
                let body = read_json_body(&mut resp)?;
                let hash = body
                    .get("hash")
                    .and_then(|v| v.as_str())
                    .and_then(|s| ContentHash::parse(s).ok())
                    .ok_or_else(|| TransportError::Protocol {
                        message: format!("GET .../trash 响应体 hash 字段解析失败：{body}"),
                    })?;
                let size = body.get("size").and_then(|v| v.as_u64()).ok_or_else(|| {
                    TransportError::Protocol {
                        message: format!("GET .../trash 响应体缺少 size：{body}"),
                    }
                })?;
                Ok(Some(Recoverable { hash, size }))
            }
            StatusCode::NOT_FOUND => Ok(None),
            other => Err(TransportError::Protocol {
                message: format!("GET .../trash/{} 返回意外状态码 {other}", item_id.to_hex()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::model::Actor;
    use std::io::BufRead;
    use std::net::{TcpListener, TcpStream};

    /// 一次收到的原始请求——只解析测试断言需要的三样：请求行（拆成方法/
    /// 路径）、头（小写键，供大小写不敏感比对）、请求体原始字节。
    struct CapturedRequest {
        method: String,
        path: String,
        headers: std::collections::HashMap<String, String>,
        body: Vec<u8>,
    }

    /// 极简的单连接 HTTP/1.1 mock server——**不是** `arcad` 的替身，只用来
    /// 验证 `HttpTransport` 在真实 TCP 连接上收发的字节符合预期（请求行、
    /// 头、body、状态码解析）。`arca-cli` 是 MIT 而 `arcad` 是 AGPL-3.0-only
    /// （CLAUDE.md「许可证分层」：依赖方向单向，MIT 不能反向依赖 AGPL，
    /// 即便只是测试用的 dev-dependency），所以这里手撸最小 HTTP 解析，
    /// 不能像 `arcad` 自己的测试那样直接内嵌一个真实 `axum::Router`——
    /// 两机端到端演示（本切片另一半交付物）才是对真实 `arcad` 的验证，
    /// 走的是两个独立进程，没有这个依赖方向问题。
    ///
    /// 只接受一个连接、处理一个请求，用 `response` 原样回写（调用方负责
    /// 拼出合法的状态行/头/body，含 `Connection: close`——不实现连接池，
    /// 每个测试独立起一个 listener）。返回监听地址与捕获到的请求（阻塞
    /// 到请求到达）。
    fn serve_once(response: &'static [u8]) -> (String, std::thread::JoinHandle<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let req = read_request(&stream);
            write_response(&stream, response);
            req
        });
        (format!("http://{addr}"), handle)
    }

    fn read_request(stream: &TcpStream) -> CapturedRequest {
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut parts = request_line.trim_end().splitn(3, ' ');
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();

        let mut headers = std::collections::HashMap::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let content_length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            use std::io::Read;
            reader.read_exact(&mut body).unwrap();
        }
        CapturedRequest {
            method,
            path,
            headers,
            body,
        }
    }

    fn write_response(mut stream: &TcpStream, response: &[u8]) {
        use std::io::Write as _;
        stream.write_all(response).unwrap();
        stream.flush().unwrap();
    }

    fn actor() -> Actor {
        Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        }
    }

    #[test]
    fn commit创建成功时正确设置请求头_解析201响应() {
        let item_id = crate::ids::new_item_id();
        let v1 = crate::ids::new_version_id();
        let body = format!(
            "{{\"item_id\":\"{}\",\"version_id\":\"{}\",\"hash\":\"blake3:00\",\"size\":5}}",
            item_id.to_hex(),
            v1.as_str()
        );
        let response = format!(
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let response: &'static [u8] = Box::leak(response.into_bytes().into_boxed_slice());
        let (url, handle) = serve_once(response);

        let sid = Sid::new("20260808T090000Z", "0123456789abcdef").unwrap();
        let transport =
            HttpTransport::new(&url, "9c41000000000000000000000000abcd", Some(sid.clone()));
        let outcome = transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id,
                version_id: v1.clone(),
                parent: None,
                bytes: b"hello".to_vec(),
                mtime: "2026-08-08T09:00:00Z".to_string(),
                actor: actor(),
            })
            .unwrap();
        assert_eq!(
            outcome,
            CommitOutcome::Committed {
                item_id,
                version_id: v1
            }
        );

        let req = handle.join().unwrap();
        assert_eq!(req.method, "PUT");
        assert_eq!(
            req.path,
            format!("/v1/datasets/9c41000000000000000000000000abcd/files/a.txt")
        );
        assert_eq!(
            req.headers.get("if-none-match").map(String::as_str),
            Some("*")
        );
        assert_eq!(
            req.headers.get("arca-item-id").map(String::as_str),
            Some(item_id.to_hex()).as_deref()
        );
        assert_eq!(
            req.headers.get("arca-session").map(String::as_str),
            Some(sid.as_str())
        );
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn commit_412冲突时解析出conflict而不是err() {
        let theirs_item = crate::ids::new_item_id();
        let theirs_version = crate::ids::new_version_id();
        let body = format!(
            "{{\"code\":\"commit.stale_parent\",\"base\":null,\"theirs\":{{\"item_id\":\"{}\",\"version_id\":\"{}\",\"hash\":\"blake3:{}\",\"size\":3}},\"yours\":{{}}}}",
            theirs_item.to_hex(),
            theirs_version.as_str(),
            "0".repeat(64),
        );
        let response = format!(
            "HTTP/1.1 412 Precondition Failed\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let response: &'static [u8] = Box::leak(response.into_bytes().into_boxed_slice());
        let (url, handle) = serve_once(response);

        let transport = HttpTransport::new(&url, "9c41000000000000000000000000abcd", None);
        let stale = crate::ids::new_version_id();
        let outcome = transport
            .commit(&CommitRequest {
                path: "a.txt".to_string(),
                item_id: theirs_item,
                version_id: crate::ids::new_version_id(),
                parent: Some(stale.clone()),
                bytes: b"xyz".to_vec(),
                mtime: "t".to_string(),
                actor: actor(),
            })
            .unwrap();
        match outcome {
            CommitOutcome::Conflict {
                expected_parent,
                actual,
            } => {
                assert_eq!(expected_parent, Some(stale));
                match actual {
                    RemoteState::Present { item_id, .. } => assert_eq!(item_id, theirs_item),
                    other => panic!("应为 Present，实得 {other:?}"),
                }
            }
            other => panic!("应为 Conflict，实得 {other:?}"),
        }
        let _ = handle.join().unwrap();
    }

    #[test]
    fn read_content_into流式读出200响应体() {
        let response: &'static [u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world";
        let (url, handle) = serve_once(response);
        let transport = HttpTransport::new(&url, "9c41000000000000000000000000abcd", None);
        let mut buf = Vec::new();
        let n = transport.read_content_into("a.txt", &mut buf).unwrap();
        assert_eq!(n, 11);
        assert_eq!(buf, b"hello world");
        let req = handle.join().unwrap();
        assert_eq!(req.method, "GET");
    }

    /// **评审 I4 攻击重跑**：服务端按 RFC 9110 把越界的 `end` 钳到文件末尾、
    /// 正常回 206（一份 2 字节文件上请求 100 万字节，服务端只能给出这 2
    /// 字节）——客户端此前从不校验返回长度是否等于请求的 `len`，会把这
    /// 2 字节静默当作"这次续传成功了"，续传场景下这就是一个被截断的文件。
    /// `local.rs::read_range` 对越界请求报 `Io`（`read_exact` 读不满就
    /// 失败），修复后 `http.rs` 必须给出同样的处置，不能在这一点上比
    /// `file://` 更宽松。
    #[test]
    fn read_range响应长度与请求不符时报io错误而不是静默短读() {
        let response: &'static [u8] = b"HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Range: bytes 0-1/2\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi";
        let (url, handle) = serve_once(response);
        let transport = HttpTransport::new(&url, "9c41000000000000000000000000abcd", None);
        // 请求 100 万字节，服务端（真实 arcad）会把越界的 end 钳到文件
        // 末尾，只返回 2 字节——这里用 mock 直接固定这个响应形状。
        let err = transport.read_range("a.txt", 0, 1_000_000).unwrap_err();
        assert!(matches!(err, TransportError::Io { .. }), "实得 {err:?}");
        assert_eq!(err.class(), arca_format::trace::ErrorClass::NeedsHuman);
        let _ = handle.join().unwrap();
    }

    #[test]
    fn read_remote解析state数组() {
        let item_id = crate::ids::new_item_id();
        let version_id = crate::ids::new_version_id();
        let body = format!(
            "[{{\"path\":\"a.txt\",\"item_id\":\"{}\",\"version_id\":\"{}\",\"hash\":\"blake3:{}\",\"size\":5,\"state\":\"present\"}}]",
            item_id.to_hex(),
            version_id.as_str(),
            "1".repeat(64),
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let response: &'static [u8] = Box::leak(response.into_bytes().into_boxed_slice());
        let (url, handle) = serve_once(response);
        let transport = HttpTransport::new(&url, "9c41000000000000000000000000abcd", None);
        let remote = transport.read_remote().unwrap();
        assert!(matches!(
            remote.get("a.txt"),
            Some(RemoteState::Present { item_id: i, .. }) if *i == item_id
        ));
        let _ = handle.join().unwrap();
    }

    /// **评审 I2 攻击重跑**：`GET .../state` 响应体超过 ureq 默认的 10MB
    /// 上限（此前从未被本模块显式覆盖）——用一个刻意超长的 `path` 字段把
    /// 单条响应体撑到 11MB（超过旧默认值，远小于新上限
    /// `MAX_RESPONSE_BODY_BYTES`）。修复前这里会失败并报
    /// `TransportError::Protocol`（`class=Bug`，"去看代码"）；修复后必须
    /// 正常解析成功——`file://` 的 `LocalTransport` 从来没有这个限制，
    /// 两条传输路径不能在这里分叉。
    #[test]
    fn read_remote解析超过ureq默认10mb上限的大响应体() {
        let item_id = crate::ids::new_item_id();
        let version_id = crate::ids::new_version_id();
        // 11MB 的 path——超过 ureq 默认的 10MB 上限，远小于
        // `MAX_RESPONSE_BODY_BYTES`（256MB）。
        let huge_path = "a".repeat(11 * 1024 * 1024);
        let body = format!(
            "[{{\"path\":\"{huge_path}\",\"item_id\":\"{}\",\"version_id\":\"{}\",\"hash\":\"blake3:{}\",\"size\":5,\"state\":\"present\"}}]",
            item_id.to_hex(),
            version_id.as_str(),
            "1".repeat(64),
        );
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body.as_bytes());
        let response: &'static [u8] = Box::leak(response.into_boxed_slice());
        let (url, handle) = serve_once(response);
        let transport = HttpTransport::new(&url, "9c41000000000000000000000000abcd", None);
        let remote = transport.read_remote().unwrap();
        assert!(matches!(
            remote.get(&huge_path),
            Some(RemoteState::Present { item_id: i, .. }) if *i == item_id
        ));
        let _ = handle.join().unwrap();
    }

    #[test]
    fn 数据集离线503时返回offline类错误而不是当空库() {
        let response: &'static [u8] =
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 41\r\nConnection: close\r\n\r\n{\"code\":\"mount.absent\",\"message\":\"x\"}";
        let (url, handle) = serve_once(response);
        let transport = HttpTransport::new(&url, "9c41000000000000000000000000abcd", None);
        let err = transport.read_remote().unwrap_err();
        assert!(matches!(err, TransportError::Offline { .. }));
        assert_eq!(err.class(), arca_format::trace::ErrorClass::NeedsHuman);
        let _ = handle.join().unwrap();
    }

    #[test]
    fn 连不上时返回network类错误可重试() {
        // 127.0.0.1:1（保留端口，本机通常拒绝连接）——不需要真的起服务器
        // 就能触发连接失败，验证「网络抖动」与「协议错误」分开处理
        // （brief 原话）：这里必须是 `Retryable`，不是 `Offline`/`Protocol`。
        let transport = HttpTransport::new(
            "http://127.0.0.1:1",
            "9c41000000000000000000000000abcd",
            None,
        );
        let err = transport.read_remote().unwrap_err();
        assert_eq!(err.class(), arca_format::trace::ErrorClass::Retryable);
    }

    #[test]
    fn encode_path对非ascii与保留字符正确编码且不编码斜杠() {
        assert_eq!(encode_path("a/b.txt"), "a/b.txt");
        assert!(encode_path("京都/鸭川.png").contains('/'));
        assert!(!encode_path("a b.txt").contains(' '));
    }
}
