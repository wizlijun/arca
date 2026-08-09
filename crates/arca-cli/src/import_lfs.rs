//! Git LFS 迁入桥（M5c，spec §8、§12.3 的 M5 行）。
//!
//! 把一个既有 LFS 仓库里的**指针文件**换回真实内容，交给 `arca adopt` 接管。
//!
//! # 风险不在解析，在覆盖
//!
//! LFS 指针只有三行，解析不会出错。真正会出事的是第二步：**用一份字节覆盖
//! 掉指针文件**。如果那份字节不是指针声称的那一份（对象缺失、被截断、
//! 被换过），覆盖之后：
//!
//! - **指针没了**——`oid` 是找回原内容的唯一线索，它就在被覆盖的那几行里；
//! - **内容是错的**——而且看起来像「迁移成功」。
//!
//! 所以核心纪律：**先校验 SHA-256 等于 `oid`，再写**；写用 tmp → rename；
//! 任何一步不确定就跳过这个文件并记进审计报告，**绝不中止整批**——
//! 一个对象缺失不该让另外 999 个迁不进来。
//!
//! 具体到代码：**校验通过之前，目标文件的字节一个都不能动。** 这类代码
//! 很容易写成「先打开目标文件准备写、再校验」，那样一个 `truncate` 就已经
//! 把指针毁了。
//!
//! # 绝不调用 `git lfs`
//!
//! 对象就在 `.git/lfs/objects/<xx>/<yy>/<oid>` 里，布局是固定的。依赖外部
//! 二进制会让「一键迁入」变成「先装 git-lfs」——而迁入是获客的第一入口
//! （spec §8），多一步就少一批人。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 指针文件的体积上限。真指针只有百来字节；超过这个就直接不当指针看，
/// 免得把一个大文本文件整个读进来做匹配。
const MAX_POINTER_BYTES: u64 = 4096;

const VERSION_LINE: &str = "version https://git-lfs.github.com/spec/v1";

/// 一份解析出来的 LFS 指针。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsPointer {
    /// 小写十六进制的 SHA-256（不含 `sha256:` 前缀）。
    pub oid: String,
    pub size: u64,
}

impl LfsPointer {
    /// 严格解析。**不是指针就返回 `None`，不是错误**——绝大多数文件本来
    /// 就不是指针，把「这不是指针」当错误会让扫描寸步难行。
    ///
    /// 只认 `sha256`：LFS spec 目前也只有这一种，遇到别的哈希算法宁可不认
    /// （返回 `None`）也不猜——猜错的后果是拿一个算不出来的 oid 去比对，
    /// 然后把所有文件都判成「哈希不符」。
    pub fn parse(text: &str) -> Option<Self> {
        let mut version_seen = false;
        let mut oid = None;
        let mut size = None;
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            if line == VERSION_LINE {
                version_seen = true;
            } else if let Some(rest) = line.strip_prefix("oid sha256:") {
                if rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit()) {
                    // LFS spec 要求小写；大写十六进制的指针是畸形的，不认。
                    if rest.bytes().any(|b| b.is_ascii_uppercase()) {
                        return None;
                    }
                    oid = Some(rest.to_string());
                } else {
                    return None;
                }
            } else if let Some(rest) = line.strip_prefix("size ") {
                size = rest.trim().parse::<u64>().ok();
                size?;
            }
            // 其它行（未来的扩展键）忽略——LFS spec 允许。
        }
        match (version_seen, oid, size) {
            (true, Some(oid), Some(size)) => Some(LfsPointer { oid, size }),
            _ => None,
        }
    }

    /// 这个 oid 对应的对象在 `.git/lfs/objects/` 下的路径。
    pub fn object_path(&self, git_dir: &Path) -> PathBuf {
        git_dir
            .join("lfs")
            .join("objects")
            .join(&self.oid[0..2])
            .join(&self.oid[2..4])
            .join(&self.oid)
    }
}

