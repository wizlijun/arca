#!/bin/sh
# 逃生舱恢复演示（I1）——本文件不得出现任何 arca 代码：不调用 arca / arcad，
# 不链接本仓库的任何 crate。整个恢复与校验过程只依赖 POSIX shell + coreutils
# + b3sum。这正是这份脚本存在的全部意义：如果恢复需要 arca 自己参与，
# I1「逃生舱」的承诺就是假的。
#
# 用法：recover.sh <dataset_root> <dest>
#   dataset_root  一个 arca hub 存储根（含 files/ 与 .arca/，见 FORMAT.md §4）
#   dest          恢复目标目录（会被创建；已存在的同名文件会被覆盖）
#
# 退出码：
#   0   恢复完成且逐文件哈希/大小校验通过
#   2   用法错误 / 存储根不合法（缺 files/）
#   1   恢复完成但发现至少一个不一致（大小、哈希、缺文件、索引缺失、版本链损坏）
#
# 依赖：POSIX shell + coreutils（cp / grep / sed / awk / wc）+ b3sum。
# b3sum 是 BLAKE3 的官方 CLI，严格说不属于 coreutils——I1 的承诺是
# 「不需要任何 arca 代码」，而非「只用 coreutils」，FORMAT.md §11 已明示这一点。
#
# 兼容性：刻意只用 POSIX sh 语法（无数组、无 [[ ]]、无 local、无进程替换），
# 因为 NAS 用户可能是在 BusyBox ash 下运行本脚本，而不是 bash。
# 变量名刻意使用 ASCII——中文标识符在 dash / busybox ash 下无法解析
# （`哈希=abc` 会被当成一条名为 `哈希=abc` 的外部命令去执行，而不是赋值），
# 这是本仓库早期草稿踩过的一个坑，本脚本反其道而行之。中文只出现在注释与输出里。
set -eu

root=${1:?用法: recover.sh <dataset_root> <dest>}
dest=${2:?用法: recover.sh <dataset_root> <dest>}

command -v b3sum >/dev/null 2>&1 || {
    echo "缺少依赖: b3sum（BLAKE3 官方 CLI，见 README 的安装说明）" >&2
    exit 2
}

test -d "$root/files" || { echo "缺少 $root/files" >&2; exit 2; }

# ---------------------------------------------------------------------------
# 1. 恢复本身就是一次普通拷贝——这正是 I1 的全部含义。
#    files/ 下当前版本永远完整平放（FORMAT.md §4、§8），不涉及 chunks/ 重建。
# ---------------------------------------------------------------------------
mkdir -p "$dest"
cp -R "$root/files/." "$dest/"

# ---------------------------------------------------------------------------
# 2. 用 .arca/items/ 里的当前版本记录逐个校验：文件存在、大小一致、哈希一致。
#
#    items/<xx>/<item_id>.jsonl 是 append-only 的版本链（FORMAT.md §7.1），
#    “当前版本”是链上最后一条完整记录，不是第一条。规范明确规定了两种损坏
#    的处置方式，二者刻意不同：
#      - 末行不完整（进程写到一半被杀）→ 截断到最后一个完整行边界，这是
#        崩溃后的正常残留，不算问题；
#      - 中间行损坏 → 必须失败，绝不静默跳过去找更早的“看起来完整”的行
#        （否则就是在假装某个真实提交过的版本从未存在过）。
#
#    只用 `grep '^{'` 挑“看起来完整”的行是不够的：它只检查行首字符，一行
#    在写到一半时被截断，只要截断点之前恰好也是以 `{` 开头，这一行就会被
#    grep 误判为“完整”。下面这段 awk 额外要求行尾是 `}`、且整行花括号配平
#    （开合数量相等）——足以挡住“末尾缺右花括号”和“写到某个嵌套对象的右
#    花括号后被杀、外层还没收尾”这两种真实会发生的截断形态；它仍不是完整
#    的 JSON 校验（比如字符串里恰好含不配对花括号的病态输入不会被挡住），
#    但比单看行首字符强得多，且不需要引入 jq 依赖。
# ---------------------------------------------------------------------------

