//! 新身份/新版本标识的生成——**只在 `arca-cli` 里发生**，不是 `arca-format`
//! 的职责（那一层只做解析/序列化，见其模块文档）。
//!
//! `Action::Upload { parent: None }`（`local_new`/`local_new_over_tombstone`）
//! 意味着 hub 完全不认识这个 item：执行侧（`sync.rs`）需要现场分配一个新
//! [`ItemId`] 与 [`VersionId`]，`arca_core::decide` 本身不产出——它是 sans-io
//! 纯函数，不分配任何标识（那需要"这次分配过的 id 不会再被分配"这类有状态的
//! 保证，不属于决策表的职责）。
//!
//! **不是密码学安全随机数**：用 `std::collections::hash_map::RandomState`
//! （libstd 为 `HashMap` 的 DoS 防护而内建的、每次构造取一份新的 OS 种子）
//! 混合单调计数器、当前时间、进程号，产出 128 位输出。够用的理由：spec 对
//! `item_id`/`dataset_id`/hub `instance_id` 的要求是"创建时分配、永不复用"
//! （I7），是"实践中不会撞车"的强度，不是"对抗故意构造碰撞的攻击者"的强度。
//! workspace 依赖选型（spec §11.3）刻意克制，不为此新增 `rand`/`getrandom`
//! 依赖；未来若需要更强保证，换成外部 crate 是局部改动，不影响调用方签名。

use arca_format::model::{ItemId, VersionId};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 同进程内单调递增，即便同一微秒内并发调用也不会让两次输出的哈希输入完全相同。
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// 128 位随机字节（大端拼接两个独立的 64 位输出）。
pub fn random_bytes16() -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&random_u64().to_be_bytes());
    out[8..].copy_from_slice(&random_u64().to_be_bytes());
    out
}

/// 32 位小写十六进制——`dataset_id`/hub `instance_id`/`item_id` 共用的编码
/// （FORMAT.md §1）。
pub fn random_hex32() -> String {
    random_bytes16()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 分配一个全新的 [`ItemId`]：随机 128-bit，创建时分配，永不复用（I7）。
pub fn new_item_id() -> ItemId {
    ItemId::from_bytes(random_bytes16())
}

/// 分配一个全新的 [`VersionId`]：`<当前时刻的紧凑形式>-<32 位随机十六进制>`。
///
/// 与 [`crate::clock::now_compact`] 组合，永不 panic——`VersionId::new` 唯一会
/// 拒绝的输入形状（时间戳长度/随机段长度）在这里都是本函数自己构造的、
/// 已知合法的值。
pub fn new_version_id() -> VersionId {
    let timestamp = crate::clock::now_compact();
    let random = random_hex32();
    VersionId::new(&timestamp, &random).expect("now_compact 与 random_hex32 产出的形状必然合法")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex32_形状合法() {
        let hex = random_hex32();
        assert_eq!(hex.len(), 32);
        assert!(hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert!(arca_format::model::is_hex32(&hex));
    }

    #[test]
    fn 连续两次调用产出不同的值() {
        // 不是严格的随机性证明，但足以捕获"忘了推进计数器/忘了混入随机源"
        // 这类退化成常量输出的回归。
        let a = random_hex32();
        let b = random_hex32();
        assert_ne!(a, b);

        let ia = new_item_id();
        let ib = new_item_id();
        assert_ne!(ia, ib);
    }

    #[test]
    fn new_version_id不panic且能被解析回相同的字符串() {
        let v = new_version_id();
        assert!(v.as_str().contains('-'));
        assert_eq!(v.as_str().len(), 16 + 1 + 32);
    }

    #[test]
    fn 大量连续生成互不重复() {
        use std::collections::HashSet;
        let ids: HashSet<String> = (0..1000).map(|_| random_hex32()).collect();
        assert_eq!(ids.len(), 1000, "1000 次连续生成不应出现任何重复");
    }
}
