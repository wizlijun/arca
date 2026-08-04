//! porcelain 命令（plumbing 之上的薄壳，spec §3.2、§12.3 M1–M2）：
//!
//! | 命令 | 语义 |
//! | --- | --- |
//! | `setup` | 一次性引导：读 `.gitarca` → 建绑定 → 选角色（对齐 `git lfs install` 的角色） |
//! | `adopt` | 就地纳管既有附件：算哈希、上传、写 `.gitignore` 块，**文件原地不动**（只阻止未来膨胀，不瘦身历史——输出里必须讲清楚） |
//! | `add` / `register` | 新数据集声明 / 孤儿数据集显式登记 |
//! | `status` | 比对本地与 hub，不动数据；按数据集分别报告健康度与 server 副本数 |
//! | `fetch` / `pull` / `push` / `sync` | 与 git 动词语义对齐；file:// 或 https:// |
//! | `verify` | fixity 巡检（BLAKE3 重算对账），机器可读报告 |
//! | `history` / `restore` | 版本链查看 / 保留期内一条命令找回 |
//! | `gc` | 显式销毁（I3）：`--dry-run` 先出清单 |
//! | `bundle` | 自包含归档交付（含 `--verify` 离线校验，§4.4.3） |
//! | `doctor` | 一致性断言：`.gitignore` 反选块（`git check-ignore` 实测）、孤儿数据集、缺失文件统计 |
//! | `rebuild` | 投影删掉重建 + adopt 认领（I9） |
//! | `pin` / `unpin` | 驻留策略（M3） |
//! | `import` | Dropbox / Google Drive / LFS 迁入，厂商校验和验证 + 审计报告（M5） |
//! | `publish-map` / `export` | 发布（M5，委托 arca-publish） |
//!
//! TODO(M1 起)：逐命令实现。
