# golden vectors

版本化样例库（spec §11.2）：每种磁盘格式的合法/非法样例进仓库，
跨版本兼容性回归。任何格式变更必须同时更新此处并通过全部旧样例。

- `gitarca/` —— 注册表样例（TODO）
- `dataset/` —— dataset.toml 样例（TODO）
- `manifest/` —— 行式清单样例，含确定性序列化断言（TODO）
- `format-json/` —— hub 卷身份标记样例（TODO）
- `trace/` —— trace 事件流样例（FORMAT.md §10）：`basic.jsonl` 合法、逐字节往返；
  `damaged.jsonl` 损坏，锁住「坏行跳过并计数、未知事件原样透传」这条**与 journal 相反**的纪律
