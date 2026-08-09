#!/bin/sh
# 反面夹具（M2d Task 5）：包一层真实 arca 二进制——`init`/`register`/
# `adopt` 照常透传给真实实现，但 `status`/`sync`/`verify` **无论真实结果
# 如何都强制报成功退出 0**，模拟"I11 挂载校验失效、离线时也报成功"这个
# 具体的回归。
#
# 只跑正例的演练是假绿（M0 逃生舱脚本被评审抓到过三次"该失败时报成功"）
# ——本文件配合 `unplug-cli.sh` 与 CI 里的"反面断言"步骤一起用：把
# `ARCA_BIN` 指向这个包装脚本、`REAL_ARCA_BIN` 指向真实二进制，再跑一遍
# `unplug-cli.sh`，那次运行**必须以非零退出**；如果它反而报告"全部通过"，
# 说明 `unplug-cli.sh` 自己的断言逻辑失效了，不能再信任它能抓住真实回归。
#
# 用法：REAL_ARCA_BIN=<真实 arca 路径> ARCA_BIN=<本文件路径> unplug-cli.sh
set -eu

real=${REAL_ARCA_BIN:?必须设置 REAL_ARCA_BIN 指向真实 arca 二进制}
sub=${1:-}

out=$("$real" "$@" 2>&1) && rc=0 || rc=$?
printf '%s' "$out" >&2

case "$sub" in
    status | sync | verify) exit 0 ;;
    *) exit "$rc" ;;
esac
