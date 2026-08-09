//! `Transport::changes` 的两个实现必须给出**同一套游标语义**（M3a Task 3，
//! PROTOCOL.md §3）。
//!
//! M2c 建了服务端 `GET /changes` 与 longpoll，但客户端一直没有消费者；
//! agentd 的增量回路要建在它上面，所以这条语义一旦在两种传输之间分叉，
//! 就会变成「同一个数据集，file:// 能增量、http:// 静默漏事件」这类
//! 最难查的 bug——`Transport` 抽象当初要消除的正是这类分叉（M2d 评审原话）。
//!
//! 四条必须一致的性质：
//!
//! 1. `since = None` → 从头开始，给出全部事件与当前游标；
//! 2. 正常游标 → 只给它之后的事件；
//! 3. **epoch 不符 / 游标超前 → `ResetRequired`，不是错误、也不是「从头开始」**；
//! 4. `limit` 截断时，游标只推进到**这一批的最后一条**，不是最新游标。
//!
//! 第 3 与第 4 条是协议里最容易写错的两处，各自都会造成静默的数据后果：
//! 前者是无声地重下全库，后者是被截掉的事件**永久丢失**且没有任何征兆。
//!
//! http 那一侧用手撸的最小 mock（**不是** `arcad` 的替身——`arca-cli` 是 MIT、
//! `arcad` 是 AGPL-3.0-only，即便只是 dev-dependency 也不能反向依赖，
//! 见 CLAUDE.md「许可证分层」）。

use arca_cli::transport::{ChangesOutcome, Transport};
use arca_format::hub_layout::FormatJson;
use arca_format::journal::{Cursor, JournalEvent, Op};
use arca_format::model::{Actor, ItemId};
use arca_store::root::StorageRoot;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

const DATASET_ID: &str = "9c41000000000000000000000000abcd";

fn actor() -> Actor {
    Actor {
        account: "t".into(),
        device: "t".into(),
        session: "t".into(),
    }
}

