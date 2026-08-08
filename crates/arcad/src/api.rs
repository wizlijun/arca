//! HTTP API（`PROTOCOL.md` §1.2）：RFC 9110 条件请求 · Range 续传 · CAS 写入。
//!
//! - ETag = BLAKE3；`If-Match` CAS，过期 → 412 + 结构化冲突体；
//! - 数据集离线（卷未挂载 / 身份不符）→ 503，绝不呈现为空库（I11）；
//! - longpoll/SSE 留给 M2c（`PROTOCOL.md` §1.2 开篇已注明），本文件只实现
//!   M2b 交付的五个端点（读三个 + 写两个）。
//!
//! # 薄壳：决策全部来自 `arca-core`/`arca-cli::transport`（M2b 三条纪律之一）
//!
//! 本文件不重新判断"该做什么"——CAS 的比较、冲突的构造，全部委托给
//! [`arca_cli::transport::local::LocalTransport`]（`arca-cli` 是 MIT，`arcad`
//! 是 AGPL，依赖方向合法，见 `Cargo.toml` 注释）：`LocalTransport::commit`/
//! `tombstone`/`read_remote`/`read_content`/`recoverable` 已经是"给定
//! `StorageRoot`，正确处理 CAS 比较与内容先于指针发布"的完整实现（M2b
//! Task 1 交付，测试覆盖见 `arca-cli/src/transport/local.rs`）。本文件只做
//! HTTP 表面：解析请求头/路径/查询参数、把结果翻译成状态码与响应体。
//!
//! # 挂载检查：每请求重新打开，见 [`crate::storage::Dataset::open`]
//!
//! 每个 handler 的第一步永远是「按 `dataset_id` 查登记表（未知 → 404）→
//! 重新打开存储根（失败 → 503）」——不缓存跨请求存活的 `StorageRoot`。
//!
//! # 写入临界区：`Dataset::write_lock`
//!
//! `PUT`/`DELETE` 在拿到已打开的存储根之后、调用 `commit`/`tombstone` 之前，
//! 必须持有 `dataset.write_lock`——见 `storage.rs` 模块文档「`write_lock`」
//! 一节：这是让"两个并发客户端提交同一路径，一个成功一个 412"这个 CAS
//! 承诺在服务端成立的唯一机制。

use crate::storage::{Dataset, Registry};
use arca_chunk::hash::ContentHash;
use arca_cli::transport::local::LocalTransport;
use arca_cli::transport::{CommitOutcome, CommitRequest, TombstoneRequest, Transport};
use arca_core::state::RemoteState;
use arca_format::hub_layout::layout;
use arca_format::model::{Actor, ItemId, Version, VersionId};
use arca_format::path_rules;
use arca_store::root::{MountError, StorageRoot};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

/// `PUT` 请求体的上限：M2b 尚未接入 CDC 分块上传（那是后续里程碑的传输优化，
/// PROTOCOL.md §2 仍是 TODO），这里整份内容作为一次 HTTP 请求体收下——
/// 服务端假设客户端是善意的，就是把安全性建在善意上（dispatch 纪律第 3
/// 条），所以必须有一个硬上限，防止恶意/失控客户端发一个宣称的
/// `Content-Length` 或干脆流式发送无穷字节把服务进程内存耗尽。1 GiB 对
/// "个人笔记/照片库"这个目标场景是宽松的上限，未来接入分块上传后这个值
/// 可以下调。
const MAX_BODY_BYTES: usize = 1024 * 1024 * 1024;

/// 构建 HTTP 路由——`state` 是全部已配置数据集的登记表（`Arc` 包裹以满足
/// axum `State` 要求 `Clone`；`Registry` 本身不需要也不应该是 `Clone`：
/// 里面的 `write_lock` 一旦被复制，"同一数据集共享同一把锁"这个前提就没了）。
pub fn router(state: Arc<Registry>) -> Router {
    Router::new()
        .route(
            "/v1/datasets/{id}/files/{*path}",
            get(get_file).put(put_file).delete(delete_file),
        )
        .route("/v1/datasets/{id}/state", get(get_state))
        .route("/v1/datasets/{id}/trash/{item_id}", get(get_trash))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// 通用：错误响应构造（PROTOCOL.md §1.2「HTTP 状态码 ↔ code」表）
// ---------------------------------------------------------------------------

fn error_body(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(json!({"code": code, "message": message.into()})),
    )
        .into_response()
}

/// 数据集不在 `hub.toml` 里配置过——路由层面的「没有这个数据集」，与「配置
/// 了但离线」是两种不同的失败，不折叠成同一个状态码（见 `storage.rs`
/// `Registry::get` 文档）。协议表未登记这个 code：这是路由前置校验，不是
/// 表里任何一个端点定义的失败态，标准 404 语义已自解释，不额外造 code。
fn unknown_dataset() -> Response {
    (StatusCode::NOT_FOUND, "unknown dataset_id").into_response()
}

/// 挂载失败 → 503（I11）。只映射协议已登记的两个 code：`IdentityMismatch`
/// 单独区分，其余（`Absent`/`Io`/`Malformed`/`BadExpectedId`）统一算
/// `mount.absent`——PROTOCOL.md §1.2「503：数据集离线」只定义了这两种，
/// 其余失败形状本质上都是「这个卷此刻不是一个可用的、身份相符的存储根」。
fn mount_error_response(e: &MountError) -> Response {
    let code = match e {
        MountError::IdentityMismatch { .. } => "mount.identity_mismatch",
        _ => "mount.absent",
    };
    error_body(StatusCode::SERVICE_UNAVAILABLE, code, e.to_string())
}

