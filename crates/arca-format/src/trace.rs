//! trace 事件格式：诊断轨迹的 schema 与 sink 抽象。
//!
//! 设计依据 spec §3.3（可诊断性），字节契约 FORMAT.md §10，
//! 命令与错误码契约 PROTOCOL.md §5.1/§7。
//! 命名与结构对齐 git 的 `trace2`（`GIT_TRACE2_EVENT`）。
//!
//! **trace 是可丢弃的诊断产物，不是真相**——真相在 journal（§7.2）与 `.txn`。
//! 由此推出本模块两条与其余格式相反的纪律：
//!
//! - 读侧坏行**跳过并计数**，绝不因一行损坏丢掉其余线索（[`read_lines`]）；
//!   journal 的纪律恰恰相反（中间行损坏则失败），因为 journal 读错一行等于伪造历史。
//! - 写侧**永不失败**：[`TraceEvent::to_json_line`] 无 `Result`。
//!   诊断设施绝不能成为命令失败的原因（PROTOCOL.md §5.2）。
//!
//! sans-io：本模块不碰时钟、不碰文件系统。`t_abs_us` 由调用方注入
//! （确定性模拟测试注入模拟时钟即可逐字节复现 trace，spec §11.2）。

use crate::error::FormatError;
use crate::path_rules::PathStatus;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;

/// 本实现能写出、也是能读懂的最高 trace 记录格式版本（FORMAT.md §0）。
pub const TRACE_VERSION: u32 = 1;

/// sid 的最大段数（FORMAT.md §10.2）：超过则拒绝，防止无界嵌套撑爆路径（I5）。
pub const MAX_SID_SEGMENTS: usize = 8;

/// [`RingSink`] 的默认容量（约 1 MB 量级的事件）。
pub const DEFAULT_RING_CAPACITY: usize = 4096;

/// 信封字段名。载荷不得占用这些键——占用时以信封为准（FORMAT.md §10.1）。
const ENVELOPE_KEYS: [&str; 5] = ["v", "sid", "seq", "t_abs", "event"];

// ---------------------------------------------------------------------------
// sid
// ---------------------------------------------------------------------------

/// 层次化会话标识（FORMAT.md §10.2）。
///
/// 单段形如 `20260805T093012Z-0123456789abcdef`：紧凑时间戳 + `-` + 16 位小写十六进制。
/// 时间戳前缀使**字典序即时间序**，与 [`crate::model::VersionId`] 同构。
/// 子进程继承父 sid 并以 `/` 追加自己的一段，于是 `arca sync` 与它调起的
/// `arca fetch` / `arca push` 天然串成一棵树（借 git trace2）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sid(String);

impl Sid {
    /// 从时间戳与随机段构造根 sid。
    ///
    /// `timestamp` 形如 `20260805T093012Z`；`random_hex` 为 16 位小写十六进制。
    pub fn new(timestamp: &str, random_hex: &str) -> Result<Self, FormatError> {
        Ok(Sid(build_segment(timestamp, random_hex)?))
    }

    /// 派生子 sid：在自身之后追加一段。
    pub fn child(&self, timestamp: &str, random_hex: &str) -> Result<Self, FormatError> {
        if self.depth() >= MAX_SID_SEGMENTS {
            return Err(FormatError::Malformed {
                line: 0,
                reason: format!("sid 段数已达上限 {MAX_SID_SEGMENTS}，拒绝继续嵌套"),
            });
        }
        let segment = build_segment(timestamp, random_hex)?;
        Ok(Sid(format!("{}/{}", self.0, segment)))
    }

    /// 解析完整 sid（含层次）。任何不合规输入返回 `Err`，绝不 panic（I5）。
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        if text.is_empty() {
            return Err(FormatError::Malformed {
                line: 0,
                reason: "sid 为空".to_string(),
            });
        }
        let segments: Vec<&str> = text.split('/').collect();
        if segments.len() > MAX_SID_SEGMENTS {
            return Err(FormatError::Malformed {
                line: 0,
                reason: format!("sid 有 {} 段，超过上限 {MAX_SID_SEGMENTS}", segments.len()),
            });
        }
        for segment in &segments {
            check_segment(segment)?;
        }
        Ok(Sid(text.to_string()))
    }

    /// 根段——同一棵进程树的所有 sid 共享它。
    pub fn root(&self) -> &str {
        match self.0.split_once('/') {
            Some((head, _)) => head,
            None => &self.0,
        }
    }

    /// 末段——落盘文件名用它（FORMAT.md §10.6）。
    pub fn leaf(&self) -> &str {
        match self.0.rsplit_once('/') {
            Some((_, tail)) => tail,
            None => &self.0,
        }
    }

    pub fn depth(&self) -> usize {
        self.0.split('/').count()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn build_segment(timestamp: &str, random_hex: &str) -> Result<String, FormatError> {
    let candidate = format!("{timestamp}-{random_hex}");
    check_segment(&candidate)?;
    Ok(candidate)
}

/// 校验单段 sid。
///
/// 注意（I5）：全程按 `as_bytes()` 的字节切片比较。若退化回按字符边界切片 `str`，
/// 对多字节 UTF-8 输入可能切在字符中间而 panic——与 [`crate::model::VersionId::new`]
/// 同源的陷阱，那里已有回归测试，此处照同一纪律处理。
fn check_segment(segment: &str) -> Result<(), FormatError> {
    let bytes = segment.as_bytes();
    // 16 字节时间戳 + 1 字节 '-' + 16 字节十六进制。
    let ok = bytes.len() == 33
        && bytes[8] == b'T'
        && bytes[15] == b'Z'
        && bytes[16] == b'-'
        && bytes[..8].iter().all(|b| b.is_ascii_digit())
        && bytes[9..15].iter().all(|b| b.is_ascii_digit())
        && bytes[17..]
            .iter()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if ok {
        Ok(())
    } else {
        Err(FormatError::Malformed {
            line: 0,
            reason: format!("sid 段 {segment:?} 不是 <YYYYMMDDTHHMMSSZ>-<16 位小写十六进制> 形式"),
        })
    }
}

// ---------------------------------------------------------------------------
// 事件类型
// ---------------------------------------------------------------------------

/// 事件类型（FORMAT.md §10.3）。写侧封闭，读侧以 [`EventKind::Unknown`] 容忍未知——
/// 这是「agent 友好」的前提：agent 对 `event` 做精确匹配，不必正则捞字符串。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventKind {
    // 会话骨架（借 git trace2）
    Start,
    Exit,
    RegionEnter,
    RegionLeave,
    Error,
    Panic,
    /// 环形缓冲挤掉的事件数——绝不静默截断线索（I5，FORMAT.md §10.6）。
    TraceDropped,

    // arca 领域事件
    MountCheck,
    LockAcquire,
    LockWait,
    LockRelease,
    PathReject,
    ScanSummary,
    ReconcileDecide,
    CommitAttempt,
    CommitResult,
    ConflictCopy,
    TxnBegin,
    TxnCommit,
    TxnRollback,
    TransferSummary,

    /// 只在读侧产生：本实现不认识的事件名，原样透传（向前兼容，FORMAT.md §10.5）。
    Unknown(String),
}

