//! referenced-only 扫描：解析待发布 md 的相对路径引用（M5a，spec §4.9 约束 ③）。
//!
//! > 直接公开整个数据集会暴露没被任何已发布笔记引用的文件——**这是隐私事故
//! > 的常见来源**。`--referenced-only` 是默认；`--all` 必须显式指定。
//! > 这是「绝不猜测」在发布路径上的对应物：**扩大暴露面必须是显式动作。**
//!
//! # 提取宁可多不可少
//!
//! 这里的漏报与误报**代价完全不对称**：
//!
//! - **漏报**（该发布的没发布）→ 站点上一张图裂了。用户立刻看得见，一眼能修。
//! - **误报**（不该发布的发布了）→ 一张私密照片挂上了公网 CDN。
//!   用户看不见，且**不可撤回**（已经被抓取、被缓存）。
//!
//! 所以本模块的取舍是**在语法上尽量宽**（把看起来像引用的都算上），
//! 但**在结果上严格闭合**：只有真的出现在某个 md 里的路径才进集合，
//! 绝不做「这个目录看起来都该发布」这类推断。
//!
//! # 只解析文本，不访问文件系统
//!
//! 输入是 md 的**内容**，不是路径——与 `map` 同一条纪律。这让它能被穷举
//! 测试，也让「扫描时会不会不小心读到别的东西」这个问题不存在。

use std::collections::BTreeSet;