/// 一个文件的迁入结论。**每一个被扫到的指针都会有一条**——
/// spec §8：「逐文件用厂商提供的校验和验证完整性，**出具迁移审计报告**」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 已把指针换成真实内容。
    Migrated { size: u64 },
    /// 校验通过、但这次是 dry-run，没有动它。
    Ready { size: u64 },
    /// 跳过。**指针文件原封不动。**
    Skipped(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `.git/lfs/objects/` 下没有这个对象——多半是没跑过 `git lfs pull`。
    ObjectMissing {
        oid: String,
    },
    /// 对象在，但 SHA-256 与指针声称的 `oid` 不符。**这是最危险的一种**：
    /// 如果不校验就覆盖，用户会得到一份内容错误却看起来迁移成功的文件。
    HashMismatch {
        expected: String,
        actual: String,
    },
    /// 对象在、哈希对，但字节数与指针声称的 `size` 不符。
    ///
    /// 哈希对而 size 不对在数学上不可能——所以这条命中时说明**指针本身
    /// 损坏了**（有人手改过它）。两者都验正是为了让这种情况能被认出来，
    /// 而不是默默按哈希放行。
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    Io {
        reason: String,
    },
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::ObjectMissing { oid } => write!(
                f,
                "`.git/lfs/objects/` 下没有 {oid} 这个对象——多半是这个克隆还没跑过 \
                 `git lfs pull`。先把对象拉全再重跑迁入"
            ),
            SkipReason::HashMismatch { expected, actual } => write!(
                f,
                "对象的 SHA-256 是 {actual}，而指针声称 {expected}——**这份内容不是\
                 指针指的那一份**。已跳过、指针原封不动：覆盖它会同时毁掉指针\
                 （oid 是找回原内容的唯一线索）并留下一份看起来迁移成功的错误文件"
            ),
            SkipReason::SizeMismatch { expected, actual } => write!(
                f,
                "对象是 {actual} 字节，而指针声称 {expected} 字节——哈希对得上而\
                 字节数对不上在数学上不可能，说明**指针本身被改过**。已跳过"
            ),
            SkipReason::Io { reason } => write!(f, "读取失败：{reason}"),
        }
    }
}

/// `.gitattributes` 里一条把文件交给 LFS filter 的规则。
///
/// # 为什么必须处理它
///
/// 只把指针换回内容是**不够的**：只要 `.gitattributes` 里还留着
/// `*.png filter=lfs diff=lfs merge=lfs`，用户下一次 `git add` 就会让 git 的
/// clean filter 把文件**重新变回指针**——整个迁入在下一次提交时被静默撤销，
/// 而且没有任何征兆。
///
/// 这也正是 spec §1.2 不做 clean/smudge filter 的理由的反面注脚：
/// **寄生 git 管道正是 LFS 的失败根源**，而它的粘性就体现在这里——
/// 迁出去比迁进来难。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsAttrRule {
    /// `.gitattributes` 相对仓库根的路径。
    pub file: String,
    /// 1 起的行号。
    pub line_no: usize,
    pub line: String,
}

/// 一次迁入的完整结果。
#[derive(Debug, Default)]
pub struct Report {
    /// (vault 内相对路径, 结论)，按路径排序（确定性）。
    pub files: Vec<(String, Outcome)>,
    /// 仍然把文件交给 LFS filter 的 `.gitattributes` 规则。
    /// **非空即意味着迁入会在下次 `git add` 时被撤销。**
    pub lfs_attrs: Vec<LfsAttrRule>,
    /// `apply` 且真的注释掉了多少条。
    pub attrs_disabled: usize,
}

impl Report {
    pub fn migrated(&self) -> usize {
        self.files
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Migrated { .. }))
            .count()
    }
    pub fn ready(&self) -> usize {
        self.files
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Ready { .. }))
            .count()
    }
    pub fn skipped(&self) -> usize {
        self.files
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Skipped(_)))
            .count()
    }
}

