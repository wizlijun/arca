#!/bin/sh
# agentd 崩溃演练（M3a Task 4，spec §3.1 的分层降级关系）。
#
# CLAUDE.md 里那句「agentd 崩了，手动命令必须照常工作」是承诺，不是注释。
# 本脚本把它变成可执行断言：
#
#   1. agentd 确实在自动同步（不是「起来了没报错」，是**字节真的过去了**）
#   2. `kill -9` agentd——最难看的死法，不给它任何清理机会
#   3. 断言 `arca sync`/`status`/`verify` 全部照常工作、退出码正确、数据完好
#   4. 断言 agentd 没有留下任何让手动命令无法继续的中间态
#   5. agentd 可以重新起来接着干（锁被内核释放了，不需要手工删文件）
#
# 反面断言由 CI 用 `fake-always-success-wrapper.sh` 提供：把手动命令换成
# 「无论真实结果如何都报成功」的包装，本演练必须失败——否则它抓不住
# 「手动命令其实已经依赖 agentd 了」这个真实回归。
#
# 用法：agentd-crash.sh <工作目录>
#   ARCA_BIN / ARCA_AGENTD_BIN 可覆盖二进制路径（反面夹具用）。
set -eu

work=${1:?用法：agentd-crash.sh <工作目录>}
root=$(cd "$(dirname "$0")/../../../.." && pwd)
ARCA=${ARCA_BIN:-$root/target/debug/arca}
AGENTD=${ARCA_AGENTD_BIN:-$root/target/debug/arca-agentd}

fail() { echo "演练失败：$*" >&2; exit 1; }
step() { echo; echo "== $* =="; }

rm -rf "$work"
mkdir -p "$work"
vault=$work/vault
store=$work/store
mkdir -p "$vault/assets" "$store"

step "0. 建 vault、纳管"
printf 'ORIGINAL\n' > "$vault/assets/a.bin"
git -C "$vault" init -q
git -C "$vault" config user.email drill@example.com
git -C "$vault" config user.name drill
(cd "$vault" && "$ARCA" init . >/dev/null)
(cd "$vault" && "$ARCA" register assets --hub home --hub-url "file://$store" >/dev/null)
(cd "$vault" && "$ARCA" adopt assets >/dev/null)
[ -f "$store/files/a.bin" ] || fail "前置条件：adopt 之后 hub 上应当有 a.bin"

step "1. agentd 确实在自动同步（断言字节，不是断言日志）"
printf 'BY-AGENTD\n' > "$vault/assets/b.bin"
(cd "$vault" && "$AGENTD" --once) || fail "agentd --once 应当成功"
[ -f "$store/files/b.bin" ] || fail "agentd 没有把 b.bin 传上去——自动同步根本没在工作，后续断言没有意义"
cmp -s "$store/files/b.bin" "$vault/assets/b.bin" || fail "b.bin 内容不一致"
echo "  ✓ agentd 把 b.bin 传上去了"

step "2. 起一个长期运行的 agentd，然后 kill -9"
# `exec` 是关键：不加它，`$!` 拿到的是这层子 shell 的 pid，`kill -9` 打死
# 子 shell 而 agentd 作为孤儿进程继续活着——演练会在第 6 步以「锁泄漏了」
# 失败，而真正泄漏的是这个脚本自己的进程语义。第一版就踩了这个坑，
# 排查时先 `pgrep arca-agentd` 才看清楚是谁还活着。
(cd "$vault" && exec "$AGENTD" --interval 1 >"$work/agentd.log" 2>&1) &
agentd_pid=$!
# 等它真的起来（拿到锁、打出启动行），最多 10 秒。
i=0
while [ $i -lt 100 ]; do
    grep -q "已启动" "$work/agentd.log" 2>/dev/null && break
    sleep 0.1
    i=$((i + 1))
done
grep -q "已启动" "$work/agentd.log" 2>/dev/null || fail "agentd 没能在 10 秒内启动：$(cat "$work/agentd.log")"
kill -9 "$agentd_pid" 2>/dev/null || true
wait "$agentd_pid" 2>/dev/null || true
# 断言它真的死了——否则后面几步测的是「一个活着的 agentd 旁边手动命令能不能跑」，
# 那是另一回事，而且会以一条误导的信息（「锁泄漏了」）失败。
if kill -0 "$agentd_pid" 2>/dev/null; then
    fail "kill -9 之后 agentd（pid $agentd_pid）还活着——本演练的前提不成立"
fi
echo "  ✓ agentd 已被 kill -9 且确认已死（没有任何清理机会）"

step "3. 手动命令必须照常工作"
printf 'AFTER-CRASH\n' > "$vault/assets/c.bin"
(cd "$vault" && "$ARCA" sync assets) || fail "agentd 崩了之后 arca sync 也不工作——分层降级关系被破坏了"
[ -f "$store/files/c.bin" ] || fail "arca sync 报成功但 c.bin 没上去"
cmp -s "$store/files/c.bin" "$vault/assets/c.bin" || fail "c.bin 内容不一致"
echo "  ✓ arca sync 照常工作，c.bin 已同步"

(cd "$vault" && "$ARCA" status assets) || fail "agentd 崩了之后 arca status 应当退出 0（此刻已完全同步）"
echo "  ✓ arca status 退出 0"

(cd "$vault" && "$ARCA" verify assets) || fail "agentd 崩了之后 arca verify 应当退出 0"
echo "  ✓ arca verify 退出 0"

step "4. 原有数据完好——崩溃不该动任何一个字节"
for f in a.bin b.bin c.bin; do
    [ -f "$vault/assets/$f" ] || fail "$f 在工作区里不见了"
    cmp -s "$store/files/$f" "$vault/assets/$f" || fail "$f 在 hub 与工作区之间对不上"
done
echo "  ✓ a/b/c 三个文件在两侧逐字节一致"

step "5. 没有留下让后续操作卡住的中间态"
# `.arca/tmp` 里的残留由 arca 自己的 sweep 处理；这里断言的是**用户视角**：
# doctor 不该因为 agentd 崩过就报出问题。
(cd "$vault" && "$ARCA" doctor) || fail "agentd 崩溃之后 arca doctor 报出了问题——留下了需要人处理的中间态"
echo "  ✓ arca doctor 干净"

step "6. agentd 可以重新起来（锁随进程死亡被内核释放，不需要手工删文件）"
printf 'AFTER-RESTART\n' > "$vault/assets/d.bin"
(cd "$vault" && "$AGENTD" --once) || fail "被 kill -9 之后 agentd 起不来了——单实例锁泄漏了（这正是不用 pid 文件的理由）"
[ -f "$store/files/d.bin" ] || fail "重启后的 agentd 没有同步 d.bin"
echo "  ✓ agentd 重启成功并继续同步"

echo
echo "agentd 崩溃演练全部通过：agentd 是增强，不是依赖。"
