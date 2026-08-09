//! `items/<xx>/<item_id>.jsonl`：append-only 版本链（FORMAT.md §7.1）。
//!
//! 一行一个版本记录，按提交顺序追加。hub 上的链是线性的——
//! CAS 失败产生的分叉以冲突副本（新身份）落地，不进链（spec §4.1）。

use crate::error::FormatError;
use crate::model::{Actor, ItemId, Version, VersionId};
use arca_chunk::hash::ContentHash;
use serde::{Deserialize, Serialize};

const RECORD_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u32,
    version_id: String,
    item_id: String,
    parent: Option<String>,
    hash: String,
    size: u64,
    mtime: String,
    actor: Actor,
    committed_at: String,
    /// FORMAT.md §7.1：可选追加字段。`skip_serializing_if` 保证**没有块清单
    /// 时这个键根本不出现**——而不是写成 `null` 或 `[]`。老读者看不见未知键，
    /// 新读者能把「缺省」与「空数组」区分开（见 `Version::chunks` 的文档）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chunks: Option<Vec<String>>,
}

/// 序列化一条版本记录为单行 JSON。
///
/// 返回 `Result` 而非把序列化失败静默吞成空字符串（brief 判断点、Task 6 先例：
/// `dataset::DatasetConfig::to_toml` / `gitarca::Registry::to_toml`）。`items` 是
/// append-only 版本链，若这里用 `unwrap_or_default()`，一次序列化失败会把空字符串
/// 当作一行追加进链——那正是「中间行损坏」的样子，且是我们自己制造的、本可避免的
/// 损坏。当前 `Wire` 全是标量/字符串字段，序列化实际不可能失败（`Err` 分支不可达），
/// 但保留 `Result` 签名为未来加字段留防线，与 Task 6 的判断一致。
pub fn to_line(version: &Version) -> Result<String, FormatError> {
    let wire = Wire {
        v: RECORD_VERSION,
        version_id: version.version_id.as_str().to_string(),
        item_id: version.item_id.to_hex(),
        parent: version.parent.as_ref().map(|p| p.as_str().to_string()),
        hash: version.hash.to_text(),
        size: version.size,
        mtime: version.mtime.clone(),
        actor: version.actor.clone(),
        committed_at: version.committed_at.clone(),
        chunks: version
            .chunks
            .as_ref()
            .map(|cs| cs.iter().map(|c| c.to_hex()).collect()),
    };
    serde_json::to_string(&wire).map_err(|e| FormatError::Malformed {
        line: 0,
        reason: format!("版本记录序列化失败：{e}"),
    })
}

