# 里程碑归档

每个完成的阶段一份总结，供日后回溯：交付了什么、做了哪些偏离原规格的决定及理由、
评审抓到了什么、留了什么给后续。

这些文档的存在理由是：实现过程中的推敲记录（评审报告、修复轮的往返）保存在
`.superpowers/sdd/` 下，那是 git 忽略的临时目录，随时会被清掉。真正有长期价值的部分——
**为什么这么定**、**哪些坑是被实测踩出来的**——必须落到仓库里，否则半年后没人记得
「MSRV 为什么是 1.85 而不是 spec 里写的 1.75」。

| 阶段 | 交付 | 提交 | 状态 |
| --- | --- | --- | --- |
| [M0 格式与核心](M0-格式与核心.md) | FORMAT.md v1 · arca-format · arca-chunk · fsck · trace schema · fuzz 与 CI | 50 | ✅ |
| [M1a 存储根 IO 地基](M1a-存储根IO地基.md) | StorageRoot 身份校验（I11）· 原子写入 · tmp 清理 | 10 | ✅ |
| [M1b 调和状态机](M1b-调和状态机.md) | 18 格三态决策表 · reconcile.decide 发射 · proptest 收敛性 · 确定性模拟 | 14 | ✅ |
| [M1c arca-git](M1c-arca-git.md) | `.gitignore` 反选块 · 追踪冲突检测 · pre-push 钩子 · 噩梦路径 | 6 | ✅ |
| [M1d CLI 与 file:// 同步闭环](M1d-CLI与file同步闭环.md) | init/register/adopt/sync/status/verify/doctor + plumbing · 批量提交 · trace 落盘 | 12 | ✅ |
| [M2a tombstone 与删除安全地基](M2a-tombstone与删除安全地基.md) | 下载 fsync · hub journal · tombstone · **删除传播四道闸门** · restore | 12 | ✅ |
| [M2b arcad 与 HTTP CAS](M2b-arcad与HTTP-CAS.md) | Transport 抽象 · PROTOCOL §1.2 定稿 · arcad 服务端 · CAS 写入 · arca.lock | 4 | ✅ |

后续切片见 [M1a 文档末尾的拆分表](M1a-存储根IO地基.md#m1-的其余切片)。

## 阅读顺序建议

想了解**项目为什么这样设计**：读 `docs/2026-08-03-arca-spec.md`（唯一真相源）。
想了解**磁盘上到底是什么字节**：读 `FORMAT.md`。
想了解**实现过程中哪些假设被推翻了**：读这个目录。
