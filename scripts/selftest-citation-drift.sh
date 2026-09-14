#!/usr/bin/env bash
# The selftest for scripts/check-citation-drift.py: proof that it still catches
# drift. Five probes. Each one edits a real file in place and restores it from a
# backup whatever the outcome; the run ends by printing `git status`.
#
# The first version of the checker asked only two questions — is the line number
# in bounds, is that line non-empty. A citation moved to a line that exists and
# is non-empty but has nothing to do with the claim returned 0. The probes are
# aimed at that hole:
#
#   probe 1  move a citation to a line that exists, is non-empty, unrelated  => red
#   probe 2  leave the document alone, edit a line inside a cited range      => red
#   probe 3  change nothing                                                  => green
#   probe 4  add a dangling citation to contracts/ (W32)                     => red
#   probe 5  every contracts/*.md is in the scan set (W32)                    => green
#
# 🔴 Probes 1 and 2 name coordinates in real files, and coordinates rot when
#    those files move. They had rotted by W32: both could no longer apply their
#    own edit and reported the selftest itself as void. That is why every probe
#    below checks that its edit landed *before* it judges the checker — a stale
#    coordinate must fail loudly here, never pass quietly.

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK="python3 $REPO/scripts/check-citation-drift.py"
cd "$REPO" || exit 2

TMP="$(mktemp -d)"
FAILED=0