/// 从一篇 md 里抽出所有相对路径引用，并入 `out`。
///
/// 认三种写法：
///
/// - 标准 markdown：`![alt](路径)` 与 `[文字](路径)`
/// - wiki 链接（Obsidian）：`[[路径]]` 与 `![[路径]]`
/// - HTML：`<img src="路径">`（笔记里手写 HTML 很常见）
///
/// **绝对 URL（`http://`、`https://`、`//`、`data:`）一律跳过**——它们不是
/// 本 vault 的资源，收进来只会污染集合。锚点（`#…`）与查询串（`?…`）会被
/// 截掉，因为它们不属于路径。
pub fn extract_refs(markdown: &str, out: &mut BTreeSet<String>) {
    let bytes: Vec<char> = markdown.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        // `[[…]]` / `![[…]]`
        if bytes[i] == '[' && i + 1 < bytes.len() && bytes[i + 1] == '[' {
            if let Some(end) = find_seq(&bytes, i + 2, "]]") {
                push_ref(&collect(&bytes, i + 2, end), out);
                i = end + 2;
                continue;
            }
        }
        // `](…)`——标准 markdown 链接/图片的路径部分
        if bytes[i] == ']' && i + 1 < bytes.len() && bytes[i + 1] == '(' {
            if let Some(end) = find_char(&bytes, i + 2, ')') {
                push_ref(&collect(&bytes, i + 2, end), out);
                i = end + 1;
                continue;
            }
        }
        // `src="…"` / `src='…'`
        if starts_with(&bytes, i, "src=") {
            let q = i + 4;
            if q < bytes.len() && (bytes[q] == '"' || bytes[q] == '\'') {
                if let Some(end) = find_char(&bytes, q + 1, bytes[q]) {
                    push_ref(&collect(&bytes, q + 1, end), out);
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
}

fn collect(chars: &[char], start: usize, end: usize) -> String {
    chars[start..end].iter().collect()
}

fn starts_with(chars: &[char], at: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    at + p.len() <= chars.len() && chars[at..at + p.len()] == p[..]
}

fn find_char(chars: &[char], from: usize, c: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == c)
}

fn find_seq(chars: &[char], from: usize, pat: &str) -> Option<usize> {
    (from..chars.len()).find(|&i| starts_with(chars, i, pat))
}

/// 归一化一条候选引用并放进集合。返回是否被接受（供测试断言）。
fn push_ref(raw: &str, out: &mut BTreeSet<String>) -> bool {
    let mut s = raw.trim();

    // markdown 的 `](路径 "标题")` —— 标题不属于路径。
    if let Some(sp) = s.find(char::is_whitespace) {
        s = &s[..sp];
    }
    // Obsidian 的 `[[路径|显示名]]` 与 `[[路径#锚点]]`。
    for sep in ['|', '#', '?'] {
        if let Some(p) = s.find(sep) {
            s = &s[..p];
        }
    }
    let s = s.trim().trim_matches('<').trim_matches('>');
    if s.is_empty() {
        return false;
    }
    // 绝对 URL 与协议相对 URL——不是本 vault 的资源。
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("//")
        || lower.starts_with("data:")
        || lower.starts_with("mailto:")
    {
        return false;
    }
    // 纯锚点（`#标题`）在上面的循环里已经被截空，这里兜底。
    if s.starts_with('#') {
        return false;
    }
    out.insert(s.to_string());
    true
}

/// 把一批引用（vault 内的完整相对路径）折算成**某个数据集内的相对路径集合**。
///
/// `dataset_path` 是数据集在 vault 里的相对路径（如 `assets`）。
/// 只保留落在这个数据集里的引用，并去掉前缀——正好是
/// [`crate::map::add_dataset`] 的 `only` 参数要的形状。
///
/// **前缀匹配按路径分量**，不是字符串前缀：`assets` 不该把 `assets-old/x.png`
/// 算进来。这类「看起来对」的字符串前缀匹配是把不该发布的东西发出去的
/// 经典途径（与 `arca-agentd` 的 `.arca` 判定同一条教训）。
pub fn scope_to_dataset(refs: &BTreeSet<String>, dataset_path: &str) -> BTreeSet<String> {
    let prefix = format!("{}/", dataset_path.trim_end_matches('/'));
    refs.iter()
        .filter_map(|r| r.strip_prefix(&prefix))
        .filter(|rest| !rest.is_empty())
        .map(|rest| rest.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 抽(md: &str) -> BTreeSet<String> {
        let mut s = BTreeSet::new();
        extract_refs(md, &mut s);
        s
    }

    #[test]
    fn 标准markdown图片与链接() {
        let s = 抽("看这张 ![鸭川](assets/京都/鸭川.png) 还有 [附件](assets/doc.pdf)。");
        assert!(s.contains("assets/京都/鸭川.png"), "{s:?}");
        assert!(s.contains("assets/doc.pdf"), "{s:?}");
    }

    #[test]
    fn wiki链接与嵌入() {
        let s = 抽("![[assets/a.png]] 与 [[assets/b.pdf]]");
        assert!(s.contains("assets/a.png"), "{s:?}");
        assert!(s.contains("assets/b.pdf"), "{s:?}");
    }

    #[test]
    fn wiki链接的显示名与锚点被截掉() {
        let s = 抽("[[assets/a.png|我的图]] [[assets/b.md#第二节]]");
        assert!(s.contains("assets/a.png"), "{s:?}");
        assert!(s.contains("assets/b.md"), "{s:?}");
    }

    #[test]
    fn html的src被认出来() {
        let s = 抽(r#"<img src="assets/x.png" width="200">"#);
        assert!(s.contains("assets/x.png"), "{s:?}");
        let s2 = 抽("<img src='assets/y.png'>");
        assert!(s2.contains("assets/y.png"), "{s2:?}");
    }

    #[test]
    fn markdown的标题参数不算进路径() {
        let s = 抽(r#"![x](assets/a.png "这是标题")"#);
        assert!(s.contains("assets/a.png"), "{s:?}");
        assert!(!s.iter().any(|r| r.contains("标题")), "{s:?}");
    }

    /// 绝对 URL 不是本 vault 的资源，收进来只会污染集合。
    #[test]
    fn 绝对url与协议相对url被跳过() {
        let s = 抽(
            "![a](https://example.com/x.png) ![b](http://example.com/y.png) \
             ![c](//cdn.example.com/z.png) ![d](data:image/png;base64,AAAA) \
             [e](mailto:someone@example.com)",
        );
        assert!(s.is_empty(), "绝对 URL 不该进集合：{s:?}");
    }

    #[test]
    fn 纯锚点不算引用() {
        let s = 抽("[跳转](#第三节)");
        assert!(s.is_empty(), "{s:?}");
    }

    #[test]
    fn 空引用与畸形语法不panic() {
        for md in ["![]()", "[[", "]](", "<img src=", "![[未闭合", "[](  )"] {
            let _ = 抽(md); // 只要不 panic
        }
    }

    /// **本文件里最重要的一条。** 前缀匹配必须按路径分量——
    /// `assets` 不该把 `assets-old/私密.png` 算进来。字符串前缀匹配是
    /// 「把不该发布的东西发出去」的经典途径。
    #[test]
    fn 数据集前缀匹配按分量而不是字符串前缀() {
        let refs: BTreeSet<String> = [
            "assets/对的.png".to_string(),
            "assets-old/私密.png".to_string(),
            "assetsfoo.png".to_string(),
        ]
        .into_iter()
        .collect();

        let scoped = scope_to_dataset(&refs, "assets");
        assert_eq!(scoped.len(), 1, "{scoped:?}");
        assert!(scoped.contains("对的.png"));
        assert!(
            !scoped.iter().any(|p| p.contains("私密")),
            "assets-old/ 里的东西绝不能被算成 assets 的：{scoped:?}"
        );
    }

    #[test]
    fn 折算到数据集时去掉前缀() {
        let refs: BTreeSet<String> = ["assets/京都/鸭川.png".to_string()].into_iter().collect();
        let scoped = scope_to_dataset(&refs, "assets");
        assert!(scoped.contains("京都/鸭川.png"), "{scoped:?}");
    }

    /// 数据集本身被引用（`assets/`）不该产生一条空路径。
    #[test]
    fn 只有前缀本身时不产生空路径() {
        let refs: BTreeSet<String> = ["assets/".to_string()].into_iter().collect();
        assert!(scope_to_dataset(&refs, "assets").is_empty());
    }

    /// 一篇笔记引用多个数据集时，各自只拿自己那部分。
    #[test]
    fn 多数据集各自折算互不串台() {
        let s = 抽("![a](assets/a.png) ![b](photo/b.raw)");
        assert_eq!(scope_to_dataset(&s, "assets"), ["a.png".to_string()].into());
        assert_eq!(scope_to_dataset(&s, "photo"), ["b.raw".to_string()].into());
    }
}
