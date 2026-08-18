#!/usr/bin/env bash
# scripts/check-citation-drift.py 的自测: 证明它真的能检出漂移。
#
# 上一版检查器只校验"行号没越界、那一行非空", 于是把一条引用改成 main.rs:397
# (那行是 `options: Vec<String>`, 跟任何断言都不相干) 它照样返回 0。这里的三个
# 探针就是冲着那个漏洞去的:
#
#   探针 1  把一条引用改到【存在且非空但不相干】的行号  => 必须红
#   探针 2  不动文档, 改被引代码那一行的内容           => 必须红
#   探针 3  什么都不动                                => 必须绿
#
# 三个探针都在原地改真文件, 无论成败都用备份恢复; 结束时自查工作区是否干净。

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK="python3 $REPO/scripts/check-citation-drift.py"
cd "$REPO" || exit 2

TMP="$(mktemp -d)"
FAILED=0

restore() {
  [ -f "$TMP/threat-model.md" ] && cp "$TMP/threat-model.md" "$REPO/docs/threat-model.md"
  [ -f "$TMP/store.rs" ] && cp "$TMP/store.rs" "$REPO/crates/chat-stasher/src/store.rs"
}
trap 'restore; rm -rf "$TMP"' EXIT

expect() { # expect <期望退出码> <实际退出码> <说明>
  if [ "$1" -eq "$2" ]; then
    echo "  ✔ 期望 rc=$1, 实际 rc=$2 — $3"
  else
    echo "  ✘ 期望 rc=$1, 实际 rc=$2 — $3"
    FAILED=1
  fi
}

echo "=============================================================="
echo "探针 1: 把一条引用改到存在且非空、但内容不相干的行号"
echo "  位置: docs/threat-model.md:252 的 \`:3213-3260\` -> \`:397\`"
echo "  (选它是因为这是【省略文件名的续写引用】, 靠上一条引用推断出 main.rs;"
echo "   实现时我脑子里想的是带完整路径的那种写法, 这条走的是另一条解析分支。"
echo "   main.rs:397 是 'options: Vec<String>', 存在、非空、与该段断言无关 ——"
echo "   正是上一版检查器放过去的那种情形。)"
echo "=============================================================="
cp "$REPO/docs/threat-model.md" "$TMP/threat-model.md"
sed -i '' '252s/`:3213-3260`/`:397`/' "$REPO/docs/threat-model.md"
if ! grep -q '`:397`' "$REPO/docs/threat-model.md"; then
  echo "  ✘ 探针 1 没能改动文档, 自测本身失效"
  FAILED=1
fi
$CHECK
rc=$?
expect 1 "$rc" "引用漂到不相干的合法行, 必须红"
cp "$TMP/threat-model.md" "$REPO/docs/threat-model.md"
echo

echo "=============================================================="
echo "探针 2: 文档一个字不改, 改被引代码那一行的内容"
echo "  位置: crates/chat-stasher/src/store.rs:948 —— 它落在被引范围 917-983 的"
echo "  【中间】, 不是首行。lockfile 里人眼看到的摘要是首行, 首行不变;"
echo "  所以这条只有靠整段的哈希才抓得到, 靠摘要抓不到。"
echo "  (这是真实世界最常见的漂移: 代码被编辑, 行号还在, 内容变了。)"
echo "=============================================================="
cp "$REPO/crates/chat-stasher/src/store.rs" "$TMP/store.rs"
sed -i '' '948s/.*/    let tmp = parent.join(format!(".{}.PROBE2", name.to_string_lossy()));/' \
  "$REPO/crates/chat-stasher/src/store.rs"
if ! sed -n '948p' "$REPO/crates/chat-stasher/src/store.rs" | grep -q PROBE2; then
  echo "  ✘ 探针 2 没能改动代码, 自测本身失效"
  FAILED=1
fi
$CHECK
rc=$?
expect 1 "$rc" "被引范围内容变了, 必须红"
cp "$TMP/store.rs" "$REPO/crates/chat-stasher/src/store.rs"
echo

echo "=============================================================="
echo "探针 3: 什么都不动"
echo "=============================================================="
$CHECK
rc=$?
expect 0 "$rc" "干净状态必须绿"
echo

echo "=============================================================="
echo "收尾: 工作区应当只剩有意新增的文件"
echo "=============================================================="
git status --porcelain
echo

if [ "$FAILED" -eq 0 ]; then
  echo "自测通过: 三个探针的退出码都符合预期。"
  exit 0
fi
echo "自测失败: 有探针的退出码不符合预期。"
exit 1