impl EventKind {
    pub fn as_str(&self) -> &str {
        match self {
            EventKind::Start => "start",
            EventKind::Exit => "exit",
            EventKind::RegionEnter => "region_enter",
            EventKind::RegionLeave => "region_leave",
            EventKind::Error => "error",
            EventKind::Panic => "panic",
            EventKind::TraceDropped => "trace.dropped",
            EventKind::MountCheck => "mount.check",
            EventKind::LockAcquire => "lock.acquire",
            EventKind::LockWait => "lock.wait",
            EventKind::LockRelease => "lock.release",
            EventKind::PathReject => "path.reject",
            EventKind::ScanSummary => "scan.summary",
            EventKind::ReconcileDecide => "reconcile.decide",
            EventKind::CommitAttempt => "commit.attempt",
            EventKind::CommitResult => "commit.result",
            EventKind::ConflictCopy => "conflict.copy",
            EventKind::TxnBegin => "txn.begin",
            EventKind::TxnCommit => "txn.commit",
            EventKind::TxnRollback => "txn.rollback",
            EventKind::TransferSummary => "transfer.summary",
            EventKind::Unknown(name) => name,
        }
    }

    /// 解析事件名。**不会失败**——未知名字归入 [`EventKind::Unknown`]，
    /// 因为新版本 arca 写出的 trace 必须能被旧版本读出来（FORMAT.md §10.5）。
    pub fn parse(name: &str) -> Self {
        match name {
            "start" => EventKind::Start,
            "exit" => EventKind::Exit,
            "region_enter" => EventKind::RegionEnter,
            "region_leave" => EventKind::RegionLeave,
            "error" => EventKind::Error,
            "panic" => EventKind::Panic,
            "trace.dropped" => EventKind::TraceDropped,
            "mount.check" => EventKind::MountCheck,
            "lock.acquire" => EventKind::LockAcquire,
            "lock.wait" => EventKind::LockWait,
            "lock.release" => EventKind::LockRelease,
            "path.reject" => EventKind::PathReject,
            "scan.summary" => EventKind::ScanSummary,
            "reconcile.decide" => EventKind::ReconcileDecide,
            "commit.attempt" => EventKind::CommitAttempt,
            "commit.result" => EventKind::CommitResult,
            "conflict.copy" => EventKind::ConflictCopy,
            "txn.begin" => EventKind::TxnBegin,
            "txn.commit" => EventKind::TxnCommit,
            "txn.rollback" => EventKind::TxnRollback,
            "transfer.summary" => EventKind::TransferSummary,
            other => EventKind::Unknown(other.to_string()),
        }
    }
}

/// 错误处置类别（FORMAT.md §10.4、PROTOCOL.md §7）。
///
/// **agent 只看这个就知道该重试、该停下、还是该报 bug，无需理解 `code` 的语义。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ErrorClass {
    /// 网络抖动、锁竞争 → 退避重试。
    Retryable,
    /// 卷身份不符、孤儿数据集、一致性冲突 → **停下**（I5），报告给人。
    NeedsHuman,
    /// CAS 412 等 → 走结构化冲突流程，不作为错误处理。
    Protocol,
    /// 内部不变量被破坏 → 提 issue。
    Bug,
}

impl ErrorClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorClass::Retryable => "retryable",
            ErrorClass::NeedsHuman => "needs_human",
            ErrorClass::Protocol => "protocol",
            ErrorClass::Bug => "bug",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "retryable" => Some(ErrorClass::Retryable),
            "needs_human" => Some(ErrorClass::NeedsHuman),
            "protocol" => Some(ErrorClass::Protocol),
            "bug" => Some(ErrorClass::Bug),
            _ => None,
        }
    }

    /// agent 是否可以自行退避重试。其余类别都必须停下或转交别的流程。
    pub fn is_retryable(&self) -> bool {
        matches!(self, ErrorClass::Retryable)
    }
}

// ---------------------------------------------------------------------------
// 载荷
// ---------------------------------------------------------------------------

/// 载荷字段值：**只允许标量**（FORMAT.md §10.1）。
///
/// 不提供嵌套是刻意的——`jq` 处理无需展开，且防止有人把整个结构体塞进 trace
/// 让 schema 失控。需要表达集合时用多条事件。
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Str(Cow<'static, str>),
    U64(u64),
    I64(i64),
    Bool(bool),
    Null,
}

impl From<&'static str> for FieldValue {
    fn from(value: &'static str) -> Self {
        FieldValue::Str(Cow::Borrowed(value))
    }
}

impl From<String> for FieldValue {
    fn from(value: String) -> Self {
        FieldValue::Str(Cow::Owned(value))
    }
}

impl From<Cow<'static, str>> for FieldValue {
    fn from(value: Cow<'static, str>) -> Self {
        FieldValue::Str(value)
    }
}

