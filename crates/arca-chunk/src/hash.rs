//! BLAKE3 内容哈希（原生地址，`blake3:…` 前缀）+ SHA-256 懒计算（互操作）。
//!
//! ETag = BLAKE3 内容哈希（PROTOCOL.md）；流式计算支持大文件与 Range 验证。
//!
//! TODO(M0)：哈希类型、流式计算接口、`blake3:` 文本表示的解析/格式化。

use std::fmt;

/// BLAKE3 内容哈希——arca 的原生内容地址（I2：blob 不可变）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; 32]);

/// 流式哈希计算器：大文件与 Range 校验用。
pub struct Hasher(blake3::Hasher);

#[derive(Debug, PartialEq, Eq)]
pub enum HashParseError {
    /// 缺少 `blake3:` 前缀
    MissingPrefix,
    /// 十六进制部分长度不是 64
    BadLength(usize),
    /// 含非小写十六进制字符
    BadDigit(char),
}

impl ContentHash {
    pub fn from_bytes(data: &[u8]) -> Self {
        ContentHash(*blake3::hash(data).as_bytes())
    }

    pub fn hasher() -> Hasher {
        Hasher(blake3::Hasher::new())
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
            out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
        }
        out
    }

    pub fn to_text(&self) -> String {
        format!("blake3:{}", self.to_hex())
    }

    pub fn parse(text: &str) -> Result<Self, HashParseError> {
        let hex = text.strip_prefix("blake3:").ok_or(HashParseError::MissingPrefix)?;
        if hex.len() != 64 {
            return Err(HashParseError::BadLength(hex.len()));
        }
        let mut bytes = [0u8; 32];
        let raw = hex.as_bytes();
        for (i, slot) in bytes.iter_mut().enumerate() {
            let hi = lower_hex_value(raw[i * 2] as char)?;
            let lo = lower_hex_value(raw[i * 2 + 1] as char)?;
            *slot = (hi << 4) | lo;
        }
        Ok(ContentHash(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn lower_hex_value(c: char) -> Result<u8, HashParseError> {
    match c {
        '0'..='9' => Ok(c as u8 - b'0'),
        'a'..='f' => Ok(c as u8 - b'a' + 10),
        _ => Err(HashParseError::BadDigit(c)),
    }
}

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> ContentHash {
        ContentHash(*self.0.finalize().as_bytes())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_text())
    }
}

impl fmt::Display for HashParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HashParseError::MissingPrefix => write!(f, "缺少 blake3: 前缀"),
            HashParseError::BadLength(n) => write!(f, "十六进制长度为 {n}，应为 64"),
            HashParseError::BadDigit(c) => write!(f, "非小写十六进制字符：{c:?}"),
        }
    }
}

impl std::error::Error for HashParseError {}

/// SHA-256 懒计算——仅为互操作（Git LFS oid、Dropbox 导入校验，spec §8）。
/// 不是 arca 的内容地址，绝不用于寻址。
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // BLAKE3 官方测试向量：空输入
    const EMPTY_HEX: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn 空输入的哈希匹配官方向量() {
        let h = ContentHash::from_bytes(b"");
        assert_eq!(h.to_hex(), EMPTY_HEX);
    }

    #[test]
    fn 流式与一次性计算结果相同() {
        let data = b"git manages text, arca manages binaries";
        let once = ContentHash::from_bytes(data);
        let mut hasher = ContentHash::hasher();
        hasher.update(&data[..10]);
        hasher.update(&data[10..]);
        assert_eq!(once, hasher.finish());
    }

    #[test]
    fn 文本表示往返一致() {
        let h = ContentHash::from_bytes(b"round trip");
        let text = h.to_text();
        assert!(text.starts_with("blake3:"));
        assert_eq!(ContentHash::parse(&text).unwrap(), h);
    }

    #[test]
    fn 拒绝错误前缀而不是_panic() {
        assert!(ContentHash::parse("sha256:00").is_err());
        assert!(ContentHash::parse("blake3:xyz").is_err());
        assert!(ContentHash::parse("blake3:").is_err());
        assert!(ContentHash::parse("").is_err());
        // 大写十六进制不接受：文本表示必须确定性（同内容必同字节）
        assert!(ContentHash::parse(&format!("blake3:{}", EMPTY_HEX.to_uppercase())).is_err());
    }

    #[test]
    fn sha256_空输入匹配官方向量() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_输出恒为64位小写十六进制() {
        let hex = sha256_hex(b"git manages text, arca manages binaries");
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }
}
