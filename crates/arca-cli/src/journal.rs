//! hub 侧 journal 的读写（M2a tombstone 计划 Task 2）：`.arca/journal/epoch`
//! 指针 + `.arca/journal/<epoch>.jsonl` 事件流（FORMAT.md §4、§7.2）。
//!
//! `arca_format::journal` 只管解析/序列化单行与整段文本（sans-io，那一层的
//! 纪律见其模块文档）；本模块补上"落到磁盘的哪个位置、怎么原子追加"这一层，
//! 与 `arca-cli` 里 `sync.rs::append_item_version`（items 版本链的追加）是
//! 同一职责在不同事件流上的对应实现。
//!
//! # 损坏处置纪律（FORMAT.md §7.2，直接复用 `arca_format::journal::parse_stream`）
//!
//! 末行不完整 → 截断到最后一个完整行（崩溃残留，正常）；**中间行损坏 → 失败**
//! （journal 是真相，读错一行等于伪造历史，I5）。本模块不重新实现这条纪律——
//! [`read_all`]/[`append`] 都把读到的原始文本交给 `parse_stream`，由它一次性
//! 完成截断与校验（含 `seq` 连续性）。
//!
//! # `append` 为什么要先解析现有内容再整体重写
//!
//! `arca_store::atomic` 只提供整文件原子替换，没有原子追加（与 `items.jsonl`
//! 同样的限制，见 `sync.rs::append_item_version` 的文档）。但如果只是"读现有
//! 原始字节 + 拼接新行 + 整体重写"（`append_item_version` 现在的做法），一旦
//! 上一次追加在崩溃中留下了一条撕裂的末行，这条撕裂行就不再是"末行"
//! 了——它会变成新写入内容之前的一条**中间行**，下次任何读取都会把它当成
//! 中间行损坏而报错，永久损坏这条 journal。所以这里先用 `parse_stream`
//! 解析现有文本（天然截断掉撕裂的末行、留下的都是完整合法的事件），再用
//! 这些事件重新序列化 + 追加新事件、整体重写——上一次崩溃留下的撕裂尾巴在
//! 下一次成功追加时被"治愈"，而不是被继续背在文件里等下一次读取时爆炸。
//! 若现有内容存在**中间行损坏**（真正的数据损坏，不是撕裂尾巴），
//! `parse_stream` 会返回 `Err`，`append` 原样把这个错误报出来并拒绝写入——
//! 绝不在已知损坏的 journal 上继续追加，制造一份看起来更完整、实则建立在
//! 损坏地基上的假象（I5）。
//!
//! # `seq` 连续性：`append` 主动校验，不只依赖读取时才发现
//!
//! `parse_stream` 会在**读取**时拒绝 `seq` 空洞（FORMAT.md §7.2），但如果
//! `append` 对调用方传入的 `seq` 来者不拒，一次调用方自己算错 `seq` 的 bug
//! 就会把空洞真的写进磁盘——journal 是 append-only 的真相源，这个错误一旦
//! 落盘就再也无法悄悄修复。所以 `append` 自己也算一遍"现有链最后一条 `seq`
//! 之后应该是什么"，与调用方传入的 `event.seq` 不一致就拒绝（[`JournalError::SeqMismatch`]），
//! 从源头堵住空洞，而不是留给下一次读取才发现。
//!
//! # epoch 指针缺失是合法的未初始化态（FORMAT.md §4）
//!
//! [`current_epoch`] 对"指针文件不存在"返回 `Ok(None)`，不是错误；
//! [`append`] 在这种情况下现场生成一个新 epoch 并原子创建指针文件——这是
//! FORMAT.md §4 明文允许的"首次写入 journal 前必须先原子创建该文件"。

