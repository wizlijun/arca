//! golden vectors 回归：journal 事件流格式（FORMAT.md §7.2，spec §11.2）。
//!
//! 与 `golden_items.rs` 同一判断依据：样例文本本身、而不是"crate 自己生成再
//! 自己解析"，才能锁住线上字段名/顺序，防止破坏性改名被内部往返测试放过
//! （评审 Important #7）。样例的两条事件（`upsert` → `tombstone`，`seq` 42→43）
//! 也顺带覆盖了评审 Important #6 新加的 seq 连续性校验。

use arca_format::journal::{parse_stream, JournalEvent};

#[test]
fn basic_样例逐行往返字节一致() {
    let text = include_str!("golden/journal/basic.jsonl");
    for (zero_based, line) in text.lines().enumerate() {
        let event = JournalEvent::parse_line(line, zero_based + 1).expect("样例应可解析");
        assert_eq!(
            event.to_line().unwrap(),
            line,
            "第 {} 行往返后字节必须完全一致",
            zero_based + 1
        );
    }
}

#[test]
fn basic_样例的seq连续可整体解析() {
    let text = include_str!("golden/journal/basic.jsonl");
    let events = parse_stream(text).expect("样例的 seq 应连续，可整体解析");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, 42);
    assert_eq!(events[1].seq, 43);
}
