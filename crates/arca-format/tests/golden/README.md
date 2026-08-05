# golden vectors

版本化样例库（spec §11.2）：每种磁盘格式的合法/非法样例进仓库，
跨版本兼容性回归。任何格式变更必须同时更新此处并通过全部旧样例。

往返测试对改名不变是设计目的，不是免费获得的——样例文本必须是**冻结的字面
字节**（写死在 `.jsonl`/`.json` 文件里），而不是「代码构造一个值 → 用同一份代码
序列化 → 用同一份代码再解析回来」。后者只能证明 `to_line`/`parse_line` 两个
函数彼此还认得，任何一次把线上字段名/顺序一起改掉的破坏性变更都会被这种
自产自销的往返测试放过；只有把样例字节钉死在仓库里、由测试断言「解析 →
重新序列化」逐字节等于这份冻结文本，才能真正锁住磁盘/线上格式（评审
Important #7 的教训：`items`/`journal`/`index`/`format.json` 这四个承载用户
身份与版本数据的格式此前完全没有 golden vectors，字节契约只是碰巧成立）。

- `gitarca/` —— 注册表样例（TODO）
- `dataset/` —— dataset.toml 样例（TODO）
- `manifest/` —— 行式清单样例，含确定性序列化断言
- `items/` —— hub 版本链记录样例（FORMAT.md §7.1）：`basic.jsonl` 两条记录，
  构成一条合法的线性版本链（首版 `parent: null`，第二版指回首版）
- `journal/` —— hub 事件流样例（FORMAT.md §7.2）：`basic.jsonl` 两条事件
  （`upsert` seq 42 → `tombstone` seq 43），同时覆盖 seq 连续性校验
- `index/` —— hub 路径 → 身份映射样例（FORMAT.md §6）
- `format-json/` —— hub 卷身份标记样例（FORMAT.md §5）
- `trace/` —— trace 事件流样例（FORMAT.md §10）：`basic.jsonl` 合法、逐字节往返；
  `damaged.jsonl` 损坏，锁住「坏行跳过并计数、未知事件原样透传」这条**与 journal 相反**的纪律
