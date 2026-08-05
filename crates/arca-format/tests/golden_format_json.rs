//! golden vectors 回归：`format.json` 卷身份标记格式（FORMAT.md §5，spec §11.2）。

use arca_format::hub_layout::FormatJson;

#[test]
fn basic_样例往返字节一致() {
    let text = include_str!("golden/format-json/basic.json");
    let line = text.trim_end_matches('\n');
    let parsed = FormatJson::parse(line).expect("样例应可解析");
    assert_eq!(parsed.to_json().unwrap(), line, "往返后字节必须完全一致");
}
