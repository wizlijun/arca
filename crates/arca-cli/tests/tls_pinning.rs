//! `https://` 的证书信任模型端到端验收（M2e Task 4，spec §9、FORMAT.md §9.1）。
//!
//! 三条必须成立的性质，每条一个测试，全部用**真的 TLS 握手**（rustls 服务端
//! + 现造的自签名证书），不是 mock：
//!
//! 1. 自签名 + **正确的 pin** → 连通；
//! 2. **pin 不符** → 拒连，且错误可诊断（点名两个指纹、告诉用户下一步）；
//! 3. **无 pin 的自签名** → 拒连，并提示如何 pin（**绝不 TOFU**）。
//!
//! 外加两条守住配置面的：`arca hub trust` 不带 `--fingerprint` 时只打印不
//! 写入；带错的指纹时拒绝写入。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{mpsc, Arc};

const DATASET_ID: &str = "9c41000000000000000000000000abcd";

fn arca(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("arca 二进制应能正常启动")
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("需要可用的 git")
        .success();
    assert!(ok, "git {args:?} 失败");
}

/// 起一个自签名证书的 TLS 服务器，对任何请求回同一段响应。返回
/// (端口, 叶子证书 DER)。**用 `localhost` 作为 SAN**——客户端连的也是
/// `localhost`，这样主机名校验这一环是真的在跑（rustls 会验 SAN，
/// pin 并不豁免它）。
fn serve_tls(response: &'static str) -> (u16, Vec<u8>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_der = issued.cert.der().to_vec();
    let key_der = issued.signing_key.serialize_der();

    // 绑到 `localhost:0` 而不是 `127.0.0.1:0`：客户端连的是 `localhost`
    // （证书 SAN 也是它），本机装了 IPv6 时 `localhost` 会解析出 `::1` 与
    // `127.0.0.1` 两条，只绑其中一条会让测试依赖"客户端恰好先试对了哪条"。
    // 生产侧 `tls::probe_leaf_cert` 已经改成逐个尝试全部地址，这里绑
    // `localhost` 是为了让服务端这一侧也不挑地址族。
    let listener = TcpListener::bind("localhost:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let certs = vec![rustls_pki_types::CertificateDer::from(cert_der.clone())];
    let key =
        rustls_pki_types::PrivateKeyDer::Pkcs8(rustls_pki_types::PrivatePkcs8KeyDer::from(key_der));
    let server_cfg = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap(),
    );

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        tx.send(()).ok();
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { continue };
            let Ok(mut conn) = rustls::ServerConnection::new(server_cfg.clone()) else {
                continue;
            };
            let mut tls = rustls::Stream::new(&mut conn, &mut sock);
            let mut buf = [0u8; 4096];
            // 客户端的 pin 探测握手完成后就断开、不发请求，这里读到
            // EOF/错误是正常的，不当回事。
            if tls.read(&mut buf).is_ok() {
                let _ = tls.write_all(response.as_bytes());
                let _ = tls.flush();
            }
        }
    });
    rx.recv().unwrap();
    (port, cert_der)
}

/// `GET .../state` 的空清单响应——测试只关心"连没连上"，不关心内容。
fn empty_state_response() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]"
}

fn sha256_pin(der: &[u8]) -> String {
    arca_cli::tls::fingerprint(der)
}

/// 建一个绑定到 `https://localhost:<port>` 的 vault；`pin` 非空时写进
/// `.gitarca` 的 `[hub.home].tls_pin`。
fn 建vault(port: u16, pin: Option<&str>) -> tempfile::TempDir {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("assets")).unwrap();
    git(vault.path(), &["init", "-q"]);
    git(vault.path(), &["config", "user.email", "t@example.com"]);
    git(vault.path(), &["config", "user.name", "t"]);
    assert!(arca(vault.path(), &["init", "."]).status.success());
    let out = arca(
        vault.path(),
        &[
            "register",
            "assets",
            "--hub",
            "home",
            "--hub-url",
            &format!("https://localhost:{port}"),
            "--dataset-id",
            DATASET_ID,
        ],
    );
    assert!(out.status.success(), "register 失败：{out:?}");

    if let Some(pin) = pin {
        let path = vault.path().join(".gitarca");
        let text = std::fs::read_to_string(&path).unwrap();
        // 追加到 `[hub.home]` 表里——`.gitarca` 的 hub 表是 TOML 表，
        // 在 `url = ...` 那行后面插一行即可。
        let patched = text.replace(
            &format!("url = \"https://localhost:{port}\""),
            &format!("url = \"https://localhost:{port}\"\ntls_pin = \"{pin}\""),
        );
        assert_ne!(patched, text, "测试前置条件：应当成功插入 tls_pin");
        std::fs::write(&path, patched).unwrap();
    }
    vault
}

