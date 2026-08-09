//! `https://` 的证书信任模型（M2e Task 4，spec §9、FORMAT.md §9.1）。
//!
//! spec §9 一句话：**系统根静默通过；自签名走指纹人工确认 + pin，指纹变更
//! 即拒连。** 这个模块只做这一件事，不做别的。
//!
//! # 三种状态，没有第四种
//!
//! | `.gitarca` 的 `tls_pin` | 服务端证书 | 结果 |
//! | --- | --- | --- |
//! | 缺失 | 公共 CA 签发 | **静默通过**（走 WebPki 根，与任何 HTTPS 客户端一致） |
//! | 缺失 | 自签名 | **拒连**，错误信息告知怎么 pin（见 [`PinError::NotPinned`]） |
//! | 有 | 指纹相符 | 通过——**只信这一张证书**，公共根一概不参与判断 |
//! | 有 | 指纹不符 | **拒连**（[`PinError::Mismatch`]），要用户重新人工确认 |
//!
//! **绝不 TOFU**（首次使用即信任）。TOFU 把"一次静默的中间人机会"做成了
//! 默认行为：第一次连接恰好被劫持，此后所有连接都会"忠实地"信任攻击者的
//! 证书，且再也不会报警。所以 `arca hub trust` 不带 `--fingerprint` 时只
//! **打印**指纹并拒绝写入，要求用户用带外渠道（在 hub 那台机器上跑
//! `openssl x509 -fingerprint -sha256`）核对之后显式抄进来。
//!
//! # 怎么在不放弃校验的前提下实现"只信这一张证书"
//!
//! `ureq` 3 没有暴露"自定义证书校验器"的钩子（它只给
//! `RootCerts::{WebPki, PlatformVerifier, Specific}` 三选一，外加一个
//! `disable_verification`——后者是完全关掉校验，绝不可接受）。所以做法是：
//!
//! 1. [`probe_leaf_cert`]：用 `rustls` 自己做一次**只为取证书**的握手，
//!    捕获服务端叶子证书的 DER。这一步刻意不校验证书（`ProbeVerifier`
//!    接受任何证书）——它的产物**不被信任**，只被"拿去和 pin 比对"。
//! 2. 比对 SHA-256 指纹。不符 → 拒连，握手到此为止，**没有任何应用层字节
//!    被发送**（`probe` 只跑到握手完成就关掉连接）。
//! 3. 相符 → 把这张证书作为 `RootCerts::Specific` 交给 ureq，真正的请求
//!    走**完整的 rustls 校验**，只是信任锚点是这一张而不是公共根集合。
//!
//! 第 3 步不是"把校验关掉再自己比一次哈希"——真实连接的证书链、有效期、
//! SAN/主机名匹配全部由 rustls 正常校验，攻击者拿一张指纹相同但主机名不符
//! 的证书同样连不上。第 1 步的不校验握手是安全的，因为它的输出唯一的去向
//! 就是「和一个已经人工确认过的指纹比大小」。
//!
//! # 为什么不引入异步运行时
//!
//! `rustls` 本身是纯同步的状态机（`rustls::ClientConnection` +
//! `rustls::Stream` 直接跑在 `std::net::TcpStream` 上），`ureq` 同理。
//! `cargo tree -p arca-cli | grep -c tokio` 仍然是 0（spec §3.1：客户端
//! 零常驻，一次性进程不为几个请求起运行时）。

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::fmt;
use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// pin 的算法前缀——本版本只认 SHA-256（FORMAT.md §9.1）。
const PIN_PREFIX: &str = "sha256:";

