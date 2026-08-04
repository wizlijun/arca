# 逃生舱恢复演示（I1，进 CI）

I1「逃生舱」的承诺：hub 上的库是一棵普通文件树（`files/`）+ 一个旁路的元数据目录
（`.arca/`）。**删掉 arca，用 shell + coreutils 就能把数据完整取回来。** 用户押注的是
三十年后还能打开——尤其当这些文件是他们笔记里唯一的照片副本。所以这条承诺不能只是
写在 README 里的一句话，必须是每晚在 CI 里跑的可执行断言。本目录就是那个断言。

## 文件

- `recover.sh <dataset_root> <dest>` —— 恢复演示本体：把 `files/` 整体拷到 `dest`，
  再用 `.arca/items/` 里每个 item 的**当前版本**记录逐个校验大小与 BLAKE3 哈希，
  路径从 `.arca/index/` 反查（items 记录里只有 `item_id`，没有路径）。
  任何不一致都打印到 stderr 并以非零退出码结束；成功时打印一行统计到 stdout。
- `make-fixture.sh <dest>` —— 造一个最小但合法的存储根：一个文件 + `format.json` +
  一条 items 记录 + 一条 index 记录，布局与字段取值与
  `crates/arca-store/tests/fsck.rs` 里的 `造一个健康的存储根` 同构，便于交叉核对
  arca 自己的 `arca fsck` 与本演示是否看法一致。

**这两个文件里不得出现任何 arca 代码**：不调用 `arca` / `arcad`，不 `cargo run`，
不链接本仓库的任何 crate。整个恢复与夹具生成过程只用 POSIX shell + coreutils +
b3sum 现算哈希——这是演示的全部意义：如果恢复需要 arca 自己参与，那就不叫逃生舱。

## 依赖

- POSIX shell（`#!/bin/sh` + `set -eu`；不用 bash 专属语法——数组、`[[ ]]`、
  `local`、进程替换都不用，因为 NAS 用户可能是在 BusyBox ash 下运行，
  不是 bash。已在 macOS 自带 `/bin/sh`（bash 3.2 的 POSIX 兼容模式）、`dash`
  两种环境下手动跑通，并过了 `shellcheck -s sh` 零告警）。
- coreutils：`cp` / `grep` / `sed` / `awk` / `wc` / `mkdir` / `cut`。
- **`b3sum`**（BLAKE3 官方 CLI）。它严格说不属于 coreutils——I1 的承诺是
  「不需要任何 **arca** 代码」，而非「只用 coreutils」，`FORMAT.md` §11 已经诚实
  写明这一点，不回避。
  - macOS: `brew install b3sum`
  - 有 Rust 工具链: `cargo install b3sum`
  - CI（ubuntu-latest）：上面两种任一均可；也可以直接下载官方 release 二进制。
  - 两个脚本启动时都会先 `command -v b3sum` 自检，缺失时给出明确报错（退出码 2），
    不会静默跳过校验然后谎报成功。

## 本地跑一遍

```bash
chmod +x crates/arca-conformance/tests/escape-hatch/*.sh

crates/arca-conformance/tests/escape-hatch/make-fixture.sh /tmp/arca-fixture
crates/arca-conformance/tests/escape-hatch/recover.sh /tmp/arca-fixture /tmp/arca-recovered
# 预期：恢复并校验 1 个文件，0 个问题；退出码 0
```

### 确认演示真的会失败（而不是永远绿灯）

一个永远返回成功的校验脚本比没有脚本更糟——它会让人以为逃生舱被验证过了。
所以每次改动 `recover.sh` 之后，除了正例，也要跑一遍反例：

```bash
# 篡改夹具本身（不是篡改 dest！recover.sh 每次都会用 cp -R 整体重新覆盖 dest，
# 篡改 dest 会被下一次恢复悄悄冲掉，测不出问题）
printf 'tampered' > /tmp/arca-fixture/files/note.txt
crates/arca-conformance/tests/escape-hatch/recover.sh /tmp/arca-fixture /tmp/arca-recovered2
# 预期：退出码非 0，stderr 打印「问题: 哈希不符: note.txt」或「大小不符」
```

## items 版本链的损坏处置（为什么不只是 `grep '^{'`）

`items/<xx>/<item_id>.jsonl` 是 append-only 的版本链（`FORMAT.md` §7.1），
**当前版本是链上最后一条完整记录，不是第一条**。规范对两种损坏的处置刻意不同：

- **末行不完整**（进程写到一半被杀）→ 截断到最后一个完整行边界，这是崩溃后的
  正常残留，不算问题；
- **中间行损坏** → 必须失败，绝不静默跳过去找更早的「看起来完整」的行——那等于
  假装某个真实提交过的版本从未存在过。

只用 `grep '^{'` 判断「哪一行是完整的」是不够的：它只检查行首字符。一次写入如果
在写完某个**嵌套对象**（比如 `actor` 字段）之后、但在写完整条记录之前被杀掉，
残留的这一截**同时以 `{` 开头、以 `}` 结尾**（`}` 来自 `actor` 内层，不是整条记录
真正的收尾）——这不是假设，`recover.sh` 的作者在写这份文件时实测复现过这个场景。
所以 `recover.sh` 额外做了花括号配平检查（开合数量相等）才认定一行「完整」；
这仍不是完整的 JSON 校验（比如字段值里恰好含不配对花括号的病态输入不会被挡住），
但比只看首尾字符强得多，且不需要引入 `jq` 依赖。深度校验（含花括号配平之外的
情形）是 `arca fsck` 的职责，不是这个不含 arca 代码的演示脚本的职责。

## 在 CI 中调用

建议作为每晚（cron）而非每次 PR 触发的独立 job，因为它验证的是长期承诺而不是
本次改动：

```yaml
escape-hatch:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - name: 安装 b3sum
      run: cargo install b3sum --locked
    - name: 造夹具并跑恢复演示
      run: |
        chmod +x crates/arca-conformance/tests/escape-hatch/*.sh
        crates/arca-conformance/tests/escape-hatch/make-fixture.sh "$RUNNER_TEMP/arca-fixture"
        crates/arca-conformance/tests/escape-hatch/recover.sh "$RUNNER_TEMP/arca-fixture" "$RUNNER_TEMP/arca-recovered"
```

退出码非 0 即失败整个 job——不需要额外断言，`recover.sh` 自己的退出码就是答案
（Rule of Silence：成功时只有一行统计，不需要人读日志判断对错）。
