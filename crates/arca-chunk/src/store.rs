//! 内容寻址块存储的**路径计算**——纯函数，不做 IO（core 可嵌入纪律）。

use crate::hash::ContentHash;

/// 返回块相对于 `.arca/` 的路径：`chunks/<前两位十六进制>/<64 位十六进制>.zst`。
/// 两级分片避免单目录条目数过大（FORMAT.md §4、§8）。
pub fn chunk_relative_path(hash: &ContentHash) -> String {
    let hex = hash.to_hex();
    format!("chunks/{}/{}.zst", &hex[..2], hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 块路径按前两位分片() {
        let hash = ContentHash::from_bytes(b"x");
        let path = chunk_relative_path(&hash);
        let hex = hash.to_hex();
        assert_eq!(path, format!("chunks/{}/{}.zst", &hex[..2], hex));
    }

    #[test]
    fn 块路径以_zst_结尾且是确定性的() {
        let hash = ContentHash::from_bytes(b"deterministic");
        assert_eq!(chunk_relative_path(&hash), chunk_relative_path(&hash));
        assert!(chunk_relative_path(&hash).ends_with(".zst"));
    }

    #[test]
    fn 不同内容产生不同路径() {
        let a = chunk_relative_path(&ContentHash::from_bytes(b"a"));
        let b = chunk_relative_path(&ContentHash::from_bytes(b"b"));
        assert_ne!(a, b);
    }
}