fn 造存储根(dir: &std::path::Path) -> StorageRoot {
    std::fs::create_dir_all(dir.join(".arca")).unwrap();
    std::fs::create_dir_all(dir.join("files")).unwrap();
    for sub in [".arca/tmp", ".arca/trash", ".arca/journal"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let format = FormatJson {
        format: 1,
        dataset_id: DATASET_ID.to_string(),
        hash_algo: "blake3".to_string(),
        created_at: "2026-08-09T09:00:00Z".to_string(),
    };
    std::fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    StorageRoot::open(dir, Some(DATASET_ID)).unwrap()
}

/// 往 journal 里写 `n` 条 upsert 事件，返回写完之后的游标。
fn 写入事件(root: &StorageRoot, n: u64) -> Cursor {
    for i in 0..n {
        let seq = arca_cli::journal::next_seq(root).unwrap();
        arca_cli::journal::append(
            root,
            &JournalEvent {
                seq,
                op: Op::Upsert,
                item_id: ItemId::from_bytes([i as u8; 16]),
                version_id: arca_cli::ids::new_version_id(),
                path: format!("f{i}.bin"),
                from: None,
                actor: actor(),
                at: "2026-08-09T10:00:00Z".to_string(),
            },
        )
        .unwrap();
    }
    arca_cli::journal::read_all(root).unwrap().0.unwrap()
}

// ---------------------------------------------------------------------------
// file:// 一侧
// ---------------------------------------------------------------------------

#[test]
fn local_从头开始给出全部事件与当前游标() {
    let dir = tempfile::tempdir().unwrap();
    let root = 造存储根(dir.path());
    let now = 写入事件(&root, 3);
    let t = arca_cli::transport::local::LocalTransport::new(&root);

    match t.changes(None, Duration::ZERO, 1000).unwrap() {
        ChangesOutcome::Events { events, cursor } => {
            assert_eq!(events.len(), 3);
            assert_eq!(cursor, Some(now));
        }
        other => panic!("应为 Events，实得 {other:?}"),
    }
}

#[test]
fn local_正常游标只给它之后的事件() {
    let dir = tempfile::tempdir().unwrap();
    let root = 造存储根(dir.path());
    写入事件(&root, 3);
    let t = arca_cli::transport::local::LocalTransport::new(&root);

    let ChangesOutcome::Events { events, .. } = t.changes(None, Duration::ZERO, 1000).unwrap()
    else {
        panic!()
    };
    let 第一条之后 = Cursor {
        epoch: arca_cli::journal::current_epoch(&root).unwrap().unwrap(),
        seq: events[0].seq,
    };

    match t.changes(Some(&第一条之后), Duration::ZERO, 1000).unwrap() {
        ChangesOutcome::Events { events, .. } => {
            assert_eq!(events.len(), 2, "只该给第一条之后的两条：{events:?}");
        }
        other => panic!("应为 Events，实得 {other:?}"),
    }
}

/// epoch 不符 → `ResetRequired`。**绝不当成「从头开始」**：那会静默重下全库，
/// 把一个可诊断的状态变成一次无声的巨量传输（I5）。
#[test]
fn local_epoch不符时给出reset_required而不是从头开始() {
    let dir = tempfile::tempdir().unwrap();
    let root = 造存储根(dir.path());
    let now = 写入事件(&root, 3);
    let t = arca_cli::transport::local::LocalTransport::new(&root);

    let 别的epoch = Cursor {
        epoch: "ffffffffffffffffffffffffffffffff".to_string(),
        seq: 0,
    };
    match t.changes(Some(&别的epoch), Duration::ZERO, 1000).unwrap() {
        ChangesOutcome::ResetRequired { cursor } => {
            assert_eq!(cursor, Some(now), "必须给出服务端当前的有效游标供续接");
        }
        other => panic!("epoch 不符必须是 ResetRequired，实得 {other:?}"),
    }
}

/// 游标超前于我们所知的末尾——它声称见过我们没有的东西，同样没法诚实续接。
#[test]
fn local_游标超前时给出reset_required() {
    let dir = tempfile::tempdir().unwrap();
    let root = 造存储根(dir.path());
    let now = 写入事件(&root, 3);
    let t = arca_cli::transport::local::LocalTransport::new(&root);

    let 超前 = Cursor {
        epoch: now.epoch.clone(),
        seq: now.seq + 999,
    };
    assert!(matches!(
        t.changes(Some(&超前), Duration::ZERO, 1000).unwrap(),
        ChangesOutcome::ResetRequired { .. }
    ));
}

/// **本文件里最重要的一条。** `limit` 截断时游标只能推进到这一批的最后一条；
/// 若给出最新游标，客户端下一次从那里继续，中间被截掉的事件就**永久丢失**，
/// 而且没有任何征兆——它会表现为「某个文件的某次改动从未发生过」。
#[test]
fn local_limit截断时游标只推进到这一批的最后一条() {
    let dir = tempfile::tempdir().unwrap();
    let root = 造存储根(dir.path());
    let 最新 = 写入事件(&root, 5);
    let t = arca_cli::transport::local::LocalTransport::new(&root);

    let ChangesOutcome::Events { events, cursor } = t.changes(None, Duration::ZERO, 2).unwrap()
    else {
        panic!()
    };
    assert_eq!(events.len(), 2, "limit=2 应当只给两条");
    let cursor = cursor.expect("截断时必须给出可续接的游标");
    assert_ne!(cursor, 最新, "游标绝不能跳到最新——中间三条会被永久跳过");
    assert_eq!(cursor.seq, events[1].seq);

    // 从截断游标继续，剩下三条必须一条不少地拿到。
    let ChangesOutcome::Events { events: rest, .. } =
        t.changes(Some(&cursor), Duration::ZERO, 1000).unwrap()
    else {
        panic!()
    };
    assert_eq!(rest.len(), 3, "续接必须补齐剩余全部事件：{rest:?}");
}

/// 数据集一条事件都没有、客户端却拿着游标——同样没法续接。
#[test]
fn local_空journal但客户端有游标时给出reset_required() {
    let dir = tempfile::tempdir().unwrap();
    let root = 造存储根(dir.path());
    let t = arca_cli::transport::local::LocalTransport::new(&root);
    let 凭空 = Cursor {
        epoch: "ffffffffffffffffffffffffffffffff".to_string(),
        seq: 7,
    };
    assert!(matches!(
        t.changes(Some(&凭空), Duration::ZERO, 1000).unwrap(),
        ChangesOutcome::ResetRequired { cursor: None }
    ));
}

// ---------------------------------------------------------------------------
// http:// 一侧
// ---------------------------------------------------------------------------

fn serve(status_line: &'static str, body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        tx.send(()).ok();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                match reader.read_line(&mut h) {
                    Ok(0) => break,
                    Ok(_) if h == "\r\n" || h == "\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    rx.recv().unwrap();
    format!("http://{addr}")
}

#[test]
fn http_200解析事件与游标() {
    let ev = JournalEvent {
        seq: 4,
        op: Op::Upsert,
        item_id: ItemId::from_bytes([7; 16]),
        version_id: arca_cli::ids::new_version_id(),
        path: "x.bin".into(),
        from: None,
        actor: actor(),
        at: "2026-08-09T10:00:00Z".into(),
    };
    let body = format!(
        r#"{{"events":[{}],"cursor":"{}:4"}}"#,
        ev.to_line().unwrap(),
        "a".repeat(32)
    );
    let base = serve("200 OK", body);
    let t = arca_cli::transport::http::HttpTransport::new(&base, DATASET_ID, None);

    match t.changes(None, Duration::ZERO, 1000).unwrap() {
        ChangesOutcome::Events { events, cursor } => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].path, "x.bin");
            assert_eq!(cursor.unwrap().seq, 4);
        }
        other => panic!("应为 Events，实得 {other:?}"),
    }
}

