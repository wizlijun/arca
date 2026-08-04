#![no_main]
use libfuzzer_sys::fuzz_target;

// chunks/<xx>/<hash>.zst：读任意磁盘字节（含损坏/截断/恶意构造的 zstd 帧）。
// 不要求输入是合法 UTF-8——块文件本就是任意二进制。
// I5：任意字节输入 → 明确错误，绝不 panic；也不得被 decompression bomb 拖垮内存。
fuzz_target!(|data: &[u8]| {
    let _ = arca_chunk::compress::decompress(data);
});
