//! golden vectors 回归：index 记录格式（FORMAT.md §6，spec §11.2）。

use arca_format::index::IndexRecord;

#[test]
fn basic_样例往返字节一致() {
    let text = include_str!("golden/index/basic.json");
    let line = text.trim_end_matches('\n');
    let record = IndexRecord::parse(line).expect("样例应可解析");
    assert_eq!(record.to_json().unwrap(), line, "往返后字节必须完全一致");
}
