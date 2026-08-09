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
| [M2c journal 与 longpoll](M2c-journal与longpoll.md) | Transport 补四缺口 · /changes 游标 · longpoll · sid 闭环 · **HttpTransport 与两机端到端** | 9 | ✅ |
| [M2d 角色与拔盘演练](M2d-角色与拔盘演练.md) | server/client 副本角色 · 多 hub 独立故障域 · 副本数告警 · **带反面夹具的拔盘演练** | 6 | ✅ |
| [M2e TLS 与 bugreport](M2e-TLS与bugreport.md) | 本地回收站可管理 · **`arca gc`（第一个被授权销毁的命令）** · 健康检查支持 http(s) · TLS 指纹 pin · bugreport | 7 | ✅ |
| [M3a agentd 自动同步](M3a-agentd自动同步.md) | 单实例锁 · 每数据集独立回路与退避 · **`Transport::changes`（/changes 的第一个客户端消费者）** · 增量游标 · **agentd 崩溃演练** | 5 | ✅ |
| [M3b 本地 watcher](M3b-本地watcher.md) | 实时事件 → 去抖 → **溢出即全扫** · 四路唤醒 · 监听不可用即降级 · agentd 心跳与 `arca status` 可见性 | 3 | ✅ |
| [M3c 分级驻留策略](M3c-分级驻留策略.md) | `Intent` 三态 · 8 MiB 阈值/pin/热度 · 水化队列（合并·限流·拒绝） · `Provider` 边界与全量物化 · **§6.3 第 7 条必过测试** | 3 | ✅ |

后续切片见 [M1a 文档末尾的拆分表](M1a-存储根IO地基.md#m1-的其余切片)。

## 阅读顺序建议

想了解**项目为什么这样设计**：读 `docs/2026-08-03-arca-spec.md`（唯一真相源）。
想了解**磁盘上到底是什么字节**：读 `FORMAT.md`。
想了解**实现过程中哪些假设被推翻了**：读这个目录。
