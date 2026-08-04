//! `journal/<epoch>.jsonl`：append-only 事件流（FORMAT.md §7.2）+ 游标 `Cursor`。
//!
//! 每条事件描述一次身份/映射变更：`upsert`（新版本落地）、`tombstone`（删除）、
//! `rename`（身份不动、路径映射搬家）；三者都不直接携带内容，`version_id` 指向
//! items 版本链（§7.1）里已存在的一条记录。损坏处置纪律与 items 版本链相同——
//! 末行不完整截断，中间行损坏必须失败（§7.2、继承 lazync STORAGE.md）。
//!
//! `Cursor`（`<epoch>:<seq>`）是客户端增量拉取的进度标记，`seq` 在一个 epoch 内
//! 单调递增、无空洞；游标早于保留区间由调用方触发 `reset_required` 全量对账兜底
//! （M0 不实现该兜底逻辑，只提供游标的解析/格式化原语）。

use crate::error::FormatError;
use crate::model::{Actor, ItemId, VersionId};
use serde::{Deserialize, Serialize};
use std::fmt;

const RECORD_VERSION: u32 = 1;

/// 事件操作码。`#[serde(rename_all = "lowercase")]` 使线上文本恰为
/// `"upsert"` / `"tombstone"` / `"rename"`（FORMAT.md §7.2）；未知取值走 serde
/// 的正常失败路径而不是被吞掉，天然满足「拒绝未知操作码而不是忽略」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Upsert,
    Tombstone,
    Rename,
}

/// journal 事件流的一条记录（FORMAT.md §7.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEvent {
    pub seq: u64,
    pub op: Op,
    pub item_id: ItemId,
    /// `upsert`：新写入版本的 id；`tombstone`/`rename`：改动前最后一个存活版本的 id
    /// （tombstone/rename 都不产生新版本，沿用旧 id）。
    pub version_id: Option<VersionId>,
    pub path: String,
    /// 仅 `rename` 必填（改名前的路径）；`upsert`/`tombstone` 不出现，线上表示为
    /// 该字段整体缺失（而非序列化为 `null`，见 `to_line`）。
    pub from: Option<String>,
    pub actor: Actor,
    pub at: String,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u32,
    seq: u64,
    op: Op,
    item_id: String,
    version_id: Option<String>,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    actor: Actor,
    at: String,
}

impl JournalEvent {
    /// 序列化为单行 JSON。判断点同 `items::to_line`：返回 `Result` 而非
    /// `unwrap_or_default()`，避免序列化失败被静默写成空行追加进 append-only 事件流
    /// （Task 6 先例）。`Wire` 全是标量/字符串字段，`Err` 分支当前不可达，保留
    /// `Result` 签名为未来加字段留防线。
    pub fn to_line(&self) -> Result<String, FormatError> {
        let wire = Wire {
            v: RECORD_VERSION,
            seq: self.seq,
            op: self.op,
            item_id: self.item_id.to_hex(),
            version_id: self.version_id.as_ref().map(|v| v.as_str().to_string()),
            path: self.path.clone(),
            from: self.from.clone(),
            actor: self.actor.clone(),
            at: self.at.clone(),
        };
        serde_json::to_string(&wire)
            .map_err(|e| FormatError::Malformed { line: 0, reason: format!("journal 事件序列化失败：{e}") })
    }

    /// 解析单行事件。除结构/取值合法性外，还校验 FORMAT.md §7.2 字段表规定的
    /// `op` 与 `from` 的搭配关系：`rename` 必须携带 `from`，`upsert`/`tombstone`
    /// 不得携带——这是表里明写的结构性约束，携带矛盾字段是歧义状态，必须拒绝
    /// 而非放行（I5：绝不猜测该信任哪一个）。
    pub fn parse_line(line: &str, line_no: usize) -> Result<Self, FormatError> {
        let wire: Wire = serde_json::from_str(line).map_err(|e| FormatError::Malformed {
            line: line_no,
            reason: format!("JSON 解析失败：{e}"),
        })?;
        if wire.v > RECORD_VERSION {
            return Err(FormatError::UnsupportedVersion { found: wire.v, max: RECORD_VERSION });
        }
        let bad = |reason: String| FormatError::Malformed { line: line_no, reason };

        let item_id = ItemId::parse(&wire.item_id).map_err(|_| bad(format!("item_id {:?} 不合法", wire.item_id)))?;
        let version_id = match wire.version_id {
            Some(ref v) => Some(parse_version_id(v).map_err(|_| bad(format!("version_id {v:?} 不合法")))?),
            None => None,
        };
        match wire.op {
            Op::Rename if wire.from.is_none() => {
                return Err(bad("op=rename 必须携带 from（改名前路径）".to_string()))
            }
            Op::Upsert | Op::Tombstone if wire.from.is_some() => {
                return Err(bad(format!("op={:?} 不应携带 from 字段", wire.op)))
            }
            _ => {}
        }

        Ok(JournalEvent {
            seq: wire.seq,
            op: wire.op,
            item_id,
            version_id,
            path: wire.path,
            from: wire.from,
            actor: wire.actor,
            at: wire.at,
        })
    }
}

fn parse_version_id(text: &str) -> Result<VersionId, FormatError> {
    let (timestamp, random) = text.split_once('-').ok_or(FormatError::Malformed {
        line: 0,
        reason: format!("version_id {text:?} 缺少分隔符"),
    })?;
    VersionId::new(timestamp, random)
}

