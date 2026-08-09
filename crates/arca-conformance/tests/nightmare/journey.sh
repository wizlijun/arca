#!/bin/sh
# spec §6.3 噩梦路径：端到端用户旅程演练。
#
# 这个文件存在的理由：**887 个单元测试证明的是「零件对」，没有证明
# 「产品能用」。** 最近三次各五分钟的实机实验，每次都找到一个测试没碰到的
# 产品级问题（status 在 git checkout 之后静默报成功、历史版本字节从未留存、
# git clone 不带 .git/hooks/）。三个都在「零件之间」。
#
# §6.3 是 spec 自己的完整度清单——用它意味着「完整」有客观定义，
# 而不取决于谁的判断。
#
# 本脚本覆盖其中**平台无关且此前从未端到端验过**的三条：
#
#   第 2 条  离线修改 + 重连（基线对账 + CAS 提交 + 可能的冲突副本）
#   第 5 条  同一文件在 A 改名、B 编辑（改名与新版本在 hub 汇合）
#   第 12 条 数据集搬迁：photo/（含 .arca/）整体移到另一个仓库，
#            adopt 后身份/清单/hub 归属复位，**零重传**
#
# 其余各条的归属见同目录 README.md 的账本。
#
# 用法：journey.sh <工作目录>
#   ARCA_BIN 可覆盖二进制路径。
set -eu

work=${1:?用法：journey.sh <工作目录>}
root=$(cd "$(dirname "$0")/../../../.." && pwd)
ARCA=${ARCA_BIN:-$root/target/debug/arca}

fail() { echo "演练失败：$*" >&2; exit 1; }
step() { echo; echo "== $* =="; }
ok()   { echo "  ✓ $*"; }

rm -rf "$work"; mkdir -p "$work"

git_init() {
    git -C "$1" init -q
    git -C "$1" config user.email drill@example.com
    git -C "$1" config user.name drill
}

# ---------------------------------------------------------------------------
step "第 2 条：离线修改 + 重连"
# ---------------------------------------------------------------------------
# 用户在飞机上改了文件（hub 够不着），落地后重连。基线仍是起飞前那份，
# 本地已变，远端没变 —— 这一轮应当把改动**上传**，而不是把本地覆盖回去。

s2=$work/s2; mkdir -p "$s2/store" "$s2/v/assets"
printf 'BEFORE-FLIGHT' > "$s2/v/assets/doc.bin"
git_init "$s2/v"
(cd "$s2/v" && "$ARCA" init . >/dev/null)
(cd "$s2/v" && "$ARCA" register assets --hub home --hub-url "file://$s2/store" >/dev/null)
(cd "$s2/v" && "$ARCA" adopt assets >/dev/null)

# 「拔网线」：把存储根移走。
mv "$s2/store" "$s2/store.offline"
printf 'EDITED-OFFLINE' > "$s2/v/assets/doc.bin"

# 离线期间：必须报离线（退出码 2，I11），**且绝不动本地文件**。
if (cd "$s2/v" && "$ARCA" sync assets >/dev/null 2>&1); then
    fail "第 2 条：hub 不可达时 sync 不该报成功"
fi
[ "$(cat "$s2/v/assets/doc.bin")" = "EDITED-OFFLINE" ] \
    || fail "第 2 条：离线期间本地改动被动过"
ok "离线期间：明确失败，本地改动完好"

# 重连。
mv "$s2/store.offline" "$s2/store"
(cd "$s2/v" && "$ARCA" sync assets >/dev/null) || fail "第 2 条：重连之后 sync 失败"
[ "$(cat "$s2/store/files/doc.bin")" = "EDITED-OFFLINE" ] \
    || fail "第 2 条：重连之后离线期间的改动没有上传（hub 上是 $(cat "$s2/store/files/doc.bin")）"
[ "$(cat "$s2/v/assets/doc.bin")" = "EDITED-OFFLINE" ] \
    || fail "第 2 条：重连之后本地被远端旧版本覆盖了——这是丢数据"
ok "重连之后：离线期间的改动被上传，本地未被旧版本覆盖"

(cd "$s2/v" && "$ARCA" status assets >/dev/null 2>&1) || fail "第 2 条：收敛之后 status 应当干净"
ok "收敛：status 退出 0"

