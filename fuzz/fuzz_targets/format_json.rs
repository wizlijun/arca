#![no_main]
use libfuzzer_sys::fuzz_target;

// .arca/format.json：hub 存储根的卷身份标记（FORMAT.md §5，I11）。
// I5：任意字节输入 → 明确错误，绝不 panic。
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arca_format::hub_layout::FormatJson::parse(text);
    }
});
