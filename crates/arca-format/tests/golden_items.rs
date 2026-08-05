//! golden vectors 回归：items 版本链格式（FORMAT.md §7.1，spec §11.2）。
//!
//! 往返测试对改名不变：这份样例逐字节冻结了线上字段名与顺序，任何把
//! `Wire` 字段改名/换序的改动都会让本测试炸掉，而不只是「crate 内部的
//! `to_line`/`parse_line` 互相还认得」——那种自产自销的往返测试测不出破坏性
//! 改名（评审 Important #7）。

use arca_format::items::{parse_chain, parse_line, to_line};

#[test]
fn basic_样例逐行往返字节一致() {
    let text = include_str!("golden/items/basic.jsonl");
    for (zero_based, line) in text.lines().enumerate() {
        let version = parse_line(line, zero_based + 1).expect("样例应可解析");
        assert_eq!(
            to_line(&version).unwrap(),
            line,
            "第 {} 行往返后字节必须完全一致",
            zero_based + 1
        );
    }
}

#[test]
fn basic_样例是一条合法的两版本线性链() {
    let text = include_str!("golden/items/basic.jsonl");
    let chain = parse_chain(text).expect("样例应是一条合法版本链");
    assert_eq!(chain.len(), 2);
    assert!(chain[0].parent.is_none(), "首版 parent 必须为 null");
    assert_eq!(
        chain[1].parent.as_ref(),
        Some(&chain[0].version_id),
        "第二版必须指向第一版"
    );
}