# ---------------------------------------------------------------------------
step "第 5 条：A 改名、B 编辑，两者在 hub 汇合"
# ---------------------------------------------------------------------------
# 这是身份模型（I7：身份跨改名稳定）最尖锐的一次考验：A 把 old.bin 改名成
# new.bin，B 在不知情的情况下编辑了 old.bin。两边先后同步到同一个 hub。
#
# 正确的结果是**没有任何一份内容丢失**：B 的编辑必须以某种形式存活
# （落到 new.bin 上、或作为冲突副本），绝不能被一次改名静默吞掉。

s5=$work/s5; mkdir -p "$s5/store"
mkdir -p "$s5/A/assets"
printf 'ORIGINAL' > "$s5/A/assets/old.bin"
git_init "$s5/A"
(cd "$s5/A" && "$ARCA" init . >/dev/null)
(cd "$s5/A" && "$ARCA" register assets --hub home --hub-url "file://$s5/store" >/dev/null)
(cd "$s5/A" && "$ARCA" adopt assets >/dev/null)
git -C "$s5/A" add -A && git -C "$s5/A" commit -q -m base

# B 从 A 克隆并取回内容。
git clone -q "$s5/A" "$s5/B"
(cd "$s5/B" && "$ARCA" setup >/dev/null 2>&1) || fail "第 5 条：B 的 setup 失败"
[ -f "$s5/B/assets/old.bin" ] || fail "第 5 条：B 没有拿到 old.bin"

# A 改名并同步。
mv "$s5/A/assets/old.bin" "$s5/A/assets/new.bin"
(cd "$s5/A" && "$ARCA" sync assets >/dev/null) || fail "第 5 条：A 的改名同步失败"

# B 在不知情的情况下编辑了老路径，然后同步。
printf 'B-EDIT' > "$s5/B/assets/old.bin"
(cd "$s5/B" && "$ARCA" sync assets >/dev/null 2>&1) || true   # 冲突时非 0 是合法的

# 判据：**B 的字节不能丢**。arca 的正确处置不是「一定要传上 hub」，而是
# 「明确报冲突、不动数据、退出码非 0」——把决定权交回给人（I5），
# 同时一个字节都不销毁（I3）。
[ "$(cat "$s5/B/assets/old.bin")" = "B-EDIT" ] \
    || fail "第 5 条：**B 的编辑被动过了**——改名与并发编辑汇合时改写用户\
未同步的内容，是这个项目最不能接受的结果（I3）"
ok "B 的编辑完好地留在本地（未被改名传播覆盖）"

if (cd "$s5/B" && "$ARCA" status assets >/dev/null 2>&1); then
    fail "第 5 条：B 这边存在未解决的结构化冲突，status 不该报成功（退出码应为 1）"
fi
ok "B 的 status 退出码非 0——冲突不会被当成「已同步」"

# A 的改名结果也要在。
[ -f "$s5/store/files/new.bin" ] || fail "第 5 条：A 的改名没有落到 hub"
ok "A 的改名已落到 hub（new.bin）"

# ---------------------------------------------------------------------------
step "第 5 条（续）：改名必须被识别成改名——两种 hub 行为一致"
# ---------------------------------------------------------------------------
# I7「身份跨改名稳定」：一次改名应当在 hub 上保持同一个 item_id、零传输，
# 而不是退化成「上传新文件 + tombstone 旧文件」（那会新建身份、全量重传、
# 让版本链分叉，并且让协作者白下载一遍一模一样的字节）。
#
# **这一条目前会失败**，见同目录 README.md 的账本：`arca sync` 对 `file://`
# 走 `sync_lib::sync`（无改名检测），对 `http://` 走 `sync_lib::sync_transport`
# （有改名检测）——同一个改名在两种 hub 上结果不同。

s5b=$work/s5b; mkdir -p "$s5b/store" "$s5b/v/assets"
printf 'SAME-BYTES' > "$s5b/v/assets/x.bin"
git_init "$s5b/v"
(cd "$s5b/v" && "$ARCA" init . >/dev/null)
(cd "$s5b/v" && "$ARCA" register assets --hub home --hub-url "file://$s5b/store" >/dev/null)
(cd "$s5b/v" && "$ARCA" adopt assets >/dev/null)
item_before=$(find "$s5b/store/.arca/items" -name '*.jsonl' | head -1)
[ -n "$item_before" ] || fail "第 5 条（续）：前置条件——adopt 之后应当有版本链"

