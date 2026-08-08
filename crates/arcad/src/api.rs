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
use arca_cli::transport::{CommitOutcome, TombstoneRequest, Transport};
use arca_core::state::RemoteState;
use arca_format::hub_layout::layout;
use arca_format::model::{Actor, ItemId, Version, VersionId};
use arca_format::path_rules;
use arca_store::atomic::TmpWriter;
use arca_store::root::{MountError, StorageRoot};
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// `PUT` 请求体的上限——**C2 修复**：请求体现在流式写进 `.arca/tmp`
/// （见 [`put_file`]），不再整份缓冲进内存，这个值因此不再直接决定内存
/// 占用；但它仍然是单次请求能占用的磁盘与处理时间的上限，M2b 尚未接入
/// CDC 分块上传（PROTOCOL.md §2 仍是 TODO）之前，把它从 1 GiB 降到
/// 256 MiB——对"个人笔记/照片库"这个目标场景依然宽松，同时把一次失控/
/// 恶意请求能占用的资源收窄到更容易兜底的量级（评审 C2）。
const MAX_BODY_BYTES: u64 = 256 * 1024 * 1024;

/// 单个 `arcad` 进程同时处理的请求数上限——**C2 修复**：此前没有任何并发
/// 上限，任意多个并发请求可以无界叠加内存/文件句柄/磁盘 IO 占用。数值本身
/// 不追求精确调优（M2b 目标场景是个人/团队规模部署，不是高并发服务），
/// 只求"存在一个界"。
const MAX_CONCURRENT_REQUESTS: usize = 64;

/// 单次请求的处理超时——**C2 修复**：配合并发上限，防止一个卡住的请求
/// （慢客户端、网络分区、対端进程挂起）永久占着并发配额里的一个名额。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// 构建 HTTP 路由——`state` 是全部已配置数据集的登记表（`Arc` 包裹以满足
/// axum `State` 要求 `Clone`；`Registry` 本身不需要也不应该是 `Clone`：
/// 里面的 `write_lock` 一旦被复制，"同一数据集共享同一把锁"这个前提就没了）。
///
/// 中间件顺序（评审 C2）：并发上限包住超时——等待一个并发名额的时间不计入
/// 单次请求的处理超时，只有真正拿到名额、开始执行 handler 之后才开始计时；
/// 反过来则会在高并发下把排队本身误判成"处理超时"，对合法的忙时流量不公平。
pub fn router(state: Arc<Registry>) -> Router {
    let concurrency = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
    Router::new()
        .route(
            "/v1/datasets/{id}/files/{*path}",
            get(get_file).put(put_file).delete(delete_file),
        )
        .route("/v1/datasets/{id}/state", get(get_state))
        .route("/v1/datasets/{id}/trash/{item_id}", get(get_trash))
        .layer(middleware::from_fn(timeout_middleware))
        .layer(middleware::from_fn_with_state(
            concurrency,
            concurrency_limit_middleware,
        ))
        .with_state(state)
}

/// 单次请求处理超时——超时后返回 `504`，不留请求悬挂到客户端自己放弃
/// （评审 C2）。协议表未登记专属 `code`：这不是本节端点表定义的任何一种
/// 业务失败，是传输层兜底，与 [`unknown_dataset`] 的处置纪律一致（标准
/// 状态码语义已自解释，不为此新造一个诊断码）。
async fn timeout_middleware(req: Request, next: Next) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "request timed out").into_response(),
    }
}