/// 解析整段事件流。处置纪律与 `items::parse_chain` 相同（FORMAT.md §7.2 明文要求
/// 与 §7.1 一致）：末行不完整截断到最后一个完整行边界；边界内任何一行解析失败
/// 都返回 `Err`，绝不跳过。
pub fn parse_stream(text: &str) -> Result<Vec<JournalEvent>, FormatError> {
    let complete_upto = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let complete = &text[..complete_upto];

    let mut events = Vec::new();
    for (zero_based, raw) in complete.lines().enumerate() {
        let line_no = zero_based + 1;
        let line = raw.trim_end_matches('\r');
        events.push(JournalEvent::parse_line(line, line_no)?);
    }
    Ok(events)
}

/// 增量游标：`<epoch>:<seq>`（FORMAT.md §7.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

impl Cursor {
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let (epoch, seq) = text.split_once(':').ok_or_else(|| FormatError::Malformed {
            line: 0,
            reason: format!("游标 {text:?} 缺少 ':' 分隔符"),
        })?;
        if epoch.is_empty() {
            return Err(FormatError::Malformed { line: 0, reason: "游标 epoch 部分为空".to_string() });
        }
        let seq: u64 = seq.parse().map_err(|_| FormatError::Malformed {
            line: 0,
            reason: format!("游标 seq 部分 {seq:?} 不是无符号整数"),
        })?;
        Ok(Cursor { epoch: epoch.to_string(), seq })
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.epoch, self.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 游标往返一致() {
        let cursor = Cursor { epoch: "abc123".into(), seq: 42 };
        assert_eq!(cursor.to_string(), "abc123:42");
        assert_eq!(Cursor::parse("abc123:42").unwrap(), cursor);
    }

    #[test]
    fn 拒绝畸形游标() {
        assert!(Cursor::parse("no-colon").is_err());
        assert!(Cursor::parse("abc:notanumber").is_err());
        assert!(Cursor::parse("").is_err());
    }

    #[test]
    fn 事件往返一致() {
        let event = JournalEvent {
            seq: 42,
            op: Op::Upsert,
            item_id: crate::model::ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            version_id: Some(crate::model::VersionId::new("20260804T102302Z", &"0".repeat(32)).unwrap()),
            path: "京都/鸭川.png".into(),
            from: None,
            actor: crate::model::Actor::default(),
            at: "2026-08-04T10:23:05Z".into(),
        };
        let line = event.to_line().unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(JournalEvent::parse_line(&line, 1).unwrap(), event);
    }

    #[test]
    fn rename_事件携带来源路径() {
        let line = r#"{"v":1,"seq":1,"op":"rename","item_id":"3f2a000000000000000000000000beef","version_id":null,"path":"新.png","from":"旧.png","actor":{"account":"","device":"","session":""},"at":"2026-08-04T10:00:00Z"}"#;
        let event = JournalEvent::parse_line(line, 1).unwrap();
        assert_eq!(event.op, Op::Rename);
        assert_eq!(event.from.as_deref(), Some("旧.png"));
    }

    #[test]
    fn 拒绝未知操作码而不是忽略() {
        let line = r#"{"v":1,"seq":1,"op":"魔法","item_id":"3f2a000000000000000000000000beef","version_id":null,"path":"a.png","from":null,"actor":{"account":"","device":"","session":""},"at":"t"}"#;
        assert!(JournalEvent::parse_line(line, 1).is_err());
    }

    #[test]
    fn upsert_事件序列化时不带_from_字段() {
        // FORMAT.md §7.2 字段表：upsert/tombstone 的 from「不出现」，只有 rename 携带。
        let event = JournalEvent {
            seq: 1,
            op: Op::Upsert,
            item_id: crate::model::ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            version_id: None,
            path: "a.png".into(),
            from: None,
            actor: crate::model::Actor::default(),
            at: "t".into(),
        };
        let line = event.to_line().unwrap();
        assert!(!line.contains("\"from\""), "upsert 不应携带 from 字段：{line}");
    }

    #[test]
    fn 拒绝非_rename_却携带_from的事件() {
        let line = r#"{"v":1,"seq":1,"op":"upsert","item_id":"3f2a000000000000000000000000beef","version_id":null,"path":"a.png","from":"b.png","actor":{"account":"","device":"","session":""},"at":"t"}"#;
        assert!(JournalEvent::parse_line(line, 1).is_err(), "upsert 携带 from 是结构性矛盾，必须拒绝而非忽略");
    }

    #[test]
    fn 拒绝_rename_却缺失_from的事件() {
        let line = r#"{"v":1,"seq":1,"op":"rename","item_id":"3f2a000000000000000000000000beef","version_id":null,"path":"a.png","actor":{"account":"","device":"","session":""},"at":"t"}"#;
        assert!(JournalEvent::parse_line(line, 1).is_err(), "rename 必须携带 from（改名前路径）");
    }

    #[test]
    fn 末行不完整时截断而非报错() {
        let event = JournalEvent {
            seq: 1,
            op: Op::Upsert,
            item_id: crate::model::ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            version_id: None,
            path: "a.png".into(),
            from: None,
            actor: crate::model::Actor::default(),
            at: "t".into(),
        };
        let text = format!("{}\n{{\"v\":1,\"seq\":2,\"op\":\"up", event.to_line().unwrap());
        let events = parse_stream(&text).expect("末行不完整应截断到最后完整行");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn 中间行损坏则失败() {
        let event = JournalEvent {
            seq: 1,
            op: Op::Upsert,
            item_id: crate::model::ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            version_id: None,
            path: "a.png".into(),
            from: None,
            actor: crate::model::Actor::default(),
            at: "t".into(),
        };
        let line = event.to_line().unwrap();
        let text = format!("{line}\n损坏的行\n{line}\n");
        assert!(parse_stream(&text).is_err(), "中间行损坏必须失败，不得跳过");
    }
}
