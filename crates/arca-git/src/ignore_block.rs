//! `.gitignore` arca 标记块——**本设计最易出错、最必须被测试覆盖的一处**（spec §4.3、风险表）。
//!
//! 必须用反选写法（父目录被排除后其内容无法再被反选，故不能写 `/assets/`）：
//!
//! ```gitignore
//! # >>> arca managed (do not edit inside) >>>
//! /assets/*
//! !/assets/.arca/
//! /assets/.arca/client/
//! # <<< arca managed <<<
//! ```
//!
//! 要求：生成器只此一处 + golden 样例；幂等、可人工审阅、可随手删除。
//! `arca doctor` 断言的是 `git check-ignore` 的**实际结果**而非文本
//! （§6.3 第 9 条：`.arca/dataset.toml` 与 `manifest` 被追踪、
//! `client/` 与受管二进制未被追踪）。

/// 块起始标记行（不含换行）。
const HEADER: &str = "# >>> arca managed (do not edit inside) >>>";
/// 块结束标记行（不含换行）。
const FOOTER: &str = "# <<< arca managed <<<";

/// 渲染标记块本身（含首尾标记行，`\n` 结尾）。
///
/// `datasets` 按路径**字节序**排序后去重——同一组数据集，无论调用方传入的顺序
/// 如何，恒产出同一段字节（确定性、`upsert` 幂等的基础）。每个数据集恰好三行，
/// 顺序固定：先整体排除，再反选 `.arca/`，再排除其中设备本地的 `client/`。
///
/// 数据集路径两侧的 `/` 会被去掉再拼接，容错调用方传入 `"assets/"` 这类写法。
pub fn render(datasets: &[&str]) -> String {
    let mut sorted: Vec<&str> = datasets.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut out = String::new();
    out.push_str(HEADER);
    out.push('\n');
    for raw in sorted {
        let ds = raw.trim_matches('/');
        out.push('/');
        out.push_str(ds);
        out.push_str("/*\n");
        out.push_str("!/");
        out.push_str(ds);
        out.push_str("/.arca/\n");
        out.push('/');
        out.push_str(ds);
        out.push_str("/.arca/client/\n");
    }
    out.push_str(FOOTER);
    out.push('\n');
    out
}

/// 把 `existing`（现有 `.gitignore` 全文）中的 arca 标记块替换为
/// `render(datasets)` 的结果；块外内容逐字保留。找不到已有块时，
/// 在文件末尾追加（若原文件非空且不以换行结尾，先补一个换行，
/// 不改动原有字节，只补全文件规范要求的行尾）。
///
/// 幂等：对同一个 `datasets` 集合，`upsert(upsert(existing, ds), ds)`
/// 与 `upsert(existing, ds)` 产出完全相同的字节。
pub fn upsert(existing: &str, datasets: &[&str]) -> String {
    let block = render(datasets);
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    match find_block(&lines) {
        Some((start, end)) => {
            let mut out = String::new();
            for line in &lines[..start] {
                out.push_str(line);
            }
            out.push_str(&block);
            for line in &lines[end + 1..] {
                out.push_str(line);
            }
            out
        }
        None => {
            let mut out = existing.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&block);
            out
        }
    }
}

/// 从 `existing` 中删除 arca 标记块（含首尾标记行），块外内容逐字保留。
/// 找不到块时原样返回——`remove` 只删块，不代表"文件里没有 arca 痕迹就是错"。
pub fn remove(existing: &str) -> String {
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    match find_block(&lines) {
        Some((start, end)) => {
            let mut out = String::new();
            for line in &lines[..start] {
                out.push_str(line);
            }
            for line in &lines[end + 1..] {
                out.push_str(line);
            }
            out
        }
        None => existing.to_string(),
    }
}

/// 在按行切片（每行含自己的换行符，末行可能没有）中定位标记块，
/// 返回 `(起始行下标, 结束行下标)`，均含端点。要求先出现 `HEADER` 行、
/// 之后才出现 `FOOTER` 行，否则视为"没有块"（不冒险猜测半个块的边界，
/// 交给上层原样保留或整体追加，绝不吞用户内容）。
fn find_block(lines: &[&str]) -> Option<(usize, usize)> {
    fn strip(l: &str) -> &str {
        l.trim_end_matches('\n').trim_end_matches('\r')
    }
    let header_idx = lines.iter().position(|l| strip(l) == HEADER)?;
    let footer_idx = lines[header_idx + 1..]
        .iter()
        .position(|l| strip(l) == FOOTER)?
        + header_idx
        + 1;
    Some((header_idx, footer_idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_单数据集三行加首尾标记() {
        let text = render(&["assets"]);
        assert_eq!(
            text,
            "# >>> arca managed (do not edit inside) >>>\n\
             /assets/*\n\
             !/assets/.arca/\n\
             /assets/.arca/client/\n\
             # <<< arca managed <<<\n"
        );
    }

    #[test]
    fn render_多数据集按字节序排序且去重() {
        let text = render(&["photo", "assets", "assets"]);
        let expected = "# >>> arca managed (do not edit inside) >>>\n\
             /assets/*\n\
             !/assets/.arca/\n\
             /assets/.arca/client/\n\
             /photo/*\n\
             !/photo/.arca/\n\
             /photo/.arca/client/\n\
             # <<< arca managed <<<\n";
        assert_eq!(text, expected);
    }

    #[test]
    fn render_非_ascii_数据集路径() {
        let text = render(&["资料"]);
        assert!(text.contains("/资料/*\n"));
        assert!(text.contains("!/资料/.arca/\n"));
        assert!(text.contains("/资料/.arca/client/\n"));
    }

    #[test]
    fn upsert_在空文件追加块() {
        let text = upsert("", &["assets"]);
        assert_eq!(text, render(&["assets"]));
    }

    #[test]
    fn upsert_保留块外用户内容_块追加在末尾() {
        let existing = "node_modules/\n*.log\n";
        let text = upsert(existing, &["assets"]);
        assert!(text.starts_with(existing), "块外用户内容必须逐字保留在前");
        assert!(text.ends_with(&render(&["assets"])));
    }

    #[test]
    fn upsert_替换已有块_保留块前后的用户内容() {
        let existing = "before\n".to_string() + &render(&["assets"]) + "after\n";
        let text = upsert(&existing, &["assets", "photo"]);
        assert!(text.starts_with("before\n"));
        assert!(text.ends_with("after\n"));
        assert!(text.contains(&render(&["assets", "photo"])));
        assert!(!text.contains("/photo/*\n/photo/*"), "不应该重复块");
    }

    #[test]
    fn upsert_幂等() {
        let once = upsert("existing\n", &["assets", "photo"]);
        let twice = upsert(&once, &["assets", "photo"]);
        assert_eq!(once, twice, "对已有块再跑一次必须产出完全相同的字节");
    }

    #[test]
    fn remove_只删块_保留其余内容() {
        let existing = "before\n".to_string() + &render(&["assets"]) + "after\n";
        let text = remove(&existing);
        assert_eq!(text, "before\nafter\n");
    }

    #[test]
    fn remove_找不到块时原样返回() {
        let existing = "no block here\n";
        assert_eq!(remove(existing), existing);
    }
}