/// 探测握手的超时。比业务请求的超时短得多：这一步只做 TCP 连接 + TLS
/// 握手，不传任何应用层数据，慢到这个程度就已经不是"网络有点抖"了。
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// 证书信任相关的失败——每一种都要给出**用户下一步该做什么**（I5：停下来
/// 并可诊断，不是尽力恢复）。
#[derive(Debug)]
pub enum PinError {
    /// `.gitarca` 里的 `tls_pin` 值本身不合规。
    MalformedPin { value: String },
    /// 配置了 pin，但服务端证书的指纹与它不符——**指纹变更即拒连**。
    Mismatch {
        host: String,
        expected: String,
        actual: String,
    },
    /// 没有配置 pin，而服务端用的是自签名证书（公共根校验不通过）。
    /// 绝不 TOFU：报出实际指纹，让用户带外核对后显式 pin。
    NotPinned { host: String, actual: String },
    /// 探测握手本身没走完（连不上、超时、对端不是 TLS 等）。
    Probe { host: String, reason: String },
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PinError::MalformedPin { value } => write!(
                f,
                ".gitarca 里的 tls_pin {value:?} 不合规——必须是 `sha256:` 加 64 位小写\
                 十六进制（FORMAT.md §9.1）。已停止，不做任何猜测。"
            ),
            PinError::Mismatch {
                host,
                expected,
                actual,
            } => write!(
                f,
                "{host} 的 TLS 证书指纹已变更，拒绝连接（spec §9）。\n\
                   .gitarca 里 pin 的是：{expected}\n\
                   服务端此刻出示的是：{actual}\n\
                 这可能是 hub 换了证书（正常运维），也可能是有人在中间——arca 无法\
                 分辨，所以停下来问你（I5）。确认新证书确实是你的 hub 之后，运行\
                 `arca hub trust <hub 名> --fingerprint {actual}` 更新 pin。"
            ),
            PinError::NotPinned { host, actual } => write!(
                f,
                "{host} 用的是自签名（或不被系统根信任的）TLS 证书，而 .gitarca 里没有\
                 为它记录 tls_pin，拒绝连接。\n\
                   服务端出示的证书指纹：{actual}\n\
                 arca **绝不**「首次使用即信任」——那等于把一次静默的中间人机会做成默认\
                 行为。请在 hub 那台机器上用带外渠道核对这个指纹（例如\
                 `openssl x509 -in <证书> -noout -fingerprint -sha256`），确认一致后运行\
                 `arca hub trust <hub 名> --fingerprint {actual}`。"
            ),
            PinError::Probe { host, reason } => {
                write!(f, "{host}：TLS 握手探测失败：{reason}")
            }
        }
    }
}

impl std::error::Error for PinError {}

/// 校验 `tls_pin` 的字节格式（FORMAT.md §9.1）。返回规范化后的原值。
pub fn parse_pin(value: &str) -> Result<String, PinError> {
    let bad = || PinError::MalformedPin {
        value: value.to_string(),
    };
    let hex = value.strip_prefix(PIN_PREFIX).ok_or_else(bad)?;
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(bad());
    }
    Ok(value.to_string())
}

