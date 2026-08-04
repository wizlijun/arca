//! zstd 压缩（RFC 8878）——chunks 落盘形态。
//!
//! TODO(M0)：压缩级别选型（弱 NAS 友好，§1.1 目标 9）、流式接口。

use std::io::Read;

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

/// 解压后数据不得超过的上限：块按定义（FORMAT.md §8.1）不会超过 `cdc::MAX_CHUNK`。
/// 用于拒绝 decompression bomb（见 `decompress` 文档）。
const MAX_DECOMPRESSED_SIZE: u64 = crate::cdc::MAX_CHUNK as u64;

/// 解压 zstd 帧。对任意字节输入（含空、截断、随机噪声）返回 `Result`，绝不 panic（I5）。
///
/// **decompression bomb 防护**：`zstd::decode_all` 会把解压结果写进一个无上限的
/// `Vec<u8>`——一个损坏或恶意构造的帧，帧头可以声明任意大的解压尺寸，直接调用
/// `decode_all` 会一路把内存吃到 OOM。这不是 panic，但进程被 OOM kill 同样不是
/// I5 要的"可诊断的失败"，而且这条路径不是理论风险：fsck 要对磁盘上任意块文件
/// （包括已损坏的）调用本函数。防护分两层：
///
/// 1. 解压前先用 `zstd_safe::get_frame_content_size` 读帧头声明的内容尺寸；
///    声明值一旦超过 `MAX_CHUNK`，直接拒绝，不进入真正的解压（可诊断、零内存代价）。
/// 2. 帧头未声明尺寸是合法情况（`zstd::encode_all` 走的流式编码默认就不声明），
///    这时退回到有上限的流式解压：用 `Read::take(MAX_CHUNK + 1)` 卡住读取总量，
///    读满上限＋1 字节就说明超限——无论帧头是否可信，内存占用都不会超过这个上限。
pub fn decompress(packed: &[u8]) -> Result<Vec<u8>, CompressError> {
    if let Ok(Some(declared)) = zstd::zstd_safe::get_frame_content_size(packed) {
        if declared > MAX_DECOMPRESSED_SIZE {
            return Err(CompressError(format!(
                "帧声明的解压尺寸 {declared} 字节超过块上限 {MAX_DECOMPRESSED_SIZE} 字节（decompression bomb 防护）"
            )));
        }
    }

    let limit = MAX_DECOMPRESSED_SIZE + 1;
    let mut decoder = zstd::Decoder::new(packed).map_err(|e| CompressError(e.to_string()))?;
    let mut out = Vec::new();
    decoder
        .by_ref()
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|e| CompressError(e.to_string()))?;
    if out.len() as u64 >= limit {
        return Err(CompressError(format!(
            "解压后的数据超过块上限 {MAX_DECOMPRESSED_SIZE} 字节（decompression bomb 防护）"
        )));
    }
    Ok(out)
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
        assert_eq!(
            decompress(&compress(b"").unwrap()).unwrap(),
            Vec::<u8>::new()
        );
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

    #[test]
    fn 超过块上限的帧被拒绝而不是吃满内存() {
        // decompression bomb 防护：块按定义不超过 MAX_CHUNK（256 KiB），构造一个解压后
        // 超过这个上限的帧（全零字节，压缩后体积很小，验证过 zstd::encode_all 默认不在
        // 帧头声明内容尺寸——防护真正生效的是有上限的流式解压，不是帧头快速路径）。
        let oversized = vec![0u8; crate::cdc::MAX_CHUNK + 1];
        let packed = compress(&oversized).unwrap();
        assert!(decompress(&packed).is_err(), "超过块上限的帧必须被拒绝");
    }

    #[test]
    fn 恰好等于块上限的数据仍能正常解压往返() {
        // 上一条测试的边界回归：MAX_CHUNK+1 被拒绝不代表 MAX_CHUNK 本身被误拒。
        let data: Vec<u8> = (0..crate::cdc::MAX_CHUNK as u32)
            .map(|i| (i % 251) as u8)
            .collect();
        let packed = compress(&data).unwrap();
        assert_eq!(decompress(&packed).unwrap(), data);
    }
}
