//! `status` / `verify` / `doctor` 对 `http://` hub 工作（M2e Task 3）。
//!
//! M2d 的切片评审原话：「**arcad 是 M2 的主线，而主健康检查命令对主 hub
//! 类型不工作**」——这三条命令此前在 `local_root()` 处直接 bail，绑定
//! `http://` 的数据集（也就是 M2b/M2c 一路建起来的那条主线配置）跑
//! `arca status` 只会得到一句"这条命令目前只支持本地存储根"。
//!
//! 与 `tests/http_sync_diagnostics.rs` 同一手法：命令壳依赖进程级 `cwd()`，
//! 所以用真实编译好的 `arca` 二进制 + 独立工作目录跑；HTTP 那一侧用一个
//! 手撸的最小 mock（**不是** `arcad` 的替身——`arca-cli` 是 MIT、`arcad` 是
//! AGPL-3.0-only，依赖方向单向，即便只是 dev-dependency 也不能反向依赖，
//! 见 CLAUDE.md「许可证分层」）。对真实 `arcad` 的验证走两机端到端演示。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc;

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

/// 一个只认路径后缀的最小 HTTP/1.1 mock：`routes` 是 (路径包含的子串, 完整
/// 响应字节) 的列表，按顺序取第一个匹配的；没有匹配则回 404。在后台线程里
/// 一直服务到测试结束（进程退出时线程随之消失，不需要显式关停）。
fn serve(routes: Vec<(&'static str, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        tx.send(()).ok();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let path = request_line.split(' ').nth(1).unwrap_or("").to_string();
            // 吃掉请求头（这些端点都是 GET，没有请求体）。
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line.trim_end().is_empty() => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let body = routes
                .iter()
                .find(|(needle, _)| path.contains(needle))
                .map(|(_, resp)| resp.clone())
                .unwrap_or_else(|| {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                });
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        }
    });
    rx.recv().unwrap();
    format!("http://{addr}")
}

fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn bytes_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// 503 + `mount.absent`——`PROTOCOL.md` §1.2「503：数据集离线」。
fn offline_response() -> String {
    let body = r#"{"code":"mount.absent","message":"存储根未挂载"}"#;
    format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// 建一个绑定到 `hub_url` 的 vault（含一个 `assets` 数据集与一个本地文件）。
fn 建vault(hub_url: &str) -> tempfile::TempDir {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("assets")).unwrap();
    std::fs::write(vault.path().join("assets/a.txt"), b"hello").unwrap();
    git(vault.path(), &["init", "-q"]);
    git(vault.path(), &["config", "user.email", "t@example.com"]);
    git(vault.path(), &["config", "user.name", "t"]);
    let out = arca(vault.path(), &["init", "."]);
    assert!(out.status.success(), "arca init 失败：{out:?}");
    let out = arca(
        vault.path(),
        &[
            "register",
            "assets",
            "--hub",
            "home",
            "--hub-url",
            hub_url,
            "--dataset-id",
            DATASET_ID,
        ],
    );
    assert!(out.status.success(), "register 失败：{out:?}");
    vault
}

/// `GET .../state` 的响应：一个 `a.txt`，内容就是 `hello`。
fn state_with_hello() -> String {
    let hash = arca_chunk::hash::ContentHash::from_bytes(b"hello").to_text();
    json_response(&format!(
        r#"[{{"path":"a.txt","item_id":"{}","version_id":"20260809T090000Z-{}","hash":"{hash}","size":5,"state":"present"}}]"#,
        "3f".repeat(16),
        "1".repeat(32),
    ))
}

// =====================================================================
// status
// =====================================================================

#[test]
fn status对http_hub工作而不是报这条命令只支持本地() {
    let url = serve(vec![("/state", state_with_hello())]);
    let vault = 建vault(&url);

    let out = arca(vault.path(), &["status", "assets"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("只支持本地"),
        "status 必须对 http:// hub 工作：{stderr}"
    );
    // hub 上已有 a.txt（内容相同），本地基线为空 → 决策表判为零传输认领
    // （`AdoptBaseline`），是一条待办 → 退出码 1、明确列出。
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr.contains("待认领") || stderr.contains("a.txt"),
        "应当报出这个路径的待办：{stderr}"
    );
}

