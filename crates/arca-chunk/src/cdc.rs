//! FastCDC 内容定义分块。
//!
//! 用途（spec §4.2 决策 1）：历史版本去重存储、修改文件的增量上传、
//! 跨文件去重（同一张照片从两台设备导入只存一份历史块）。
//!
//! TODO(M0)：分块参数（min/avg/max，出处记入 LIMITS 文档）、切块接口、块索引结构。

use crate::hash::ContentHash;

/// FastCDC 参数（FORMAT.md §8.1）。出处：FastCDC 论文（USENIX ATC'16）推荐区间；
/// avg 64 KiB 在去重率与块元数据开销之间取平衡。
pub const MIN_CHUNK: usize = 16 * 1024;
pub const AVG_CHUNK: usize = 64 * 1024;
pub const MAX_CHUNK: usize = 256 * 1024;

/// 一个内容定义块：`[offset, offset+len)` 是 `data` 中的字节区间，`hash` 是该区间内容的 BLAKE3。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: usize,
    pub len: usize,
    pub hash: ContentHash,
}

/// 按 FastCDC 切块。块首尾相接、覆盖全部字节，结果对同一输入确定。
///
/// 仅服务历史版本去重与增量传输（模块顶部 doc comment）；`files/` 的当前版本
/// **绝不**经过这条路径（I1）。
pub fn split(data: &[u8]) -> Vec<Chunk> {
    if data.is_empty() {
        return Vec::new();
    }
    fastcdc::v2020::FastCDC::new(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
        .map(|entry| Chunk {
            offset: entry.offset,
            len: entry.length,
            hash: ContentHash::from_bytes(&data[entry.offset..entry.offset + entry.length]),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性伪随机数据（splitmix64，固定种子）：同一长度总是产出同一字节序列。
    ///
    /// brief 参考实现原用 `((i * 7 + i / 251) % 256) as u8`（纯算术级数）生成测试数据，
    /// 实测会让 gear-hash CDC 退化成定长切分——`fastcdc` 自身文档在 `cut_gear` 里明确写着
    /// "pathological data, such as all zeroes" 会触发"找不到切点就退到 max_size"的兜底分支；
    /// 算术级数每步只 +7（mod 256）的平滑递增序列踩中了同一类退化（用真随机数据、相同调用
    /// 方式对照验证过：切块行为正常，5/6 块在插入后保持哈希不变）。换成本函数后，
    /// 500_000 字节数据切出 6 块、插入 64 字节后 5/6 块哈希不变，符合 CDC 应有的局部性；
    /// 这不是调低断言阈值，是换掉一份对 gear-hash CDC 病态的测试夹具。
    fn 确定性伪随机数据(len: usize) -> Vec<u8> {
        fn splitmix64(state: &mut u64) -> u64 {
            *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = *state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let word = splitmix64(&mut seed).to_le_bytes();
            let remaining = len - out.len();
            out.extend_from_slice(&word[..remaining.min(word.len())]);
        }
        out
    }

    #[test]
    fn 切块覆盖全部字节且不重叠() {
        let data = 确定性伪随机数据(1_000_000);
        let chunks = split(&data);
        assert!(!chunks.is_empty());
        let mut cursor = 0;
        for chunk in &chunks {
            assert_eq!(chunk.offset, cursor, "块必须首尾相接");
            cursor += chunk.len;
        }
        assert_eq!(cursor, data.len(), "块必须覆盖全部字节");
    }

    #[test]
    fn 块大小落在参数区间内() {
        let data = 确定性伪随机数据(1_000_000);
        let chunks = split(&data);
        for chunk in chunks.iter().take(chunks.len() - 1) {
            assert!(chunk.len >= MIN_CHUNK, "非末块不得小于 min");
            assert!(chunk.len <= MAX_CHUNK, "块不得大于 max");
        }
    }

    #[test]
    fn 切块是确定性的() {
        let data = 确定性伪随机数据(500_000);
        assert_eq!(split(&data), split(&data));
    }

    #[test]
    fn 中间插入只影响局部块() {
        // CDC 的核心价值：插入不应导致后续所有块边界移位
        let base = 确定性伪随机数据(500_000);
        let mut modified = base.clone();
        modified.splice(250_000..250_000, [0xffu8; 64]);

        let a = split(&base);
        let b = split(&modified);
        let 共享 = a
            .iter()
            .filter(|c| b.iter().any(|d| d.hash == c.hash))
            .count();
        assert!(
            共享 * 2 > a.len(),
            "多数块应保持不变，实得 {共享}/{}",
            a.len()
        );
    }

    #[test]
    fn 空输入产生零个块() {
        assert!(split(b"").is_empty());
    }
}
