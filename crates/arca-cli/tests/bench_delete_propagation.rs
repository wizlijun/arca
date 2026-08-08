//! 删除传播的批量化验收（M2a 切片评审 Important #3，与
//! `crates/arca-cli/tests/bench_10k.rs` 同一纪律：`#[ignore]`，手工执行，
//! 报告实测数字，不要改标准）。
//!
//! 评审实测 400 个文件的删除传播：接收端（`gates::check_delete` 第 4 道每次
//! 重新 `read_dir` 整个 `.arca/trash/`）2.2 秒、发起端（`journal::append` 每条
//! 事件重读+重写整段 journal，`next_seq` 又读一遍，同一形状）12.1 秒——两者
//! 都是 O(n·m)/O(n²)，M1d 曾为"目录 fsync 一万次"做过同一形状的批量化，这里
//! 是同一课再上一遍：`sync()` 现在把 `.arca/trash/` 的目录遍历、journal 的
//! 读写都各自收敛成"整个 `sync()` 调用一次"，不再是"每个文件一次"。
//!
//! 跑法：
//!
//! ```sh
//! cargo test -p arca-cli --release --test bench_delete_propagation -- --ignored --nocapture
//! ```

use arca_format::model::Actor;
use arca_format::trace::NullSink;
use arca_store::root::StorageRoot;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const FILE_COUNT: usize = 1_000;
/// 评审的原始测量是 400 个文件、O(n²) 实现下发起端 12.1 秒；这里的规模是它
/// 的 2.5 倍，外推到 O(n²) 会是 12.1 秒 ×(1000/400)² ≈ 75 秒。批量化之后
/// 实测发起端约 17.8 秒（`trash::move_to_trash` 每次 `rename` 仍各自立即
/// fsync 两侧目录——这是 M2a Task 3 就有的既有成本，不是本条评审 Important #3
/// 点名的那两处 O(n)/O(n²)，见模块顶部文档；批量化它属于另一轮改动，本轮
/// 不做）——这个预算刻意留了远超实测值的余量，只用来拦住"O(n²) 复发"这类
/// 量级的回归，不是一个精确的性能 SLA（不同机器的 fsync 延迟差异很大）。
const BUDGET: Duration = Duration::from_secs(60);

fn actor() -> Actor {
    Actor {
        account: "bench".into(),
        device: "bench".into(),
        session: "s1".into(),
    }
}

fn 造存储根(dir: &Path) -> StorageRoot {
    StorageRoot::create(
        dir,
        "9c41000000000000000000000000abcd",
        "2026-08-08T09:00:00Z",
    )
    .unwrap()
}

fn populate(dir: &Path, count: usize) {
    for i in 0..count {
        let shard = format!("{:03}/{:02}", i / 1000, (i / 10) % 100);
        let dir_path = dir.join(&shard);
        fs::create_dir_all(&dir_path).unwrap();
        let content = format!("bench file #{i}, payload {}", i.wrapping_mul(2_654_435_761));
        fs::write(dir_path.join(format!("f{i:05}.bin")), content.as_bytes()).unwrap();
    }
}

#[test]
#[ignore]
fn 一千文件的删除传播在预算内完成_评审important3验收() {
    let store = tempfile::tempdir().unwrap();
    let root = 造存储根(store.path());
    let mut sink = NullSink;

    let device_a = tempfile::tempdir().unwrap();
    populate(device_a.path(), FILE_COUNT);
    let report_a1 = arca_cli::sync::sync(device_a.path(), &root, &actor(), &mut sink).unwrap();
    assert_eq!(report_a1.uploaded.len(), FILE_COUNT);
    assert!(report_a1.is_clean(), "{report_a1:?}");

    let device_b = tempfile::tempdir().unwrap();
    let report_b1 = arca_cli::sync::sync(device_b.path(), &root, &actor(), &mut sink).unwrap();
    assert_eq!(report_b1.downloaded.len(), FILE_COUNT);
    assert!(report_b1.is_clean(), "{report_b1:?}");

    // 发起端：设备甲把全部文件删掉，一次 sync() 提交 FILE_COUNT 条 tombstone——
    // 批量化之前 journal::append 每条都重读+重写整段 journal，是这里的 O(n²)。
    for i in 0..FILE_COUNT {
        let shard = format!("{:03}/{:02}", i / 1000, (i / 10) % 100);
        fs::remove_file(device_a.path().join(&shard).join(format!("f{i:05}.bin"))).unwrap();
    }
    let originate_start = Instant::now();
    let report_a2 = arca_cli::sync::sync(device_a.path(), &root, &actor(), &mut sink).unwrap();
    let originate_elapsed = originate_start.elapsed();
    assert_eq!(report_a2.tombstone_submitted.len(), FILE_COUNT);
    assert!(report_a2.is_clean(), "{report_a2:?}");

    // 接收端：设备乙同步一次，FILE_COUNT 次 DeleteLocal 都要过闸门第 4 道——
    // 批量化之前 `trash::list` 每次都重新 read_dir 整个 `.arca/trash/`，是
    // 这里的 O(n·m)。
    let receive_start = Instant::now();
    let report_b2 = arca_cli::sync::sync(device_b.path(), &root, &actor(), &mut sink).unwrap();
    let receive_elapsed = receive_start.elapsed();
    assert_eq!(report_b2.deleted_local.len(), FILE_COUNT);
    assert!(
        report_b2.delete_blocked.is_empty(),
        "四道闸门应当全过，不应被拦下：{:?}",
        report_b2.delete_blocked
    );
    assert!(report_b2.is_clean(), "{report_b2:?}");

    let total = originate_elapsed + receive_elapsed;
    eprintln!(
        "{FILE_COUNT} 文件删除传播：发起端 {originate_elapsed:?}，接收端 {receive_elapsed:?}，\
         合计 {total:?}（预算 {BUDGET:?}）"
    );
    assert!(
        total <= BUDGET,
        "删除传播耗时 {total:?}，超出预算 {BUDGET:?}\
         （发起端 {originate_elapsed:?}，接收端 {receive_elapsed:?}）——\
         报告实测数字，不要改标准（brief 原话）"
    );
}
