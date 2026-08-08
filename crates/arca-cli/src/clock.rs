//! 当前时间的两种字符串形式：`Version.mtime`/`committed_at`/`format.json.created_at`
//! 用的 RFC 3339 全形式，与 `VersionId` 用的紧凑形式（`YYYYMMDDTHHMMSSZ`）。
//!
//! **只在 `arca-cli` 里读系统时钟**——`arca-core` 是 sans-io 的纯状态机
//! （见 `arca_core::reconcile` 的确定性模拟测试要求：`t_abs_us` 由调用方注入，
//! 内部绝不读时钟）；`arca-store` 的 `open_traced` 同理。CLI 是一次性进程，
//! 是"读时钟"这件事真正发生的地方。
//!
//! 不引入 `chrono`/`time` 等外部 crate——workspace 依赖选型（spec §11.3）刻意
//! 克制，日期换算用的是公开的 civil calendar 算法（Howard Hinnant，公有领域），
//! 只在 `u64` 上做整数运算，不含任何 `unsafe`。

use std::time::{SystemTime, UNIX_EPOCH};

/// 把 Unix 纪元以来的天数换算成 `(年, 月, 日)`（proleptic Gregorian，UTC）。
///
/// 算法来自 Howard Hinnant 的 `civil_from_days`
/// <http://howardhinnant.github.io/date_algorithms.html>（公有领域），
/// 在 [-0000-03-01, 0000-03-01) 到远未来的范围内都成立，这里只用到
/// 1970 年之后，远在有效范围内。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// 当前 UTC 时刻的 `(年,月,日,时,分,秒)`。`SystemTime::now()` 读不出结果时
/// （极罕见：系统时钟设置在 Unix 纪元之前）退化为纪元零点——好过 panic（I5：
/// 不确定的状态不去猜，但这里选择一个明确、可诊断的兜底值而不是让调用方
/// 处理一个"时钟读不出来"的新错误类型，因为下游用途（时间戳字段）本身
/// 就是尽力而为的元数据，不是判定正确性的关键路径）。
fn now_parts() -> (i64, u32, u32, u32, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    parts_from_unix_secs(secs)
}

fn parts_from_unix_secs(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = (time_of_day / 3600) as u32;
    let mm = ((time_of_day % 3600) / 60) as u32;
    let ss = (time_of_day % 60) as u32;
    (y, m, d, hh, mm, ss)
}

/// RFC 3339 全形式：`"2026-08-08T09:00:00Z"`——`format.json.created_at`、
/// `Version.mtime`/`committed_at` 用这个。
pub fn now_rfc3339() -> String {
    let (y, m, d, hh, mm, ss) = now_parts();
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// 紧凑形式：`"20260808T090000Z"`——`VersionId::new` 的 `timestamp` 参数要这个形状。
pub fn now_compact() -> String {
    let (y, m, d, hh, mm, ss) = now_parts();
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// 任意 Unix 纪元秒数 → RFC 3339 全形式。供 `sync.rs` 把文件的实际 `mtime`
/// （`fs::metadata().modified()`，不是"现在"）格式化成 `Version.mtime` 字段。
pub fn rfc3339_from_unix_secs(secs: i64) -> String {
    let (y, m, d, hh, mm, ss) = parts_from_unix_secs(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 纪元零点对应_1970_01_01() {
        assert_eq!(parts_from_unix_secs(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn 已知时间戳换算正确() {
        // 2024-01-01T00:00:00Z == 1704067200（用 `date -u -d @1704067200` 交叉验证）。
        assert_eq!(parts_from_unix_secs(1_704_067_200), (2024, 1, 1, 0, 0, 0));
        // 2026-08-08T12:34:56Z == 1786192496。
        assert_eq!(
            parts_from_unix_secs(1_786_192_496),
            (2026, 8, 8, 12, 34, 56)
        );
    }

    #[test]
    fn rfc3339_与紧凑形式对同一时刻只是格式不同() {
        // 两次调用之间系统时钟可能跨秒，只断言格式本身，不断言两次调用相等。
        let rfc = now_rfc3339();
        let compact = now_compact();
        assert_eq!(rfc.len(), 20, "应为 YYYY-MM-DDTHH:MM:SSZ：{rfc:?}");
        assert!(rfc.ends_with('Z'));
        assert_eq!(compact.len(), 16, "应为 YYYYMMDDTHHMMSSZ：{compact:?}");
        assert!(compact.ends_with('Z'));
        assert!(!compact.contains(['-', ':']));
    }

    #[test]
    fn 紧凑形式满足_version_id_的形状要求() {
        let compact = now_compact();
        assert!(
            arca_format::model::VersionId::new(&compact, &"0".repeat(32)).is_ok(),
            "now_compact 产出的形状必须能喂给 VersionId::new：{compact:?}"
        );
    }

    #[test]
    fn 跨月边界换算正确() {
        // 2026-03-01T00:00:00Z == 1772323200（2026 非闰年，2 月只有 28 天）。
        assert_eq!(parts_from_unix_secs(1_772_323_200), (2026, 3, 1, 0, 0, 0));
    }
}