use arca_format::error::FormatError;
use arca_format::hub_layout::{layout, parse_epoch_pointer};
use arca_format::journal::{Cursor, JournalEvent};
use arca_store::atomic::{self, AtomicError};
use arca_store::root::StorageRoot;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// journal 读写失败——彼此可区分（I5：如实报告失败的性质）。
#[derive(Debug)]
pub enum JournalError {
    /// 一段 journal 文本（epoch 指针或事件流）解析/序列化失败：中间行损坏、
    /// 编码不合法、`op`/`from` 搭配矛盾等，见 `arca_format::journal` 的校验。
    Format(FormatError),
    /// 落盘失败：常规 IO 故障，或 `arca_store::atomic` 报告的原子写入失败
    /// （tmp/rename/fsync 任一环节）。
    Io {
        path: String,
        reason: String,
    },
    Atomic(AtomicError),
    /// 调用方传入的 `event.seq` 与"现有链最后一条 `seq` 之后应该是什么"不符
    /// ——见模块顶部「`seq` 连续性」一节，从源头拒绝制造空洞或倒退/重复。
    SeqMismatch {
        expected: u64,
        actual: u64,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::Format(e) => write!(f, "journal 记录无法解析：{e}"),
            JournalError::Io { path, reason } => write!(f, "journal {path} 读写失败：{reason}"),
            JournalError::Atomic(e) => write!(f, "journal 原子写入失败：{e}"),
            JournalError::SeqMismatch { expected, actual } => write!(
                f,
                "journal seq 不连续：本次追加应为 {expected}，实得 {actual}——\
                 拒绝写入，绝不制造空洞或覆盖历史（FORMAT.md §7.2）"
            ),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JournalError::Format(e) => Some(e),
            JournalError::Atomic(e) => Some(e),
            JournalError::Io { .. } | JournalError::SeqMismatch { .. } => None,
        }
    }
}

fn io_err(path: &Path, e: io::Error) -> JournalError {
    JournalError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// `.arca/journal/<epoch>.jsonl` 的相对路径（FORMAT.md §4、§7.2）。
fn journal_path(epoch: &str) -> String {
    format!("{}/{}.jsonl", layout::JOURNAL_DIR, epoch)
}

/// 读当前 epoch（FORMAT.md §4 的三态处置，直接复用
/// `arca_format::hub_layout::parse_epoch_pointer`）：
///
/// - 指针文件不存在 → `Ok(None)`——合法的未初始化态。
/// - 内容不是合法的 32 位小写十六进制 → `Err`（I5：绝不猜测该用哪个 epoch）。
/// - 合法 → `Ok(Some(epoch))`。
pub fn current_epoch(root: &StorageRoot) -> Result<Option<String>, JournalError> {
    let path = root.path().join(layout::EPOCH_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err(&path, e)),
    };
    parse_epoch_pointer(content.as_deref()).map_err(JournalError::Format)
}

/// 原子创建 epoch 指针文件（tmp → fsync → rename，FORMAT.md §4）。
///
/// 只在 [`current_epoch`] 已确认指针缺失时调用——不检查覆盖与否，调用方
/// （[`append`]）负责这个前提。
fn create_epoch_pointer(root: &StorageRoot, epoch: &str) -> Result<(), JournalError> {
    let content = format!("{epoch}\n");
    atomic::write(root, layout::EPOCH_FILE, content.as_bytes()).map_err(JournalError::Atomic)
}

/// 读某个 epoch 事件流的原始文本；文件不存在视为空（epoch 指针刚创建、
/// 尚未写入任何事件的瞬间是合法的中间态——`append` 总是"创建指针 + 写入
/// 首条事件"在同一次调用里完成，但读取侧不假设这个先后关系一定已经完整
/// 发生过）。
fn read_epoch_text(root: &StorageRoot, epoch: &str) -> Result<String, JournalError> {
    let full = root.path().join(journal_path(epoch));
    match fs::read_to_string(&full) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(io_err(&full, e)),
    }
}

