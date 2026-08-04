# arca 线上协议规范（PROTOCOL.md）

> 状态：**骨架，未定稿**。与代码同仓、同评审（I10）。
> 设计依据：docs/2026-08-03-arca-spec.md §3.1、§5、§8。

## 0. 原则

- 能复用公开规范的绝不发明私有协议：条件请求 / ETag / Range 全部对齐 RFC 9110。
- ETag = BLAKE3 内容哈希；一切写入走 CAS（If-Match，I4），过期即 412。
- 传输与对象模型分离：v1 支持 `https://`（经 arcad）与 `file://` 直连；`ssh://` 列 v2。

## 1. 传输

### 1.1 file:// 直连（M1）

- dataset_root 所在卷本地挂载时，无 daemon 完成同步；排他由 `arca.lock` 保证。
- TODO：锁协议、崩溃恢复（`.txn` 前滚/回滚）。

### 1.2 HTTP API（M2，arcad）

- 条件请求：If-Match / If-None-Match / 412 Precondition Failed。
- 断点续传：Range / 206 + If-Match 版本钉住。
- 变更流：longpoll（30–90 秒挂起）+ SSE（agent 场景）；2 秒短轮询为降级路径。
- 挂载缺失即离线：数据集离线 → 503，绝不呈现为空库（I11）。
- TODO：端点表、请求/响应格式、错误码表。

## 2. 上传协议

- 分块断点续传、幂等五元组、丢失 commit 的 no-op 恢复（继承 Lazync §7）。
- 修改过的文件只传变化的 CDC 块（FastCDC + BLAKE3 索引，hub 已有块跳过）。
- 上传前两轮稳定性签名防抖。
- TODO：会话状态机定义。

## 3. journal 与游标

- append-only、`epoch:seq` 游标、压缩后 `reset_required` 全量对账兜底。
- 每个事件带 actor（账号 + 设备/agent + 会话，I8）。
- TODO：事件类型表与序列化。

## 4. 认证与令牌

- 密码 → 设备令牌（服务端只存哈希）→ 内存会话；agent 令牌为第四形态
  `{scope, caps, ttl, actor_label}`（spec §9）。
- TODO：握手流程。

## 5. plumbing 输出契约（spec §3.2）

`arca ls --json` / `arca cat <hash>` / `arca resolve <path>` / `arca state dump --json`
的输出格式与退出码属于本规范，受兼容性承诺约束。

- Rule of Silence：成功时安静；数据走 stdout，进度与诊断走 stderr。
- TODO：各命令的 JSON schema 与退出码表。

## 6. Git LFS 桥（M5）

- 实现 LFS Batch API 与指针格式（oid 为 SHA-256，懒计算缓存）。
- TODO：映射规则。
