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
//! TODO(M1)：块生成/更新/移除、check-ignore 断言接口。
