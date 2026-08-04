#![no_main]
use libfuzzer_sys::fuzz_target;

// <epoch>:<seq> 增量游标（FORMAT.md、PROTOCOL.md §3）：客户端提供的不可信输入。
// I5：任意字节输入 → 明确错误，绝不 panic。
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arca_format::journal::Cursor::parse(text);
    }
});