/// 一张证书 DER 的指纹文本（`sha256:<64 位小写十六进制>`）。
pub fn fingerprint(der: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(PIN_PREFIX.len() + 64);
    out.push_str(PIN_PREFIX);
    for b in digest {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// 只为取证书的 rustls 校验器：接受任何证书，把叶子证书的 DER 记下来。
///
/// **它的产物不被信任**——唯一去向是与一个已经人工确认过的指纹比对
/// （见模块顶部「怎么在不放弃校验的前提下实现"只信这一张证书"」）。真正的
/// 业务连接走的是 `RootCerts::Specific` + rustls 的完整校验，不经过这里。
#[derive(Debug)]
struct ProbeVerifier {
    captured: Mutex<Option<Vec<u8>>>,
    /// 签名算法的校验仍然交给 rustls 自己的实现——这一层只放过"这张证书
    /// 由谁签发/是否受信"这一个判断，不去碰密码学本身。
    inner: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for ProbeVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self
            .captured
            .lock()
            .expect("探测握手是单线程的，这把锁不可能被毒化") = Some(end_entity.to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.inner.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.inner.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    // 进程级 default provider 可能已经被别处装过（重复安装返回 Err，
    // 无害）——不依赖它，直接构造一份自己的。
    Arc::new(rustls::crypto::ring::default_provider())
}

/// 连到 `host:port` 做一次**只为取证书**的 TLS 握手，返回服务端叶子证书的
/// DER。不发送任何应用层字节，握手一完成就断开。
pub fn probe_leaf_cert(host: &str, port: u16) -> Result<Vec<u8>, PinError> {
    let err = |reason: String| PinError::Probe {
        host: host.to_string(),
        reason,
    };

    let provider = provider();
    let verifier = Arc::new(ProbeVerifier {
        captured: Mutex::new(None),
        inner: provider.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| err(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();

    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| err(format!("{host:?} 不是合法的 TLS 服务器名")))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| err(e.to_string()))?;

    // **逐个尝试全部解析出的地址**，不是只试第一个：`localhost` 在装了
    // IPv6 的机器上通常解析成 `::1` 与 `127.0.0.1` 两条，而服务只监听其中
    // 一条是极常见的情形（本机自建 hub 尤其如此）。只试第一条会得到一句
    // "Connection refused"，把一个纯粹的地址族问题误报成"hub 没起来"。
    let addr = format!("{host}:{port}");
    let candidates: Vec<std::net::SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .map_err(|e| err(e.to_string()))?
        .collect();
    if candidates.is_empty() {
        return Err(err(format!("{addr} 解析不出任何地址")));
    }
    let mut last: Option<std::io::Error> = None;
    let mut sock = None;
    for candidate in &candidates {
        match TcpStream::connect_timeout(candidate, PROBE_TIMEOUT) {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let mut sock = match sock {
        Some(s) => s,
        None => {
            return Err(err(format!(
                "{}（已尝试 {} 个地址）",
                last.map(|e| e.to_string())
                    .unwrap_or_else(|| "连接失败".to_string()),
                candidates.len()
            )))
        }
    };
    sock.set_read_timeout(Some(PROBE_TIMEOUT))
        .map_err(|e| err(e.to_string()))?;
    sock.set_write_timeout(Some(PROBE_TIMEOUT))
        .map_err(|e| err(e.to_string()))?;

    // 推动握手直到完成：`complete_io` 会一直读写到 rustls 不再需要 IO。
    conn.complete_io(&mut sock)
        .map_err(|e| err(e.to_string()))?;
    // 礼貌地关闭，不给服务端留半开连接。
    conn.send_close_notify();
    let _ = conn.complete_io(&mut sock);
    let _ = sock.flush();

    let captured = verifier
        .captured
        .lock()
        .expect("探测握手是单线程的")
        .clone();
    captured.ok_or_else(|| err("握手完成但没有捕获到服务端证书".to_string()))
}

/// 这个 hub 该用什么 TLS 信任配置——[`crate::transport::http::HttpTransport`]
/// 构造 `ureq::Agent` 时的输入。
#[derive(Debug)]
pub enum Trust {
    /// 走公共根（WebPki）：没有 pin 的 `https://`，以及所有 `http://`
    /// （后者压根不走 TLS，这个值对它无意义，调用方不会问）。
    PublicRoots,
    /// 只信任这一张证书（DER）——配置了 pin 且指纹已核对通过。
    PinnedCert(Vec<u8>),
}

/// 按 spec §9 决定这次连接的信任配置。
///
/// - `pin` 为 `None` → [`Trust::PublicRoots`]。**不在这里探测证书**：公网
///   证书的场合探测是纯粹的浪费，而自签名的场合握手会在真正的请求上失败，
///   由 [`explain_handshake_failure`] 补上"你需要 pin"这句诊断。
/// - `pin` 为 `Some` → 探测 + 比对；相符才返回 [`Trust::PinnedCert`]。
pub fn decide(host: &str, port: u16, pin: Option<&str>) -> Result<Trust, PinError> {
    let Some(pin) = pin else {
        return Ok(Trust::PublicRoots);
    };
    let expected = parse_pin(pin)?;
    let der = probe_leaf_cert(host, port)?;
    let actual = fingerprint(&der);
    if actual != expected {
        return Err(PinError::Mismatch {
            host: host.to_string(),
            expected,
            actual,
        });
    }
    Ok(Trust::PinnedCert(der))
}

/// 从 `<scheme>://<host>[:port]` 拆出 (host, port)——`https` 缺省 443，
/// `http` 缺省 80。IPv6 字面量（`[::1]:8443`）按 RFC 3986 处理。
pub fn host_port(base_url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = base_url.split_once("://")?;
    let default_port = match scheme {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    if let Some(close) = authority.strip_prefix('[').and_then(|r| r.find(']')) {
        let host = &authority[1..=close];
        let port = authority[close + 2..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((authority.to_string(), default_port)),
    }
}

/// [`decide`] 的 URL 版本：`http://`（明文，没有证书可验）直接给
/// [`Trust::PublicRoots`]（调用方对它不会真的去建 TLS）；`https://` 才走
/// 探测与比对。
pub fn decide_for_url(base_url: &str, pin: Option<&str>) -> Result<Trust, PinError> {
    if !base_url.starts_with("https://") {
        return Ok(Trust::PublicRoots);
    }
    let (host, port) = host_port(base_url).ok_or_else(|| PinError::Probe {
        host: base_url.to_string(),
        reason: "无法从 URL 解析主机与端口".to_string(),
    })?;
    decide(&host, port, pin)
}

/// 一次 `https://` 请求握手失败、且这个 hub **没有** pin 时的补充诊断：
/// 探测一次拿到实际指纹，产出 [`PinError::NotPinned`]（告诉用户怎么 pin）。
///
/// 探测本身也可能失败（服务器根本没起来）——那时候原样返回探测的失败，
/// 不硬套"你需要 pin"这个结论（I5：不猜）。
pub fn explain_handshake_failure(host: &str, port: u16) -> PinError {
    match probe_leaf_cert(host, port) {
        Ok(der) => PinError::NotPinned {
            host: host.to_string(),
            actual: fingerprint(&der),
        },
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pin接受合规值() {
        let v = format!("sha256:{}", "a1".repeat(32));
        assert_eq!(parse_pin(&v).unwrap(), v);
    }

    #[test]
    fn parse_pin拒绝未知算法前缀() {
        assert!(matches!(
            parse_pin(&format!("sha1:{}", "a".repeat(64))),
            Err(PinError::MalformedPin { .. })
        ));
        assert!(matches!(
            parse_pin(&"a".repeat(64)),
            Err(PinError::MalformedPin { .. })
        ));
    }

    #[test]
    fn parse_pin拒绝长度或字符集不对的值() {
        assert!(parse_pin("sha256:").is_err());
        assert!(parse_pin(&format!("sha256:{}", "a".repeat(63))).is_err());
        assert!(
            parse_pin(&format!("sha256:{}", "A".repeat(64))).is_err(),
            "大写不接受"
        );
        assert!(parse_pin(&format!("sha256:{}", "z".repeat(64))).is_err());
    }

    /// 指纹是 DER 的 SHA-256——用一个已知向量钉住编码（空输入的 SHA-256）。
    #[test]
    fn fingerprint是der的sha256且小写十六进制() {
        assert_eq!(
            fingerprint(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // 产出的值必须能被 `parse_pin` 接受——两者是同一套编码。
        assert!(parse_pin(&fingerprint(b"whatever")).is_ok());
    }

    #[test]
    fn 没有pin时走公共根() {
        assert!(matches!(
            decide("example.invalid", 443, None).unwrap(),
            Trust::PublicRoots
        ));
    }

    #[test]
    fn pin本身不合规时立刻报错而不去连接() {
        // 主机名故意是不可解析的——如果实现先去连接再校验 pin 格式，
        // 这里会得到 `Probe` 而不是 `MalformedPin`。
        let err = decide("nonexistent.invalid", 443, Some("garbage")).unwrap_err();
        assert!(matches!(err, PinError::MalformedPin { .. }), "实得 {err:?}");
    }
}