# 取一个 items/*.jsonl 版本链的“当前版本”：
#   - 打印该行到 stdout 并以 0 退出：找到了合法的当前版本
#     （若丢弃了不完整的末行，会额外打印一行诊断到 stderr，但不计入问题数）
#   - 不打印、以非 0 退出：版本链损坏（中间行坏，或压根没有完整记录）
current_version() {
    awk '
        { lines[NR] = $0 }
        function complete(line,    t, opens, closes) {
            if (substr(line, 1, 1) != "{") return 0
            if (substr(line, length(line), 1) != "}") return 0
            t = line
            opens = gsub(/\{/, "{", t)
            t = line
            closes = gsub(/\}/, "}", t)
            return (opens == closes)
        }
        END {
            n = NR
            if (n == 0) {
                print "版本链为空" > "/dev/stderr"
                exit 3
            }
            bad_middle = 0
            last_good = 0
            for (i = 1; i <= n; i++) {
                if (complete(lines[i])) {
                    last_good = i
                } else if (i < n) {
                    bad_middle = i
                }
                # i == n 且不完整：允许——崩溃时的正常残留（FORMAT.md §7.1），
                # 不计入 bad_middle。
            }
            if (bad_middle > 0) {
                printf("版本链第 %d 行损坏，中间行损坏必须失败（FORMAT.md §7.1）\n", bad_middle) > "/dev/stderr"
                exit 3
            }
            if (last_good == 0) {
                print "没有找到任何完整记录" > "/dev/stderr"
                exit 3
            }
            if (last_good < n) {
                print "末行不完整，已按崩溃残留丢弃，取上一条完整记录" > "/dev/stderr"
            }
            print lines[last_good]
        }
    ' "$1"
}

problems=0
file_count=0

for item in "$root"/.arca/items/*/*.jsonl; do
    test -e "$item" || continue

    if ! record=$(current_version "$item"); then
        echo "问题: $item 版本链无法确定当前版本" >&2
        problems=$((problems + 1))
        continue
    fi

    want_hash=$(printf '%s' "$record" | sed -n 's/.*"hash":"blake3:\([0-9a-f]\{64\}\)".*/\1/p')
    want_size=$(printf '%s' "$record" | sed -n 's/.*"size":\([0-9]*\).*/\1/p')
    item_id=$(printf '%s' "$record" | sed -n 's/.*"item_id":"\([0-9a-f]\{32\}\)".*/\1/p')

    if [ -z "$want_hash" ] || [ -z "$want_size" ] || [ -z "$item_id" ]; then
        echo "问题: $item 当前版本记录字段不完整: $record" >&2
        problems=$((problems + 1))
        continue
    fi

    # 从 .arca/index/ 反查逻辑路径——items 记录里只有 item_id，没有路径
    # （FORMAT.md §6：index 记录是 item_id → path，files/ 落盘路径以它为准）。
    index_file=$(grep -l "\"item_id\":\"$item_id\"" "$root"/.arca/index/*/*.json 2>/dev/null | head -n 1) || true
    if [ -z "${index_file:-}" ]; then
        echo "问题: item $item_id 没有对应的 index 记录" >&2
        problems=$((problems + 1))
        continue
    fi
    logical_path=$(sed -n 's/.*"path":"\([^"]*\)".*/\1/p' "$index_file")
    if [ -z "$logical_path" ]; then
        echo "问题: $index_file 缺少 path 字段" >&2
        problems=$((problems + 1))
        continue
    fi

    target="$dest/$logical_path"
    if [ ! -f "$target" ]; then
        echo "问题: 缺少文件: $logical_path" >&2
        problems=$((problems + 1))
        continue
    fi

    actual_size=$(wc -c < "$target" | tr -d ' ')
    if [ "$actual_size" != "$want_size" ]; then
        echo "问题: 大小不符: $logical_path ($actual_size != $want_size)" >&2
        problems=$((problems + 1))
        continue
    fi

    actual_hash=$(b3sum --no-names "$target")
    if [ "$actual_hash" != "$want_hash" ]; then
        echo "问题: 哈希不符: $logical_path" >&2
        problems=$((problems + 1))
        continue
    fi

    file_count=$((file_count + 1))
done

echo "恢复并校验 $file_count 个文件，$problems 个问题"
test "$problems" -eq 0