impl From<u64> for FieldValue {
    fn from(value: u64) -> Self {
        FieldValue::U64(value)
    }
}

impl From<usize> for FieldValue {
    fn from(value: usize) -> Self {
        FieldValue::U64(value as u64)
    }
}

impl From<i64> for FieldValue {
    fn from(value: i64) -> Self {
        FieldValue::I64(value)
    }
}

impl From<i32> for FieldValue {
    fn from(value: i32) -> Self {
        FieldValue::I64(value as i64)
    }
}

impl From<bool> for FieldValue {
    fn from(value: bool) -> Self {
        FieldValue::Bool(value)
    }
}

impl From<PathStatus> for FieldValue {
    fn from(value: PathStatus) -> Self {
        FieldValue::Str(Cow::Borrowed(value.as_str()))
    }
}

impl From<ErrorClass> for FieldValue {
    fn from(value: ErrorClass) -> Self {
        FieldValue::Str(Cow::Borrowed(value.as_str()))
    }
}

/// 载荷：**不含信封字段**（`sid` / `seq` 由 sink 补齐）。
///
/// 这是 arca-core 唯一需要构造的类型——core 不持有 sid，也不持有时钟以外的环境。
#[derive(Debug, Clone, PartialEq)]
pub struct TraceRecord {
    pub event: EventKind,
    /// 自本会话 `start` 起的单调微秒数。由调用方注入的时钟提供（sans-io）。
    pub t_abs_us: u64,
    fields: Vec<(Cow<'static, str>, FieldValue)>,
}

impl TraceRecord {
    pub fn new(event: EventKind, t_abs_us: u64) -> Self {
        TraceRecord {
            event,
            t_abs_us,
            fields: Vec::new(),
        }
    }

    /// 链式添加载荷字段。同名字段以最后一次为准（序列化时去重，绝不产生重复键）。
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<FieldValue>) -> Self {
        self.push(Cow::Borrowed(key), value);
        self
    }

    pub fn push(&mut self, key: impl Into<Cow<'static, str>>, value: impl Into<FieldValue>) {
        self.fields.push((key.into(), value.into()));
    }

    /// 取字段值。同名多次写入时返回最后一次——与序列化行为一致。
    pub fn field(&self, key: &str) -> Option<&FieldValue> {
        self.fields
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn fields(&self) -> &[(Cow<'static, str>, FieldValue)] {
        &self.fields
    }

    /// 与信封字段同名、因而会在序列化时被丢弃的载荷键（FORMAT.md §10.1）。
    ///
    /// 写侧永不失败，所以冲突只能靠测试发现——本方法就是给测试用的。
    pub fn envelope_conflicts(&self) -> Vec<&str> {
        let mut hits: Vec<&str> = self
            .fields
            .iter()
            .map(|(k, _)| k.as_ref())
            .filter(|k| ENVELOPE_KEYS.contains(k))
            .collect();
        hits.sort_unstable();
        hits.dedup();
        hits
    }
}

// ---------------------------------------------------------------------------
// 落盘的一行
// ---------------------------------------------------------------------------

/// 落盘的完整一行 = 信封 + 载荷（FORMAT.md §10.1）。
#[derive(Debug, Clone, PartialEq)]
pub struct TraceEvent {
    pub sid: Sid,
    pub seq: u64,
    pub record: TraceRecord,
}

impl TraceEvent {
    pub fn new(sid: Sid, seq: u64, record: TraceRecord) -> Self {
        TraceEvent { sid, seq, record }
    }

    /// 序列化为一行 JSON（不含结尾换行）。
    ///
    /// **无 `Result` 是设计的一部分**：诊断设施绝不能成为命令失败的原因
    /// （PROTOCOL.md §5.2）。载荷与信封同名时丢弃载荷侧，绝不产生重复键。
    /// 载荷按键名字节序升序输出——同内容必产生同字节（FORMAT.md §10.1、§9.3）。
    pub fn to_json_line(&self) -> String {
        let mut out = String::with_capacity(128);
        out.push_str("{\"v\":");
        out.push_str(&TRACE_VERSION.to_string());
        out.push_str(",\"sid\":");
        push_json_string(&mut out, self.sid.as_str());
        out.push_str(",\"seq\":");
        out.push_str(&self.seq.to_string());
        out.push_str(",\"t_abs\":");
        out.push_str(&self.record.t_abs_us.to_string());
        out.push_str(",\"event\":");
        push_json_string(&mut out, self.record.event.as_str());

        // 稳定排序 + 保留同键的最后一次写入（`with`/`push` 的覆盖语义）。
        let mut payload: Vec<(&str, &FieldValue)> = self
            .record
            .fields
            .iter()
            .map(|(k, v)| (k.as_ref(), v))
            .filter(|(k, _)| !ENVELOPE_KEYS.contains(k))
            .collect();
        payload.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut previous: Option<&str> = None;
        for index in 0..payload.len() {
            let (key, value) = payload[index];
            // 同键相邻，只写最后一个。
            if payload.get(index + 1).map(|(k, _)| *k) == Some(key) {
                continue;
            }
            debug_assert!(previous != Some(key), "去重后不应出现重复键");
            previous = Some(key);
            out.push(',');
            push_json_string(&mut out, key);
            out.push(':');
            push_json_value(&mut out, value);
        }
        out.push('}');
        out
    }

    /// 解析一行。载荷字段名的顺序不影响结果。
    pub fn parse_line(line: &str) -> Result<Self, FormatError> {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|err| FormatError::Malformed {
                line: 0,
                reason: format!("不是合法 JSON：{err}"),
            })?;
        let object = value.as_object().ok_or_else(|| FormatError::Malformed {
            line: 0,
            reason: "trace 行必须是 JSON 对象".to_string(),
        })?;

        let version = object
            .get("v")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| FormatError::Malformed {
                line: 0,
                reason: "缺少 v 字段".to_string(),
            })?;
        if version > u64::from(TRACE_VERSION) {
            return Err(FormatError::UnsupportedVersion {
                found: u32::try_from(version).unwrap_or(u32::MAX),
                max: TRACE_VERSION,
            });
        }

        let sid_text = object
            .get("sid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| FormatError::Malformed {
                line: 0,
                reason: "缺少 sid 字段".to_string(),
            })?;
        let sid = Sid::parse(sid_text)?;

