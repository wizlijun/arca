#![no_main]
use libfuzzer_sys::fuzz_target;

// index/<xx>/<hash>.json：路径 → 身份映射（FORMAT.md §6）。
// I5：任意字节输入 → 明确错误，绝不 panic。
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arca_format::index::IndexRecord::parse(text);
    }
});