fn transport_error_response(e: arca_cli::transport::TransportError) -> Response {
    error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

/// 按 `dataset_id` 查登记表 + 重新打开存储根——每个 handler 共用的前两步。
///
/// `Err` 分支装的是要直接返回给调用方的 `Response`——`Box` 一下是纯粒度
/// 考量（`clippy::result_large_err`）：`axum::http::Response` 本身有上百
/// 字节，作为 `Err` 变体会把 `Ok` 路径的每一次函数返回都拖大到同一尺寸；
/// 装箱后 `Ok` 路径不必为一个几乎不会发生的分支付这份栈空间。
fn open_dataset<'a>(
    registry: &'a Registry,
    id: &str,
) -> Result<(&'a Dataset, StorageRoot), Box<Response>> {
    let dataset = registry
        .get(id)
        .ok_or_else(|| Box::new(unknown_dataset()))?;
    let root = dataset
        .open()
        .map_err(|e| Box::new(mount_error_response(&e)))?;
    Ok((dataset, root))
}

/// 路径必须先过 `path_rules::check` 才允许拼进文件系统——HTTP 是不可信输入
/// 的入口（`code=path.rejected`，dispatch 纪律第 3 条 + PROTOCOL.md §1.2
/// 通用约定）。`Box<Response>`：理由同 [`open_dataset`]。
fn checked_path(raw: &str) -> Result<String, Box<Response>> {
    path_rules::check(raw).map_err(|status| {
        Box::new(error_body(
            StatusCode::BAD_REQUEST,
            "path.rejected",
            format!("路径 {raw:?} 被拒绝：{}", status.as_str()),
        ))
    })
}

// ---------------------------------------------------------------------------
// GET /v1/datasets/{id}/files/{path}
// ---------------------------------------------------------------------------