/// **I11**：hub 回 503 时 `status` 必须报"离线"、退出码 2，
/// **绝不能呈现成"库是空的、本地文件全都没上传"**。
#[test]
fn status在http_hub回503时报离线退出2而不是当空库() {
    let url = serve(vec![("/state", offline_response())]);
    let vault = 建vault(&url);

    let out = arca(vault.path(), &["status", "assets"]);
    assert_eq!(out.status.code(), Some(2), "I11：离线应退出码 2：{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("离线"), "{stderr}");
    assert!(
        stderr.contains("hub=home"),
        "离线诊断必须点名是哪个 hub：{stderr}"
    );
    assert!(
        !stderr.contains("待上传"),
        "离线绝不能被呈现成「hub 是空的、这些文件都还没上传」：{stderr}"
    );
}

// =====================================================================
// verify
// =====================================================================

/// 默认档：只验元数据，而且**必须明说它没验什么**——一个自称 verify 却
/// 只对了对元数据的命令，比没有更危险。
#[test]
fn verify默认档对http_hub只验元数据且明说没验内容() {
    let url = serve(vec![("/state", state_with_hello())]);
    let vault = 建vault(&url);

    let out = arca(vault.path(), &["verify", "assets"]);
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("元数据一致性检查"),
        "必须说明这次跑的是哪一档：{stderr}"
    );
    assert!(
        stderr.contains("不重算任何内容字节"),
        "必须明说默认档验不出位腐：{stderr}"
    );
    assert!(
        stderr.contains("--deep"),
        "必须告诉用户怎么才能真的验内容：{stderr}"
    );
}

/// `--deep` 档：真的把内容拉下来重算——hub 声明的哈希与实际字节不符
/// （位腐 / 被外部工具改写）时必须报出来。这正是默认档发现不了的那类问题。
#[test]
fn verify_deep档拉内容重算能发现hub上的位腐() {
    // `/state` 声称 a.txt 的哈希是 `hello` 的，但 `/files/a.txt` 实际返回
    // 的是 `ROTTEN`——等价于 hub 的 files/ 字节被外部改写过。
    let url = serve(vec![
        ("/state", state_with_hello()),
        ("/files/", bytes_response("ROTTEN")),
    ]);
    let vault = 建vault(&url);

    // 默认档看不出问题（它只对元数据，而 hub 自己的元数据是自洽的）。
    let out = arca(vault.path(), &["verify", "assets"]);
    assert!(
        out.status.success(),
        "默认档确实发现不了位腐——这正是它需要被明说的原因：{out:?}"
    );

    // --deep 必须抓到。
    let out = arca(vault.path(), &["verify", "assets", "--deep"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("深度 fixity 巡检"), "{stderr}");
    assert!(
        stderr.contains("a.txt") && stderr.contains("不一致"),
        "必须点名是哪个路径、什么问题：{stderr}"
    );
}

#[test]
fn verify在http_hub回503时报离线退出2() {
    let url = serve(vec![("/state", offline_response())]);
    let vault = 建vault(&url);

    let out = arca(vault.path(), &["verify", "assets"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("离线") && stderr.contains("hub=home"),
        "{stderr}"
    );
}

// =====================================================================
// doctor
// =====================================================================

#[test]
fn doctor对http_hub工作而不是报resolvefailed() {
    let url = serve(vec![("/state", state_with_hello())]);
    let vault = 建vault(&url);

    let out = arca(vault.path(), &["doctor"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("解析失败") && !stderr.contains("只支持本地"),
        "doctor 必须对 http:// hub 工作：{stderr}"
    );
    // I5：hub 侧回收站没查过这件事必须被说出来，不能用一个空列表蒙混。
    assert!(
        stderr.contains("未执行"),
        "hub 侧回收站巡检跳过了就要明说：{stderr}"
    );
}

/// **I11**：hub 回 503 时 doctor 必须把这个数据集报成"离线"、退出码 2，
/// 绝不能因此假装"本地没有未同步的文件"（那本该是 `local_only` 检查回答的
/// 问题，而离线状态下它根本没跑）。
#[test]
fn doctor在http_hub回503时报离线退出2且不假装干净() {
    let url = serve(vec![("/state", offline_response())]);
    let vault = 建vault(&url);

    let out = arca(vault.path(), &["doctor"]);
    assert_eq!(out.status.code(), Some(2), "I11：离线应退出码 2：{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("离线"), "{stderr}");
}
