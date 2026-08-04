#![no_main]
use libfuzzer_sys::fuzz_target;

// <dataset>/.arca/dataset.toml：数据集自描述（spec §4.3）。
// I5：任意字节输入 → 明确错误，绝不 panic。
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arca_format::dataset::DatasetConfig::parse(text);
    }
});
