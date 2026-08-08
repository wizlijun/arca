//! `.arca/locks/arca.lock`：存储根级别的跨进程排他锁
//! （`PROTOCOL.md` §1.1，M2b 切片评审 I3）。
//!
//! # 要堵住的威胁
//!
//! [`crate::root`] 的 `StorageRoot` 本身不做任何互斥；`arcad` 的
//! `Dataset::write_lock`（`std::sync::Mutex`，见 `crates/arcad/src/storage.rs`）
//! 只把并发 HTTP 写入序列化在**单个进程内部**——两个 `arcad` 实例挂在同一个
//! 存储根上，或 `arcad` 与一次直连的 `arca sync`（`file://`）并发跑在同一个
//! 根上，各自持有的是不同进程里互不相干的内存锁，`读当前状态 → CAS 校验 →
//! 写入` 这段临界区完全不受保护：两个并发写者都可能在第一步读到同一个
//! "当前版本"、都通过第二步的 CAS 比较、都各自完成第三步的写入，后写入的
//! 静默覆盖先写入的——正是 I4「一切写入走 CAS」要挡住的场景，只是威胁来自
//! 跨进程并发而不是单个客户端的无条件写。`PROTOCOL.md` §1.1 早就写明
//! "排他由 `arca.lock` 保证"，但直到本轮评审之前，这个锁在代码库里完全不
//! 存在——协议文本描述了一个不存在的保证。
//!
//! # 实现选择：OS 级 `flock`/`LockFileEx`，阻塞式获取
//!
//! 经 [`fs4`] crate（`FileExt::lock`）在 `.arca/locks/arca.lock`
//! 上取一把整文件独占锁——Unix 上是 `flock(2)`，Windows 上是 `LockFileEx`，
//! 都是持有进程崩溃/被杀时由内核自动释放的建议锁（advisory lock），不会
//! 像"锁文件里写 PID、靠存在性判断"那类方案一样在崩溃后留下永久性死锁，
//! 与本项目"崩溃后必须能自愈，不能需要人工介入清理"的一贯纪律一致。
//! `unsafe` 全部封在 `fs4` crate 内部——本 crate 仍然
//! `#![forbid(unsafe_code)]`。
//!
//! 选**阻塞**式获取（[`acquire`] 内部调用阻塞版 `FileExt::lock`，不是
//! 非阻塞版 `try_lock`）：本模块保护的临界区本身很短（几次本地小文件读写，
//! 不含网络 IO），阻塞等待比引入一整套"忙时退避重试"的调用方协议更简单、
//! 也更不容易在改动量已经不小的这一轮里引入新的竞态。`PROTOCOL.md` §7
//! 已经登记了 `lock.busy`（`retryable`）这个码，为未来切换成非阻塞 + 显式
//! 重试留了口子，本轮不需要用到它。
//!
//! # 谁获取它
//!
//! `arca-cli::transport::local::LocalTransport` 的 `commit`/`commit_streamed`/
//! `tombstone`——两个消费者（`arcad` 的 HTTP 写入端点、`arca-cli` 的
//! `file://` 直连同步）全部经这三个方法落盘，是唯一需要跨进程互斥的临界区
//! （纯读操作——`GET`/`arca status`——不需要，同一论证见 `storage.rs`
//! 「`write_lock`」一节）。

use crate::root::StorageRoot;
use arca_format::hub_layout::layout;
// 不 `use fs4::FileExt`：见 `acquire` 里 `fs4::FileExt::lock(&file)` 调用点
// 的注释——完全限定语法不需要 trait 在作用域内，这里刻意不引入这个
// `use`，避免它在够新的工具链上（`std::fs::File` 从 1.89 起有同名 inherent
// 方法）被判定为"引入了但从未通过普通方法调用语法用到"的假警告。
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;

const LOCK_FILE_NAME: &str = "arca.lock";

/// 获取排他锁失败——彼此可区分（I5）。
#[derive(Debug)]
pub enum LockError {
    /// 创建/打开 `.arca/locks/arca.lock`，或在其上取锁本身失败（权限、
    /// 磁盘满、`.arca/locks/` 路径上某一级类型不对等）。
    Io { path: String, reason: String },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Io { path, reason } => {
                write!(f, "获取 {path} 上的排他锁失败：{reason}")
            }
        }
    }
}

impl std::error::Error for LockError {}

