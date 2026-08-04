# arca 磁盘格式规范（FORMAT.md）

> 状态：**骨架，未定稿**。本文件与代码同仓、同评审、先行合并（I10：格式先于代码）。
> 格式变更需要 RFC 流程。规范落地于 `crates/arca-format`，并以 golden vectors 回归。
> 设计依据：docs/2026-08-03-arca-spec.md §4。

## 0. 版本与兼容性承诺

- 所有磁盘格式版本化（`schema` / `format.json`），只向前迁移，永不静默改格式。
- TODO：格式版本号协商与迁移规则。

## 1. hub 存储根布局（spec §4.2）

每个数据集一个存储根：

- `files/` —— 逃生舱（I1）：普通文件树，当前版本永远完整平放。
- `.arca/format.json` —— 格式版本 + `dataset_id` 卷身份标记（I11）。
- `.arca/index/` —— 路径 → item_id 映射。
- `.arca/items/` —— item_id → 元数据 + 版本链（append-only 平文件）。
- `.arca/chunks/` —— FastCDC 内容块（zstd 压缩，BLAKE3 寻址），仅服务历史版本与增量传输。
- `.arca/journal/` —— append-only 事件流，含 actor（I8），`epoch:seq` 游标。
- `.arca/trash/` —— tombstone 对象与保留期数据（I3）。
- `.arca/uploads/` —— 断点续传会话（幂等五元组）。
- `.arca/tmp/` —— 写入暂存（tmp → fsync → rename 原子提交）。
- `.arca/locks/` —— `arca.lock`（OS 级排他锁）+ `.txn` 事务日志。

TODO：逐目录的字节级格式定义。

## 2. vault 侧文件（spec §4.3）

- `/.gitarca` —— 根注册表（TOML，`schema = 1`）：hub 端点表 + 数据集声明表。
- `<dataset>/.arca/dataset.toml` —— 数据集自描述：`dataset_id` · `hub_instance_id` · 发布配置。
- `<dataset>/.arca/manifest` —— 行式清单（`#%arca-manifest v1`）：
  一行一条、按路径字节序排序、Tab 分隔、确定性序列化。
  字段：`路径 \t blake3:哈希 \t 字节数 \t mtime(RFC3339)`。
- `<dataset>/.arca/catalog/` —— 目录卡（纯文本，git 追踪）。
- `<dataset>/.arca/client/` —— 本地投影（gitignored，可抛弃，I9）。
- `/.gitignore` arca 标记块 —— 反选写法（`/<ds>/*` + `!/<ds>/.arca/` + `/<ds>/.arca/client/`）。

TODO：各文件的完整字段表与解析规则、路径规则（禁用字符、规范化）。

## 3. 身份与版本模型（spec §4.1）

- `item_id`：随机 128-bit，创建时分配，永不复用；跨改名稳定（I7）。
- `version`：`{version_id, item_id, parent_version, content_hash, size, mtime, actor, committed_at}`；
  hub 上线性历史。
- `content_hash`：BLAKE3 原生地址；SHA-256 懒计算缓存（互操作）。

TODO：序列化格式与 golden vectors。

## 4. 不变量对照

实现不得违反 spec §2 的 I1–I11；本规范每一节标注其约束来源。
