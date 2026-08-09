#!/bin/sh
# 反面夹具（M3a Task 4）：模拟「手动命令开始依赖 agentd」这个具体回归。
#
# 包一层真实 arca 二进制——`init`/`register`/`adopt` 照常透传，但
# `sync`/`status`/`verify` 在**没有 agentd 在跑**的时候直接失败，就像某次
# 重构把「从 daemon 拿状态」写成了硬依赖一样。
#
# 这正是 `agentd-crash.sh` 要抓的东西：CLAUDE.md 的分层降级关系说
# 「agentd 崩了，手动命令必须照常工作」，而一旦有人把这条打破，
# 演练必须变红。用本夹具重跑 `agentd-crash.sh`，那次运行**必须以非零退出**；
# 如果它反而报告"全部通过"，说明演练自己的断言逻辑失效了。
#
# 为什么不复用 `fake-always-success-wrapper.sh`：那个夹具模拟的是
# 「I11 挂载校验失效、离线也报成功」，它强制**成功**——而本演练的正例本来
# 就期望成功、真正的判据是字节是否落地，所以那个夹具对本演练是个空操作。
# 一个夹具只能证伪它模拟的那一种回归，用错夹具比没有夹具更危险：
# 它会让人以为演练被验过了。
#
# 用法：REAL_ARCA_BIN=<真实 arca 路径> ARCA_BIN=<本文件路径> agentd-crash.sh <dir>
set -eu

real=${REAL_ARCA_BIN:?必须设置 REAL_ARCA_BIN 指向真实 arca 二进制}
sub=${1:-}

case "$sub" in
    sync | status | verify)
        if ! pgrep -f arca-agentd >/dev/null 2>&1; then
            echo "（反面夹具）$sub 需要 arca-agentd 在运行" >&2
            exit 1
        fi
        ;;
esac

exec "$real" "$@"
