//! zstd 压缩（RFC 8878）——chunks 落盘形态。
//!
//! TODO(M0)：压缩级别选型（弱 NAS 友好，§1.1 目标 9）、流式接口。

/// zstd 压缩级别。3 是 zstd 默认值——压缩比与 ARM NAS 的 CPU 成本平衡
/// （spec §1.1 目标 9：弱硬件友好）。
pub const LEVEL: i32 = 3;

/// 压缩/解压失败。不携带原始字节——调用方已经有输入的所有权，无需我们复制一份。
#[derive(Debug)]
pub struct CompressError(String);

impl std::fmt::Display for CompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "zstd 处理失败：{}", self.0)
    }
}

impl std::error::Error for CompressError {}

/// 压缩为 zstd 帧。**失败时返回 `Err`，绝不静默回退成未压缩字节**——
/// 见模块级判断记录：块以 `.zst` 结尾落盘，若这里悄悄塞入非 zstd 字节，
/// 会把一次可诊断的写入失败伪装成日后读取时的"块损坏"，违反 I5。
/// `Err` 分支目前无法用合法输入构造出来（内存缓冲区编码在 `zstd` crate 内部
/// 已知条件下不会失败），保留 `Result` 是为了不重蹈 brief 参考实现的覆辙。
pub fn compress(data: &[u8]) -> Result<Vec<u8>, CompressError> {
    zstd::encode_all(data, LEVEL).map_err(|e| CompressError(e.to_string()))
}

/// 解压 zstd 帧。对任意字节输入（含空、截断、随机噪声）返回 `Result`，绝不 panic（I5）。
pub fn decompress(packed: &[u8]) -> Result<Vec<u8>, CompressError> {
    zstd::decode_all(packed).map_err(|e| CompressError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 压缩解压往返一致() {
        let data: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
        let packed = compress(&data).unwrap();
        assert!(packed.len() < data.len(), "可压缩数据应变小");
        assert_eq!(decompress(&packed).unwrap(), data);
    }

    #[test]
    fn 空输入往返一致() {
        assert_eq!(decompress(&compress(b"").unwrap()).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn 损坏输入返回错误而不是_panic() {
        assert!(decompress(b"not zstd at all").is_err());
        assert!(decompress(&[]).is_err());
    }

    #[test]
    fn 截断的合法帧返回错误而不是_panic() {
        let data: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
        let packed = compress(&data).unwrap();
        // 只截取前半段：帧头合法但帧体不完整，模拟写入过程中被中断的块文件。
        assert!(decompress(&packed[..packed.len() / 2]).is_err());
    }

    #[test]
    fn 随机噪声返回错误而不是_panic() {
        // 约束要求 decompress 对"空、截断、随机噪声"三类输入都不得 panic（I5）；
        // 上面两条测试覆盖了前两类，这条覆盖第三类：确定性伪随机字节（不依赖外部 crate）。
        let mut state = 0x2545F4914F6CDD1Du64;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xff) as u8
            })
            .collect();
        assert!(decompress(&noise).is_err());
    }
}
