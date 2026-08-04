//! golden vectors 回归：格式变更必须通过全部既有样例（spec §11.2）。

use arca_format::manifest::Manifest;

#[test]
fn basic_样例往返字节一致() {
    let text = include_str!("golden/manifest/basic.manifest");
    let manifest = Manifest::parse(text).expect("样例应可解析");
    assert_eq!(manifest.to_string(), text, "往返后字节必须完全一致");
}