restore() {
  [ -f "$TMP/threat-model.md" ] && cp "$TMP/threat-model.md" "$REPO/docs/threat-model.md"
  [ -f "$TMP/engine.ts" ] && cp "$TMP/engine.ts" "$REPO/apps/extension/lib/backfill/engine.ts"
  [ -f "$TMP/store.rs" ] && cp "$TMP/store.rs" "$REPO/crates/chat-stasher/src/store.rs"
  [ -f "$TMP/nativehost-protocol.md" ] && cp "$TMP/nativehost-protocol.md" "$REPO/contracts/nativehost-protocol.md"
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
echo "Probe 1: move a citation to a line that exists, is not empty, and"
echo "  has nothing to do with the claim it is attached to."
echo "  Target: docs/threat-model.md:148, \`:180\` -> \`:1\`"
echo "  (Chosen because it is a *continuation* citation: the file name is"
echo "   omitted and inferred from the citation before it on the same line, so"
echo "   this exercises the other parsing branch. view.rs:1 is that module's own"
echo "   doc comment — it exists, it is not empty, and it has nothing to do with"
echo "   the constant-time token check the sentence cites. That is exactly what"
echo "   the previous checker let through: bounds and non-emptiness were the"
echo "   whole test.)"
echo "=============================================================="
cp "$REPO/docs/threat-model.md" "$TMP/threat-model.md"
sed -i '' '148s/`:180`/`:1`/' "$REPO/docs/threat-model.md"
if ! grep -q '`:1`' "$REPO/docs/threat-model.md"; then
  echo "  ✘ probe 1 could not modify the document; the selftest itself is void"
  FAILED=1
fi
$CHECK
rc=$?
expect 1 "$rc" "a citation moved to an unrelated but legal line must be red"
cp "$TMP/threat-model.md" "$REPO/docs/threat-model.md"
echo

echo "=============================================================="
echo "Probe 2: leave the document alone and edit a line *inside* a cited"
echo "  range."
# Re-pointed after the Claude merge moved engine.ts: the probe must edit a line
# that sits inside a range the lockfile actually holds, so it now looks the
# range up first and fails loudly (void selftest) if the range is gone,
# instead of editing an uncited line and "passing" while testing nothing.
PROBE2_RANGE='crates/chat-stasher/src/store.rs:261-296'
PROBE2_FILE='crates/chat-stasher/src/store.rs'
PROBE2_LINE=281
echo "  Target: ${PROBE2_FILE}:${PROBE2_LINE}, inside the cited range 261-296."
echo "  It sits in the middle, not on the first line: the snippet a human reads"
echo "  in the lockfile is the range's first non-empty line, and that line does"
echo "  not change."
echo "  (This is the most common drift in the wild: code is edited, the line"
echo "   numbers survive, the content moves on. Only hashing the whole range"
echo "   catches it — comparing the snippet would not.)"
echo "=============================================================="
if ! grep -q "^${PROBE2_RANGE} " "$REPO/docs/citations.lock"; then
  echo "  ✘ probe 2's range ${PROBE2_RANGE} is no longer in the lockfile; the selftest itself is void"
  FAILED=1
fi
cp "$REPO/$PROBE2_FILE" "$TMP/store.rs"
sed -i '' "${PROBE2_LINE}s/.*/        \/\/ PROBE2: content changed inside the cited range/" "$REPO/$PROBE2_FILE"
if ! sed -n "${PROBE2_LINE}p" "$REPO/$PROBE2_FILE" | grep -q PROBE2; then
  echo "  ✘ probe 2 could not modify the code; the selftest itself is void"
  FAILED=1
fi
$CHECK
rc=$?
expect 1 "$rc" "content inside the cited range changed, must be red"
cp "$TMP/store.rs" "$REPO/$PROBE2_FILE"
echo

echo "=============================================================="
echo "Probe 3: change nothing"
echo "=============================================================="
$CHECK
rc=$?
expect 0 "$rc" "a clean tree must be green"
echo

echo "=============================================================="
echo "Probe 4 (W32): a dangling citation inside contracts/"
echo "  contracts/ was outside the scan until W32, so a citation there"
echo "  could name a file that does not exist and every gate stayed"
echo "  green. This probe appends exactly that to the real contract"
echo "  document and demands a red."
echo "  Scope: the citation is written in the syntax the checker parses"
echo "  (a path followed by :line). A bare path with no line number is"
echo "  outside that syntax and is NOT what this probe covers."
echo "=============================================================="
cp "$REPO/contracts/nativehost-protocol.md" "$TMP/nativehost-protocol.md"
printf '\nW32 probe: see `crates/chat-stasher/src/w32-probe-missing.rs:1`.\n' \
  >> "$REPO/contracts/nativehost-protocol.md"
if ! grep -q 'w32-probe-missing.rs:1' "$REPO/contracts/nativehost-protocol.md"; then
  echo "  ✘ probe 4 could not modify the contract document; the selftest itself is void"
  FAILED=1
fi
$CHECK
rc=$?
expect 1 "$rc" "a citation naming a file that does not exist, in contracts/, must be red"
cp "$TMP/nativehost-protocol.md" "$REPO/contracts/nativehost-protocol.md"
echo

echo "=============================================================="
echo "Probe 5 (W32): every contracts/*.md is in the scan set"
echo "  Probe 4 proves a red; this one proves what the red is for —"
echo "  the scan set really is DOC_FILES plus every contract document,"
echo "  read from the checker itself rather than from the prose here."
echo "=============================================================="
python3 - "$REPO" <<'PY'
import glob, importlib.util, os, sys

root = sys.argv[1]
spec = importlib.util.spec_from_file_location(
    "citation_drift", os.path.join(root, "scripts", "check-citation-drift.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

scanned = set(module.doc_files())
on_disk = {
    os.path.relpath(path, root)
    for path in glob.glob(os.path.join(root, "contracts", "*.md"))
}
if not on_disk:
    print("  no contract document found at all — the probe checked nothing")
    sys.exit(1)
missing = sorted(on_disk - scanned)
if missing:
    print(f"  scan set is missing: {', '.join(missing)}")
    sys.exit(1)
print(f"  {len(on_disk)} contract document(s) in the scan set: {', '.join(sorted(on_disk))}")
PY
rc=$?
expect 0 "$rc" "contracts/*.md must all be in the scan set"
echo

echo "=============================================================="
echo "After: the working tree should hold only the intended new files"
echo "=============================================================="
git status --porcelain
echo

if [ "$FAILED" -eq 0 ]; then
  echo "SELFTEST PASS: all five probes returned the exit code they must."
  exit 0
fi
echo "SELFTEST FAIL: a probe returned the wrong exit code."
exit 1
