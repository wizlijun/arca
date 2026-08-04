# 架构决策记录（ADR）

文档先行（spec §12.4）：重要架构决策公开记录；每个魔法数字有出处
（继承 lazync LIMITS.md 的纪律）。

格式：`NNNN-标题.md`，包含：背景 / 决策 / 备选方案 / 后果。

已由 spec 定案、无需重复开 ADR 的决策（引用 spec 章节即可）：

- 目录级归属，放弃 glob（§4.3.1）
- manifest 行式而非 TOML（§4.4.1）
- hub-and-spoke + 线性历史，放弃版本向量（§5.1）
- current 平放、历史 CDC 分块（§4.2）
- 客户端手动同步为基线，daemon 为可选增强（§3.1）
- 不做 clean/smudge filter（§1.2）

后续新决策从 `0001-….md` 开始编号。