/// 读出当前 epoch 的完整事件流 + 对应的游标。
///
/// 没有 epoch（从未写过 journal）→ `Ok((None, vec![]))`——不编造一个假的
/// `Cursor`（`Cursor` 的 `epoch` 字段按 FORMAT.md §4 必须是合法的 32 位
/// 十六进制，这里没有任何 epoch 可用，`None` 比塞一个空字符串更诚实）。
/// 这是相对 brief 字面签名的一处刻意偏离，与 `sync.rs`/`scan.rs` 顶部同一条
/// 先例（brief 落后于对"未初始化态该怎么表达"的实际考量）。
///
/// 损坏处置：整段文本交给 `arca_format::journal::parse_stream`——末行撕裂
/// 截断，中间行损坏则整体报错，不跳过（I5）。
pub fn read_all(root: &StorageRoot) -> Result<(Option<Cursor>, Vec<JournalEvent>), JournalError> {
    let Some(epoch) = current_epoch(root)? else {
        return Ok((None, Vec::new()));
    };
    let text = read_epoch_text(root, &epoch)?;
    let events = arca_format::journal::parse_stream(&text).map_err(JournalError::Format)?;
    let seq = events.last().map(|e| e.seq).unwrap_or(0);
    Ok((Some(Cursor { epoch, seq }), events))
}

/// 下一个应该使用的 `seq`——当前完整事件流的游标 + 1（没有任何历史事件时
/// 为 1）。[`append`] 内部会做同样的推导并校验调用方传入的 `event.seq`
/// 是否与之相符；这个函数供调用方（`sync.rs::execute_tombstone`、
/// `trash.rs::restore`）提前算出正确值，不必各自重新实现"读游标再加一"。
pub fn next_seq(root: &StorageRoot) -> Result<u64, JournalError> {
    let (cursor, _events) = read_all(root)?;
    Ok(cursor.map(|c| c.seq + 1).unwrap_or(1))
}

/// 追加一条 journal 事件：整行原子落盘，写完 fsync（经 [`arca_store::atomic::write`]
/// 的完整持久化事务链）。
///
/// - epoch 指针缺失 → 现场生成一个新 epoch 并原子创建指针（FORMAT.md §4）。
/// - 现有事件流中间行损坏 → 拒绝写入，原样报出解析失败（模块顶部「为什么
///   要先解析现有内容再整体重写」一节）。
/// - `event.seq` 与"现有链下一个应该的 seq"不符 → 拒绝写入（[`JournalError::SeqMismatch`]）。
///
/// 单条事件的一次性写入——`arca restore` 这类一次只产生一条事件的命令用它。
/// 一次调用要连续追加多条事件（如 `sync()` 一轮里的多个 `TombstoneRemote`）
/// 应该用 [`AppendBatch`]：本函数每次调用都重新读一遍整段现有事件流，
/// N 次调用是 O(N²)（评审 Important #3）。
pub fn append(root: &StorageRoot, event: &JournalEvent) -> Result<(), JournalError> {
    let mut batch = AppendBatch::open(root)?;
    batch.push(event.clone())?;
    batch.commit()
}