async fn get_file(
    State(registry): State<Arc<Registry>>,
    Path((dataset_id, raw_path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, root) = match open_dataset(&registry, &dataset_id) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let path = match checked_path(&raw_path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    let transport = LocalTransport::new(&root);
    let remote = match transport.read_remote() {
        Ok(m) => m,
        Err(e) => return transport_error_response(e),
    };
    let current = remote.get(&path).cloned().unwrap_or(RemoteState::Absent);
    let (item_id, version_id, hash, size) = match &current {
        RemoteState::Present {
            item_id,
            version_id,
            hash,
            size,
        } => (*item_id, version_id.clone(), *hash, *size),
        // Absent 与 Tombstoned 统一折叠成 404——这个端点回答的是"给我内容"，
        // 不需要区分"从未存在"与"被删的"（PROTOCOL.md §1.2 端点表下方说明）。
        RemoteState::Absent | RemoteState::Tombstoned { .. } => {
            return StatusCode::NOT_FOUND.into_response()
        }
    };
    let _ = item_id;

    if let Some(inm) = headers.get("if-none-match") {
        if let Ok(text) = inm.to_str() {
            if if_none_match_hits(text, &hash) {
                let mut resp = StatusCode::NOT_MODIFIED.into_response();
                set_cache_headers(resp.headers_mut(), &hash, &version_id);
                return resp;
            }
        }
    }

    let bytes = match transport.read_content(&path) {
        Ok(b) => b,
        Err(e) => return transport_error_response(e),
    };

    if let Some(range) = headers.get("range") {
        let Ok(range_text) = range.to_str() else {
            return StatusCode::BAD_REQUEST.into_response();
        };

        // Range 续传应携带 If-Match 钉住版本（PROTOCOL.md §1.2）：与此刻的
        // 版本不符 → 412（内容在续传期间被改写，续传的偏移量已不可信）。
        if let Some(if_match) = headers.get("if-match") {
            if let Ok(claimed) = if_match.to_str() {
                if claimed != version_id.as_str() {
                    return error_body(
                        StatusCode::PRECONDITION_FAILED,
                        "commit.stale_parent",
                        format!(
                            "Range 续传的 If-Match {claimed:?} 与当前版本 {:?} 不符——\
                             内容在续传期间被改写",
                            version_id.as_str()
                        ),
                    );
                }
            }
        }

        match parse_range(range_text, bytes.len()) {
            Some(Some((start, end))) => {
                let slice = bytes[start..=end].to_vec();
                let mut resp = (StatusCode::PARTIAL_CONTENT, slice).into_response();
                set_cache_headers(resp.headers_mut(), &hash, &version_id);
                resp.headers_mut().insert(
                    "content-range",
                    format!("bytes {start}-{end}/{size}").parse().unwrap(),
                );
                return resp;
            }
            Some(None) => {
                // 语法上是合法的 Range 头，但语法不认得（多重区间等）——
                // RFC 9110 §14.2 允许忽略语法上不理解的 Range，退回整份内容。
            }
            None => {
                // 数值上超出内容边界——416，带 Content-Range: bytes */<size>
                // 告知调用方合法范围（RFC 9110 §14.4）。
                let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                resp.headers_mut()
                    .insert("content-range", format!("bytes */{size}").parse().unwrap());
                return resp;
            }
        }
    }

    let mut resp = (StatusCode::OK, bytes).into_response();
    set_cache_headers(resp.headers_mut(), &hash, &version_id);
    resp
}

fn set_cache_headers(headers: &mut HeaderMap, hash: &ContentHash, version_id: &VersionId) {
    headers.insert("etag", etag_value(hash).parse().unwrap());
    headers.insert("arca-version-id", version_id.as_str().parse().unwrap());
}

fn etag_value(hash: &ContentHash) -> String {
    format!("\"{}\"", hash.to_text())
}

/// `If-None-Match` 命中判断：`*` 或逗号分隔列表中任意一项（去引号后）与内容
/// 哈希相等即命中（RFC 9110 §13.1.2）。
fn if_none_match_hits(header_value: &str, hash: &ContentHash) -> bool {
    let want = hash.to_text();
    header_value.split(',').any(|raw| {
        let trimmed = raw.trim().trim_matches('"');
        trimmed == "*" || trimmed == want
    })
}

/// 解析单区间 `Range: bytes=start-end` / `bytes=start-`。
///
/// - `Some(Some((start,end)))`：语法与数值都合法，闭区间字节偏移。
/// - `Some(None)`：语法本身不认得（多重区间等）——按 RFC 9110 §14.2 忽略，
///   调用方应回退到整份内容。
/// - `None`：语法认得但数值上不满足（起点越界，或起点大于终点）——416。
fn parse_range(header_value: &str, len: usize) -> Option<Option<(usize, usize)>> {
    let spec = header_value.strip_prefix("bytes=")?;
    // 只支持单一区间；含逗号说明是多重区间，语法上不理解，忽略退回整份内容。
    if spec.contains(',') {
        return Some(None);
    }
    let (start_str, end_str) = spec.split_once('-')?;
    if len == 0 {
        return None; // 空内容上任何具体区间都不可满足。
    }
    let last = len - 1;
    if start_str.is_empty() {
        // 后缀形式 bytes=-N：最后 N 个字节。
        let suffix_len: usize = end_str.parse().ok()?;
        if suffix_len == 0 {
            return None;
        }
        let start = len.saturating_sub(suffix_len);
        return Some(Some((start, last)));
    }
    let start: usize = start_str.parse().ok()?;
    if start > last {
        return None;
    }
    let end = if end_str.is_empty() {
        last
    } else {
        let requested: usize = end_str.parse().ok()?;
        requested.min(last)
    };
    if start > end {
        return None;
    }
    Some(Some((start, end)))
}

// ---------------------------------------------------------------------------
// GET /v1/datasets/{id}/state
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StateEntry {
    path: String,
    item_id: String,
    version_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    state: &'static str,
}

async fn get_state(
    State(registry): State<Arc<Registry>>,
    Path(dataset_id): Path<String>,
) -> Response {
    let (_, root) = match open_dataset(&registry, &dataset_id) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let transport = LocalTransport::new(&root);
    let remote = match transport.read_remote() {
        Ok(m) => m,
        Err(e) => return transport_error_response(e),
    };

    // `BTreeMap<String, _>` 迭代天然按 UTF-8 字节序排序——PROTOCOL.md §1.2
    // 要求的排序无需额外排序步骤。
    let entries: Vec<StateEntry> = remote
        .into_iter()
        .map(|(path, state)| match state {
            RemoteState::Present {
                item_id,
                version_id,
                hash,
                size,
            } => StateEntry {
                path,
                item_id: item_id.to_hex(),
                version_id: version_id.as_str().to_string(),
                hash: Some(hash.to_text()),
                size: Some(size),
                state: "present",
            },
            RemoteState::Tombstoned {
                item_id,
                version_id,
            } => StateEntry {
                path,
                item_id: item_id.to_hex(),
                version_id: version_id.as_str().to_string(),
                hash: None,
                size: None,
                state: "tombstoned",
            },
            RemoteState::Absent => unreachable!("read_remote 不产出 Absent 条目"),
        })
        .collect();

    (StatusCode::OK, axum::Json(entries)).into_response()
}

// ---------------------------------------------------------------------------
// GET /v1/datasets/{id}/trash/{item_id}?hash=<blake3-hex>
// ---------------------------------------------------------------------------

async fn get_trash(
    State(registry): State<Arc<Registry>>,
    Path((dataset_id, item_id_text)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let (_, root) = match open_dataset(&registry, &dataset_id) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };

    let Ok(item_id) = ItemId::parse(&item_id_text) else {
        return error_body(
            StatusCode::BAD_REQUEST,
            "request.item_id_invalid",
            format!("item_id {item_id_text:?} 不是合法的 32 位小写十六进制"),
        );
    };

    let Some(hash) = raw_query
        .as_deref()
        .and_then(find_hash_param)
        .and_then(parse_hex_hash)
    else {
        return error_body(
            StatusCode::BAD_REQUEST,
            "request.hash_missing",
            "缺少或格式不合法的 hash 查询参数",
        );
    };

    let transport = LocalTransport::new(&root);
    match transport.recoverable(item_id, hash) {
        Ok(Some(r)) => (
            StatusCode::OK,
            axum::Json(json!({
                "recoverable": true,
                "hash": r.hash.to_text(),
                "size": r.size,
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"recoverable": false})),
        )
            .into_response(),
        Err(e) => transport_error_response(e),
    }
}

/// 从原始 query 字符串里找 `hash` 参数的值——值本身是纯十六进制字符，
/// 不含任何需要百分号解码的字符，因此不引入额外的 URL 解码依赖。
fn find_hash_param(query: &str) -> Option<&str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "hash").then_some(value)
    })
}

