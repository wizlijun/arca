#!/bin/sh
# 造一个最小但合法的 arca hub 存储根，供 recover.sh 演示与 CI 使用。
#
# 用法：make-fixture.sh <dest>
#   dest  要创建的存储根目录（必须不存在或为空目录，避免误覆盖）
#
# 依赖：POSIX shell + coreutils + b3sum（同 recover.sh，见该文件顶部注释与
# README 的依赖说明）。本文件同样不得出现任何 arca 代码——夹具的哈希与索引键
# 都用 b3sum 现算，不调用 arca 自己的实现来生成"权威答案"，否则夹具与
# recover.sh 就会共享同一个可能有 bug 的假设，校验也就失去了意义。
#
# 布局与字段取值特意与 crates/arca-store/tests/fsck.rs 里的
# `造一个健康的存储根` 同构（同一个 item_id / 内容 / 路径），
# 便于交叉核对 arca 自己的 fsck 与本演示是否看法一致。
set -eu

dest=${1:?用法: make-fixture.sh <dest>}

command -v b3sum >/dev/null 2>&1 || {
    echo "缺少依赖: b3sum（BLAKE3 官方 CLI，见 README 的安装说明）" >&2
    exit 2
}

if [ -e "$dest" ] && [ -n "$(ls -A "$dest" 2>/dev/null)" ]; then
    echo "拒绝：$dest 已存在且非空，避免误覆盖" >&2
    exit 2
fi

item_id="3f2a000000000000000000000000beef"
item_shard="3f"
path="note.txt"
content="hello arca"
version_id="20260804T102302Z-00000000000000000000000000000000"
dataset_id="9c41000000000000000000000000abcd"

mkdir -p "$dest/files"
mkdir -p "$dest/.arca/items/$item_shard"
mkdir -p "$dest/.arca/index"

# files/：逃生舱本体，当前版本完整平放（FORMAT.md §4）
printf '%s' "$content" > "$dest/files/$path"

content_hash=$(printf '%s' "$content" | b3sum --no-names)
content_size=$(printf '%s' "$content" | wc -c | tr -d ' ')

# format.json：卷身份标记（FORMAT.md §5）
cat > "$dest/.arca/format.json" <<EOF
{"v":1,"format":1,"dataset_id":"$dataset_id","hash_algo":"blake3","created_at":"2026-08-04T10:00:00Z"}
EOF

# items/<xx>/<item_id>.jsonl：版本链，此处只有一条（首版，parent 为 null，
# FORMAT.md §7.1）
cat > "$dest/.arca/items/$item_shard/$item_id.jsonl" <<EOF
{"v":1,"version_id":"$version_id","item_id":"$item_id","parent":null,"hash":"blake3:$content_hash","size":$content_size,"mtime":"2026-08-04T10:00:00Z","actor":{"account":"","device":"","session":""},"committed_at":"2026-08-04T10:00:00Z"}
EOF

# index/<xx>/<hash>.json：路径 → 身份映射（FORMAT.md §6）。
# 索引键 = BLAKE3(小写规范化路径)，与 arca_format::path_rules::index_key 一致
# （crates/arca-format/src/path_rules.rs）。"note.txt" 本身已是规范化且全小写
# 的相对路径，所以这里直接对路径原文取哈希，不需要额外的规范化/小写化步骤。
index_key=$(printf '%s' "$path" | b3sum --no-names)
index_shard=$(printf '%s' "$index_key" | cut -c1-2)
mkdir -p "$dest/.arca/index/$index_shard"
cat > "$dest/.arca/index/$index_shard/$index_key.json" <<EOF
{"v":1,"item_id":"$item_id","path":"$path"}
EOF

echo "夹具已生成: ${dest} (1 个文件, item ${item_id}, 索引键 ${index_key})"