/// 批量追加 journal 事件：只读一次现有事件流，后续每次 [`AppendBatch::push`]
/// 只在内存里追加 + 校验 `seq` 连续性，[`AppendBatch::commit`] 时才整体
/// 序列化、原子写一次（评审 Important #3）。
///
/// # 为什么需要它：`append` 逐次调用是 O(n²)
///
/// [`append`] 每次调用都要"读现有内容 + 拼接新行 + 整体重写"（模块顶部「为
/// 什么要先解析现有内容再整体重写」一节的纪律），这对**单条**事件的一次性
/// 写入没有问题；但 `sync.rs::execute_tombstone` 在一次 `sync()` 里可能被
/// 调用几十上百次，每次都重新读一遍**当时已有的全部**事件——这与 M1d 修复
/// 过的"目录 fsync 一万次"同一个形状：实测 400 个文件的删除传播，发起端
/// 耗时 12.1 秒，`journal::append` 的这个 O(n) 读写乘以 O(n) 次调用正是主因。
///
/// # 与单次 `write` 崩溃安全性的取舍（同 `arca_store::atomic::Batch` 的先例）
///
/// 批量提交把"每条事件各自落盘"的窗口，放宽成"整批一起落盘或都不落盘"。
/// 崩溃如果发生在 `commit()` 之前，本批次内已经调用过
/// [`crate::trash::move_to_trash`] 的那些路径会暂时处于"内容已进回收站、
/// journal 里还没有对应事件"的中间态——但这正是 `hub.rs::is_pending_tombstone`
/// （评审 Important #1）已经能诊断、可自愈的状态，不是静默丢失或误报"存储根
/// 损坏"；崩溃窗口变宽，但没有引入新的失败形态。`sync.rs::sync` 必须在
/// `commit()` 成功之后才保存基线，与内容侧的 `arca_store::atomic::Batch`
/// 同一条纪律（I3）。
pub struct AppendBatch<'a> {
    root: &'a StorageRoot,
    epoch: String,
    events: Vec<JournalEvent>,
    next_seq: u64,
}

impl<'a> AppendBatch<'a> {
    /// 打开一个批次：读一次当前 epoch 的完整事件流（不存在则现场创建新
    /// epoch），算出"下一条事件应该用的 seq"。之后的 [`push`](Self::push)
    /// 全部只在内存里操作，不再碰磁盘。
    pub fn open(root: &'a StorageRoot) -> Result<Self, JournalError> {
        let epoch = match current_epoch(root)? {
            Some(epoch) => epoch,
            None => {
                let epoch = crate::ids::random_hex32();
                create_epoch_pointer(root, &epoch)?;
                epoch
            }
        };
        let text = read_epoch_text(root, &epoch)?;
        let events = arca_format::journal::parse_stream(&text).map_err(JournalError::Format)?;
        let next_seq = events.last().map(|e| e.seq + 1).unwrap_or(1);
        Ok(Self {
            root,
            epoch,
            events,
            next_seq,
        })
    }

    /// 下一条事件应该使用的 `seq`——调用方（`execute_tombstone`）用它构造
    /// 待追加的 [`JournalEvent`]，语义与独立函数 [`next_seq`] 相同，只是这里
    /// 全程只读内存里已经缓存的状态，不重新触发一次磁盘读取。
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// 在内存里追加一条事件——`event.seq` 必须等于 [`next_seq`](Self::next_seq)，
    /// 否则拒绝（[`JournalError::SeqMismatch`]），与 [`append`] 同一条纪律，
    /// 只是校验对象是内存里的批次状态，不是重新读一遍磁盘。
    pub fn push(&mut self, event: JournalEvent) -> Result<(), JournalError> {
        if event.seq != self.next_seq {
            return Err(JournalError::SeqMismatch {
                expected: self.next_seq,
                actual: event.seq,
            });
        }
        self.next_seq += 1;
        self.events.push(event);
        Ok(())
    }