fn io_err(path: &std::path::Path, e: io::Error) -> LockError {
    LockError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// 持有它即代表"当前进程独占这个存储根的写入临界区"。`Drop` 时关闭文件
/// 描述符——OS 级 `flock`/`LockFileEx` 随句柄关闭自动释放，不需要显式
/// `unlock`（进程异常退出、被杀同样会触发内核释放，见模块文档）。
pub struct RootLock {
    _file: File,
    path: PathBuf,
}

impl fmt::Debug for RootLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootLock")
            .field("path", &self.path)
            .finish()
    }
}

/// 阻塞获取 `<root>/.arca/locks/arca.lock` 上的排他锁——见模块文档。
///
/// `.arca/locks/` 目录若不存在会现场 `create_dir_all`：不要求调用方预先
/// 建好这层骨架目录——存量的、手工拼装的存储根 fixture（测试里到处都是）
/// 不会因为多了这把锁就集体失败，符合 I10「只向前迁移」。
pub fn acquire(root: &StorageRoot) -> Result<RootLock, LockError> {
    let locks_dir = root.path().join(layout::LOCKS_DIR);
    fs::create_dir_all(&locks_dir).map_err(|e| io_err(&locks_dir, e))?;

    let lock_path = locks_dir.join(LOCK_FILE_NAME);
    // 锁文件本身不承载任何有意义的内容，只是 `flock`/`LockFileEx` 的挂靠
    // 对象——显式 `.truncate(false)`：既有文件（正常情况，另一个进程已经
    // 建过它）不需要被清空重写，`clippy::suspicious_open_options` 要求
    // `create(true)` 必须显式声明截断意图，这里选"不截断"。
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| io_err(&lock_path, e))?;

    // 显式用完全限定语法调用 `fs4::FileExt::lock`，不写成 `file.lock()`——
    // Rust 1.89 给 `std::fs::File` 添加了同名 inherent 方法，方法解析里
    // inherent 方法总是优先于 trait 方法；写成 `file.lock()` 在够新的工具
    // 链上会悄悄改成调用 std 自己的实现（而不是本模块选定的 `fs4`），
    // 在 MSRV 1.85 工具链上又只有 `fs4` 的实现能用——同一行代码在两种
    // 工具链下调用两个不同的实现，且 `cargo clippy -D warnings` 的
    // `incompatible_msrv` lint 会直接拒绝编译（正确地指出 `File::lock`
    // 直到 1.89 才稳定，本仓库 MSRV 是 1.85）。完全限定语法把调用点钉死
    // 在 `fs4::FileExt` 这一个实现上，不随工具链版本漂移。
    fs4::FileExt::lock(&file).map_err(|e| io_err(&lock_path, e))?;

    Ok(RootLock {
        _file: file,
        path: lock_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::hub_layout::FormatJson;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn 造存储根(dir: &std::path::Path) {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        let format = FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    #[test]
    fn 首次获取成功且自动创建locks目录() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        assert!(!dir.path().join(".arca/locks").exists());

        let root = StorageRoot::open(dir.path(), None).unwrap();
        let _lock = acquire(&root).unwrap();
        assert!(dir.path().join(".arca/locks/arca.lock").exists());
    }

    #[test]
    fn 释放后可以再次获取() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = StorageRoot::open(dir.path(), None).unwrap();

        let lock1 = acquire(&root).unwrap();
        drop(lock1);
        let _lock2 = acquire(&root).unwrap();
    }

    /// 跨进程排他的核心复现：同一存储根，两个**独立的操作系统线程**
    /// （模拟两个并发进程/两个 `arcad` 请求处理线程）竞争同一把锁——第二个
    /// 必须真正阻塞到第一个释放为止，不能两个同时"持有"。用一个共享计数器
    /// 见证"临界区里任何时刻只有一个持有者"：如果锁不生效，两个线程会同时
    /// 把计数器推到 2；锁生效则计数器任何时刻都不超过 1。
    #[test]
    fn 两个线程竞争同一把锁时后者阻塞到前者释放为止() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root_path = dir.path().to_path_buf();

        let inside = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();

        let mut handles = Vec::new();
        for _ in 0..2 {
            let root_path = root_path.clone();
            let inside = inside.clone();
            let max_seen = max_seen.clone();
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let root = StorageRoot::open(&root_path, None).unwrap();
                let _lock = acquire(&root).unwrap();
                let now = inside.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
                inside.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                tx.send(()).unwrap();
            }));
        }
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            max_seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "临界区里同时出现了超过一个持有者——排他锁没有真正生效"
        );
    }
}