/// `hash` 查询参数是裸十六进制（不带 `blake3:` 前缀，与响应体 `hash` 字段
/// 的完整形式不同——查询参数只是"要核验的这一个哈希"，端点表用
/// `hash=<blake3-hex>` 命名，未写前缀）。
fn parse_hex_hash(text: &str) -> Option<ContentHash> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    ContentHash::parse(&format!("blake3:{text}")).ok()
}

// ---------------------------------------------------------------------------
// PUT /v1/datasets/{id}/files/{path}
// ---------------------------------------------------------------------------

async fn put_file(
    State(registry): State<Arc<Registry>>,
    Path((dataset_id, raw_path)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(dataset) = registry.get(&dataset_id) else {
        return unknown_dataset();
    };
    let path = match checked_path(&raw_path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    let parent = match parse_cas_condition(&headers) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    let item_id = match headers
        .get("arca-item-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| ItemId::parse(s).ok())
    {
        Some(id) => id,
        None => return metadata_missing("Arca-Item-Id"),
    };
    let version_id = match headers
        .get("arca-version-id")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_version_id_header)
    {
        Some(v) => v,
        None => return metadata_missing("Arca-Version-Id"),
    };
    let mtime = match headers
        .get("arca-mtime")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        Some(m) => m.to_string(),
        None => return metadata_missing("Arca-Mtime"),
    };
    let actor = actor_from_headers(&headers);

    // 临界区：拿到锁之后才重新打开存储根、才做 CAS 提交——见 storage.rs
    // 「write_lock」一节，这是并发正确性的唯一来源。
    let _guard = dataset.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let root = match dataset.open() {
        Ok(r) => r,
        Err(e) => return mount_error_response(&e),
    };
    let transport = LocalTransport::new(&root);

    let req = CommitRequest {
        path: path.clone(),
        item_id,
        version_id: version_id.clone(),
        parent: parent.clone(),
        bytes: body.to_vec(),
        mtime,
        actor,
    };

    match transport.commit(&req) {
        Ok(CommitOutcome::Committed {
            item_id,
            version_id,
        }) => {
            let hash = ContentHash::from_bytes(&body);
            let status = if parent.is_none() {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let mut resp = (
                status,
                axum::Json(json!({
                    "item_id": item_id.to_hex(),
                    "version_id": version_id.as_str(),
                    "hash": hash.to_text(),
                    "size": body.len(),
                })),
            )
                .into_response();
            set_cache_headers(resp.headers_mut(), &hash, &version_id);
            resp
        }
        Ok(CommitOutcome::Conflict {
            expected_parent,
            actual,
        }) => conflict_response(&root, item_id, &expected_parent, &actual, &body),
        Err(e) => transport_error_response(e),
    }
}

fn metadata_missing(header: &str) -> Response {
    error_body(
        StatusCode::BAD_REQUEST,
        "request.metadata_missing",
        format!("缺少或不合法的 {header} 请求头"),
    )
}

/// `Arca-Session` 放进 `Actor.session`——I8 审计闭环（PROTOCOL.md §1.2 通用
/// 约定：缺失时记一个空串，不拒绝请求，trace 是诊断产物，不应该因为它缺失
/// 就中止一次合法的写入）。`account`/`device` 留空：设备/账号令牌握手是
/// §4 的 TODO（`auth.rs`），M2b 尚未接入认证，这里不伪造身份。
fn actor_from_headers(headers: &HeaderMap) -> Actor {
    let session = headers
        .get("arca-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    Actor {
        account: String::new(),
        device: String::new(),
        session,
    }
}

/// `<紧凑时间戳>-<32 位随机十六进制>` 形式的 `version_id` 解析——与
/// `arca_format::journal`/`items` 内部的同名私有解析逻辑等价（那两处不对
/// 外公开，本函数是 HTTP 层对同一线上文本形式的独立最小实现）。
fn parse_version_id_header(text: &str) -> Option<VersionId> {
    let (timestamp, random) = text.split_once('-')?;
    VersionId::new(timestamp, random).ok()
}

/// 解析 `If-Match`/`If-None-Match: *` 二选一（I4：一切写入走 CAS）。
///
/// - 都未提供 → `400 request.if_match_required`。
/// - 都提供了（歧义：客户端到底想表达哪一个？）→ 同样 `400`，不猜测该信哪个。
/// - `If-Match` 提供但解析不出合法 `version_id` → 视同未提供有效条件头，
///   同样 `400`（一个语法都不对的 CAS 令牌不能被当作"提供了条件"）。
/// - `If-None-Match` 提供但不是字面量 `*` → `400`（协议只定义了这一种用法，
///   见 PROTOCOL.md §1.2「仅创建」一行）。
fn parse_cas_condition(headers: &HeaderMap) -> Result<Option<VersionId>, Box<Response>> {
    let if_match = headers.get("if-match").and_then(|v| v.to_str().ok());
    let if_none_match = headers.get("if-none-match").and_then(|v| v.to_str().ok());
    match (if_match, if_none_match) {
        (Some(v), None) => match parse_version_id_header(v) {
            Some(vid) => Ok(Some(vid)),
            None => Err(Box::new(if_match_required())),
        },
        (None, Some("*")) => Ok(None),
        _ => Err(Box::new(if_match_required())),
    }
}

fn if_match_required() -> Response {
    error_body(
        StatusCode::BAD_REQUEST,
        "request.if_match_required",
        "写入必须携带 If-Match: <version_id> 或 If-None-Match: * 二选一（I4）",
    )
}

/// 构造 412 的结构化冲突体（`base`/`theirs`/`yours`，PROTOCOL.md §1.2）。
fn conflict_response(
    root: &StorageRoot,
    item_id: ItemId,
    expected_parent: &Option<VersionId>,
    actual: &RemoteState,
    body: &[u8],
) -> Response {
    let hash = ContentHash::from_bytes(body);
    let base = base_json(root, item_id, expected_parent);
    let theirs = theirs_json(actual);
    (
        StatusCode::PRECONDITION_FAILED,
        axum::Json(json!({
            "code": "commit.stale_parent",
            "base": base,
            "theirs": theirs,
            "yours": {
                "item_id": item_id.to_hex(),
                "hash": hash.to_text(),
                "size": body.len(),
            },
        })),
    )
        .into_response()
}

/// `base`：客户端声明的 `If-Match`（它认为的"当前版本"）。`None`（即客户端
/// 用的是 `If-None-Match: *`，仅创建语义）时没有"它认为的版本"可言，用
/// `null` 表达——PROTOCOL.md 的示例只覆盖了"确实声明了一个版本"的情形，
/// 这是本实现对"仅创建冲突"这个协议文本未明确覆盖的分支所做的最小一致
/// 延伸：`null` 与 `theirs`/`yours` 在"这里没有这个东西"上用同一个记号。
fn base_json(root: &StorageRoot, item_id: ItemId, parent: &Option<VersionId>) -> serde_json::Value {
    let Some(version_id) = parent else {
        return serde_json::Value::Null;
    };
    match read_item_chain(root, item_id) {
        Some(chain) => match chain.into_iter().find(|v| &v.version_id == version_id) {
            Some(v) => json!({
                "item_id": item_id.to_hex(),
                "version_id": version_id.as_str(),
                "hash": v.hash.to_text(),
                "size": v.size,
            }),
            None => json!({
                "item_id": item_id.to_hex(),
                "version_id": version_id.as_str(),
            }),
        },
        None => json!({
            "item_id": item_id.to_hex(),
            "version_id": version_id.as_str(),
        }),
    }
}

fn theirs_json(actual: &RemoteState) -> serde_json::Value {
    match actual {
        RemoteState::Absent => serde_json::Value::Null,
        RemoteState::Present {
            item_id,
            version_id,
            hash,
            size,
        } => json!({
            "item_id": item_id.to_hex(),
            "version_id": version_id.as_str(),
            "hash": hash.to_text(),
            "size": size,
        }),
        RemoteState::Tombstoned {
            item_id,
            version_id,
        } => json!({
            "tombstoned": true,
            "item_id": item_id.to_hex(),
            "version_id": version_id.as_str(),
        }),
    }
}

/// 读某个 `item_id` 的完整版本链，仅用于丰富 412 诊断体里的 `base.hash`/
/// `base.size`——找不到/读不懂时返回 `None`，调用方（[`base_json`]）优雅
/// 降级为只留 `item_id`/`version_id`（PROTOCOL.md 原文：「这个版本本身
/// 已经找不到时——理论上不该发生——只留 item_id/version_id」）。绝不因为
/// 这个辅助诊断信息读取失败就让整个 412 响应本身失败。
fn read_item_chain(root: &StorageRoot, item_id: ItemId) -> Option<Vec<Version>> {
    let rel = layout::item_path(&item_id);
    let full = root.path().join(rel);
    let text = std::fs::read_to_string(full).ok()?;
    arca_format::items::parse_chain(&text).ok()
}

// ---------------------------------------------------------------------------
// DELETE /v1/datasets/{id}/files/{path}
// ---------------------------------------------------------------------------

async fn delete_file(
    State(registry): State<Arc<Registry>>,
    Path((dataset_id, raw_path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(dataset) = registry.get(&dataset_id) else {
        return unknown_dataset();
    };
    let path = match checked_path(&raw_path) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    let Some(if_match) = headers.get("if-match").and_then(|v| v.to_str().ok()) else {
        return if_match_required();
    };
    let Some(parent) = parse_version_id_header(if_match) else {
        return if_match_required();
    };
    let Some(item_id) = headers
        .get("arca-item-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| ItemId::parse(s).ok())
    else {
        return metadata_missing("Arca-Item-Id");
    };
    let actor = actor_from_headers(&headers);

    let _guard = dataset.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let root = match dataset.open() {
        Ok(r) => r,
        Err(e) => return mount_error_response(&e),
    };
    let transport = LocalTransport::new(&root);

    let remote = match transport.read_remote() {
        Ok(m) => m,
        Err(e) => return transport_error_response(e),
    };
    match remote.get(&path).cloned().unwrap_or(RemoteState::Absent) {
        RemoteState::Present { .. } => {}
        // 已经不存在（从未有过，或已经被删过）——无事可删。
        RemoteState::Absent | RemoteState::Tombstoned { .. } => {
            return StatusCode::NOT_FOUND.into_response()
        }
    }

    let at = arca_cli::clock::now_rfc3339();
    let req = TombstoneRequest {
        path: path.clone(),
        item_id,
        parent: parent.clone(),
        actor,
        at,
    };
    match transport.tombstone(&req) {
        Ok(CommitOutcome::Committed { .. }) => StatusCode::NO_CONTENT.into_response(),
        Ok(CommitOutcome::Conflict {
            expected_parent,
            actual,
        }) => delete_conflict_response(&root, item_id, &expected_parent, &actual),
        Err(e) => transport_error_response(e),
    }
}

fn delete_conflict_response(
    root: &StorageRoot,
    item_id: ItemId,
    expected_parent: &Option<VersionId>,
    actual: &RemoteState,
) -> Response {
    let base = base_json(root, item_id, expected_parent);
    let theirs = theirs_json(actual);
    (
        StatusCode::PRECONDITION_FAILED,
        axum::Json(json!({
            "code": "commit.stale_parent",
            "base": base,
            "theirs": theirs,
            "yours": {
                "item_id": item_id.to_hex(),
                "tombstoned": true,
            },
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DatasetConfig, HubConfig};
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn 造存储根(dir: &std::path::Path, dataset_id: &str) {
        std::fs::create_dir_all(dir.join(".arca")).unwrap();
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        std::fs::create_dir_all(dir.join(".arca/trash")).unwrap();
        std::fs::create_dir_all(dir.join(".arca/journal")).unwrap();
        let format = arca_format::hub_layout::FormatJson {
            format: 1,
            dataset_id: dataset_id.to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        std::fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    fn build_router(datasets: Vec<(&str, PathBuf)>) -> Router {
        let cfg = HubConfig {
            instance_id: "0".repeat(32),
            datasets: datasets
                .into_iter()
                .map(|(id, path)| DatasetConfig {
                    id: id.to_string(),
                    path,
                })
                .collect(),
        };
        router(Arc::new(Registry::from_config(&cfg)))
    }

    async fn body_bytes(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        serde_json::from_slice(&body_bytes(resp).await).unwrap()
    }

    fn create_request(
        dataset: &str,
        path: &str,
        item_id: ItemId,
        version_id: &VersionId,
        bytes: &[u8],
    ) -> Request<Body> {
        Request::builder()
            .method(Method::PUT)
            .uri(format!("/v1/datasets/{dataset}/files/{path}"))
            .header("if-none-match", "*")
            .header("arca-item-id", item_id.to_hex())
            .header("arca-version-id", version_id.as_str())
            .header("arca-mtime", "2026-08-08T09:00:00Z")
            .header("arca-session", "20260808T090000Z-0123456789abcdef")
            .body(Body::from(bytes.to_vec()))
            .unwrap()
    }

    use std::path::PathBuf;

    // -----------------------------------------------------------------
    // Task 3：路由前置校验 + 挂载检查
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn 未配置的数据集返回404() {
        let app = build_router(vec![]);
        let req = Request::builder()
            .uri("/v1/datasets/9c41000000000000000000000000abcd/state")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn 存储根缺失返回503且code为mount_absent() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        // 不创建 format.json——等价于卷未挂载。
        let app = build_router(vec![(id, dir.path().to_path_buf())]);
        let req = Request::builder()
            .uri(format!("/v1/datasets/{id}/state"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "mount.absent");
    }

    #[tokio::test]
    async fn 卷身份不符返回503且code为identity_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let actual_id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), actual_id);
        let configured_id = "a1b2000000000000000000000000c3d4";
        let app = build_router(vec![(configured_id, dir.path().to_path_buf())]);
        let req = Request::builder()
            .uri(format!("/v1/datasets/{configured_id}/state"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "mount.identity_mismatch");
    }

    /// Task 3 判据原文：根被移走后该数据集 503，其余数据集照常 200——
    /// 独立故障域（spec §4.3.2），一个数据集离线不得牵连别的数据集。
    #[tokio::test]
    async fn 一个数据集离线不影响另一个数据集() {
        let broken_dir = tempfile::tempdir().unwrap();
        let broken_id = "9c41000000000000000000000000abcd";
        造存储根(broken_dir.path(), broken_id);

        let healthy_dir = tempfile::tempdir().unwrap();
        let healthy_id = "a1b2000000000000000000000000c3d4";
        造存储根(healthy_dir.path(), healthy_id);

        let app = build_router(vec![
            (broken_id, broken_dir.path().to_path_buf()),
            (healthy_id, healthy_dir.path().to_path_buf()),
        ]);

        // 卸载 broken 数据集的卷。
        std::fs::remove_dir_all(broken_dir.path()).unwrap();

        let broken_req = Request::builder()
            .uri(format!("/v1/datasets/{broken_id}/state"))
            .body(Body::empty())
            .unwrap();
        let broken_resp = app.clone().oneshot(broken_req).await.unwrap();
        assert_eq!(broken_resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let healthy_req = Request::builder()
            .uri(format!("/v1/datasets/{healthy_id}/state"))
            .body(Body::empty())
            .unwrap();
        let healthy_resp = app.oneshot(healthy_req).await.unwrap();
        assert_eq!(healthy_resp.status(), StatusCode::OK);
    }

    // -----------------------------------------------------------------
    // Task 4：读取端点 + 条件请求
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn 路径穿越在触碰文件系统前被拒绝() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);
        let req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/..%2F..%2Fetc%2Fpasswd"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "path.rejected");
    }

    #[tokio::test]
    async fn get_不存在的路径返回404() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);
        let req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put后get命中内容并带etag与版本号() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let version_id = arca_cli::ids::new_version_id();
        let put_req = create_request(id, "a.txt", item_id, &version_id, b"hello world");
        let put_resp = app.clone().oneshot(put_req).await.unwrap();
        assert_eq!(put_resp.status(), StatusCode::CREATED);

        let get_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .body(Body::empty())
            .unwrap();
        let get_resp = app.clone().oneshot(get_req).await.unwrap();
        assert_eq!(get_resp.status(), StatusCode::OK);
        let etag = get_resp
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let expected_hash = ContentHash::from_bytes(b"hello world");
        assert_eq!(etag, format!("\"{}\"", expected_hash.to_text()));
        let got_version = get_resp
            .headers()
            .get("arca-version-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(got_version, version_id.as_str());
        assert_eq!(body_bytes(get_resp).await, b"hello world");

        // If-None-Match 命中 → 304。
        let cond_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("if-none-match", format!("\"{}\"", expected_hash.to_text()))
            .body(Body::empty())
            .unwrap();
        let cond_resp = app.clone().oneshot(cond_req).await.unwrap();
        assert_eq!(cond_resp.status(), StatusCode::NOT_MODIFIED);

        // Range 请求 → 206。
        let range_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("range", "bytes=0-4")
            .body(Body::empty())
            .unwrap();
        let range_resp = app.clone().oneshot(range_req).await.unwrap();
        assert_eq!(range_resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_bytes(range_resp).await, b"hello");

        // Range 越界 → 416。
        let oob_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("range", "bytes=1000-2000")
            .body(Body::empty())
            .unwrap();
        let oob_resp = app.oneshot(oob_req).await.unwrap();
        assert_eq!(oob_resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn get_state返回按路径排序的数组() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        for name in ["b.txt", "a.txt"] {
            let req = create_request(
                id,
                name,
                arca_cli::ids::new_item_id(),
                &arca_cli::ids::new_version_id(),
                b"x",
            );
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let req = Request::builder()
            .uri(format!("/v1/datasets/{id}/state"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["path"], "a.txt");
        assert_eq!(arr[1]["path"], "b.txt");
        assert_eq!(arr[0]["state"], "present");
    }

    // -----------------------------------------------------------------
    // Task 5：CAS 写入端点
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn put没有if_match返回400() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("arca-item-id", arca_cli::ids::new_item_id().to_hex())
            .header("arca-version-id", arca_cli::ids::new_version_id().as_str())
            .header("arca-mtime", "2026-08-08T09:00:00Z")
            .body(Body::from(b"hello".to_vec()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "request.if_match_required");
    }

    #[tokio::test]
    async fn delete没有if_match返回400() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);
        let req = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "request.if_match_required");
    }

    #[tokio::test]
    async fn 过期parent返回412且响应体可解析出三方哈希() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let v1 = arca_cli::ids::new_version_id();
        let put1 = create_request(id, "a.txt", item_id, &v1, b"v1-content");
        let resp1 = app.clone().oneshot(put1).await.unwrap();
        assert_eq!(resp1.status(), StatusCode::CREATED);

        // 再次用 If-None-Match:* 提交同一路径——parent 声明"应为不存在"，
        // 但此刻已经有 v1，必须 412。
        let v2 = arca_cli::ids::new_version_id();
        let put2 = create_request(id, "a.txt", item_id, &v2, b"v2-content-conflict");
        let resp2 = app.oneshot(put2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::PRECONDITION_FAILED);
        let body = body_json(resp2).await;
        assert_eq!(body["code"], "commit.stale_parent");
        assert!(
            body["base"].is_null(),
            "If-None-Match:* 场景下 base 应为 null"
        );
        assert_eq!(body["theirs"]["item_id"], item_id.to_hex());
        assert_eq!(body["theirs"]["version_id"], v1.as_str());
        assert_eq!(
            body["theirs"]["hash"],
            ContentHash::from_bytes(b"v1-content").to_text()
        );
        assert_eq!(body["yours"]["item_id"], item_id.to_hex());
        assert_eq!(
            body["yours"]["hash"],
            ContentHash::from_bytes(b"v2-content-conflict").to_text()
        );

        // hub 上的内容必须仍是 v1——冲突时不应写入。
        assert_eq!(
            std::fs::read(dir.path().join("files/a.txt")).unwrap(),
            b"v1-content"
        );
    }

    #[tokio::test]
    async fn put推进正确parent后成功且旧parent再次提交仍412() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let v1 = arca_cli::ids::new_version_id();
        let put1 = create_request(id, "a.txt", item_id, &v1, b"v1");
        assert_eq!(
            app.clone().oneshot(put1).await.unwrap().status(),
            StatusCode::CREATED
        );

        let v2 = arca_cli::ids::new_version_id();
        let put2 = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("if-match", v1.as_str())
            .header("arca-item-id", item_id.to_hex())
            .header("arca-version-id", v2.as_str())
            .header("arca-mtime", "2026-08-08T09:10:00Z")
            .body(Body::from(b"v2".to_vec()))
            .unwrap();
        let resp2 = app.clone().oneshot(put2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);

        // 用已经过期的 v1 再提交一次——412。
        let v3 = arca_cli::ids::new_version_id();
        let put3 = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("if-match", v1.as_str())
            .header("arca-item-id", item_id.to_hex())
            .header("arca-version-id", v3.as_str())
            .header("arca-mtime", "2026-08-08T09:20:00Z")
            .body(Body::from(b"v3-stale".to_vec()))
            .unwrap();
        let resp3 = app.oneshot(put3).await.unwrap();
        assert_eq!(resp3.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn delete提交tombstone且trash可取回() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let v1 = arca_cli::ids::new_version_id();
        let put1 = create_request(id, "a.txt", item_id, &v1, b"content");
        assert_eq!(
            app.clone().oneshot(put1).await.unwrap().status(),
            StatusCode::CREATED
        );

        let del = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("if-match", v1.as_str())
            .header("arca-item-id", item_id.to_hex())
            .body(Body::empty())
            .unwrap();
        let del_resp = app.clone().oneshot(del).await.unwrap();
        assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);

        // GET 此刻应 404（tombstoned 折叠进 404）。
        let get_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .body(Body::empty())
            .unwrap();
        let get_resp = app.clone().oneshot(get_req).await.unwrap();
        assert_eq!(get_resp.status(), StatusCode::NOT_FOUND);

        // trash 查询：正确哈希 → 200 recoverable:true。
        let hash = ContentHash::from_bytes(b"content");
        let trash_req = Request::builder()
            .uri(format!(
                "/v1/datasets/{id}/trash/{}?hash={}",
                item_id.to_hex(),
                hash.to_hex()
            ))
            .body(Body::empty())
            .unwrap();
        let trash_resp = app.clone().oneshot(trash_req).await.unwrap();
        assert_eq!(trash_resp.status(), StatusCode::OK);
        let body = body_json(trash_resp).await;
        assert_eq!(body["recoverable"], true);
        assert_eq!(body["hash"], hash.to_text());

        // 错误哈希 → 404 recoverable:false。
        let wrong_hash = ContentHash::from_bytes(b"not the content");
        let miss_req = Request::builder()
            .uri(format!(
                "/v1/datasets/{id}/trash/{}?hash={}",
                item_id.to_hex(),
                wrong_hash.to_hex()
            ))
            .body(Body::empty())
            .unwrap();
        let miss_resp = app.clone().oneshot(miss_req).await.unwrap();
        assert_eq!(miss_resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(miss_resp).await;
        assert_eq!(body["recoverable"], false);

        // 缺少 hash 查询参数 → 400。
        let bad_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/trash/{}", item_id.to_hex()))
            .body(Body::empty())
            .unwrap();
        let bad_resp = app.oneshot(bad_req).await.unwrap();
        assert_eq!(bad_resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete不存在的路径返回404() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);
        let req = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/datasets/{id}/files/never-existed.txt"))
            .header("if-match", arca_cli::ids::new_version_id().as_str())
            .header("arca-item-id", arca_cli::ids::new_item_id().to_hex())
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// 硬要求：两个客户端真的并发对同一路径提交——一个成功、一个 412。
    ///
    /// 用 `tokio::spawn` 在多线程运行时上把两次 `PUT` 分别丢给不同的 worker
    /// 线程，两者共享同一个 `Arc<Registry>`（`app.clone()` 只克隆了外层
    /// `Router`/`Arc` 本身，内部的 `Dataset::write_lock` 是同一把锁）——
    /// 这是真实的 OS 线程级并发，不是同一个 future 里顺序 `.await` 两次
    /// 的假并发。断言只看最终结果（一个 201、一个 412），不假设哪个先到：
    /// 两次提交都用 `If-None-Match: *`（都声明"路径此刻应不存在"），
    /// `write_lock` 序列化之后，先拿到锁的那个必然看到 `Absent` 而创建成功，
    /// 后拿到锁的那个必然看到刚被创建的版本而冲突——不可能两个都成功
    /// （那正是 I4 要挡住的双赢），也不可能两个都失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 并发提交同一路径一个成功一个412() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();

        let app_a = app.clone();
        let va = arca_cli::ids::new_version_id();
        let handle_a = tokio::spawn(async move {
            let req = create_request(id, "race.txt", item_id, &va, b"from-a");
            app_a.oneshot(req).await.unwrap().status()
        });

        let app_b = app.clone();
        let vb = arca_cli::ids::new_version_id();
        let handle_b = tokio::spawn(async move {
            let req = create_request(id, "race.txt", item_id, &vb, b"from-b-longer-body");
            app_b.oneshot(req).await.unwrap().status()
        });

        let (status_a, status_b) = tokio::join!(handle_a, handle_b);
        let status_a = status_a.unwrap();
        let status_b = status_b.unwrap();

        let statuses = [status_a, status_b];
        let created = statuses
            .iter()
            .filter(|s| **s == StatusCode::CREATED)
            .count();
        let conflicted = statuses
            .iter()
            .filter(|s| **s == StatusCode::PRECONDITION_FAILED)
            .count();
        assert_eq!(
            (created, conflicted),
            (1, 1),
            "并发提交同一路径必须恰好一个 201、一个 412，实得 {statuses:?}"
        );

        // 存储根上最终只有一份内容，且与"胜出"的那次提交一致（长度匹配）。
        let final_bytes = std::fs::read(dir.path().join("files/race.txt")).unwrap();
        assert!(final_bytes == b"from-a" || final_bytes == b"from-b-longer-body");
    }
}