/// 并发请求数上限——`Semaphore` 许可持有到 `next.run` 返回为止，超过上限
/// 的请求排队等待，不是直接拒绝：`arcad` 面向的是个人/团队规模的忙时
/// 突发，不是需要立刻甩负载的高并发服务（评审 C2）。
async fn concurrency_limit_middleware(
    State(semaphore): State<Arc<Semaphore>>,
    req: Request,
    next: Next,
) -> Response {
    let _permit = semaphore
        .acquire()
        .await
        .expect("semaphore 从不 close()，acquire 不应失败");
    next.run(req).await
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

/// `Transport` 失败翻译成 HTTP——**I2 修复**：此前一律吐裸字符串
/// `code="internal"`，`PROTOCOL.md` §7 从未注册过这个码，agent 按 `code`
/// 分支时无从下手；且这条路径最常见的真实触发原因（链断裂、内容缺失、
/// `EACCES` 等）本质上是存储根的结构性问题，属于 `needs_human`（停下报告，
/// 运维排查），不是 `bug`（提 issue）——把它们错分成 `bug` 会让 agent 去开
/// 一个不该开的代码缺陷工单，而不是去跑 `arca fsck`。已注册的
/// `code=store.corrupt`（`class=needs_human`）统一覆盖本函数能收到的全部
/// `TransportError` 变体：走到这里说明请求本身已经通过了挂载检查与语法
/// 校验，唯一剩下的可能性就是存储根这一层出了问题。
fn transport_error_response(e: arca_cli::transport::TransportError) -> Response {
    error_body(
        StatusCode::INTERNAL_SERVER_ERROR,
        "store.corrupt",
        e.to_string(),
    )
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

/// 取同名头部**恰好一次**出现的值——Minor 修复：`headers.get()` 只返回第一
/// 条、静默丢弃其余同名头，评审实测两条矛盾的 `If-Match` 仍然放行 200 并
/// 覆盖内容，与本文件在别处反复强调的"不猜测该信哪个"纪律（[`parse_cas_condition`]
/// 的既有注释）不一致——那条注释只覆盖了"`If-Match` 与 `If-None-Match`
/// 都出现"这一种歧义，没堵住"同一个头出现两次"这一种。HTTP 允许把多行
/// 同名头等价折叠成逗号分隔的一行（RFC 9110 §5.3），但 arca 协议的
/// `If-Match`/`If-None-Match`/`Arca-Item-Id` 等载荷都是单一不透明令牌，不是
/// 可折叠的列表语法——收到多于一条就是格式不对的输入，统一当作"未提供有效
/// 条件"处理（`Err(())`），不比较内容是否碰巧相同：相同也不该纵容客户端
/// 发送这种形状的请求。
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    first.to_str().map(Some).map_err(|_| ())
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
    let (item_id, version_id, recorded_hash, recorded_size) = match &current {
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

    let full_path = match root.join(&format!("{}/{}", layout::FILES_DIR, path)) {
        Ok(p) => p,
        Err(e) => {
            return error_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store.corrupt",
                e.to_string(),
            )
        }
    };

    // I1 修复：ETag 与响应体必须来自同一份"此刻磁盘上的字节"。此前 ETag
    // 完全信元数据记录、响应体来自独立的一次 `fs::read`，两者没有任何
    // 一致性校验——手工编辑 `files/<path>`（I1 明确鼓励的逃生舱操作）之后，
    // arcad 会用旧哈希服务新字节，客户端拿 `If-None-Match` 重新验证会永远
    // 命中 304。这里只在磁盘实际大小与记录不一致时才现场重算哈希：多数
    // 请求（没被手工动过的文件）只多付一次 `stat`；一旦体积漂移，宁可为
    // 这一次请求多付一次全量读取 + 哈希的代价，也不能让 ETag 继续撒谎。
    // 残留缺口（与最终报告一致）：只用体积做漂移探测，堵不住"手工换成
    // 完全等长的另一份内容"这种极端情形——那类漂移只有 `arca fsck` 的
    // 全量重算能查出来。
    let (hash, size, preloaded) =
        match effective_hash_and_size(&full_path, recorded_hash, recorded_size) {
            Ok(v) => v,
            Err(e) => return io_corruption_response(&full_path, &e),
        };

    // Minor 修复：`If-None-Match` 重复且取值不一致时，`headers.get()` 会
    // 静默只信第一条——这里是纯缓存校验（不是 CAS 写入），猜错的代价只是
    // "该 304 的没 304"，不是数据损坏，所以宁可保守地当作"没有提供有效的
    // 缓存校验条件"，直接跳过 304 判断、照常吐出内容，而不是任选一条来信
    // （[`single_header`] 文档）。
    if let Ok(Some(text)) = single_header(&headers, "if-none-match") {
        if if_none_match_hits(text, &hash) {
            let mut resp = StatusCode::NOT_MODIFIED.into_response();
            set_cache_headers(resp.headers_mut(), &hash, &version_id);
            return resp;
        }
    }

    if let Some(range) = headers.get("range") {
        let Ok(range_text) = range.to_str() else {
            return StatusCode::BAD_REQUEST.into_response();
        };

        // Range 续传应携带 If-Match 钉住版本（PROTOCOL.md §1.2）：与此刻的
        // 版本不符 → 412（内容在续传期间被改写，续传的偏移量已不可信）。
        // Minor 修复：这里与 GET 缓存校验的处置不同——重复且矛盾的
        // `If-Match` 不能悄悄跳过版本钉住检查去信第一条：那正是续传安全性
        // 唯一的把关，猜错会让客户端拼接出跨版本的半新半旧内容，必须直接
        // 拒绝（400），不当作"没提供"处理。
        match single_header(&headers, "if-match") {
            Ok(Some(claimed)) => {
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
            Ok(None) => {}
            Err(()) => return ambiguous_header_response("If-Match"),
        }

        match parse_range(range_text, size as usize) {
            Some(Some((start, end))) => {
                // C2 修复：不整文件 slurp——已经在内存里的（漂移触发过全量
                // 重算）直接切片；否则 `seek` + 有界读，只分配这一段 Range
                // 大小的内存，不管文件本身多大（评审实测：对 800MB 文件做
                // 1 字节 Range GET 之前会让 RSS 涨到 824MB）。
                let slice = match &preloaded {
                    Some(bytes) => bytes[start..=end].to_vec(),
                    None => {
                        match bounded_read(&full_path, start as u64, (end - start + 1) as u64) {
                            Ok(b) => b,
                            Err(e) => return io_corruption_response(&full_path, &e),
                        }
                    }
                };
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
                // 告知调用方合法范围（RFC 9110 §14.4）。size 现在来自磁盘
                // 实际大小（I1 修复），不再可能出现"20 字节的响应体却报
                // bytes 0-4/10"这种与 RFC 9110 §14.4 矛盾的旧 bug。
                let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                resp.headers_mut()
                    .insert("content-range", format!("bytes */{size}").parse().unwrap());
                return resp;
            }
        }
    }

    let bytes = match preloaded {
        Some(b) => b,
        None => match transport.read_content(&path) {
            Ok(b) => b,
            Err(e) => return transport_error_response(e),
        },
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    set_cache_headers(resp.headers_mut(), &hash, &version_id);
    resp
}

/// I1 修复的核心判断：见 [`get_file`] 里的调用点注释。
fn effective_hash_and_size(
    full_path: &std::path::Path,
    recorded_hash: ContentHash,
    recorded_size: u64,
) -> std::io::Result<(ContentHash, u64, Option<Vec<u8>>)> {
    let on_disk_size = std::fs::metadata(full_path)?.len();
    if on_disk_size == recorded_size {
        Ok((recorded_hash, recorded_size, None))
    } else {
        let bytes = std::fs::read(full_path)?;
        let hash = ContentHash::from_bytes(&bytes);
        let size = bytes.len() as u64;
        Ok((hash, size, Some(bytes)))
    }
}

/// C2 修复：Range 请求的有界读——`seek` 到起点后只读取这一段区间需要的
/// 字节，不把整份文件读进内存。
fn bounded_read(full_path: &std::path::Path, start: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(full_path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// 服务路径在通过 `read_remote` 的存在性判定之后、再去触碰实际文件系统时
/// 遇到的 IO 失败——已经不是"文件不存在"这种正常情形（`read_remote` 已经
/// 确认过 `Present`），只可能是并发环境下的竞态或存储故障，映射为 I2
/// 注册的 `store.corrupt`（`needs_human`）。
fn io_corruption_response(full_path: &std::path::Path, e: &std::io::Error) -> Response {
    error_body(
        StatusCode::INTERNAL_SERVER_ERROR,
        "store.corrupt",
        format!("读取 {} 失败：{e}", full_path.display()),
    )
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
///
/// **Minor 修复**：区分"语法根本不认得/不合法"（RFC 9110 §14.2：忽略，
/// 退回整份内容，`Some(None)`）与"语法认得、单位也对，但数值上不满足"
/// （真正的 416，`None`）——修复前两者被混在一起，`notbytes=0-1`（单位不
/// 认识）、`bytes=abc-def`（数值非法）、`bytes=`（缺区间）都落进了后者，
/// 评审实测这三种都被错误地报了 416，与 RFC 9110 §14.2 矛盾（无法识别的
/// 单位、语法不合法的区间应当被忽略，不是当作"合法但不满足"）。
fn parse_range(header_value: &str, len: usize) -> Option<Option<(usize, usize)>> {
    let Some(spec) = header_value.strip_prefix("bytes=") else {
        return Some(None); // 无法识别的单位——忽略，不是 416。
    };
    // 只支持单一区间；含逗号说明是多重区间，语法上不理解，忽略退回整份内容。
    if spec.contains(',') {
        return Some(None);
    }
    let Some((start_str, end_str)) = spec.split_once('-') else {
        return Some(None); // 没有 `-`，语法不合法（如 `bytes=`）——忽略。
    };
    if len == 0 {
        return None; // 空内容上任何具体区间都不可满足。
    }
    let last = len - 1;
    if start_str.is_empty() {
        // 后缀形式 bytes=-N：最后 N 个字节。
        let Ok(suffix_len) = end_str.parse::<usize>() else {
            return Some(None); // 数值不合法（如 bytes=-abc）——忽略。
        };
        if suffix_len == 0 {
            return None;
        }
        let start = len.saturating_sub(suffix_len);
        return Some(Some((start, last)));
    }
    let Ok(start) = start_str.parse::<usize>() else {
        return Some(None); // 数值不合法（如 bytes=abc-def）——忽略。
    };
    if start > last {
        return None; // 语法合法，数值上超出边界——真正的 416。
    }
    let end = if end_str.is_empty() {
        last
    } else {
        match end_str.parse::<usize>() {
            Ok(requested) => requested.min(last),
            Err(_) => return Some(None), // 数值不合法（如 bytes=5-def）——忽略。
        }
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

    // 评审 I4：`GET /state` 没有基线可比对——不像 `arca-cli::sync` 那样能
    // 靠"本地记得这个路径以前存在过"精确定位异常，只能看到 `read_remote`
    // 交出的这一份 map。空 map 对全新数据集是完全合法的，但如果
    // `.arca/index/` 被抹掉、`files/` 下却确实躺着内容，这个空 map 与
    // "这本来就是个空数据集"在字节上没有任何区别——不额外核验就会把
    // "索引损坏"报告成"空库"，正是 I11 要防的形状经网络触发的变体
    // （`arca_cli::hub::empty_remote_hides_content` 文档）。只在 map 确实
    // 为空时才多付这一次核验的代价，不拖慢正常路径。
    if remote.is_empty() {
        match arca_cli::hub::empty_remote_hides_content(&root) {
            Ok(Some(example_path)) => {
                return error_body(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "store.corrupt",
                    format!(
                        ".arca/index/ 没有任何记录，但 files/ 下确实存在内容（例如 \
                         {example_path}）——索引被抹掉或与内容不同步，不是空数据集，\
                         绝不当作空库处理（评审 I4）"
                    ),
                );
            }
            Ok(None) => {} // 确实是全新的空数据集，照常返回 200 []。
            Err(e) => {
                return error_body(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "store.corrupt",
                    e.to_string(),
                )
            }
        }
    }

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
/// Minor 修复：重复的 `hash=` 查询参数不能悄悄只信第一个——`?hash=<真>&hash=<假>`
/// 这种形状此前会被 [`find_hash_param`] 静默取第一个、忽略第二个，与本文件
/// 别处（[`single_header`]）对重复条件头的处置纪律不一致。这里统一改成：
/// 出现两次或以上（无论取值是否相同）就当作没有提供合法的 `hash` 参数
/// （`Option::None`），调用方据此落回既有的 `400 request.hash_missing`——
/// 不新增错误码，"格式有歧义"与"压根没提供"共用同一个诊断结果。
fn find_hash_param(query: &str) -> Option<&str> {
    let mut found: Option<&str> = None;
    for pair in query.split('&') {
        // 与原实现同一条纪律：没有 `=` 的畸形分段直接跳过，不因此中止整个
        // 查询字符串的解析（`find_map` 原本的行为）。
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == "hash" {
            if found.is_some() {
                return None;
            }
            found = Some(value);
        }
    }
    found
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
    body: Body,
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

    // 打开存储根——先不拿锁：见下面「锁只包住 CAS 临界区」一节。挂载检查
    // 本身是只读的，不触碰任何需要与并发写入互斥的状态。
    let root_for_streaming = match dataset.open() {
        Ok(r) => r,
        Err(e) => return mount_error_response(&e),
    };

    // C2 修复：请求体流式写进 `.arca/tmp`，边到达边算哈希——绝不先把整份
    // 内容攒成 `Vec<u8>`（评审实测：改造前一次 600MB 的 PUT 会让 RSS 从
    // 6MB 涨到 1.86GB；一个宣称 1.2GB 的请求即便最终会被拒绝，也会先把
    // 1.2GB 吃进内存才返回 413）。超过 `MAX_BODY_BYTES` 时立即中止并清理
    // 已经写入的 tmp 文件，不等请求体发完。
    //
    // **锁只包住 CAS 临界区，不包住网络 IO**（这里是本轮修复顺带修正的一处
    // 实现教训，值得记下来）：最初的版本在整个流式接收循环期间持有
    // `dataset.write_lock`（`std::sync::MutexGuard`）——`.lock()` 返回的
    // 是 `std::sync::MutexGuard`，这个类型本身不是 `Send`（POSIX 互斥锁
    // 必须由加锁的同一线程解锁），一旦跨越循环里的 `.await` 持有，
    // 这个 handler 的 `Future` 就不再是 `Send`，会让 axum `Handler` trait
    // 的约束推导失败（表现成一条不知所云的「`Handler<_, _>` 未实现」
    // 编译错误，而不是直接指向真正原因的借用检查诊断）。锁真正要保护的
    // 只是`read_remote → 校验 → 写入`这一段 CAS 临界区（见 storage.rs
    // 「`write_lock`」一节），tmp 文件的写入本身不触碰任何需要与其它并发
    // 请求互斥的共享状态（每次写入的 tmp 文件名各自独立）——把锁的持有
    // 窗口收窄到只包住临界区，既修好了编译错误，也顺带缩短了锁的持有
    // 时长（不再因为一个慢客户端的网络传输而卡住同一数据集的其它请求）。
    let relative_target = format!("{}/{}", layout::FILES_DIR, path);
    let mut writer = match TmpWriter::create(&root_for_streaming, &relative_target) {
        Ok(w) => w,
        Err(e) => return store_write_error_response(e),
    };
    let mut hasher = ContentHash::hasher();
    let mut size: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(frame) = stream.next().await {
        let chunk = match frame {
            Ok(c) => c,
            Err(e) => {
                writer.abandon();
                return error_body(
                    StatusCode::BAD_REQUEST,
                    "request.body_read_failed",
                    format!("读取请求体失败：{e}"),
                );
            }
        };
        size += chunk.len() as u64;
        if size > MAX_BODY_BYTES {
            writer.abandon();
            return error_body(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request.body_too_large",
                format!(
                    "请求体超过 {MAX_BODY_BYTES} 字节上限，已中止接收（不会等整份内容收完才拒绝）"
                ),
            );
        }
        hasher.update(&chunk);
        if let Err(e) = writer.write_all(&chunk) {
            writer.abandon();
            return store_write_error_response(e);
        }
    }
    let hash = hasher.finish();

    // 临界区：拿到锁之后才重新打开存储根、才做 CAS 提交——见 storage.rs
    // 「write_lock」一节，这是并发正确性的唯一来源。到这里为止全部是
    // 同步代码（`Transport::commit_streamed` 不是 `async fn`），锁不会
    // 跨越任何 `.await`。
    let _guard = dataset.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let root = match dataset.open() {
        Ok(r) => r,
        Err(e) => {
            writer.abandon();
            return mount_error_response(&e);
        }
    };

    let transport = LocalTransport::new(&root);
    match transport.commit_streamed(
        &path,
        item_id,
        version_id.clone(),
        parent.clone(),
        writer,
        hash,
        size,
        mtime,
        actor,
    ) {
        Ok(CommitOutcome::Committed {
            item_id,
            version_id,
        }) => {
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
                    "size": size,
                })),
            )
                .into_response();
            set_cache_headers(resp.headers_mut(), &hash, &version_id);
            resp
        }
        Ok(CommitOutcome::Conflict {
            expected_parent,
            actual,
        }) => conflict_response(&root, item_id, &expected_parent, &actual, hash, size),
        Ok(CommitOutcome::IdentityMismatch {
            path,
            claimed_item_id,
            actual_item_id,
        }) => identity_mismatch_response(&path, claimed_item_id, actual_item_id),
        Err(e) => transport_error_response(e),
    }
}
/// `TmpWriter` 创建/写入失败——都是存储层故障（磁盘满、权限、`.arca/tmp`
/// 缺失等），映射到 I2 注册的 `store.corrupt`，与 [`transport_error_response`]
/// 同一处置纪律（`needs_human`，不是 `bug`）。
fn store_write_error_response(e: arca_store::atomic::AtomicError) -> Response {
    error_body(
        StatusCode::INTERNAL_SERVER_ERROR,
        "store.corrupt",
        e.to_string(),
    )
}

/// C1 修复：`Arca-Item-Id` 声称的身份与这次操作实际应归属的 item_id 不符——
/// `409`（不是 `412`：这不是"版本过期，换个 parent 重试就好"的 CAS 冲突，
/// 是"你打错了身份"，无论怎么重试 `If-Match`/`If-None-Match` 都不该成功，
/// 见 `CommitOutcome::IdentityMismatch` 的文档）。`code=request.item_id_mismatch`
/// 已在 `PROTOCOL.md` §7 注册，`class=needs_human`。
fn identity_mismatch_response(
    path: &str,
    claimed_item_id: ItemId,
    actual_item_id: Option<ItemId>,
) -> Response {
    let detail = match actual_item_id {
        Some(actual) => format!(
            "路径 {path:?} 实际归属 item_id {}，与请求声称的 {} 不符",
            actual.to_hex(),
            claimed_item_id.to_hex()
        ),
        None => format!(
            "item_id {} 已被 tombstone 终结，不能被任何后续提交复用（路径 {path:?}）",
            claimed_item_id.to_hex()
        ),
    };
    (
        StatusCode::CONFLICT,
        axum::Json(json!({
            "code": "request.item_id_mismatch",
            "message": detail,
            "path": path,
            "claimed_item_id": claimed_item_id.to_hex(),
            "actual_item_id": actual_item_id.map(|id| id.to_hex()),
        })),
    )
        .into_response()
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
/// - **Minor 修复**：`If-Match` 或 `If-None-Match` 中任何一个自己就重复出现
///   （见 [`single_header`]）→ 同样 `400`——不因为另一个头恰好合法就放过
///   这条本身就有歧义的请求，评审实测过重复 `If-Match` 会被静默放行 200
///   并覆盖内容。
fn parse_cas_condition(headers: &HeaderMap) -> Result<Option<VersionId>, Box<Response>> {
    let if_match = single_header(headers, "if-match").map_err(|_| Box::new(if_match_required()))?;
    let if_none_match =
        single_header(headers, "if-none-match").map_err(|_| Box::new(if_match_required()))?;
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

/// Minor 修复：某个条件头重复出现且取值有歧义（[`single_header`]）——只用
/// 在非 CAS 写入路径（目前只有 Range 续传的 `If-Match` 钉住检查），CAS
/// 写入路径复用既有的 [`if_match_required`]（语义已经是"没有提供有效的单一
/// 条件"，不必新造一个码）。`code=request.header_ambiguous`
/// （PROTOCOL.md §7，`class=needs_human`）。
fn ambiguous_header_response(name: &str) -> Response {
    error_body(
        StatusCode::BAD_REQUEST,
        "request.header_ambiguous",
        format!(
            "{name} 头出现了多次（或编码不合法）——arca 协议里这个头的载荷是\
             单一不透明令牌，不是可折叠的列表语法，不猜测该信哪一条，直接拒绝"
        ),
    )
}

/// 构造 412 的结构化冲突体（`base`/`theirs`/`yours`，PROTOCOL.md §1.2）。
///
/// `hash`/`size` 由调用方传入而不是这里从 `body: &[u8]` 现场算——**C2
/// 修复**：`put_file` 已经在流式接收请求体的同时增量算过一遍哈希，不应
/// 该为了拼这条诊断响应体再把整份内容读进内存重算一遍。
fn conflict_response(
    root: &StorageRoot,
    item_id: ItemId,
    expected_parent: &Option<VersionId>,
    actual: &RemoteState,
    hash: ContentHash,
    size: u64,
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
                "hash": hash.to_text(),
                "size": size,
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

    // Minor 修复：重复且矛盾的 `If-Match` 同样不能悄悄信第一条——DELETE 是
    // CAS 提交，猜错这里比猜错 Range 续传更危险（见 [`single_header`] 文档）。
    let Ok(Some(if_match)) = single_header(&headers, "if-match") else {
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
        Ok(CommitOutcome::IdentityMismatch {
            path,
            claimed_item_id,
            actual_item_id,
        }) => identity_mismatch_response(&path, claimed_item_id, actual_item_id),
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

    /// I2 修复复现：`Transport` 失败（这里用一条手工损坏的 `items/` 链模拟
    /// "内容缺失/链断裂"这类评审实测触发过的最常见真实故障）此前会翻译成
    /// 裸 `code="internal"`（`PROTOCOL.md` §7 从未注册过），现在必须是已
    /// 注册的 `code=store.corrupt`——agent 才能按 §7 的 `class` 表分支成
    /// "停下报告"而不是误当成需要开工单的代码 bug。
    #[tokio::test]
    async fn 存储损坏时500响应携带已注册的store_corrupt码() {
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

        // 手工损坏这个 item 的版本链——模拟磁盘/权限故障或人为破坏后的
        // "结构性损坏"，不是任何一次正常写入会产生的状态。
        let rel = arca_format::hub_layout::layout::item_path(&item_id);
        let full = dir.path().join(rel);
        std::fs::write(&full, "不是合法的 JSON 行\n").unwrap();

        let req = Request::builder()
            .uri(format!("/v1/datasets/{id}/state"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "store.corrupt");
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

    /// Minor 项复现：修复前 `PUT files/.arca/x` 能成功，在 hub 的逃生舱树
    /// `files/` 下凭空造出一个名叫 `.arca` 的子目录（真实元数据
    /// `<storage-root>/.arca/` 不受影响，但污染了 I1「`files/` 永远是普通
    /// 文件树」的承诺）。修复后必须在触碰文件系统前就被 `path_rules::check`
    /// 拒绝为 `400 path.rejected`，`files/.arca/` 不应该被创建出来。
    #[tokio::test]
    async fn put进入arca保留目录被拒绝且不落地() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let version_id = arca_cli::ids::new_version_id();
        let req = create_request(id, ".arca/evil.txt", item_id, &version_id, b"pwned");
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "path.rejected");

        assert!(
            !dir.path().join("files/.arca").exists(),
            "被拒绝的写入不应该在 files/ 下留下任何痕迹"
        );
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

    /// Minor 项复现：`notbytes=0-1`（无法识别的单位）、`bytes=abc-def`
    /// （数值非法）、`bytes=`（缺区间）此前都被误判为"语法合法但数值不
    /// 满足"而返回 416；RFC 9110 §14.2 要求这三种都被忽略、退回整份内容
    /// （`200`），只有语法合法、单位认得、但数值确实越界（如
    /// `bytes=1000-2000` 打在 11 字节的内容上）才是真正的 416。
    #[tokio::test]
    async fn range头语法不合法时忽略退回整份内容而不是416() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let version_id = arca_cli::ids::new_version_id();
        let put_req = create_request(id, "r.txt", item_id, &version_id, b"hello world");
        assert_eq!(
            app.clone().oneshot(put_req).await.unwrap().status(),
            StatusCode::CREATED
        );

        for bad_range in ["notbytes=0-1", "bytes=abc-def", "bytes="] {
            let req = Request::builder()
                .uri(format!("/v1/datasets/{id}/files/r.txt"))
                .header("range", bad_range)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "Range: {bad_range:?} 语法不合法，应被忽略退回整份内容，不是 416"
            );
            assert_eq!(body_bytes(resp).await, b"hello world");
        }

        // 对照：数值上真正超出边界的合法语法仍然是 416。
        let oob_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/r.txt"))
            .header("range", "bytes=1000-2000")
            .body(Body::empty())
            .unwrap();
        let oob_resp = app.oneshot(oob_req).await.unwrap();
        assert_eq!(oob_resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    /// I1 修复复现：手工编辑 `files/<path>`（I1 明确鼓励的逃生舱操作，也是
    /// PUT 第 1/2 步之间崩溃会留下的同一种漂移）之后，服务端此前完全信
    /// 元数据记录里的旧哈希，`If-None-Match` 重新验证永远命中 304——评审
    /// 实机验证过这条路径。修复后：磁盘实际大小与记录不符时现场重算，
    /// 旧的 `If-None-Match` 不应再命中，必须吐出新内容与新 ETag。
    #[tokio::test]
    async fn 手工编辑files内容后旧etag不再命中304而是吐出新内容() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let version_id = arca_cli::ids::new_version_id();
        let put_req = create_request(id, "a.txt", item_id, &version_id, b"original content");
        assert_eq!(
            app.clone().oneshot(put_req).await.unwrap().status(),
            StatusCode::CREATED
        );
        let original_hash = ContentHash::from_bytes(b"original content");

        // 逃生舱操作：直接改 files/ 下的字节，不经过任何 arca 命令——
        // index/items 记录里的哈希/大小原封不动，仍然是旧内容的。
        std::fs::write(
            dir.path().join("files/a.txt"),
            b"tampered by hand, longer now",
        )
        .unwrap();

        // 拿着旧哈希做条件请求——修复前会永远命中 304（服务端从不去看磁盘
        // 上实际的字节是否还是当初记录的那份），修复后必须识别出漂移。
        let cond_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("if-none-match", format!("\"{}\"", original_hash.to_text()))
            .body(Body::empty())
            .unwrap();
        let cond_resp = app.clone().oneshot(cond_req).await.unwrap();
        assert_ne!(
            cond_resp.status(),
            StatusCode::NOT_MODIFIED,
            "手工改过的内容不应该继续被旧 ETag 命中 304"
        );
        assert_eq!(cond_resp.status(), StatusCode::OK);
        let new_etag = cond_resp
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let expected_new_hash = ContentHash::from_bytes(b"tampered by hand, longer now");
        assert_eq!(new_etag, format!("\"{}\"", expected_new_hash.to_text()));
        assert_ne!(new_etag, format!("\"{}\"", original_hash.to_text()));
        assert_eq!(body_bytes(cond_resp).await, b"tampered by hand, longer now");
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

    /// 评审 I4 的实机复现：内容还在、`.arca/index/` 被整个抹掉——修复前
    /// `GET /state` 会静默返回 `200 []`，与"这本来就是个空数据集"在字节上
    /// 没有任何区别，正是 I11 要防的"挂载/索引缺失呈现成空库"经网络触发的
    /// 变体。修复后必须报 `500 store.corrupt`，绝不能报告成空库。
    #[tokio::test]
    async fn get_state在索引被抹掉但files非空时报corrupt而不是空数组() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let put_req = create_request(
            id,
            "precious.bin",
            arca_cli::ids::new_item_id(),
            &arca_cli::ids::new_version_id(),
            b"still here",
        );
        assert_eq!(
            app.clone().oneshot(put_req).await.unwrap().status(),
            StatusCode::CREATED
        );

        // 模拟索引被整个抹掉：files/ 原封不动。
        std::fs::remove_dir_all(dir.path().join(".arca/index")).unwrap();
        assert!(dir.path().join("files/precious.bin").is_file());

        let req = Request::builder()
            .uri(format!("/v1/datasets/{id}/state"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "索引被抹掉但内容还在时绝不能报 200 空数组"
        );
        let body = body_json(resp).await;
        assert_eq!(body["code"], "store.corrupt");
    }

    /// 对照：全新数据集（从未写过任何内容）`.arca/index/` 本就没有记录，
    /// `GET /state` 仍应正常返回 `200 []`——I4 的修复不能矫枉过正，把"真的
    /// 什么都没有"也当成损坏。
    #[tokio::test]
    async fn get_state对全新空数据集仍返回200空数组() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let req = Request::builder()
            .uri(format!("/v1/datasets/{id}/state"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    // -----------------------------------------------------------------
    // Task 5：CAS 写入端点
    // -----------------------------------------------------------------

    /// C2 修复复现：改造前，一个超限的请求体会先被整个吃进内存（即便最终
    /// 会被拒绝）才返回 413——评审实测一个宣称 1.2GB 的 PUT 会先涨到
    /// 1.2GB 常驻内存才拒绝。这里不构造一个真的 300MB 的 `Vec<u8>`（那会
    /// 让这条测试本身变成它要防的那种内存滥用）：用同一个 1MB 缓冲区的
    /// `Bytes` 克隆（引用计数，不复制底层数据）反复喂给一个惰性
    /// `Stream`，只要修复生效，处理器应该在累计超过 `MAX_BODY_BYTES`
    /// （256MB）的那个分片就中止，不需要真的把全部 300 个分片都消费完——
    /// 用测试本身的低内存占用与快速返回反证修复已经生效。
    #[tokio::test]
    async fn put超过体积上限时提前拒绝而不等请求体发完() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let chunk = axum::body::Bytes::from(vec![7u8; 1024 * 1024]); // 1 MiB
        let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> =
            std::iter::repeat_with(|| Ok(chunk.clone()))
                .take(300) // 300 MiB 总量 > 256 MiB 上限
                .collect();
        let stream = futures_util::stream::iter(chunks);
        let body = Body::from_stream(stream);

        let item_id = arca_cli::ids::new_item_id();
        let version_id = arca_cli::ids::new_version_id();
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v1/datasets/{id}/files/big.bin"))
            .header("if-none-match", "*")
            .header("arca-item-id", item_id.to_hex())
            .header("arca-version-id", version_id.as_str())
            .header("arca-mtime", "2026-08-08T09:00:00Z")
            .body(body)
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body_json_val = body_json(resp).await;
        assert_eq!(body_json_val["code"], "request.body_too_large");

        // 中止之后不应该在 .arca/tmp/ 留下任何残留（TmpWriter::abandon
        // 已清理）——绝不留孤儿临时文件。
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(".arca/tmp"))
            .unwrap()
            .collect();
        assert!(
            leftovers.is_empty(),
            "413 之后不应有 tmp 残留：{leftovers:?}"
        );

        // 也不应该在 files/ 下创建任何内容。
        assert!(!dir.path().join("files/big.bin").exists());
    }

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

    /// Minor 项复现（重复 `If-Match` 头）：修复前 `headers.get("if-match")`
    /// 静默只取第一条、忽略矛盾的第二条——评审实测两条 `If-Match`（一条是
    /// 当前正确的 parent、一条是伪造的）仍然放行 `200` 并覆盖内容。修复后
    /// 必须整体拒绝为 `400`，且内容不受影响。
    #[tokio::test]
    async fn put重复且矛盾的if_match头返回400且内容不受影响() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let v1 = arca_cli::ids::new_version_id();
        let put1 = create_request(id, "a.txt", item_id, &v1, b"original");
        assert_eq!(
            app.clone().oneshot(put1).await.unwrap().status(),
            StatusCode::CREATED
        );

        let v2 = arca_cli::ids::new_version_id();
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            // 两条 If-Match：一条是当前真正的 parent（v1），一条是伪造的——
            // 歧义必须整体拒绝，不能因为其中一条碰巧合法就放行。
            .header("if-match", v1.as_str())
            .header(
                "if-match",
                "20260101T000000Z-deadbeefdeadbeefdeadbeefdeadbeef",
            )
            .header("arca-item-id", item_id.to_hex())
            .header("arca-version-id", v2.as_str())
            .header("arca-mtime", "2026-08-08T09:00:00Z")
            .body(Body::from(b"overwritten".to_vec()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "request.if_match_required");

        // 内容必须原封不动——歧义请求绝不能有一半生效。
        let get_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .body(Body::empty())
            .unwrap();
        let get_resp = app.oneshot(get_req).await.unwrap();
        assert_eq!(body_bytes(get_resp).await, b"original");
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

    // -----------------------------------------------------------------
    // C1 修复：Arca-Item-Id 身份校验（复现原评审的三个实机利用）
    // -----------------------------------------------------------------

    /// 利用 1 复现：同一个 item_id 打两个不同路径，两次都用 If-None-Match:*
    /// 声明"创建"。修复前，两次都会成功，各自往
    /// `items/<item_id>.jsonl` 追加一条 `parent:null` 记录，链从此断裂，
    /// 该数据集的每个端点此后永久 500。修复后：第二次必须被拒绝为 409，
    /// 且被拒绝之后 `GET /state`（会读到两个 item 各自的链）必须继续正常
    /// 工作——数据集不能因为这次攻击尝试被拖垮。
    #[tokio::test]
    async fn 同一item_id打两个路径第二次返回409且数据集不被拖垮() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let v1 = arca_cli::ids::new_version_id();
        let put1 = create_request(id, "a.txt", item_id, &v1, b"a-content");
        assert_eq!(
            app.clone().oneshot(put1).await.unwrap().status(),
            StatusCode::CREATED
        );

        let v2 = arca_cli::ids::new_version_id();
        let put2 = create_request(id, "b.txt", item_id, &v2, b"b-content");
        let resp2 = app.clone().oneshot(put2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::CONFLICT);
        let body = body_json(resp2).await;
        assert_eq!(body["code"], "request.item_id_mismatch");

        // items/<item_id>.jsonl 必须仍然只有一条记录（链没有被第二次
        // 提交污染）——数据集此后仍然可以正常服务。
        let state_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/state"))
            .body(Body::empty())
            .unwrap();
        let state_resp = app.clone().oneshot(state_req).await.unwrap();
        assert_eq!(state_resp.status(), StatusCode::OK);
        let state = body_json(state_resp).await;
        let arr = state.as_array().unwrap();
        assert_eq!(arr.len(), 1, "b.txt 不应该被创建：{arr:?}");
        assert_eq!(arr[0]["path"], "a.txt");

        // b.txt 确实没有被创建。
        let get_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/b.txt"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(get_req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    /// 利用 2 复现：先创建再删除一个路径，然后用同一个（已被 tombstone
    /// 终结的）item_id 在同一路径上重新 `PUT`（`If-None-Match: *`，"仅创建"
    /// 语义）。修复前：`commit` 把 `Tombstoned` 折叠成 `None`，与
    /// `parent:None` 匹配，写入被判定成功（201），字节落地，但
    /// `read_remote` 优先信 journal，此后 `GET`/`GET /state` 永远继续报告
    /// tombstoned——这是一次被确认成功、却永久不可见的丢失写入。修复后
    /// 必须在写入之前就拒绝（409），不产生这个悬空字节。
    #[tokio::test]
    async fn 复用已tombstone的item_id创建同路径返回409且不产生悬空写入() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let item_id = arca_cli::ids::new_item_id();
        let v1 = arca_cli::ids::new_version_id();
        let put1 = create_request(id, "a.txt", item_id, &v1, b"original");
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
        assert_eq!(
            app.clone().oneshot(del).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );

        // 复用同一个（已终结的）item_id，声称"仅创建"——必须 409，绝不能
        // 得到 201。
        let v2 = arca_cli::ids::new_version_id();
        let resurrect = create_request(id, "a.txt", item_id, &v2, b"attacker-controlled");
        let resurrect_resp = app.clone().oneshot(resurrect).await.unwrap();
        assert_eq!(resurrect_resp.status(), StatusCode::CONFLICT);
        let body = body_json(resurrect_resp).await;
        assert_eq!(body["code"], "request.item_id_mismatch");
        assert_eq!(body["actual_item_id"], serde_json::Value::Null);

        // 也必须挡住"推进"形态的复用：声称知道被终结前的最后一个
        // version_id（真实攻击者能从 GET /state 读到），用 If-Match 而不是
        // If-None-Match:*。
        let v3 = arca_cli::ids::new_version_id();
        let resurrect_advance = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("if-match", v1.as_str())
            .header("arca-item-id", item_id.to_hex())
            .header("arca-version-id", v3.as_str())
            .header("arca-mtime", "2026-08-08T09:30:00Z")
            .body(Body::from(b"attacker-controlled-2".to_vec()))
            .unwrap();
        let resurrect_advance_resp = app.clone().oneshot(resurrect_advance).await.unwrap();
        assert_eq!(resurrect_advance_resp.status(), StatusCode::CONFLICT);

        // 路径此刻仍应是 tombstoned，不是 attacker 写入的新内容。
        let get_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(get_req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    /// 利用 3 复现：DELETE 带一个与该路径真实归属不符的伪造 item_id（但
    /// `If-Match` 版本号是真实、正确的，攻击者能从 `GET /state` 读到）。
    /// 修复前：`tombstone` 只按路径做 CAS 比较，从不核对 item_id，204
    /// 成功，但把伪造的 item_id 写进 `.meta` 与 journal——I8 的审计链被
    /// 伪造。修复后必须 409，且真实内容完全不受影响。
    #[tokio::test]
    async fn delete伪造item_id返回409且真实内容不受影响() {
        let dir = tempfile::tempdir().unwrap();
        let id = "9c41000000000000000000000000abcd";
        造存储根(dir.path(), id);
        let app = build_router(vec![(id, dir.path().to_path_buf())]);

        let real_item_id = arca_cli::ids::new_item_id();
        let v1 = arca_cli::ids::new_version_id();
        let put1 = create_request(id, "a.txt", real_item_id, &v1, b"real-content");
        assert_eq!(
            app.clone().oneshot(put1).await.unwrap().status(),
            StatusCode::CREATED
        );

        // 伪造的 item_id——与 a.txt 的真实归属无关，但请求者能从
        // GET /state 读到 v1 这个真实的 version_id。
        let forged_item_id = arca_cli::ids::new_item_id();
        assert_ne!(forged_item_id, real_item_id);
        let del = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .header("if-match", v1.as_str())
            .header("arca-item-id", forged_item_id.to_hex())
            .body(Body::empty())
            .unwrap();
        let del_resp = app.clone().oneshot(del).await.unwrap();
        assert_eq!(del_resp.status(), StatusCode::CONFLICT);
        let body = body_json(del_resp).await;
        assert_eq!(body["code"], "request.item_id_mismatch");
        assert_eq!(body["actual_item_id"], real_item_id.to_hex());

        // a.txt 必须继续存在、内容不变——伪造的 DELETE 完全没有生效。
        let get_req = Request::builder()
            .uri(format!("/v1/datasets/{id}/files/a.txt"))
            .body(Body::empty())
            .unwrap();
        let get_resp = app.clone().oneshot(get_req).await.unwrap();
        assert_eq!(get_resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(get_resp).await, b"real-content");

        // trash 里也不应该出现伪造 item_id 的记录（真实 item_id 也不应该，
        // 因为这次删除根本没有真的发生）。
        let trash_req = Request::builder()
            .uri(format!(
                "/v1/datasets/{id}/trash/{}?hash={}",
                real_item_id.to_hex(),
                ContentHash::from_bytes(b"real-content").to_hex()
            ))
            .body(Body::empty())
            .unwrap();
        let trash_resp = app.oneshot(trash_req).await.unwrap();
        assert_eq!(trash_resp.status(), StatusCode::NOT_FOUND);
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
