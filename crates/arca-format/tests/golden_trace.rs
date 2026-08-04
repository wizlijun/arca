//! golden vectors 回归：trace 事件格式（FORMAT.md §10，spec §11.2）。
//!
//! 两个样例锁住的是两条相反的纪律：
//! - `basic.jsonl` —— 合法样例，逐字节往返（确定性序列化）；
//! - `damaged.jsonl` —— 损坏样例，坏行跳过并计数、好行全部保留、未知事件原样透传。

use arca_format::trace::{read_lines, EventKind, TraceEvent};

#[test]
fn basic_样例逐行往返字节一致() {
    let text = include_str!("golden/trace/basic.jsonl");
    let outcome = read_lines(text);
    assert!(
        outcome.is_clean(),
        "合法样例不应有跳过行：{:?}",
        outcome.skipped
    );
    assert_eq!(outcome.events.len(), 11);

    for (index, line) in text.lines().enumerate() {
        let event = TraceEvent::parse_line(line).expect("样例应可解析");
        assert_eq!(
            event.to_json_line(),
            line,
            "第 {} 行往返后字节必须完全一致",
            index + 1
        );
    }
}

#[test]
fn basic_样例的信封序列连续无空洞() {
    let outcome = read_lines(include_str!("golden/trace/basic.jsonl"));
    let seqs: Vec<u64> = outcome.events.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, (0..11).collect::<Vec<u64>>());
    // 同一会话的所有事件共享 sid。
    let sid = outcome.events[0].sid.clone();
    assert!(outcome.events.iter().all(|event| event.sid == sid));
}

/// 与 journal 的「中间行损坏则失败」相反：trace 绝不因一行坏数据丢掉其余线索。
#[test]
fn damaged_样例只丢坏行不丢好行() {
    let outcome = read_lines(include_str!("golden/trace/damaged.jsonl"));

    // 好行：start、future.thing（未知但合法）、exit。
    assert_eq!(outcome.events.len(), 3);
    assert_eq!(outcome.events[0].record.event, EventKind::Start);
    assert_eq!(
        outcome.events[1].record.event,
        EventKind::Unknown("future.thing".to_string()),
        "未知事件名必须原样透传（向前兼容）"
    );
    assert_eq!(outcome.events[2].record.event, EventKind::Exit);

    // 坏行：非 JSON、高版本、嵌套载荷、非法 sid、被截断的末行。
    let skipped: Vec<usize> = outcome.skipped.iter().map(|item| item.line).collect();
    assert_eq!(skipped, vec![2, 4, 5, 6, 8]);
    assert!(!outcome.is_clean());
}
