#![no_main]
use libfuzzer_sys::fuzz_target;

// journal/<epoch>.jsonl：append-only 事件流（FORMAT.md §7.2）。
// 处置纪律与 items 相同：中间行损坏必须失败，绝不跳过；I5：绝不 panic。
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arca_format::journal::parse_stream(text);
    }
});
