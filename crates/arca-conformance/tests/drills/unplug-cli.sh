#!/bin/sh
# 拔盘演练：客户端侧（M2d Task 5，spec §12.3 M2 验收原文：
# 「拔盘演练：卷离线呈现为数据集离线而非空库（I11）」）。
#
# 用法：unplug-cli.sh [<workdir>]
#   workdir  演练用的临时目录（默认 mktemp -d 新建一个）；不自动清理，
#            方便 CI 失败时把现场当 artifact 上传排查。
#
# 环境变量：
#   ARCA_BIN  已编译的 arca 二进制路径（默认相对本脚本定位
#             "<repo 根>/target/debug/arca"）
#
# 退出码：0 = 全部断言通过；非 0 = 某条断言失败（脚本本身用 set -e，
# 任何一步失败都会让脚本以非零退出并打印是哪一步）。
#
# 依赖：POSIX shell + coreutils + git + 已编译好的 arca 二进制。这与
# `tests/escape-hatch/` 的"不含任何 arca 代码"刻意不同——本演练测的正是
# arca 自己的 I11 挂载校验代码路径是否真的按承诺工作，不是逃生舱那个
# "完全不需要 arca 代码"的独立承诺。
#
# 正反两面都要断言，只跑正例的演练是假绿——M0 逃生舱脚本在评审里三次被
# 抓到"该失败时报成功"，这里刻意把两个方向都写成显式断言函数，而不是
# 只跑一遍流程、盯着最后的退出码猜结论。
set -eu

unset CDPATH
repo_root=$(cd -- "$(dirname -- "$0")/../../../.." && pwd)
arca_bin=${ARCA_BIN:-"$repo_root/target/debug/arca"}
workdir=${1:-$(mktemp -d)}

command -v git >/dev/null 2>&1 || {
    echo "缺少依赖：git" >&2
    exit 2
}
[ -x "$arca_bin" ] || {
    echo "找不到可执行的 arca 二进制：${arca_bin}（先跑 cargo build -p arca-cli，\
或用 ARCA_BIN 指定路径）" >&2
    exit 2
}

vault_dir="$workdir/vault"
store_dir="$workdir/store"
unplugged_dir="$workdir/store-unplugged"

fail() {
    echo "拔盘演练失败：$1" >&2
    exit 1
}

# 断言一条命令以给定退出码结束，且 stderr 包含指定子串（离线诊断必须点明
# 原因，不能只给一个裸的非零退出码让人猜）。
assert_offline() {
    label=$1
    shift
    out=$("$@" 2>&1) && rc=0 || rc=$?
    if [ "$rc" -eq 0 ]; then
        fail "$label 在盘被拔走之后仍然报成功（退出码 0）——I11 校验失效，\
这正是本演练要抓的反面：$out"
    fi
    case "$out" in
        *离线*) ;;
        *) fail "$label 的输出没有点明「离线」（I11 诊断必须说清楚是挂载缺失，\
不能只是个裸错误码）：$out" ;;
    esac
    echo "  OK：$label 在盘被拔走后报离线且退出码 $rc"
}

assert_clean() {
    label=$1
    shift
    if ! "$@" >/tmp/unplug-cli-out.$$ 2>&1; then
        cat /tmp/unplug-cli-out.$$ >&2
        rm -f /tmp/unplug-cli-out.$$
        fail "$label 应该干净退出 0，实际失败"
    fi
    rm -f /tmp/unplug-cli-out.$$
    echo "  OK：$label 正常完成"
}

echo "== 1. 建 vault + 数据集，adopt + 首次 sync =="
mkdir -p "$vault_dir/assets"
cd "$vault_dir"
git init -q
git config user.email drill@example.com
git config user.name drill
echo "拔盘演练的测试内容" > assets/note.txt

"$arca_bin" init . >/dev/null
"$arca_bin" register assets --hub home --hub-url "file://$store_dir" >/dev/null
# server 角色（M2d Task 1/2）：把已知副本数（M2d Task 4）垫到阈值之上——
# 本演练只关心 I11 挂载语义，不想被"副本数不足"这个正交的告警混进
# assert_clean 的判断里。
"$arca_bin" role assets --set server >/dev/null
"$arca_bin" adopt assets >/dev/null

assert_clean "首次 sync（盘在场）" "$arca_bin" sync assets
assert_clean "status（盘在场，应为干净）" "$arca_bin" status assets
assert_clean "verify（盘在场）" "$arca_bin" verify assets

echo "== 2. 拔盘：把存储根整个移走 =="
mv "$store_dir" "$unplugged_dir"
[ ! -e "$store_dir" ] || fail "拔盘后 $store_dir 不应再存在"

echo "== 3. 反面断言：status / sync / verify 全部必须报离线且退出码非 0 =="
assert_offline "status" "$arca_bin" status assets
assert_offline "sync" "$arca_bin" sync assets
assert_offline "verify" "$arca_bin" verify assets

echo "== 4. 反面断言：本地一个文件都没被删 =="
[ -f "$vault_dir/assets/note.txt" ] || fail "本地文件在盘拔走后消失了——绝不可接受（I3/I11）"
content=$(cat "$vault_dir/assets/note.txt")
[ "$content" = "拔盘演练的测试内容" ] || fail "本地文件内容被改动了：$content"
echo "  OK：本地文件仍然原样存在（I3：同步路径无销毁权）"

echo "== 5. 反面断言：绝不能把离线误判成空库 =="
# 上面 assert_offline 已经断言了 stderr 含「离线」；这里额外确认不会出现
# "0 个问题"/静默成功这种"空库"式的措辞或退出码。
out=$("$arca_bin" status assets 2>&1) && rc=0 || rc=$?
[ "$rc" -ne 0 ] || fail "status 在离线时不应该退出 0（会被误判成'库是空的、一切正常'）"

echo "== 6. 插回：把存储根移回原位 =="
mv "$unplugged_dir" "$store_dir"

echo "== 7. 正面断言：恢复挂载后一切照常 =="
assert_clean "status（插回后）" "$arca_bin" status assets
assert_clean "sync（插回后）" "$arca_bin" sync assets
assert_clean "verify（插回后）" "$arca_bin" verify assets

echo
echo "拔盘演练全部通过（工作目录：${workdir}）"
