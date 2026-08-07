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
//!
//! `upsert`/`remove` 遇到**残缺**的标记块（有起始标记、找不到与之配对的结束标记；
//! 或多个起始标记、无法判断哪一对才是真正的块边界）时返回 [`BlockError`]，绝不
//! 猜测边界——猜错的代价是把标记之间的用户内容当成"块"整体吞掉（I5、I3）。

use std::fmt;

/// 块起始标记行（不含换行）。
const HEADER: &str = "# >>> arca managed (do not edit inside) >>>";
/// 块结束标记行（不含换行）。
const FOOTER: &str = "# <<< arca managed <<<";

/// `.gitignore` 中标记块本身已经残缺，无法安全定位块边界。
///
/// 调用方（`arca setup` / `arca register` 等）拿到这个错误必须**停下并提示用户**，
/// 而不是继续 `upsert`/`remove`——这两个操作在边界不确定时绝不会"尽力"猜测，
/// 那样做曾经导致把标记之间的用户内容整体吞掉（评审 Important #1）。
/// 正确的恢复方式是让用户手工检查 `.gitignore`：要么补全/删掉残缺的标记行，
/// 要么直接删除整个可疑区域后重新 `upsert`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    /// 找到了起始标记（`line`，1-indexed），但在下一个起始标记（如果有）之前
    /// 找不到与之配对的结束标记。
    UnterminatedBlock { line: usize },
    /// 文件中出现了不止一个起始标记（`lines`，1-indexed），无法判断哪一个
    /// 该与哪一个结束标记配对。
    MultipleHeaders { lines: Vec<usize> },
}

impl fmt::Display for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockError::UnterminatedBlock { line } => write!(
                f,
                ".gitignore 第 {line} 行是 arca 标记块的起始标记，但找不到与之配对的\
                 结束标记（{FOOTER:?}）；请检查该文件，手工补全或删除残缺的标记后重试"
            ),
            BlockError::MultipleHeaders { lines } => write!(
                f,
                ".gitignore 中出现了不止一个 arca 起始标记（第 {lines:?} 行），\
                 无法判断块边界；请检查该文件，只保留一对起止标记后重试"
            ),
        }
    }
}

impl std::error::Error for BlockError {}