/// 扫描 `root` 下的 LFS 指针并（在 `apply` 为真时）换成真实内容。
///
/// `git_dir` 是仓库的 `.git` 目录。**跳过 `.git/` 与 `.arca/`**。
///
/// 一个文件出问题**不中止整批**——它只是多一条 `Skipped`。理由很直接：
/// 一个对象缺失不该让另外 999 个迁不进来，而用户真正需要的是一份说清楚
/// 「哪些好了、哪些没好、为什么」的报告。
pub fn import(root: &Path, git_dir: &Path, apply: bool) -> Report {
    let mut report = Report::default();
    walk(root, root, git_dir, apply, &mut report);
    report.files.sort_by(|a, b| a.0.cmp(&b.0));
    scan_attrs(root, root, &mut report);
    report
        .lfs_attrs
        .sort_by(|a, b| (&a.file, a.line_no).cmp(&(&b.file, b.line_no)));
    if apply {
        report.attrs_disabled = disable_attrs(root, &report.lfs_attrs);
    }
    report
}

/// 扫描所有 `.gitattributes`，找出仍把文件交给 LFS filter 的规则。
fn scan_attrs(root: &Path, dir: &Path, report: &mut Report) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let path = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name == ".git" || name == ".arca" {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            scan_attrs(root, &path, report);
        } else if name == ".gitattributes" {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            for (i, line) in text.lines().enumerate() {
                if is_lfs_attr(line) {
                    report.lfs_attrs.push(LfsAttrRule {
                        file: rel.clone(),
                        line_no: i + 1,
                        line: line.to_string(),
                    });
                }
            }
        }
    }
}

/// 这一行是不是把文件交给 LFS clean/smudge filter。
///
/// 判据是 **`filter=lfs` 这个属性**，不是「这行里出现了 lfs 三个字母」——
/// `diff=lfs`/`merge=lfs` 单独出现不会把文件变成指针（它们只影响 diff/merge
/// 的呈现），而一条注释掉的行更不该被算上。
fn is_lfs_attr(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    line.split_whitespace().any(|tok| tok == "filter=lfs")
}

