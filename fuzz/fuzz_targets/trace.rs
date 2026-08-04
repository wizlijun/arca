#![no_main]
use libfuzzer_sys::fuzz_target;

// trace 事件流（FORMAT.md §10）：读侧纪律与其他解析器**相反**——
// 坏行跳过并计数、绝不失败（trace 设计 §10 / docs/superpowers/specs/2026-08-05-trace-design.md）。
// 因此这里的断言不是「返回 Err」，而是「不 panic 且 skipped 计数自洽」：
// 非空行数 == 解析出的事件数 + 跳过数（与 trace.rs 内的同名 proptest 同构）。
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let outcome = arca_format::trace::read_lines(text);
        let non_empty_lines = text
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .count();
        assert_eq!(
            non_empty_lines,
            outcome.events.len() + outcome.skipped.len(),
            "skipped 计数与事件数之和应等于非空行数"
        );
    }
});
