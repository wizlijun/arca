# 拔盘演练（I11，进 CI）

spec §12.3 的 M2 验收原文：「**拔盘演练：卷离线呈现为数据集离线而非空库（I11）**」。
`arca-store::root::StorageRoot::open`（客户端/CLI）与 `arcad::storage`（服务端）
各自都断言过挂载缺失时的错误类型，但那都是**单元测试**——真正的承诺是"整条命令行/
整条 HTTP 请求路径，从进程外部看，绝不会把'盘不在了'误报成'库本来就是空的'"。
本目录把这条承诺变成两个可执行的 shell 脚本，进 CI 每次 push/PR 都跑（不等每晚）。

## 文件

- `unplug-cli.sh [<workdir>]` —— 客户端侧：建 vault + 数据集 → `adopt` → `sync` →
  把存储根整个移走（模拟拔盘）→ 断言 `arca status`/`sync`/`verify` **全部报离线
  且退出码非 0**、**本地一个文件都没被删**（I3）→ 插回存储根 → 断言恢复正常。
- `unplug-arcad.sh [<workdir>]` —— 服务端侧：起一个真实的 `arcad` HTTP 进程，绑
  两个数据集，移走其中一个的存储根，断言**该数据集的每次请求都是 503，另一个数据集
  照常 200**（spec §4.3.2 独立故障域——不是"进程没崩"，是"没受牵连"）。这与
  `crates/arcad/src/api.rs` 里同名断言的 axum in-process 测试互补：那个测试跑得快、
  不需要真实网络；这个脚本验证的是真实二进制、真实 TCP 端口的行为，两者不是同一件事。
- `fake-always-success-wrapper.sh` —— 反面夹具：包一层真实 `arca` 二进制，把
  `status`/`sync`/`verify` 的退出码强制改写成 0（模拟"I11 校验失效、离线时也报成功"
  这个具体回归），配合 CI 里的"断言演练脚本自己也会被假绿骗过这件事不成立"步骤使用。

## 为什么客户端侧要用真实二进制，不是纯 shell（与 `tests/escape-hatch/` 的刻意不同）

`tests/escape-hatch/` 的存在理由是"就算 arca 全灭，数据也能用 coreutils 徒手拿回来"，
所以那两个脚本**不得出现任何 arca 代码**。本目录测的是相反的东西：**arca 自己的 I11
挂载校验代码路径，是否真的按承诺工作**——这必须调用真实的 `arca`/`arcad` 二进制，
不能绕过它们自己写一套等价逻辑（那样测的就是脚本作者的理解，不是 arca 的实现）。

## 只跑正例的演练是假绿

M0 的逃生舱恢复演示在评审里被抓到过三次"该失败时报成功"。本目录的两个正例脚本都
显式写了对称的反面断言（`assert_offline`/HTTP 503 检查），CI 额外用
`fake-always-success-wrapper.sh` 验证了"如果检测逻辑本身失效，演练必须能发现"——
见 `.github/workflows/ci.yml` 的 `unplug-drills` 作业最后一步。

## 依赖

POSIX shell + coreutils + git + curl（仅 `unplug-arcad.sh`）+ 已编译好的 `arca`/
`arcad` 二进制（`cargo build -p arca-cli -p arcad --bins`）。已过 `shellcheck -s sh`
零告警；在 macOS 自带 `/bin/sh`（bash 3.2 POSIX 兼容模式）与 Linux `dash` 下手动跑通
——注意 bash 3.2 对"变量展开后紧跟多字节字符"有个已知的词法解析问题（例如
`"$var）"` 在某些位置会报 `unbound variable`），本目录的脚本因此统一用 `${var}`
（带花括号）而不是裸 `$var`，绕开这个坑。

## 用法

```sh
cargo build -p arca-cli -p arcad --bins
crates/arca-conformance/tests/drills/unplug-cli.sh
crates/arca-conformance/tests/drills/unplug-arcad.sh
```

两个脚本都接受一个可选的 `workdir` 参数（默认 `mktemp -d` 新建一个），不自动清理，
方便失败时把现场当 CI artifact 上传排查。`ARCA_BIN`/`ARCAD_BIN`/`ARCAD_BIND` 三个
环境变量可覆盖默认的二进制路径与监听地址。