        let seq = object
            .get("seq")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| FormatError::Malformed {
                line: 0,
                reason: "缺少 seq 字段或不是非负整数".to_string(),
            })?;
        let t_abs_us = object
            .get("t_abs")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| FormatError::Malformed {
                line: 0,
                reason: "缺少 t_abs 字段或不是非负整数".to_string(),
            })?;
        let event_name = object
            .get("event")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| FormatError::Malformed {
                line: 0,
                reason: "缺少 event 字段".to_string(),
            })?;

        let mut record = TraceRecord::new(EventKind::parse(event_name), t_abs_us);
        for (key, raw) in object {
            if ENVELOPE_KEYS.contains(&key.as_str()) {
                continue;
            }
            let field = match raw {
                serde_json::Value::String(text) => FieldValue::Str(Cow::Owned(text.clone())),
                serde_json::Value::Bool(flag) => FieldValue::Bool(*flag),
                serde_json::Value::Null => FieldValue::Null,
                serde_json::Value::Number(number) => {
                    if let Some(unsigned) = number.as_u64() {
                        FieldValue::U64(unsigned)
                    } else if let Some(signed) = number.as_i64() {
                        FieldValue::I64(signed)
                    } else {
                        return Err(FormatError::Malformed {
                            line: 0,
                            reason: format!("字段 {key} 的数值超出 64 位整数范围"),
                        });
                    }
                }
                serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                    return Err(FormatError::Malformed {
                        line: 0,
                        reason: format!("字段 {key} 是嵌套值，trace 载荷只允许标量"),
                    });
                }
            };
            record.push(Cow::Owned(key.clone()), field);
        }

        Ok(TraceEvent::new(sid, seq, record))
    }
}

const HEX_DIGITS: [u8; 16] = *b"0123456789abcdef";

/// 按 RFC 8259 转义并加引号。**结构上没有失败路径**——
/// 既不 panic（诊断设施绝不能让命令失败），也不静默退化成空串
/// （静默丢掉一条路径，读 trace 的人会被误导，与 I5 同源）。
///
/// 这里手写而非调用 `serde_json::to_string`，是为了消掉那个「不可能发生但必须处理」
/// 的 `Err` 分支——任何对它的处理要么 panic 要么静默降级，两者都不可接受。
/// 转义表写错的风险由 `转义与_serde_json_逐字节等价` 这条 proptest 兜住。
fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            control if (control as u32) < 0x20 => {
                out.push_str("\\u");
                let code = control as u32;
                for shift in [12, 8, 4, 0] {
                    // `& 0xf` 保证下标恒在 0..16，索引不会越界。
                    out.push(HEX_DIGITS[((code >> shift) & 0xf) as usize] as char);
                }
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn push_json_value(out: &mut String, value: &FieldValue) {
    match value {
        FieldValue::Str(text) => push_json_string(out, text),
        FieldValue::U64(number) => out.push_str(&number.to_string()),
        FieldValue::I64(number) => out.push_str(&number.to_string()),
        FieldValue::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        FieldValue::Null => out.push_str("null"),
    }
}

// ---------------------------------------------------------------------------
// 读侧：坏行跳过并计数
// ---------------------------------------------------------------------------

/// 被跳过的一行及其理由。**必须报告给调用者，绝不静默**（I5）。
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedLine {
    /// 1 起的行号。
    pub line: usize,
    pub reason: String,
}

/// [`read_lines`] 的结果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TraceReadOutcome {
    pub events: Vec<TraceEvent>,
    pub skipped: Vec<SkippedLine>,
}

impl TraceReadOutcome {
    pub fn is_clean(&self) -> bool {
        self.skipped.is_empty()
    }
}

/// 读一整份 trace：**坏行跳过并计数，绝不因一行损坏丢掉其余线索**（FORMAT.md §10.5）。
///
/// 这与 journal / items 的「中间行损坏则失败」是刻意相反的纪律：
/// journal 是真相，读错一行等于伪造历史；trace 是事故现场的线索，
/// 为一行坏数据丢掉其余几千条线索是荒谬的。
///
/// 空行与仅含空白的行直接忽略，不计入 `skipped`——文件以 LF 结尾是格式要求（§1）。
pub fn read_lines(text: &str) -> TraceReadOutcome {
    let mut outcome = TraceReadOutcome::default();
    for (index, raw) in text.split('\n').enumerate() {
        // 解析时容忍 CRLF（§1：写入永不产生 CR，读取剥除）。
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            continue;
        }
        match TraceEvent::parse_line(line) {
            Ok(event) => outcome.events.push(event),
            Err(err) => outcome.skipped.push(SkippedLine {
                line: index + 1,
                reason: err.to_string(),
            }),
        }
    }
    outcome
}

// ---------------------------------------------------------------------------
// sink
// ---------------------------------------------------------------------------

/// trace 的出口。**arca-core 通过它产出诊断，自身不做任何 IO**（spec §11.3）。
pub trait TraceSink {
    fn record(&mut self, rec: TraceRecord);
}

impl<T: TraceSink + ?Sized> TraceSink for &mut T {
    fn record(&mut self, rec: TraceRecord) {
        (**self).record(rec);
    }
}

/// 零成本丢弃。未开启 trace 时的默认。
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl TraceSink for NullSink {
    fn record(&mut self, _rec: TraceRecord) {}
}