/// 把这些行**注释掉**（而不是删掉）。
///
/// 注释而非删除是有意的：`.gitattributes` 是用户的配置，删掉一行等于
/// 销毁信息——而注释保留了「这里原来配过什么」，用户能一眼看懂发生了什么、
/// 也能改回去。与 I3「物理销毁只经显式操作」同一条精神。
fn disable_attrs(root: &Path, rules: &[LfsAttrRule]) -> usize {
    use std::collections::BTreeMap;
    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for r in rules {
        by_file.entry(&r.file).or_default().push(r.line_no);
    }
    let mut done = 0;
    for (file, line_nos) in by_file {
        let path = root.join(file);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut out = String::with_capacity(text.len() + line_nos.len() * 40);
        for (i, line) in text.lines().enumerate() {
            if line_nos.contains(&(i + 1)) {
                out.push_str("# 已由 arca import lfs 注释：这条规则会让 git 在下次 add 时把文件变回 LFS 指针\n");
                out.push_str("# ");
                out.push_str(line);
                out.push('\n');
                done += 1;
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        let tmp = path.with_extension("gitattributes-arca-tmp");
        if fs::write(&tmp, &out).is_ok() && fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
    done
}

fn walk(root: &Path, dir: &Path, git_dir: &Path, apply: bool, report: &mut Report) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let path = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name == ".git" || name == ".arca" {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(root, &path, git_dir, apply, report);
            continue;
        }
        // 符号链接不跟随：一个指向仓库外的链接不该被就地改写（I6）。
        if !ft.is_file() {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if meta.len() > MAX_POINTER_BYTES {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue; // 二进制文件读不成 UTF-8——本来就不是指针。
        };
        let Some(ptr) = LfsPointer::parse(&text) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        report
            .files
            .push((rel, migrate_one(&path, &ptr, git_dir, apply)));
    }
}

/// 迁一个文件。**校验通过之前，目标文件的字节一个都不动。**
fn migrate_one(path: &Path, ptr: &LfsPointer, git_dir: &Path, apply: bool) -> Outcome {
    let object = ptr.object_path(git_dir);
    let bytes = match fs::read(&object) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Outcome::Skipped(SkipReason::ObjectMissing {
                oid: ptr.oid.clone(),
            })
        }
        Err(e) => {
            return Outcome::Skipped(SkipReason::Io {
                reason: format!("{}：{e}", object.display()),
            })
        }
    };

    // ① 哈希。这是「厂商提供的校验和」——LFS 的 oid 就是内容的 SHA-256。
    let actual = hex(&Sha256::digest(&bytes));
    if actual != ptr.oid {
        return Outcome::Skipped(SkipReason::HashMismatch {
            expected: ptr.oid.clone(),
            actual,
        });
    }
    // ② 字节数。哈希对而 size 不对在数学上不可能，命中即说明指针被改过。
    if bytes.len() as u64 != ptr.size {
        return Outcome::Skipped(SkipReason::SizeMismatch {
            expected: ptr.size,
            actual: bytes.len() as u64,
        });
    }

    if !apply {
        return Outcome::Ready { size: ptr.size };
    }

    // ③ 到这里才允许写。tmp → rename：中途崩溃要么留下旧指针、要么留下
    //    完整内容，绝不留下半份文件。
    let tmp = path.with_extension("arca-lfs-tmp");
    if let Err(e) = fs::write(&tmp, &bytes) {
        return Outcome::Skipped(SkipReason::Io {
            reason: format!("{}：{e}", tmp.display()),
        });
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Outcome::Skipped(SkipReason::Io {
            reason: format!("{}：{e}", path.display()),
        });
    }
    Outcome::Migrated { size: ptr.size }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 指针(oid: &str, size: u64) -> String {
        format!("{VERSION_LINE}\noid sha256:{oid}\nsize {size}\n")
    }

    /// 造一个「仓库」：`root` 下放指针文件，`.git/lfs/objects/` 下放对象。
    /// **不需要装 git-lfs**——布局是固定的。
    fn 造仓库(files: &[(&str, &[u8], bool)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let git = d.path().join(".git");
        for (name, content, 放对象) in files {
            let oid = hex(&Sha256::digest(content));
            let target = d.path().join(name);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, 指针(&oid, content.len() as u64)).unwrap();
            if *放对象 {
                let obj = d
                    .path()
                    .join(".git/lfs/objects")
                    .join(&oid[0..2])
                    .join(&oid[2..4]);
                fs::create_dir_all(&obj).unwrap();
                fs::write(obj.join(&oid), content).unwrap();
            }
        }
        fs::create_dir_all(&git).unwrap();
        d
    }

    // ---------------- 解析 ----------------

    #[test]
    fn 解析标准指针() {
        let oid = "a".repeat(64);
        let p = LfsPointer::parse(&指针(&oid, 12345)).unwrap();
        assert_eq!(p.oid, oid);
        assert_eq!(p.size, 12345);
    }

    #[test]
    fn 不是指针的文件返回none而不是错误() {
        for text in [
            "",
            "hello world",
            "# 一篇笔记\n内容",
            "version https://git-lfs.github.com/spec/v1\n", // 缺 oid/size
            "oid sha256:abc\nsize 1\n",                     // 缺 version
        ] {
            assert!(LfsPointer::parse(text).is_none(), "{text:?}");
        }
    }

    /// oid 畸形（长度不对、非十六进制、大写）一律**不认**——宁可不认也不猜。
    /// 猜错的后果是拿一个算不出来的 oid 去比对，然后把所有文件判成哈希不符。
    #[test]
    fn 畸形oid不被认作指针() {
        for oid in [
            "abc",           // 太短
            &"a".repeat(63), // 差一位
            &"z".repeat(64), // 非十六进制
            &"A".repeat(64), // 大写（LFS spec 要求小写）
        ] {
            assert!(
                LfsPointer::parse(&指针(oid, 1)).is_none(),
                "oid {oid:?} 不该被认作合法指针"
            );
        }
    }

    #[test]
    fn 未知扩展行被忽略而不是拒绝() {
        let oid = "b".repeat(64);
        let text = format!("{VERSION_LINE}\next-0-sha256 xxx\noid sha256:{oid}\nsize 7\n");
        assert!(LfsPointer::parse(&text).is_some(), "LFS spec 允许扩展键");
    }

    #[test]
    fn 对象路径按oid分两级() {
        let p = LfsPointer {
            oid: "abcdef0123456789".to_string() + &"0".repeat(48),
            size: 1,
        };
        let path = p.object_path(Path::new("/r/.git"));
        assert!(path.ends_with(&p.oid));
        assert!(path.to_string_lossy().contains("/lfs/objects/ab/cd/"));
    }

    // ---------------- 迁入 ----------------

    #[test]
    fn 正常情况把指针换成真实内容() {
        let d = 造仓库(&[("a.png", b"REAL-IMAGE-BYTES", true)]);
        let r = import(d.path(), &d.path().join(".git"), true);
        assert_eq!(r.migrated(), 1, "{r:?}");
        assert_eq!(
            fs::read(d.path().join("a.png")).unwrap(),
            b"REAL-IMAGE-BYTES"
        );
    }

    /// 默认是 dry-run：**看得见清单，但一个字节都没动**。
    #[test]
    fn dry_run不改动任何文件() {
        let d = 造仓库(&[("a.png", b"REAL", true)]);
        let 原文 = fs::read_to_string(d.path().join("a.png")).unwrap();
        let r = import(d.path(), &d.path().join(".git"), false);
        assert_eq!(r.ready(), 1, "{r:?}");
        assert_eq!(
            fs::read_to_string(d.path().join("a.png")).unwrap(),
            原文,
            "dry-run 绝不能动文件"
        );
    }

    /// 对象缺失（没跑过 `git lfs pull`）→ 跳过，**指针原封不动**。
    #[test]
    fn 对象缺失时跳过且指针原封不动() {
        let d = 造仓库(&[("a.png", b"REAL", false)]);
        let 原文 = fs::read_to_string(d.path().join("a.png")).unwrap();
        let r = import(d.path(), &d.path().join(".git"), true);
        assert_eq!(r.skipped(), 1);
        assert!(matches!(
            r.files[0].1,
            Outcome::Skipped(SkipReason::ObjectMissing { .. })
        ));
        assert_eq!(fs::read_to_string(d.path().join("a.png")).unwrap(), 原文);
    }

    /// **本文件里最重要的一条。** 对象内容与 `oid` 不符时绝不覆盖——
    /// 覆盖会同时毁掉指针（oid 是找回原内容的唯一线索）并留下一份
    /// 看起来迁移成功的错误文件。
    #[test]
    fn 哈希不符时绝不覆盖指针() {
        let d = 造仓库(&[("a.png", b"REAL", true)]);
        // 把对象内容偷偷换掉（模拟传输损坏 / 被人动过）。
        let oid = hex(&Sha256::digest(b"REAL"));
        let obj = d
            .path()
            .join(".git/lfs/objects")
            .join(&oid[0..2])
            .join(&oid[2..4])
            .join(&oid);
        fs::write(&obj, b"TAMPERED-CONTENT").unwrap();

        let 原指针 = fs::read_to_string(d.path().join("a.png")).unwrap();
        let r = import(d.path(), &d.path().join(".git"), true);

        assert_eq!(r.migrated(), 0, "绝不能迁：{r:?}");
        match &r.files[0].1 {
            Outcome::Skipped(SkipReason::HashMismatch { expected, actual }) => {
                assert_eq!(expected, &oid);
                assert_ne!(actual, &oid);
            }
            other => panic!("应为 HashMismatch，实得 {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(d.path().join("a.png")).unwrap(),
            原指针,
            "指针必须原封不动——它是找回原内容的唯一线索"
        );
    }

    /// 指针被人改过 `size`（哈希仍对得上）→ 认出来并跳过，
    /// 而不是默默按哈希放行。
    #[test]
    fn size被改过时认出指针损坏并跳过() {
        let d = tempfile::tempdir().unwrap();
        let content = b"REAL";
        let oid = hex(&Sha256::digest(content));
        // 指针声称 999 字节，实际 4 字节。
        fs::write(d.path().join("a.png"), 指针(&oid, 999)).unwrap();
        let obj = d
            .path()
            .join(".git/lfs/objects")
            .join(&oid[0..2])
            .join(&oid[2..4]);
        fs::create_dir_all(&obj).unwrap();
        fs::write(obj.join(&oid), content).unwrap();

        let r = import(d.path(), &d.path().join(".git"), true);
        assert!(matches!(
            r.files[0].1,
            Outcome::Skipped(SkipReason::SizeMismatch {
                expected: 999,
                actual: 4
            })
        ));
    }

    /// **一个文件出问题不中止整批**——一个对象缺失不该让另外 999 个迁不进来。
    #[test]
    fn 一个文件失败不影响其余文件迁入() {
        let d = 造仓库(&[
            ("好的.png", b"GOOD-ONE", true),
            ("缺对象.png", b"MISSING", false),
            ("也好的.png", b"GOOD-TWO", true),
        ]);
        let r = import(d.path(), &d.path().join(".git"), true);
        assert_eq!(r.migrated(), 2, "{r:?}");
        assert_eq!(r.skipped(), 1);
        assert_eq!(fs::read(d.path().join("好的.png")).unwrap(), b"GOOD-ONE");
        assert_eq!(fs::read(d.path().join("也好的.png")).unwrap(), b"GOOD-TWO");
    }

    /// 报告逐文件给结论，且**按路径排序**（确定性，供脚本消费与测试断言）。
    #[test]
    fn 报告覆盖每一个被扫到的指针且有序() {
        let d = 造仓库(&[("z.png", b"Z", true), ("a.png", b"A", true)]);
        let r = import(d.path(), &d.path().join(".git"), false);
        assert_eq!(r.files.len(), 2);
        assert_eq!(r.files[0].0, "a.png");
        assert_eq!(r.files[1].0, "z.png");
    }

    /// 非指针文件（普通笔记、二进制）不被碰，也不进报告。
    #[test]
    fn 非指针文件不被碰也不进报告() {
        let d = 造仓库(&[("a.png", b"REAL", true)]);
        fs::write(d.path().join("笔记.md"), "# 标题\n正文").unwrap();
        fs::write(d.path().join("图.jpg"), [0xffu8, 0xd8, 0xff, 0xe0]).unwrap();
        let r = import(d.path(), &d.path().join(".git"), true);
        assert_eq!(r.files.len(), 1, "只该有那一个指针：{r:?}");
        assert_eq!(
            fs::read_to_string(d.path().join("笔记.md")).unwrap(),
            "# 标题\n正文"
        );
    }

    // ---------------- .gitattributes ----------------

    /// **只换指针是不够的。** `.gitattributes` 里留着 `filter=lfs` 时，
    /// 用户下一次 `git add` 会让 git 的 clean filter 把文件重新变回指针——
    /// 整个迁入在下一次提交时被静默撤销，且没有任何征兆。
    #[test]
    fn 检出仍会把文件变回指针的gitattributes规则() {
        let d = 造仓库(&[("a.png", b"REAL", true)]);
        fs::write(
            d.path().join(".gitattributes"),
            "*.png filter=lfs diff=lfs merge=lfs -text\n*.md text\n",
        )
        .unwrap();
        let r = import(d.path(), &d.path().join(".git"), false);
        assert_eq!(r.lfs_attrs.len(), 1, "{:?}", r.lfs_attrs);
        assert_eq!(r.lfs_attrs[0].line_no, 1);
        assert!(r.lfs_attrs[0].line.contains("filter=lfs"));
    }

    /// 判据是 `filter=lfs` 这个**属性**，不是「这行里有 lfs 三个字母」。
    /// `diff=lfs`/`merge=lfs` 单独出现不会把文件变成指针；注释行更不算。
    #[test]
    fn 只认filter_lfs而不是任何带lfs的行() {
        let d = 造仓库(&[("a.png", b"REAL", true)]);
        fs::write(
            d.path().join(".gitattributes"),
            "*.psd diff=lfs merge=lfs\n# *.mov filter=lfs\n*.txt text\n",
        )
        .unwrap();
        let r = import(d.path(), &d.path().join(".git"), false);
        assert!(r.lfs_attrs.is_empty(), "{:?}", r.lfs_attrs);
    }

    /// dry-run 下只报告，**不动 `.gitattributes`**。
    #[test]
    fn dry_run不改动gitattributes() {
        let d = 造仓库(&[("a.png", b"REAL", true)]);
        let 原文 = "*.png filter=lfs diff=lfs merge=lfs\n";
        fs::write(d.path().join(".gitattributes"), 原文).unwrap();
        let r = import(d.path(), &d.path().join(".git"), false);
        assert_eq!(r.attrs_disabled, 0);
        assert_eq!(
            fs::read_to_string(d.path().join(".gitattributes")).unwrap(),
            原文
        );
    }

    /// `--yes` 下把规则**注释掉而不是删掉**——`.gitattributes` 是用户的配置，
    /// 删一行等于销毁信息；注释保留了「这里原来配过什么」，也能改回去。
    #[test]
    fn yes下注释掉规则而不是删掉() {
        let d = 造仓库(&[("a.png", b"REAL", true)]);
        fs::write(
            d.path().join(".gitattributes"),
            "*.png filter=lfs diff=lfs merge=lfs\n*.md text\n",
        )
        .unwrap();
        let r = import(d.path(), &d.path().join(".git"), true);
        assert_eq!(r.attrs_disabled, 1);

        let text = fs::read_to_string(d.path().join(".gitattributes")).unwrap();
        assert!(
            text.contains("# *.png filter=lfs"),
            "原行应当被注释保留：{text}"
        );
        assert!(text.contains("arca import lfs"), "要说明是谁改的：{text}");
        assert!(text.contains("*.md text"), "无关的行不该被动：{text}");
        // 注释之后这条规则对 git 不再生效——再扫一次应当认不出它。
        let r2 = import(d.path(), &d.path().join(".git"), false);
        assert!(r2.lfs_attrs.is_empty(), "{:?}", r2.lfs_attrs);
    }

    #[test]
    fn 子目录里的gitattributes也被扫到() {
        let d = 造仓库(&[("assets/a.png", b"REAL", true)]);
        fs::create_dir_all(d.path().join("assets")).unwrap();
        fs::write(d.path().join("assets/.gitattributes"), "*.png filter=lfs\n").unwrap();
        let r = import(d.path(), &d.path().join(".git"), false);
        assert_eq!(r.lfs_attrs.len(), 1, "{:?}", r.lfs_attrs);
        assert!(r.lfs_attrs[0].file.contains("assets"));
    }

    /// 迁入之后不留 `.arca-lfs-tmp` 残留。
    #[test]
    fn 迁入之后不留tmp残留() {
        let d = 造仓库(&[("a.png", b"REAL", true)]);
        import(d.path(), &d.path().join(".git"), true);
        let leftovers: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("arca-lfs-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
