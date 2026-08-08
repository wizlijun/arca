//! 验收演示（M1d Task 8，spec §12.3 M1 行）：1 万个文件 2 分钟内完成去重
//! 归档（哈希 + 写入存储根）+ 全量校验（`arca_store::fsck`）。
//!
//! `#[ignore]`——不进常规 CI（生成 1 万个文件本身就有可观的 IO 成本，不适合
//! 每次 `cargo test` 都跑），手工执行：
//!
//! ```sh
//! cargo test -p arca-cli --release --test bench_10k -- --ignored --nocapture
//! ```
//!
//! **归档路径用 [`arca_cli::sync::sync`] 而不是 `arca adopt`**：`adopt` 在
//! `sync` 之外还要走 `arca-git`（`.gitignore` 更新、`git rm --cached` 等）与
//! vault 解析，这些是 M1 验收演示"文件原地不动、git status 干净"那部分要
//! 覆盖的（`crates/arca-cli/src/adopt.rs` 的
//! `验收_git_status在add与commit后是干净的_清单进git_二进制不进git` 已经
//! 覆盖，用的是几个文件的小规模场景，git 操作本身的开销不是这条验收线的
//! 重点）。这里要单独测量的是 spec §12.3 点名的吞吐——哈希 + 写入 CAS 风格
//! 存储根这条路径本身能不能在预算内处理一万个文件，`adopt`/`sync` 两个
//! porcelain 命令走的都是同一个 `sync::sync` 执行器（见其模块文档），在这里
//! 量它的耗时就是在量两个命令共同的瓶颈，混入 git 调用的开销只会让"是不是
//! arca 自己的问题"这件事变得难以判断。
//!
//! **若跑不进 2 分钟：报告实测数字，不要改标准**（brief 原话）。已知的可能
//! 瓶颈是逐文件 fsync——`arca_store::atomic::write` 对每次写入都做一次
//! tmp→fsync→rename→fsync 父目录的完整事务链（M1a 的已知项，参见
//! `crates/arca-store/src/atomic.rs`），一万次写入就是数万次 fsync 系统调用。

use arca_format::model::Actor;
use arca_format::trace::NullSink;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const FILE_COUNT: usize = 10_000;
const BUDGET: Duration = Duration::from_secs(120);

fn actor() -> Actor {
    Actor {
        account: "bench".into(),
        device: "bench".into(),
        session: "s1".into(),
    }
}

/// 铺 `count` 个小文件，分三层目录（`000/00/f00000.bin` 形状）——模拟真实
/// 相册/笔记库的目录深度，同时避免单个目录下挤一万个 dirent（部分文件系统
/// /工具对单目录条目数敏感，真实 Obsidian 库也是按文件夹分层组织的）。
/// 内容各不相同（按索引派生），所以这一万个文件之间没有内容重复——
/// "去重"这条能力由另一个更小规模的测试
/// （`sync.rs` 的 `两端各自新增相同内容走零传输认领`）单独覆盖，这里的目的
/// 是测吞吐上限，混入大量重复内容反而会让测出来的数字偏乐观。
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
fn 一万文件两分钟内完成去重归档与全量校验() {
    let store = tempfile::tempdir().unwrap();
    let dataset = tempfile::tempdir().unwrap();

    let populate_start = Instant::now();
    populate(dataset.path(), FILE_COUNT);
    let populate_elapsed = populate_start.elapsed();

    let root = arca_store::root::StorageRoot::create(
        store.path(),
        "9c41000000000000000000000000abcd",
        "2026-08-08T09:00:00Z",
    )
    .unwrap();

    let start = Instant::now();
    let mut sink = NullSink;
    let report = arca_cli::sync::sync(dataset.path(), &root, &actor(), &mut sink).unwrap();
    let archive_elapsed = start.elapsed();

    assert_eq!(
        report.uploaded.len(),
        FILE_COUNT,
        "应当全部作为新增上传（各文件内容互不相同）：{} 个 uploaded，{} 个 rejected",
        report.uploaded.len(),
        report.scan_rejected.len()
    );
    assert!(report.is_clean(), "{report:?}");

    let verify_start = Instant::now();
    let fsck_report = arca_store::fsck::check_root(&root);
    let verify_elapsed = verify_start.elapsed();

    assert!(
        fsck_report.problems.is_empty(),
        "全量校验发现问题：{:?}",
        fsck_report.problems
    );
    assert_eq!(fsck_report.checked_files, FILE_COUNT);

    let total = archive_elapsed + verify_elapsed;
    eprintln!(
        "1 万文件基准：铺文件 {populate_elapsed:?}（不计入预算），\
         去重归档 {archive_elapsed:?}，全量校验 {verify_elapsed:?}，\
         合计 {total:?}（预算 {BUDGET:?}）"
    );
    assert!(
        total <= BUDGET,
        "1 万文件去重归档 + 全量校验耗时 {total:?}，超出 2 分钟预算 {BUDGET:?}\
         （归档 {archive_elapsed:?}，校验 {verify_elapsed:?}）——\
         报告实测数字，不要改标准（brief 原话）"
    );
}