mv "$s5b/v/assets/x.bin" "$s5b/v/assets/y.bin"
out=$(cd "$s5b/v" && "$ARCA" sync assets 2>&1)

echo "$out" | grep -q 'tombstone' && \
    fail "第 5 条（续）：**改名被当成了「上传 + tombstone」**（file:// 路径没有改名检测）。
后果：hub 上新建了一个 item_id（违反 I7 身份跨改名稳定）、内容被全量重传、
版本链分叉，协作者还要再下载一遍一模一样的字节。
而同样的操作在 http:// hub 上会走 sync_transport 的改名检测、结果完全不同——
同一个抽象下两条实现分叉，正是 Transport 当初要消除的那类问题。
实际输出：
$out"
ok "改名被识别成改名（未退化成上传 + tombstone）"

item_after=$(find "$s5b/store/.arca/items" -name '*.jsonl' | wc -l | tr -d ' ')
[ "$item_after" = "1" ] \
    || fail "第 5 条（续）：改名之后 hub 上出现了 $item_after 条版本链——身份没有跨改名保持稳定（I7）"
ok "hub 上仍是同一个 item（身份跨改名稳定，I7）"

# ---------------------------------------------------------------------------
step "第 12 条：数据集搬迁（含 .arca/）到另一个仓库，零重传"
# ---------------------------------------------------------------------------
# 把 photo/ 连同它的 .arca/ 整个移到另一个 git 仓库里。身份、清单、hub 归属
# 都在 .arca/dataset.toml 与 .gitarca 里，所以搬完之后应当**认得出这就是
# 同一个数据集**，并且一个字节都不用重传。

s12=$work/s12; mkdir -p "$s12/store"
mkdir -p "$s12/old/photo"
printf 'PHOTO-BYTES-AAA' > "$s12/old/photo/a.png"
printf 'PHOTO-BYTES-BBB' > "$s12/old/photo/b.png"
git_init "$s12/old"
(cd "$s12/old" && "$ARCA" init . >/dev/null)
(cd "$s12/old" && "$ARCA" register photo --hub home --hub-url "file://$s12/store" >/dev/null)
(cd "$s12/old" && "$ARCA" adopt photo >/dev/null)

before=$(find "$s12/store/files" -type f | wc -l | tr -d ' ')
[ "$before" = "2" ] || fail "第 12 条：前置条件——hub 上应当有 2 个文件，实得 $before"

# 搬迁：整个 photo/（含 .arca/）移到新仓库。
mkdir -p "$s12/new"
git_init "$s12/new"
(cd "$s12/new" && "$ARCA" init . >/dev/null)
mv "$s12/old/photo" "$s12/new/photo"
# hub 归属在 vault 根的 .gitarca 里，搬迁时要跟着带过去。
(cd "$s12/new" && "$ARCA" register photo --hub home --hub-url "file://$s12/store" >/dev/null) \
    || fail "第 12 条：在新仓库里登记搬过来的数据集失败"

# 关键判据：adopt 之后**零重传**——hub 上的文件数与内容一个都没变。
(cd "$s12/new" && "$ARCA" adopt photo >/dev/null) || fail "第 12 条：搬迁后 adopt 失败"
after=$(find "$s12/store/files" -type f | wc -l | tr -d ' ')
[ "$after" = "2" ] || fail "第 12 条：搬迁后 hub 上文件数从 $before 变成 $after——不是零重传"
[ "$(cat "$s12/store/files/a.png")" = "PHOTO-BYTES-AAA" ] || fail "第 12 条：a.png 内容变了"
ok "搬迁后身份复位、零重传（hub 上仍是 $after 个文件、内容未变）"

(cd "$s12/new" && "$ARCA" status photo >/dev/null 2>&1) \
    || fail "第 12 条：搬迁后 status 应当干净（说明身份与清单都复位了）"
ok "搬迁后 status 退出 0"

echo
echo "§6.3 噩梦路径演练（第 2 / 5 / 12 条）全部通过。"
