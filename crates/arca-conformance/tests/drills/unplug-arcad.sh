#!/bin/sh
# 拔盘演练：arcad 侧（M2d Task 5，spec §4.3.2 独立故障域 + §12.3 M2 验收）。
#
# 起一个真实的 arcad HTTP 进程（不是 axum in-process 测试——那部分已经在
# crates/arcad/src/api.rs 的「一个数据集离线不影响另一个数据集」测过），
# 绑两个数据集，移走其中一个的存储根，断言：那个数据集的每一次请求都是
# 503，**另一个数据集照常 200**——独立故障域不是"进程没崩"，是"没受牵连"。
#
# 用法：unplug-arcad.sh [<workdir>]
#
# 环境变量：
#   ARCA_BIN   已编译的 arca 二进制路径（默认 "<repo 根>/target/debug/arca"）
#   ARCAD_BIN  已编译的 arcad 二进制路径（默认 "<repo 根>/target/debug/arcad"）
#   ARCAD_BIND arcad 监听地址（默认 127.0.0.1:18523，本演练专用端口，
#              尽量避免与其它服务撞车）
#
# 依赖：POSIX shell + coreutils + git + curl + 已编译好的 arca/arcad 二进制。
set -eu

unset CDPATH
repo_root=$(cd -- "$(dirname -- "$0")/../../../.." && pwd)
arca_bin=${ARCA_BIN:-"$repo_root/target/debug/arca"}
arcad_bin=${ARCAD_BIN:-"$repo_root/target/debug/arcad"}
bind=${ARCAD_BIND:-127.0.0.1:18523}
workdir=${1:-$(mktemp -d)}

for tool in git curl; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "缺少依赖：$tool" >&2
        exit 2
    }
done
[ -x "$arca_bin" ] || {
    echo "找不到可执行的 arca 二进制：${arca_bin}（先跑 cargo build -p arca-cli）" >&2
    exit 2
}
[ -x "$arcad_bin" ] || {
    echo "找不到可执行的 arcad 二进制：${arcad_bin}（先跑 cargo build -p arcad）" >&2
    exit 2
}

fail() {
    echo "拔盘演练（arcad 侧）失败：$1" >&2
    exit 1
}

vault_dir="$workdir/vault"
store_a="$workdir/store-a"
store_b="$workdir/store-b"
unplugged_a="$workdir/store-a-unplugged"
hub_toml="$workdir/hub.toml"
arcad_log="$workdir/arcad.log"
arcad_pid=""

cleanup() {
    if [ -n "$arcad_pid" ] && kill -0 "$arcad_pid" 2>/dev/null; then
        kill "$arcad_pid" 2>/dev/null || true
        wait "$arcad_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

echo "== 1. 建两个数据集，各自独立的存储根 =="
mkdir -p "$vault_dir/a" "$vault_dir/b"
cd "$vault_dir"
git init -q
git config user.email drill@example.com
git config user.name drill
echo "dataset a" > a/one.txt
echo "dataset b" > b/one.txt

"$arca_bin" init . >/dev/null
"$arca_bin" register a --hub hub_a --hub-url "file://$store_a" >/dev/null
"$arca_bin" register b --hub hub_b --hub-url "file://$store_b" >/dev/null
"$arca_bin" adopt a >/dev/null
"$arca_bin" adopt b >/dev/null

id_a=$(sed -n 's/^dataset_id *= *"\(.*\)"/\1/p' "$vault_dir/a/.arca/dataset.toml")
id_b=$(sed -n 's/^dataset_id *= *"\(.*\)"/\1/p' "$vault_dir/b/.arca/dataset.toml")
[ -n "$id_a" ] && [ -n "$id_b" ] || fail "没能从 dataset.toml 解出 dataset_id"
echo "  数据集 a: ${id_a}（${store_a}）"
echo "  数据集 b: ${id_b}（${store_b}）"

cat > "$hub_toml" <<EOF
instance_id = "0123456789abcdef0123456789abcdef"

[[dataset]]
id = "$id_a"
path = "$store_a"

[[dataset]]
id = "$id_b"
path = "$store_b"
EOF

echo "== 2. 起 arcad =="
(
    cd "$workdir"
    exec "$arcad_bin" --config "$hub_toml" --bind "$bind"
) >"$arcad_log" 2>&1 &
arcad_pid=$!

# 轮询等待端口就绪，最多 5 秒——不用盲 sleep（慢机器/CI runner 首次冷启动
# 可能比本机开发环境慢很多）。
ready=0
i=0
while [ "$i" -lt 50 ]; do
    if curl -s -o /dev/null "http://$bind/v1/datasets/$id_a/state"; then
        ready=1
        break
    fi
    kill -0 "$arcad_pid" 2>/dev/null || fail "arcad 进程启动后意外退出，日志：$(cat "$arcad_log")"
    i=$((i + 1))
    sleep 0.1
done
[ "$ready" -eq 1 ] || fail "arcad 在 5 秒内没有起来监听 ${bind}，日志：$(cat "$arcad_log")"

http_code() {
    curl -s -o /dev/null -w '%{http_code}' "http://$bind/v1/datasets/$1/state"
}

echo "== 3. 正面断言：两个数据集此刻都应该是 200 =="
code_a=$(http_code "$id_a")
[ "$code_a" = "200" ] || fail "数据集 a 存储根健在时应为 200，实得 $code_a"
echo "  OK：数据集 a -> 200"
code_b=$(http_code "$id_b")
[ "$code_b" = "200" ] || fail "数据集 b 存储根健在时应为 200，实得 $code_b"
echo "  OK：数据集 b -> 200"

echo "== 4. 拔盘：移走数据集 a 的存储根 =="
mv "$store_a" "$unplugged_a"

echo "== 5. 核心断言：a 变 503，b 照常 200（独立故障域，spec §4.3.2） =="
code_a=$(http_code "$id_a")
[ "$code_a" = "503" ] || fail "数据集 a 拔盘后应为 503，实得 ${code_a}——I11 校验失效或没有正确映射成 503"
echo "  OK：数据集 a（已拔盘）-> 503"
code_b=$(http_code "$id_b")
[ "$code_b" = "200" ] || fail "数据集 b 完全没被动过，拔了 a 的盘之后 b 仍应是 200，实得 \
${code_b}——独立故障域被打破，一个数据集离线牵连了另一个"
echo "  OK：数据集 b（未受影响）仍然 -> 200"

echo "== 6. 插回：把数据集 a 的存储根移回原位 =="
mv "$unplugged_a" "$store_a"
code_a=$(http_code "$id_a")
[ "$code_a" = "200" ] || fail "插回后数据集 a 应恢复 200，实得 $code_a"
echo "  OK：数据集 a（已插回）恢复 -> 200"

echo
echo "拔盘演练（arcad 侧）全部通过（工作目录：${workdir}）"