/// 全量留存，供确定性模拟测试**断言决策序列**（spec §11.2）。
///
/// 这是本模块给正确性基础设施的主要增益：现有属性测试只能断言「最终三态收敛」这个结果，
/// 有了它可以断言状态机的推理路径——proptest 缩小出反例时，看到的不再是「结果不对」，
/// 而是引擎在第几步选错了哪个动作。I3（无任何路径销毁数据）也可由此断言为
/// 「trace 中不出现任何销毁性动作」。
#[derive(Debug, Clone, Default)]
pub struct VecSink {
    records: Vec<TraceRecord>,
}

impl VecSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn records(&self) -> &[TraceRecord] {
        &self.records
    }

    /// 按出现顺序取出事件类型序列——断言决策路径时最常用的形态。
    pub fn kinds(&self) -> Vec<&EventKind> {
        self.records.iter().map(|rec| &rec.event).collect()
    }

    /// 只取某一类事件，用于「这次调和做了哪些决定」这类断言。
    pub fn of_kind<'a>(&'a self, kind: &EventKind) -> Vec<&'a TraceRecord> {
        self.records
            .iter()
            .filter(|rec| &rec.event == kind)
            .collect()
    }

    /// 配上 sid 变成可落盘的事件流；`seq` 即插入序号。
    pub fn into_events(self, sid: &Sid) -> Vec<TraceEvent> {
        self.records
            .into_iter()
            .enumerate()
            .map(|(index, rec)| TraceEvent::new(sid.clone(), index as u64, rec))
            .collect()
    }
}

impl TraceSink for VecSink {
    fn record(&mut self, rec: TraceRecord) {
        self.records.push(rec);
    }
}

/// 生产用：固定容量环形缓冲。成功即丢弃，失败才落盘（FORMAT.md §10.6）。
///
/// `seq` 全程单调递增，**不因挤出而回退**——于是「中间丢了事件」在落盘文件里可检测。
/// 被挤掉的条数由 [`RingSink::dropped`] 如实给出，落盘时必须写成 `trace.dropped`
/// 事件；沉默地截断线索，读的人会误以为「前面什么都没发生」，这与 I5 同源。
#[derive(Debug, Clone)]
pub struct RingSink {
    capacity: usize,
    buffer: VecDeque<(u64, TraceRecord)>,
    next_seq: u64,
    dropped: u64,
}

impl Default for RingSink {
    fn default() -> Self {
        RingSink::new(DEFAULT_RING_CAPACITY)
    }
}

