//! git 钩子：pre-push 一致性钩子 + 预留钩子点（spec §3.2、§4.4.2）。
//!
//! pre-push（沿用 Git LFS 惯例，由 `arca init` 安装，可拒绝、可 `--no-verify` 绕过）：
//! 本次推送涉及的清单条目未全部在 hub 落地 → 阻止 push，列出未上传文件与进度。
//! **只读不改**：从不修改提交、从不自动 push，只做一致性断言（I5）。
//!
//! 预留钩子点（机制不是策略）：post-pull、pre-adopt、post-conflict。
//!
//! TODO(M1)：钩子安装/卸载、pre-push 检查逻辑。