    /// 收口：把批次内全部事件（含打开批次时已经存在的历史事件）整体序列化，
    /// 原子写一次。调用方必须显式调用——不调用就丢弃 `AppendBatch`，本批次
    /// 累积的事件不会落盘（与不调用 `arca_store::atomic::Batch::commit` 同一
    /// 条纪律：未提交的批次视为没发生过，不会有部分写入）。
    pub fn commit(self) -> Result<(), JournalError> {
        let mut content = String::new();
        for event in &self.events {
            content.push_str(&event.to_line().map_err(JournalError::Format)?);
            content.push('\n');
        }
        atomic::write(self.root, &journal_path(&self.epoch), content.as_bytes())
            .map_err(JournalError::Atomic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_format::hub_layout::FormatJson;
    use arca_format::journal::Op;
    use arca_format::model::{Actor, ItemId, VersionId};

    fn 造存储根(dir: &Path) {
        fs::create_dir_all(dir.join(".arca")).unwrap();
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join(".arca/tmp")).unwrap();
        fs::create_dir_all(dir.join(".arca/journal")).unwrap();
        let format = FormatJson {
            format: 1,
            dataset_id: "9c41000000000000000000000000abcd".to_string(),
            hash_algo: "blake3".to_string(),
            created_at: "2026-08-08T09:00:00Z".to_string(),
        };
        fs::write(dir.join(".arca/format.json"), format.to_json().unwrap()).unwrap();
    }

    fn open(dir: &Path) -> StorageRoot {
        StorageRoot::open(dir, None).unwrap()
    }

    fn actor() -> Actor {
        Actor {
            account: "bruce".into(),
            device: "test".into(),
            session: "s1".into(),
        }
    }

    fn version_id(seed: u8) -> VersionId {
        VersionId::new("20260808T090000Z", &format!("{:032x}", seed as u128)).unwrap()
    }

    fn 样例事件(seq: u64, path: &str) -> JournalEvent {
        JournalEvent {
            seq,
            op: Op::Upsert,
            item_id: ItemId::from_bytes([0x3f; 16]),
            version_id: version_id(1),
            path: path.to_string(),
            from: None,
            actor: actor(),
            at: "2026-08-08T09:00:05Z".to_string(),
        }
    }

    #[test]
    fn epoch指针缺失时read_all返回none且不报错() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        assert_eq!(current_epoch(&root).unwrap(), None);
        let (cursor, events) = read_all(&root).unwrap();
        assert_eq!(cursor, None);
        assert!(events.is_empty());
    }