impl RingSink {
    /// `capacity` 为 0 时按 1 处理——容量为 0 的环会静默吞掉一切，是个陷阱。
    pub fn new(capacity: usize) -> Self {
        RingSink {
            capacity: capacity.max(1),
            buffer: VecDeque::new(),
            next_seq: 0,
            dropped: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// 因容量上限被挤出的事件数。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 取出留存的事件流。丢弃发生过时，**在最前面插入一条 `trace.dropped`**
    /// 说明前面少了多少条（FORMAT.md §10.6）。
    ///
    /// 该条的 `t_abs` 取自留存的第一条记录，`seq` 取 `u64::MAX` 以示它是合成的、
    /// 不属于原始序列——原始 `seq` 的空洞本身就是丢弃的证据。
    pub fn drain(&mut self, sid: &Sid) -> Vec<TraceEvent> {
        let mut events = Vec::with_capacity(self.buffer.len() + 1);
        if self.dropped > 0 {
            let t_abs_us = self.buffer.front().map_or(0, |(_, rec)| rec.t_abs_us);
            events.push(TraceEvent::new(
                sid.clone(),
                u64::MAX,
                TraceRecord::new(EventKind::TraceDropped, t_abs_us).with("count", self.dropped),
            ));
        }
        for (seq, rec) in self.buffer.drain(..) {
            events.push(TraceEvent::new(sid.clone(), seq, rec));
        }
        self.dropped = 0;
        events
    }
}

impl TraceSink for RingSink {
    fn record(&mut self, rec: TraceRecord) {
        if self.buffer.len() == self.capacity {
            self.buffer.pop_front();
            self.dropped += 1;
        }
        self.buffer.push_back((self.next_seq, rec));
        self.next_seq += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &str = "20260805T093012Z";
    const HEX: &str = "0123456789abcdef";

    fn sid() -> Sid {
        Sid::new(TS, HEX).unwrap()
    }

    // --- sid -------------------------------------------------------------

    #[test]
    fn sid_的字典序即时间序() {
        let early = Sid::new("20260805T093012Z", HEX).unwrap();
        let late = Sid::new("20260805T093013Z", HEX).unwrap();
        assert!(early.as_str() < late.as_str());
    }

    #[test]
    fn sid_层次化后仍可解析且根段不变() {
        let parent = sid();
        let child = parent
            .child("20260805T093013Z", "fedcba9876543210")
            .unwrap();
        assert_eq!(child.depth(), 2);
        assert_eq!(child.root(), parent.as_str());
        assert_eq!(child.leaf(), "20260805T093013Z-fedcba9876543210");
        assert_eq!(Sid::parse(child.as_str()).unwrap(), child);
    }

    #[test]
    fn sid_拒绝错误形状() {
        assert!(Sid::new("2026-08-05T09:30:12Z", HEX).is_err()); // 非紧凑形式
        assert!(Sid::new(TS, "abc").is_err()); // 随机段太短
        assert!(Sid::new(TS, "0123456789ABCDEF").is_err()); // 大写不接受
        assert!(Sid::parse("").is_err());
        assert!(Sid::parse("not-a-sid").is_err());
        assert!(Sid::parse(&format!("{TS}-{HEX}/garbage")).is_err());
    }

    #[test]
    fn sid_段数超上限即拒绝() {
        let mut current = sid();
        for _ in 1..MAX_SID_SEGMENTS {
            current = current.child(TS, HEX).unwrap();
        }
        assert_eq!(current.depth(), MAX_SID_SEGMENTS);
        assert!(current.child(TS, HEX).is_err());

        let too_deep = vec![format!("{TS}-{HEX}"); MAX_SID_SEGMENTS + 1].join("/");
        assert!(Sid::parse(&too_deep).is_err());
    }

    /// 回归测试（I5）：与 `VersionId::new` 同源的陷阱——恰好 16 字节但由多字节
    /// UTF-8 字符组成的时间戳，若实现按字符边界切片 `str` 会 panic。只允许返回 `Err`。
    #[test]
    fn sid_对多字节输入不_panic() {
        let timestamp = "京都鸭ABCDEFG"; // 3×3 + 7 = 16 字节，第 8 字节落在字符内部
        assert_eq!(timestamp.len(), 16);
        assert!(Sid::new(timestamp, HEX).is_err());
        assert!(Sid::parse("京都鸭川书法兰亭序扫描件测试用例样本一二三").is_err());
    }

    // --- 事件类型 ---------------------------------------------------------

    #[test]
    fn 事件名往返一致() {
        let all = [
            EventKind::Start,
            EventKind::Exit,
            EventKind::RegionEnter,
            EventKind::RegionLeave,
            EventKind::Error,
            EventKind::Panic,
            EventKind::TraceDropped,
            EventKind::MountCheck,
            EventKind::LockAcquire,
            EventKind::LockWait,
            EventKind::LockRelease,
            EventKind::PathReject,
            EventKind::ScanSummary,
            EventKind::ReconcileDecide,
            EventKind::CommitAttempt,
            EventKind::CommitResult,
            EventKind::ConflictCopy,
            EventKind::TxnBegin,
            EventKind::TxnCommit,
            EventKind::TxnRollback,
            EventKind::TransferSummary,
        ];
        for kind in &all {
            assert_eq!(&EventKind::parse(kind.as_str()), kind);
        }
    }

    /// 向前兼容：新版本 arca 写出的事件名，旧版本必须能原样读出（FORMAT.md §10.5）。
    #[test]
    fn 未知事件名原样保留而不是报错() {
        let kind = EventKind::parse("future.thing");
        assert_eq!(kind, EventKind::Unknown("future.thing".to_string()));
        assert_eq!(kind.as_str(), "future.thing");
    }

    #[test]
    fn 错误类别往返一致且只有_retryable_可自动重试() {
        for class in [
            ErrorClass::Retryable,
            ErrorClass::NeedsHuman,
            ErrorClass::Protocol,
            ErrorClass::Bug,
        ] {
            assert_eq!(ErrorClass::parse(class.as_str()), Some(class));
        }
        assert!(ErrorClass::Retryable.is_retryable());
        assert!(!ErrorClass::NeedsHuman.is_retryable());
        assert!(!ErrorClass::Protocol.is_retryable());
        assert!(!ErrorClass::Bug.is_retryable());
        assert_eq!(ErrorClass::parse("whatever"), None);
    }

    // --- 序列化 -----------------------------------------------------------

    #[test]
    fn 信封在前载荷按键名字节序升序() {
        let record = TraceRecord::new(EventKind::ReconcileDecide, 48211)
            .with("remote", "modified")
            .with("path", "京都/鸭川.png")
            .with("action", "conflict")
            .with("local", "modified")
            .with("reason", "three_way_divergent");
        let line = TraceEvent::new(sid(), 17, record).to_json_line();
        assert_eq!(
            line,
            r#"{"v":1,"sid":"20260805T093012Z-0123456789abcdef","seq":17,"t_abs":48211,"event":"reconcile.decide","action":"conflict","local":"modified","path":"京都/鸭川.png","reason":"three_way_divergent","remote":"modified"}"#
        );
    }

    #[test]
    fn 各类标量的序列化形态() {
        let record = TraceRecord::new(EventKind::ScanSummary, 1)
            .with("bytes", 1884301776u64)
            .with("delta", -3i64)
            .with("ok", true)
            .with("holder", FieldValue::Null);
        let line = TraceEvent::new(sid(), 0, record).to_json_line();
        assert!(line.contains(r#""bytes":1884301776"#));
        assert!(line.contains(r#""delta":-3"#));
        assert!(line.contains(r#""ok":true"#));
        assert!(line.contains(r#""holder":null"#));
    }

    #[test]
    fn 字符串按_json_规则转义() {
        let record = TraceRecord::new(EventKind::PathReject, 0)
            .with("path", "a\"b\\c\td".to_string())
            .with("status", PathStatus::InvalidChar);
        let line = TraceEvent::new(sid(), 0, record).to_json_line();
        assert!(line.contains(r#""path":"a\"b\\c\td""#));
        // 转义正确的判据是能被通用 JSON 解析器读回来。
        let parsed = TraceEvent::parse_line(&line).unwrap();
        assert_eq!(
            parsed.record.field("path"),
            Some(&FieldValue::Str(Cow::Owned("a\"b\\c\td".to_string())))
        );
    }

    /// 载荷占用信封键名时以信封为准，绝不产生重复键——否则解析器行为未定义。
    #[test]
    fn 载荷不得覆盖信封字段() {
        let record = TraceRecord::new(EventKind::Exit, 5)
            .with("seq", 999u64)
            .with("event", "伪造")
            .with("code", 1u64);
        assert_eq!(record.envelope_conflicts(), vec!["event", "seq"]);

        let line = TraceEvent::new(sid(), 42, record).to_json_line();
        assert!(line.contains(r#""seq":42"#));
        assert!(!line.contains("999"));
        assert!(line.contains(r#""event":"exit""#));
        assert!(!line.contains("伪造"));
        assert!(line.contains(r#""code":1"#));
    }

    #[test]
    fn 同名字段以最后一次写入为准且只出现一次() {
        let record = TraceRecord::new(EventKind::CommitResult, 0)
            .with("outcome", "pending")
            .with("outcome", "ok");
        assert_eq!(
            record.field("outcome"),
            Some(&FieldValue::Str(Cow::Borrowed("ok")))
        );
        let line = TraceEvent::new(sid(), 0, record).to_json_line();
        assert_eq!(line.matches(r#""outcome""#).count(), 1);
        assert!(line.contains(r#""outcome":"ok""#));
    }

    /// 确定性序列化（FORMAT.md §9.3 的同一条纪律）：字段插入顺序不影响输出字节。
    #[test]
    fn 插入顺序不影响输出字节() {
        let forward = TraceRecord::new(EventKind::MountCheck, 7)
            .with("dataset_id", "3f2a")
            .with("expect", "3f2a")
            .with("ok", true);
        let backward = TraceRecord::new(EventKind::MountCheck, 7)
            .with("ok", true)
            .with("expect", "3f2a")
            .with("dataset_id", "3f2a");
        assert_eq!(
            TraceEvent::new(sid(), 3, forward).to_json_line(),
            TraceEvent::new(sid(), 3, backward).to_json_line()
        );
    }

    #[test]
    fn 解析与序列化逐字节往返() {
        let record = TraceRecord::new(EventKind::Error, 91442)
            .with("code", "mount.identity_mismatch")
            .with("class", ErrorClass::NeedsHuman)
            .with("retryable", false)
            .with("detail", "format.json 的 dataset_id 与绑定不符");
        let original = TraceEvent::new(sid(), 93, record);
        let line = original.to_json_line();
        let parsed = TraceEvent::parse_line(&line).unwrap();
        assert_eq!(parsed.sid, original.sid);
        assert_eq!(parsed.seq, original.seq);
        assert_eq!(parsed.record.event, original.record.event);
        assert_eq!(parsed.record.t_abs_us, original.record.t_abs_us);
        assert_eq!(parsed.to_json_line(), line);
    }

    // --- 解析的拒绝面 -----------------------------------------------------

    #[test]
    fn 高版本记录被拒绝而不是尽力解析() {
        let line = format!(r#"{{"v":2,"sid":"{TS}-{HEX}","seq":0,"t_abs":0,"event":"start"}}"#);
        assert!(matches!(
            TraceEvent::parse_line(&line),
            Err(FormatError::UnsupportedVersion { found: 2, max: 1 })
        ));
    }

    #[test]
    fn 嵌套载荷被拒绝() {
        let nested = format!(
            r#"{{"v":1,"sid":"{TS}-{HEX}","seq":0,"t_abs":0,"event":"start","argv":["arca","push"]}}"#
        );
        assert!(TraceEvent::parse_line(&nested).is_err());
        let object = format!(
            r#"{{"v":1,"sid":"{TS}-{HEX}","seq":0,"t_abs":0,"event":"start","actor":{{"account":"bruce"}}}}"#
        );
        assert!(TraceEvent::parse_line(&object).is_err());
    }

    #[test]
    fn 缺信封字段被拒绝() {
        for line in [
            format!(r#"{{"sid":"{TS}-{HEX}","seq":0,"t_abs":0,"event":"start"}}"#),
            r#"{"v":1,"seq":0,"t_abs":0,"event":"start"}"#.to_string(),
            format!(r#"{{"v":1,"sid":"{TS}-{HEX}","t_abs":0,"event":"start"}}"#),
            format!(r#"{{"v":1,"sid":"{TS}-{HEX}","seq":0,"event":"start"}}"#),
            format!(r#"{{"v":1,"sid":"{TS}-{HEX}","seq":0,"t_abs":0}}"#),
        ] {
            assert!(TraceEvent::parse_line(&line).is_err(), "应拒绝：{line}");
        }
        assert!(TraceEvent::parse_line("[1,2,3]").is_err());
        assert!(TraceEvent::parse_line("not json").is_err());
    }

    // --- 读侧容错 ---------------------------------------------------------

    /// 与 journal「中间行损坏则失败」相反：trace 是事故现场的线索，
    /// 绝不因一行坏数据丢掉其余行（FORMAT.md §10.5）。
    #[test]
    fn 坏行跳过并计数而好行全部保留() {
        let good_a =
            TraceEvent::new(sid(), 0, TraceRecord::new(EventKind::Start, 0)).to_json_line();
        let good_b = TraceEvent::new(
            sid(),
            1,
            TraceRecord::new(EventKind::Exit, 10).with("code", 1u64),
        )
        .to_json_line();
        let text = format!(
            "{good_a}\n垃圾行\n{{\"v\":9,\"sid\":\"{TS}-{HEX}\",\"seq\":2,\"t_abs\":0,\"event\":\"x\"}}\n{good_b}\n{{\"v\":1,\"sid\":\n"
        );

        let outcome = read_lines(&text);
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.events[0].record.event, EventKind::Start);
        assert_eq!(outcome.events[1].record.event, EventKind::Exit);
        assert!(!outcome.is_clean());
        // 垃圾行、高版本行、被截断的末行——三行都要如实报告，绝不静默。
        assert_eq!(
            outcome
                .skipped
                .iter()
                .map(|item| item.line)
                .collect::<Vec<_>>(),
            vec![2, 3, 5]
        );
    }

    #[test]
    fn 未知事件的整行仍被保留() {
        let line = format!(
            r#"{{"v":1,"sid":"{TS}-{HEX}","seq":0,"t_abs":0,"event":"future.thing","extra":"x"}}"#
        );
        let outcome = read_lines(&line);
        assert!(outcome.is_clean());
        assert_eq!(
            outcome.events[0].record.event,
            EventKind::Unknown("future.thing".to_string())
        );
        assert_eq!(
            outcome.events[0].record.field("extra"),
            Some(&FieldValue::Str(Cow::Owned("x".to_string())))
        );
    }

    #[test]
    fn 空行与_crlf_不计入跳过() {
        let line = TraceEvent::new(sid(), 0, TraceRecord::new(EventKind::Start, 0)).to_json_line();
        let outcome = read_lines(&format!("{line}\r\n\n   \n"));
        assert_eq!(outcome.events.len(), 1);
        assert!(outcome.is_clean());
    }

    #[test]
    fn 空输入读出空结果而不是错误() {
        assert_eq!(read_lines(""), TraceReadOutcome::default());
        assert_eq!(read_lines("\n\n"), TraceReadOutcome::default());
    }

    // --- sink -------------------------------------------------------------

    #[test]
    fn null_sink_丢弃一切() {
        let mut sink = NullSink;
        sink.record(TraceRecord::new(EventKind::Start, 0));
    }

    #[test]
    fn vec_sink_保序留存供断言决策序列() {
        let mut sink = VecSink::new();
        sink.record(TraceRecord::new(EventKind::Start, 0));
        sink.record(TraceRecord::new(EventKind::ReconcileDecide, 1).with("action", "pull"));
        sink.record(TraceRecord::new(EventKind::ReconcileDecide, 2).with("action", "push"));
        sink.record(TraceRecord::new(EventKind::Exit, 3));

        assert_eq!(
            sink.kinds(),
            vec![
                &EventKind::Start,
                &EventKind::ReconcileDecide,
                &EventKind::ReconcileDecide,
                &EventKind::Exit,
            ]
        );
        let decisions = sink.of_kind(&EventKind::ReconcileDecide);
        assert_eq!(decisions.len(), 2);
        assert_eq!(
            decisions[0].field("action"),
            Some(&FieldValue::Str(Cow::Borrowed("pull")))
        );

        let events = sink.into_events(&sid());
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn ring_sink_未满时不丢弃() {
        let mut sink = RingSink::new(4);
        for index in 0..4u64 {
            sink.record(TraceRecord::new(EventKind::ScanSummary, index));
        }
        assert_eq!(sink.len(), 4);
        assert_eq!(sink.dropped(), 0);
        let events = sink.drain(&sid());
        assert_eq!(events.len(), 4);
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
    }

    /// 丢弃必须留痕：`seq` 出现空洞 + 合成一条 `trace.dropped`。
    /// 沉默地截断线索，读的人会误以为「前面什么都没发生」（I5）。
    #[test]
    fn ring_sink_丢弃时留痕且_seq_不回退() {
        let mut sink = RingSink::new(3);
        for index in 0..10u64 {
            sink.record(TraceRecord::new(EventKind::ScanSummary, index));
        }
        assert_eq!(sink.len(), 3);
        assert_eq!(sink.dropped(), 7);

        let events = sink.drain(&sid());
        assert_eq!(events.len(), 4); // 1 条合成 + 3 条留存
        assert_eq!(events[0].record.event, EventKind::TraceDropped);
        assert_eq!(events[0].record.field("count"), Some(&FieldValue::U64(7)));
        assert_eq!(events[0].seq, u64::MAX);
        // 留存的原始 seq 是 7/8/9——空洞本身就是丢弃的证据。
        assert_eq!(
            events[1..].iter().map(|e| e.seq).collect::<Vec<_>>(),
            [7, 8, 9]
        );
        // drain 后计数归零，但 seq 继续单调。
        assert_eq!(sink.dropped(), 0);
        sink.record(TraceRecord::new(EventKind::Exit, 11));
        assert_eq!(sink.drain(&sid())[0].seq, 10);
    }

    /// 容量 0 的环会静默吞掉一切，是个陷阱——按 1 处理。
    #[test]
    fn ring_sink_容量为零时按一处理() {
        let mut sink = RingSink::new(0);
        assert_eq!(sink.capacity(), 1);
        sink.record(TraceRecord::new(EventKind::Start, 0));
        sink.record(TraceRecord::new(EventKind::Exit, 1));
        assert_eq!(sink.len(), 1);
        assert_eq!(sink.dropped(), 1);
    }

    #[test]
    fn ring_sink_默认容量为_4096() {
        let sink = RingSink::default();
        assert_eq!(sink.capacity(), DEFAULT_RING_CAPACITY);
        assert_eq!(sink.capacity(), 4096);
        assert!(sink.is_empty());
    }

    #[test]
    fn 可以按_dyn_传递_sink() {
        fn 发一条(sink: &mut dyn TraceSink) {
            sink.record(TraceRecord::new(EventKind::Start, 0));
        }
        let mut sink = VecSink::new();
        发一条(&mut sink);
        assert_eq!(sink.records().len(), 1);
    }

    // --- 属性测试 ---------------------------------------------------------

    mod 属性 {
        use super::*;
        use proptest::prelude::*;

        /// 任意 Unicode 标量序列，含控制字符——转义表最容易出错的地方。
        fn 任意字符串() -> impl Strategy<Value = String> {
            proptest::collection::vec(any::<char>(), 0..64)
                .prop_map(|chars| chars.into_iter().collect())
        }

        proptest! {
            /// 手写转义表的正确性判据：与 `serde_json` 逐字节等价。
            /// 手写是为了消掉一个「不可能发生但必须处理」的 `Err` 分支
            /// （处理它只能 panic 或静默降级，两者都不可接受），风险由本测试兜住。
            #[test]
            fn 转义与_serde_json_逐字节等价(text in 任意字符串()) {
                let mut mine = String::new();
                push_json_string(&mut mine, &text);
                let theirs = serde_json::to_string(&text).expect("&str 序列化不会失败");
                prop_assert_eq!(mine, theirs);
            }

            /// I5：任意输入都不得 panic，只能返回明确结果。
            #[test]
            fn 读侧对任意输入都不_panic(raw in ".*") {
                let outcome = read_lines(&raw);
                // skipped 计数自洽：非空行数 = 解析出的事件数 + 跳过数。
                let 非空行数 = raw
                    .split('\n')
                    .filter(|line| !line.trim().is_empty())
                    .count();
                prop_assert_eq!(非空行数, outcome.events.len() + outcome.skipped.len());
            }

            /// 任意标量载荷都能逐字节往返——确定性序列化（FORMAT.md §10.1）。
            #[test]
            fn 任意字符串载荷逐字节往返(text in 任意字符串()) {
                let record = TraceRecord::new(EventKind::PathReject, 7).with("path", text);
                let line = TraceEvent::new(sid(), 1, record).to_json_line();
                let parsed = TraceEvent::parse_line(&line).expect("自己写出的行必须能读回");
                prop_assert_eq!(parsed.to_json_line(), line);
            }
        }
    }
}