/// 性质 1：自签名 + 正确的 pin → 连通。
#[test]
fn 自签名加正确的pin可以连通() {
    let (port, der) = serve_tls(empty_state_response());
    let vault = 建vault(port, Some(&sha256_pin(&der)));

    let out = arca(vault.path(), &["status", "assets"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // 空 state + 空数据集 → 完全同步，Rule of Silence，退出码 0。
    assert!(out.status.success(), "pin 正确时应当连通：stderr={stderr}");
    assert!(
        !stderr.contains("指纹"),
        "pin 正确时不该有任何证书相关的抱怨：{stderr}"
    );
}

/// 性质 2：pin 不符 → 拒连，且错误可诊断。
#[test]
fn pin不符时拒连且错误可诊断() {
    let (port, _der) = serve_tls(empty_state_response());
    // 一个语法合规、但绝不会等于任何真实证书的指纹。
    let wrong = format!("sha256:{}", "0".repeat(64));
    let vault = 建vault(port, Some(&wrong));

    let out = arca(vault.path(), &["status", "assets"]);
    assert!(!out.status.success(), "pin 不符必须拒连：{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("指纹已变更"),
        "必须说清是指纹变了：{stderr}"
    );
    assert!(
        stderr.contains(&wrong),
        "必须报出 .gitarca 里 pin 的那个值：{stderr}"
    );
    assert!(
        stderr.contains("arca hub trust"),
        "必须告诉用户下一步怎么做：{stderr}"
    );
}

/// 性质 3：无 pin 的自签名 → 拒连，并提示如何 pin。**绝不 TOFU。**
#[test]
fn 无pin的自签名被拒连且提示如何pin() {
    let (port, der) = serve_tls(empty_state_response());
    let vault = 建vault(port, None);

    let out = arca(vault.path(), &["status", "assets"]);
    assert!(!out.status.success(), "无 pin 的自签名必须拒连：{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("arca hub trust"),
        "必须提示如何 pin：{stderr}"
    );
    assert!(
        stderr.contains(&sha256_pin(&der)),
        "必须报出服务端实际出示的指纹，供带外核对：{stderr}"
    );
    // 关键：**没有把任何东西写进 .gitarca**——绝不 TOFU。
    let gitarca = std::fs::read_to_string(vault.path().join(".gitarca")).unwrap();
    assert!(
        !gitarca.contains("tls_pin"),
        "拒连绝不能顺手把指纹记下来（那就是 TOFU）：{gitarca}"
    );
}

/// `arca hub trust` 不带 `--fingerprint`：**只打印，不写入**。
#[test]
fn hub_trust不带指纹时只打印不写入() {
    let (port, der) = serve_tls(empty_state_response());
    let vault = 建vault(port, None);

    let out = arca(vault.path(), &["hub", "trust", "home"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        sha256_pin(&der),
        "指纹本身走 stdout（可复制粘贴/可脚本消费）"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("尚未写入"), "{stderr}");
    assert!(
        stderr.contains("openssl"),
        "应给出带外核对的具体办法：{stderr}"
    );

    let gitarca = std::fs::read_to_string(vault.path().join(".gitarca")).unwrap();
    assert!(
        !gitarca.contains("tls_pin"),
        "不带指纹时绝不写入：{gitarca}"
    );
}

/// `arca hub trust --fingerprint <错的>`：拒绝写入，报出两个值。
#[test]
fn hub_trust给错指纹时拒绝写入() {
    let (port, _der) = serve_tls(empty_state_response());
    let vault = 建vault(port, None);
    let wrong = format!("sha256:{}", "b".repeat(64));

    let out = arca(
        vault.path(),
        &["hub", "trust", "home", "--fingerprint", &wrong],
    );
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("指纹不符"), "{stderr}");
    let gitarca = std::fs::read_to_string(vault.path().join(".gitarca")).unwrap();
    assert!(!gitarca.contains("tls_pin"), "{gitarca}");
}

/// `arca hub trust --fingerprint <对的>` → 写入 `.gitarca`，此后能连通。
/// 这条把"人工确认 → pin → 连通"整条链走通。
#[test]
fn hub_trust给对指纹后写入gitarca且此后能连通() {
    let (port, der) = serve_tls(empty_state_response());
    let vault = 建vault(port, None);
    let pin = sha256_pin(&der);

    let out = arca(
        vault.path(),
        &["hub", "trust", "home", "--fingerprint", &pin],
    );
    assert!(out.status.success(), "{out:?}");

    let gitarca = std::fs::read_to_string(vault.path().join(".gitarca")).unwrap();
    assert!(gitarca.contains(&pin), "pin 应被写进 .gitarca：{gitarca}");

    // 写入之后同一条命令就能连通了。
    let out = arca(vault.path(), &["status", "assets"]);
    assert!(
        out.status.success(),
        "pin 之后应当连通：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 明文 `http://` hub 上配 `tls_pin` 是配置错误——**拒绝而不是忽略**：
/// 静默忽略会让用户以为自己已经受 pin 保护，而这条连接连 TLS 都没有。
#[test]
fn 明文http_hub上配pin被拒绝而不是静默忽略() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("assets")).unwrap();
    git(vault.path(), &["init", "-q"]);
    git(vault.path(), &["config", "user.email", "t@example.com"]);
    git(vault.path(), &["config", "user.name", "t"]);
    assert!(arca(vault.path(), &["init", "."]).status.success());
    assert!(arca(
        vault.path(),
        &[
            "register",
            "assets",
            "--hub",
            "home",
            "--hub-url",
            "http://127.0.0.1:18999",
            "--dataset-id",
            DATASET_ID,
        ],
    )
    .status
    .success());

    let path = vault.path().join(".gitarca");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace(
            "url = \"http://127.0.0.1:18999\"",
            &format!(
                "url = \"http://127.0.0.1:18999\"\ntls_pin = \"sha256:{}\"",
                "c".repeat(64)
            ),
        ),
    )
    .unwrap();

    let out = arca(vault.path(), &["status", "assets"]);
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pin 不会生效"),
        "必须明说这个配置是无效的：{stderr}"
    );
}