pub fn parse_line(line: &str, line_no: usize) -> Result<Version, FormatError> {
    let wire: Wire = serde_json::from_str(line).map_err(|e| FormatError::Malformed {
        line: line_no,
        reason: format!("JSON 解析失败：{e}"),
    })?;
    if wire.v > RECORD_VERSION {
        return Err(FormatError::UnsupportedVersion {
            found: wire.v,
            max: RECORD_VERSION,
        });
    }
    let bad = |reason: String| FormatError::Malformed {
        line: line_no,
        reason,
    };
    Ok(Version {
        version_id: parse_version_id(&wire.version_id)
            .map_err(|e| bad(format!("version_id {:?} 不合法：{e}", wire.version_id)))?,
        item_id: ItemId::parse(&wire.item_id)
            .map_err(|e| bad(format!("item_id {:?} 不合法：{e}", wire.item_id)))?,
        parent: match wire.parent {
            Some(ref p) => {
                Some(parse_version_id(p).map_err(|e| bad(format!("parent {p:?} 不合法：{e}")))?)
            }
            None => None,
        },
        hash: ContentHash::parse(&wire.hash).map_err(|e| bad(format!("哈希不合规：{e}")))?,
        size: wire.size,
        mtime: wire.mtime,
        actor: wire.actor,
        committed_at: wire.committed_at,
        chunks: match wire.chunks {
            None => None,
            Some(list) => Some(
                list.iter()
                    .map(|h| {
                        // 块名是**不带 `blake3:` 前缀**的裸十六进制
                        // （FORMAT.md §8 的文件名形态），这里补上前缀再解析，
                        // 保证与 `ContentHash::parse` 的唯一入口一致。
                        ContentHash::parse(&format!("blake3:{h}"))
                            .map_err(|e| bad(format!("chunks 里的 {h:?} 不合规：{e}")))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        },
    })
}

fn parse_version_id(text: &str) -> Result<VersionId, FormatError> {
    let (timestamp, random) = text.split_once('-').ok_or(FormatError::Malformed {
        line: 0,
        reason: format!("version_id {text:?} 缺少分隔符"),
    })?;
    VersionId::new(timestamp, random)
}

/// 解析整条版本链。
///
/// 处置纪律（继承 lazync STORAGE.md）：**末行不完整 → 截断到最后一个完整行**
/// （崩溃时的正常残留）；**中间行损坏 → 失败**（真损坏，绝不跳过、绝不猜测，I5）。
///
/// 与 brief Step 3 参考实现的一处刻意偏离：参考实现在逐行解析时对空行 `continue`
/// 跳过。本实现不做这个特例——`to_line` 已保证不会产出空行，append 写入又是整行
/// 原子追加（FORMAT.md §1），所以边界内出现的空行只可能是真损坏（例如被截断的中间
/// 写入、或磁盘错误），交给 `parse_line` 对空字符串正常报错即可，不特殊放行。
/// 这与「边界之内任何一行解析失败都必须返回 Err，绝不 continue 跳过」的纪律一致。
pub fn parse_chain(text: &str) -> Result<Vec<Version>, FormatError> {
    let mut versions: Vec<Version> = Vec::new();
    let complete_upto = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let complete = &text[..complete_upto];

    for (zero_based, raw) in complete.lines().enumerate() {
        let line_no = zero_based + 1;
        let line = raw.trim_end_matches('\r');
        let version = parse_line(line, line_no)?;
        match (&version.parent, versions.last()) {
            (None, None) => {}
            (Some(parent), Some(prev)) if *parent == prev.version_id => {}
            _ => {
                return Err(FormatError::Malformed {
                    line: line_no,
                    reason: "版本链断裂：parent 不指向上一条记录".to_string(),
                })
            }
        }
        versions.push(version);
    }
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use crate::model::{Actor, ItemId, VersionId};
    use arca_chunk::hash::ContentHash;

    fn 样例版本(parent: Option<VersionId>) -> crate::model::Version {
        crate::model::Version {
            version_id: VersionId::new("20260804T102302Z", &"0".repeat(32)).unwrap(),
            item_id: ItemId::parse("3f2a000000000000000000000000beef").unwrap(),
            parent,
            hash: ContentHash::from_bytes(b"content"),
            size: 2411008,
            mtime: "2026-08-04T10:22:31Z".into(),
            actor: Actor {
                account: "bruce".into(),
                device: "mac".into(),
                session: "s1".into(),
            },
            committed_at: "2026-08-04T10:23:05Z".into(),
            chunks: None,
        }
    }

    /// **本文件里最重要的一条。** `chunks` 缺省与空数组意义不同，
    /// 且**都要能在往返之后保持原样**。
    ///
    /// 把两者合并（比如用 `Vec` 而不是 `Option<Vec>`）会让本字段落地之前
    /// 写下的**老记录被读成「零字节」**——而 `arca checkout` 会拿着那个
    /// 结论去用一个空文件覆盖用户的历史版本。
    #[test]
    fn chunks缺省与空数组不可混淆() {
        // 缺省：键根本不出现在 JSON 里。
        let 无 = 样例版本(None);
        let line = super::to_line(&无).unwrap();
        assert!(
            !line.contains("chunks"),
            "没有块清单时 chunks 键不该出现（而不是写成 null 或 []）：{line}"
        );
        assert_eq!(super::parse_line(&line, 1).unwrap().chunks, None);

        // 空数组：留存过，内容是零字节——一个合法的空文件。
        let mut 空 = 样例版本(None);
        空.chunks = Some(vec![]);
        let line = super::to_line(&空).unwrap();
        assert!(line.contains("\"chunks\":[]"), "{line}");
        assert_eq!(super::parse_line(&line, 1).unwrap().chunks, Some(vec![]));

        // 有块：裸十六进制，**不带 `blake3:` 前缀**（FORMAT.md §8 的文件名形态）。
        let mut 有 = 样例版本(None);
        let h = ContentHash::from_bytes(b"chunk-one");
        有.chunks = Some(vec![h]);
        let line = super::to_line(&有).unwrap();
        assert!(line.contains(&h.to_hex()), "{line}");
        assert!(
            !line.contains(&format!("blake3:{}", h.to_hex())),
            "块名是裸十六进制，不带前缀：{line}"
        );
        assert_eq!(super::parse_line(&line, 1).unwrap().chunks, Some(vec![h]));
    }

    /// 老记录（没有 `chunks` 键）必须能被新读者读出来，且读成 `None`——
    /// I10「只向前迁移」。
    #[test]
    fn 没有chunks键的老记录仍可解析且读成缺省() {
        let 老 = r#"{"v":1,"version_id":"20260804T102302Z-00000000000000000000000000000000","item_id":"3f2a000000000000000000000000beef","parent":null,"hash":"blake3:ed7002b439e9ac845f22357d822bac1444730fbdb6016d3ec9432297b9ec9f73","size":7,"mtime":"2026-08-04T10:22:31Z","actor":{"account":"bruce","device":"mac","session":"s1"},"committed_at":"2026-08-04T10:23:05Z"}"#;
        let v = super::parse_line(老, 1).expect("老记录必须仍可解析");
        assert_eq!(v.chunks, None, "缺省必须读成 None，绝不是 Some(vec![])");
    }

    /// `chunks` 里出现不合规的哈希 → **拒绝整行**，不是跳过那一个
    /// （I5：一条自称有块清单却给不出合法块名的记录是歧义状态）。
    #[test]
    fn chunks里的畸形哈希导致整行被拒() {
        let 坏 = r#"{"v":1,"version_id":"20260804T102302Z-00000000000000000000000000000000","item_id":"3f2a000000000000000000000000beef","parent":null,"hash":"blake3:ed7002b439e9ac845f22357d822bac1444730fbdb6016d3ec9432297b9ec9f73","size":7,"mtime":"m","actor":{"account":"a","device":"d","session":"s"},"committed_at":"c","chunks":["不是十六进制"]}"#;
        assert!(super::parse_line(坏, 1).is_err());
    }

    #[test]
    fn 版本记录往返一致() {
        let version = 样例版本(None);
        let line = super::to_line(&version).unwrap();
        assert!(!line.contains('\n'), "记录内不得含裸换行");
        assert_eq!(super::parse_line(&line, 1).unwrap(), version);
    }

    #[test]
    fn 首版的_parent_为_null() {
        let line = super::to_line(&样例版本(None)).unwrap();
        assert!(line.contains("\"parent\":null"));
    }

    #[test]
    fn 解析版本链并校验线性() {
        let v1 = 样例版本(None);
        let v2 = crate::model::Version {
            version_id: VersionId::new("20260804T102400Z", &"1".repeat(32)).unwrap(),
            parent: Some(v1.version_id.clone()),
            ..样例版本(None)
        };
        let text = format!(
            "{}\n{}\n",
            super::to_line(&v1).unwrap(),
            super::to_line(&v2).unwrap()
        );
        let chain = super::parse_chain(&text).unwrap();
        assert_eq!(chain.len(), 2);
    }

    #[test]
    fn 拒绝断裂的版本链() {
        let v1 = 样例版本(None);
        let 孤儿 = crate::model::Version {
            version_id: VersionId::new("20260804T102400Z", &"1".repeat(32)).unwrap(),
            parent: Some(
                VersionId::new("20260804T999999Z", &"9".repeat(32))
                    .unwrap_or_else(|_| v1.version_id.clone()),
            ),
            ..样例版本(None)
        };
        let text = format!(
            "{}\n{}\n",
            super::to_line(&v1).unwrap(),
            super::to_line(&孤儿).unwrap()
        );
        // parent 不指向上一行 → 链断裂，必须报错而非跳过
        assert!(super::parse_chain(&text).is_err());
    }

    #[test]
    fn 末行不完整时截断而非报错() {
        let v1 = 样例版本(None);
        let text = format!("{}\n{{\"v\":1,\"version_", super::to_line(&v1).unwrap());
        let chain = super::parse_chain(&text).expect("末行不完整应截断到最后完整行");
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn 中间行损坏则失败() {
        let v1 = 样例版本(None);
        let text = format!(
            "{}\n损坏的行\n{}\n",
            super::to_line(&v1).unwrap(),
            super::to_line(&v1).unwrap()
        );
        assert!(
            super::parse_chain(&text).is_err(),
            "中间行损坏必须失败，不得跳过"
        );
    }

    #[test]
    fn 拒绝首条记录_parent_非_none() {
        let v1 = 样例版本(Some(
            VersionId::new("20260804T102302Z", &"0".repeat(32)).unwrap(),
        ));
        let text = format!("{}\n", super::to_line(&v1).unwrap());
        assert!(
            super::parse_chain(&text).is_err(),
            "首条记录的 parent 必须为 null"
        );
    }

    #[test]
    fn 拒绝_parent_指向自身的记录() {
        let mut v1 = 样例版本(None);
        v1.parent = Some(v1.version_id.clone());
        let text = format!("{}\n", super::to_line(&v1).unwrap());
        assert!(
            super::parse_chain(&text).is_err(),
            "parent 指向自身也是链断裂的一种，必须拒绝"
        );
    }

    #[test]
    fn 换行字段被转义而不是裸换行() {
        let mut version = 样例版本(None);
        version.actor.device = "mac\nstudio".to_string();
        let line = super::to_line(&version).unwrap();
        assert!(!line.contains('\n'), "应转义而不是裸换行：{line}");
        assert!(line.contains("\\n"), "应含转义后的 \\n：{line}");
    }
}