/// 渲染标记块本身（含首尾标记行，`\n` 结尾）。
///
/// `datasets` 按路径**字节序**排序后去重——同一组数据集，无论调用方传入的顺序
/// 如何，恒产出同一段字节（确定性、`upsert` 幂等的基础）。每个数据集恰好三行，
/// 顺序固定：先整体排除，再反选 `.arca/`，再排除其中设备本地的 `client/`。
///
/// 数据集路径两侧的 `/` 会被去掉再拼接，容错调用方传入 `"assets/"` 这类写法——
/// 裁剪发生在排序/去重**之前**，因此 `render(&["assets/", "assets"])` 只产出一份，
/// 不会因为裁剪前字节不同而被去重漏判成两个数据集。
pub fn render(datasets: &[&str]) -> String {
    let mut sorted: Vec<&str> = datasets.iter().map(|raw| raw.trim_matches('/')).collect();
    sorted.sort_unstable();
    sorted.dedup();

    let mut out = String::new();
    out.push_str(HEADER);
    out.push('\n');
    for ds in sorted {
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
/// 幂等：对同一个 `datasets` 集合，`upsert(upsert(existing, ds)?, ds)?`
/// 与 `upsert(existing, ds)?` 产出完全相同的字节。
///
/// 残缺标记块（见 [`find_block`]）返回 [`BlockError`]，绝不猜测边界后继续写入——
/// 猜错的代价是把标记之间的用户内容当成"块"整体吞掉（评审 Important #1）。
pub fn upsert(existing: &str, datasets: &[&str]) -> Result<String, BlockError> {
    let block = render(datasets);
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    match find_block(&lines)? {
        Some((start, end)) => {
            let mut out = String::new();
            for line in &lines[..start] {
                out.push_str(line);
            }
            out.push_str(&block);
            for line in &lines[end + 1..] {
                out.push_str(line);
            }
            Ok(out)
        }
        None => {
            let mut out = existing.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&block);
            Ok(out)
        }
    }
}

/// 从 `existing` 中删除 arca 标记块（含首尾标记行），块外内容逐字保留。
/// 找不到块时原样返回——`remove` 只删块，不代表"文件里没有 arca 痕迹就是错"。
///
/// 残缺标记块同样返回 [`BlockError`]，不猜测边界后继续删除。
pub fn remove(existing: &str) -> Result<String, BlockError> {
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    match find_block(&lines)? {
        Some((start, end)) => {
            let mut out = String::new();
            for line in &lines[..start] {
                out.push_str(line);
            }
            for line in &lines[end + 1..] {
                out.push_str(line);
            }
            Ok(out)
        }
        None => Ok(existing.to_string()),
    }
}

/// 在按行切片（每行含自己的换行符，末行可能没有）中定位标记块，
/// 返回 `(起始行下标, 结束行下标)`，均含端点；`Ok(None)` 表示没有块
/// （没有任何起始标记——此时即便存在孤立的结束标记，也不构成风险：
/// 没有起始标记可配对，`upsert`/`remove` 不会把它当成块的一部分，
/// 只会原样保留在输出里）。
///
/// 两类残缺输入返回 `Err`，绝不猜测边界：
/// - 找到起始标记，但在下一个起始标记（如果有）之前找不到与之配对的结束标记
///   （包括标记顺序颠倒——结束标记出现在起始标记*之前*，等效于起始标记之后没有
///   结束标记）→ [`BlockError::UnterminatedBlock`]。
/// - 出现不止一个起始标记 → [`BlockError::MultipleHeaders`]（哪怕后面能凑出
///   同样数量的结束标记，也不去猜配对关系——那是两个块粘在一起的残迹，
///   应该让用户看一眼再决定，而不是自动拼出一个可能跨块吞并用户内容的边界）。
fn find_block(lines: &[&str]) -> Result<Option<(usize, usize)>, BlockError> {
    fn strip(l: &str) -> &str {
        l.trim_end_matches('\n').trim_end_matches('\r')
    }
    let header_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| strip(l) == HEADER)
        .map(|(i, _)| i)
        .collect();
    if header_indices.len() > 1 {
        return Err(BlockError::MultipleHeaders {
            lines: header_indices.iter().map(|i| i + 1).collect(),
        });
    }
    let Some(&header_idx) = header_indices.first() else {
        return Ok(None);
    };
    match lines[header_idx + 1..]
        .iter()
        .position(|l| strip(l) == FOOTER)
    {
        Some(offset) => Ok(Some((header_idx, header_idx + 1 + offset))),
        None => Err(BlockError::UnterminatedBlock {
            line: header_idx + 1,
        }),
    }
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
    fn render_裁剪发生在去重之前_不产出重复块() {
        // 评审 Minor #5：`"assets/"` 与 `"assets"` 裁剪前字节不同，若去重发生在
        // 裁剪之前就会漏判成两个数据集，产出重复的三行块。
        let text = render(&["assets/", "assets"]);
        assert_eq!(
            text,
            render(&["assets"]),
            "裁剪后应视为同一个数据集，只出现一次"
        );
        assert_eq!(text.matches("/assets/*\n").count(), 1, "不应该产出重复的块");
    }

    #[test]
    fn upsert_在空文件追加块() {
        let text = upsert("", &["assets"]).unwrap();
        assert_eq!(text, render(&["assets"]));
    }

    #[test]
    fn upsert_保留块外用户内容_块追加在末尾() {
        let existing = "node_modules/\n*.log\n";
        let text = upsert(existing, &["assets"]).unwrap();
        assert!(text.starts_with(existing), "块外用户内容必须逐字保留在前");
        assert!(text.ends_with(&render(&["assets"])));
    }

    #[test]
    fn upsert_替换已有块_保留块前后的用户内容() {
        let existing = "before\n".to_string() + &render(&["assets"]) + "after\n";
        let text = upsert(&existing, &["assets", "photo"]).unwrap();
        assert!(text.starts_with("before\n"));
        assert!(text.ends_with("after\n"));
        assert!(text.contains(&render(&["assets", "photo"])));
        assert!(!text.contains("/photo/*\n/photo/*"), "不应该重复块");
    }

    #[test]
    fn upsert_幂等() {
        let once = upsert("existing\n", &["assets", "photo"]).unwrap();
        let twice = upsert(&once, &["assets", "photo"]).unwrap();
        assert_eq!(once, twice, "对已有块再跑一次必须产出完全相同的字节");
    }

    #[test]
    fn remove_只删块_保留其余内容() {
        let existing = "before\n".to_string() + &render(&["assets"]) + "after\n";
        let text = remove(&existing).unwrap();
        assert_eq!(text, "before\nafter\n");
    }

    #[test]
    fn remove_找不到块时原样返回() {
        let existing = "no block here\n";
        assert_eq!(remove(existing).unwrap(), existing);
    }

    // --- 评审 Important #1：残缺标记块必须报错，绝不猜测边界吞掉用户内容 ---

    #[test]
    fn 缺结束标记时_upsert_报错而不是追加新块() {
        // 复现评审构造的场景：起始标记存在，规则行存在，用户内容存在，
        // 唯独没有结束标记（模拟用户手滑删掉了它）。
        let existing = "before\n".to_string()
            + HEADER
            + "\n/assets/*\n!/assets/.arca/\n/assets/.arca/client/\n"
            + "user-line-1\nuser-line-2\n";

        let err = upsert(&existing, &["assets"]).unwrap_err();
        assert_eq!(err, BlockError::UnterminatedBlock { line: 2 });
    }

    #[test]
    fn 缺结束标记时_remove_同样报错() {
        let existing = "before\n".to_string()
            + HEADER
            + "\n/assets/*\n!/assets/.arca/\n/assets/.arca/client/\n"
            + "user-line-1\nuser-line-2\n";

        let err = remove(&existing).unwrap_err();
        assert_eq!(err, BlockError::UnterminatedBlock { line: 2 });
    }

    #[test]
    fn 反复_upsert_不会因为上一次留下的残缺块而吞掉用户内容() {
        // 这正是评审复现的两步场景：第一次 upsert 在找不到块时于文件末尾追加，
        // 此时用户内容还在；关键在于第二次 upsert（幂等调用的正常模式）
        // 必须同样报错，而不是把孤立的旧起始标记到新块结束标记之间的内容
        // 当成"块"整体替换掉。
        let existing = "before\n".to_string()
            + HEADER
            + "\n/assets/*\n!/assets/.arca/\n/assets/.arca/client/\n"
            + "user-line-1\nuser-line-2\n";

        // 模拟"没有结束标记 => 判定没有块 => 追加"这一步曾经的（错误）行为，
        // 直接构造它的产物：文件里现在有孤立的旧起始标记（没有配对的结束标记），
        // 后面又跟着一个完整的新块（自带一对起止标记）——也就是两个起始标记。
        // 验证对*这个产物*再跑一次 upsert 会报错（MultipleHeaders，不去猜哪个
        // 起始标记该配哪个结束标记），而不是把旧起始标记到新块结束标记之间的
        // 全部内容当成"块"整体替换掉，那样会吞掉 user-line-1 / user-line-2。
        let after_naive_append = existing.clone() + &render(&["assets"]);
        let err = upsert(&after_naive_append, &["assets"]).unwrap_err();
        assert_eq!(err, BlockError::MultipleHeaders { lines: vec![2, 8] });
        // 报错就意味着调用方会停下、不会写回任何字节——user-line-1/2 不会丢失，
        // 因为它们仍然一字不差地留在 `after_naive_append` 里。
        assert!(after_naive_append.contains("user-line-1\nuser-line-2\n"));
    }

    #[test]
    fn 缺起始标记时_视为没有块_安全追加() {
        // 孤立的结束标记（没有起始标记与之配对）不构成风险：没有起始标记
        // 可配对，upsert 不会把它当成块的一部分，只会原样保留并在末尾追加新块。
        let existing = "before\n".to_string() + FOOTER + "\nafter\n";
        let text = upsert(&existing, &["assets"]).unwrap();
        assert!(text.starts_with(&existing));
        assert!(text.ends_with(&render(&["assets"])));
    }

    #[test]
    fn 标记顺序颠倒时报错() {
        // 结束标记出现在起始标记*之前*：起始标记之后再也找不到结束标记，
        // 等效于"缺结束标记"，同样必须报错。
        let existing = "before\n".to_string()
            + FOOTER
            + "\nmiddle\n"
            + HEADER
            + "\n/assets/*\n!/assets/.arca/\n/assets/.arca/client/\n";

        let err = upsert(&existing, &["assets"]).unwrap_err();
        assert_eq!(err, BlockError::UnterminatedBlock { line: 4 });
    }

    #[test]
    fn 两个起始标记时报错不猜配对() {
        let existing = render(&["assets"]) + &render(&["photo"]);
        let err = upsert(&existing, &["assets"]).unwrap_err();
        assert_eq!(err, BlockError::MultipleHeaders { lines: vec![1, 6] });
    }

    #[test]
    fn 正常输入的幂等性不受残缺检测影响() {
        let once = upsert("plain text\n", &["assets", "photo"]).unwrap();
        let twice = upsert(&once, &["assets", "photo"]).unwrap();
        let thrice = upsert(&twice, &["assets", "photo"]).unwrap();
        assert_eq!(once, twice);
        assert_eq!(twice, thrice);
    }
}