/// `410 journal.reset_required` 必须是 **`Ok` 的一个变体**，不是 `Err`——
/// 做成错误会被 agentd 那类泛泛的重试逻辑吞掉，而漏掉全量对账的后果是
/// 客户端永远缺一段历史。与 `CommitOutcome::Conflict` 同一条纪律（M2b）。
#[test]
fn http_410是ok的reset_required而不是错误() {
    let body = format!(
        r#"{{"code":"journal.reset_required","message":"游标早于保留区间","cursor":"{}:9"}}"#,
        "b".repeat(32)
    );
    let base = serve("410 Gone", body);
    let t = arca_cli::transport::http::HttpTransport::new(&base, DATASET_ID, None);

    match t.changes(None, Duration::ZERO, 1000).unwrap() {
        ChangesOutcome::ResetRequired { cursor } => {
            assert_eq!(cursor.unwrap().seq, 9, "必须给出可续接的游标");
        }
        other => panic!("410 应为 ResetRequired，实得 {other:?}"),
    }
}

/// `cursor` 为 `null`（数据集从未有过事件）是合法的，但一个**存在却解析不出来**
/// 的游标是协议错误——绝不悄悄降级成 `None`（那等于「从头开始」，I5）。
#[test]
fn http_畸形游标是协议错误而不是静默的从头开始() {
    let base = serve(
        "200 OK",
        r#"{"events":[],"cursor":"这不是游标"}"#.to_string(),
    );
    let t = arca_cli::transport::http::HttpTransport::new(&base, DATASET_ID, None);
    let err = t.changes(None, Duration::ZERO, 1000).unwrap_err();
    assert!(
        err.to_string().contains("解析失败"),
        "必须是可诊断的协议错误：{err}"
    );
}

#[test]
fn http_cursor为null时是合法的空数据集() {
    let base = serve("200 OK", r#"{"events":[],"cursor":null}"#.to_string());
    let t = arca_cli::transport::http::HttpTransport::new(&base, DATASET_ID, None);
    match t.changes(None, Duration::ZERO, 1000).unwrap() {
        ChangesOutcome::Events { events, cursor } => {
            assert!(events.is_empty());
            assert_eq!(cursor, None);
        }
        other => panic!("实得 {other:?}"),
    }
}