    #[test]
    fn next_seq在没有历史事件时为1_追加后依次递增() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        assert_eq!(next_seq(&root).unwrap(), 1);
        append(&root, &样例事件(1, "a.png")).unwrap();
        assert_eq!(next_seq(&root).unwrap(), 2);
        append(&root, &样例事件(2, "b.png")).unwrap();
        assert_eq!(next_seq(&root).unwrap(), 3);
    }

    #[test]
    fn 首次追加自动初始化epoch指针() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        append(&root, &样例事件(1, "a.png")).unwrap();

        let epoch = current_epoch(&root)
            .unwrap()
            .expect("首次追加后应已创建 epoch 指针");
        assert!(arca_format::model::is_hex32(&epoch));

        let (cursor, events) = read_all(&root).unwrap();
        assert_eq!(cursor, Some(Cursor { epoch, seq: 1 }));
        assert_eq!(events, vec![样例事件(1, "a.png")]);
    }

    #[test]
    fn 追加后可读回且保持顺序() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        append(&root, &样例事件(1, "a.png")).unwrap();
        append(&root, &样例事件(2, "b.png")).unwrap();
        append(&root, &样例事件(3, "c.png")).unwrap();

        let (cursor, events) = read_all(&root).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["a.png", "b.png", "c.png"]
        );
        assert_eq!(cursor.unwrap().seq, 3);
    }

    #[test]
    fn seq不连续时append拒绝写入() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        append(&root, &样例事件(1, "a.png")).unwrap();
        let err = append(&root, &样例事件(3, "b.png")).unwrap_err();
        match err {
            JournalError::SeqMismatch { expected, actual } => {
                assert_eq!(expected, 2);
                assert_eq!(actual, 3);
            }
            other => panic!("应为 SeqMismatch，实得 {other:?}"),
        }

        // 拒绝的写入不应该改变磁盘上的事件流。
        let (_, events) = read_all(&root).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn seq从1开始必须是1不能跳过() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        let err = append(&root, &样例事件(2, "a.png")).unwrap_err();
        match err {
            JournalError::SeqMismatch { expected, actual } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 2);
            }
            other => panic!("应为 SeqMismatch，实得 {other:?}"),
        }
    }

    #[test]
    fn 末行撕裂被截断而不是报错() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        append(&root, &样例事件(1, "a.png")).unwrap();

        // 手工模拟"崩溃在第二条事件写到一半"：在合法的第一行之后拼接一段
        // 不完整的 JSON，不经过 append（append 会先解析、绝不会写出这种
        // 半截内容——这里是在模拟磁盘上曾经真的发生过一次不完整写入）。
        let epoch = current_epoch(&root).unwrap().unwrap();
        let path = dir.path().join(format!(".arca/journal/{epoch}.jsonl"));
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str(r#"{"v":1,"seq":2,"op":"up"#);
        fs::write(&path, content).unwrap();

        let (cursor, events) = read_all(&root).unwrap();
        assert_eq!(events.len(), 1, "撕裂的末行应被截断，不计入");
        assert_eq!(cursor.unwrap().seq, 1);
    }

    #[test]
    fn 撕裂的末行在下一次成功追加后被治愈() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        append(&root, &样例事件(1, "a.png")).unwrap();
        let epoch = current_epoch(&root).unwrap().unwrap();
        let path = dir.path().join(format!(".arca/journal/{epoch}.jsonl"));
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str(r#"{"v":1,"seq":2,"op":"up"#);
        fs::write(&path, content).unwrap();

        // 下一次追加必须把 seq 当成"1 之后应该是 2"（撕裂的残留不算数），
        // 且写入后磁盘上不应再残留那段撕裂文本。
        append(&root, &样例事件(2, "b.png")).unwrap();

        let final_text = fs::read_to_string(&path).unwrap();
        assert!(
            !final_text.contains(r#""op":"up"#)
                || final_text.matches(r#""op":"upsert""#).count() == 2,
            "撕裂残留必须被治愈，文件里不应再有半截的 op 片段：{final_text:?}"
        );

        let (cursor, events) = read_all(&root).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(cursor.unwrap().seq, 2);
    }

    #[test]
    fn 中间行损坏则read_all整体失败而不是跳过() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        append(&root, &样例事件(1, "a.png")).unwrap();
        let epoch = current_epoch(&root).unwrap().unwrap();
        let path = dir.path().join(format!(".arca/journal/{epoch}.jsonl"));

        // 在第一条合法事件之后拼接一整行损坏内容——这不再是"末行撕裂"
        // （它后面还有换行，是一条语法完整但内容非法的行），必须报错。
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str("不是合法的journal事件\n");
        fs::write(&path, content).unwrap();

        let err = read_all(&root).unwrap_err();
        assert!(matches!(err, JournalError::Format(_)), "实得 {err:?}");
    }

    #[test]
    fn 中间行损坏时append也拒绝写入() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        append(&root, &样例事件(1, "a.png")).unwrap();
        let epoch = current_epoch(&root).unwrap().unwrap();
        let path = dir.path().join(format!(".arca/journal/{epoch}.jsonl"));
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str("不是合法的journal事件\n");
        fs::write(&path, content).unwrap();

        // 绝不能在已知损坏的 journal 上继续追加，制造一份看起来更完整、
        // 实则建在损坏地基上的假象。
        let err = append(&root, &样例事件(2, "b.png")).unwrap_err();
        assert!(matches!(err, JournalError::Format(_)), "实得 {err:?}");
    }

    #[test]
    fn rename事件必须携带from() {
        let dir = tempfile::tempdir().unwrap();
        造存储根(dir.path());
        let root = open(dir.path());

        let event = JournalEvent {
            seq: 1,
            op: Op::Rename,
            item_id: ItemId::from_bytes([0x22; 16]),
            version_id: version_id(1),
            path: "新.png".to_string(),
            from: Some("旧.png".to_string()),
            actor: actor(),
            at: "2026-08-08T09:00:05Z".to_string(),
        };
        append(&root, &event).unwrap();

        let (_, events) = read_all(&root).unwrap();
        assert_eq!(events[0].op, Op::Rename);
        assert_eq!(events[0].from.as_deref(), Some("旧.png"));
    }
}
