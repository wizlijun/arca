#![no_main]
use libfuzzer_sys::fuzz_target;

// 相对路径规则：check/normalize/index_key 三者都必须对任意输入返回明确结果。
// I5：任意字节输入 → 明确错误，绝不 panic。
fuzz_target!(|data: &[u8]| {
    if let Ok(raw) = std::str::from_utf8(data) {
        let _ = arca_format::path_rules::check(raw);
        let _ = arca_format::path_rules::normalize(raw);
        let _ = arca_format::path_rules::index_key(raw);
    }
});
