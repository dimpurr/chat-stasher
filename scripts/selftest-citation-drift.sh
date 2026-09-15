#!/usr/bin/env bash
# The selftest for scripts/check-citation-drift.py: proof that it still catches
# drift. Seven probes. Each one edits a real file in place and restores it from a
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
#   probe 6  an extensionless citation is its own anchor (.gitignore, W35)   => green
#   probe 7  a continuation behind an unresolvable path token (W35)          => red
#
# 🔴 Probes 1 and 2 name coordinates in real files, and coordinates rot when
#    those files move. They had rotted by W32: both could no longer apply their
#    own edit and reported the selftest itself as void. That is why every probe
#    below checks that its edit landed *before* it judges the checker — a stale
#    coordinate must fail loudly here, never pass quietly.
#
#    Probe 1 goes one step further since W33: its line number is not written
#    down at all. A two-line edit elsewhere in docs/threat-model.md moved the
#    dashboard row from :148 to :150 and the hardcoded 148 silently stopped
#    pointing at it. The probe now finds the line by its content and refuses to
#    run unless exactly one line matches, so the next such move is a loud void
#    here instead of a probe that edits an uncited line and passes.

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
# The coordinate is a content match, never a line number: a hardcoded number
# rotted once already (W33) and the probe then edited an unrelated line. The
# anchor is the citation itself — view.rs:256 immediately followed by the
# continuation `:180` on the dashboard row — and the guard below refuses to run
# unless it is on exactly one line.
PROBE1_ANCHOR='view.rs:256`, `:180`'
PROBE1_HITS="$(grep -c -F "$PROBE1_ANCHOR" "$REPO/docs/threat-model.md")"
PROBE1_LINE="$(grep -n -F "$PROBE1_ANCHOR" "$REPO/docs/threat-model.md" | head -1 | cut -d: -f1)"
echo "  Target: docs/threat-model.md:${PROBE1_LINE:-none}, \`:180\` -> \`:1\`"
echo "  (Chosen because it is a *continuation* citation: the file name is"
echo "   omitted and inferred from the citation before it on the same line, so"
echo "   this exercises the other parsing branch. view.rs:1 is that module's own"
echo "   doc comment — it exists, it is not empty, and it has nothing to do with"
echo "   the constant-time token check the sentence cites. That is exactly what"
echo "   the previous checker let through: bounds and non-emptiness were the"
echo "   whole test.)"
echo "=============================================================="
if [ "$PROBE1_HITS" != "1" ] || [ -z "$PROBE1_LINE" ]; then
  echo "  ✘ probe 1's anchor is on ${PROBE1_HITS} line(s) of docs/threat-model.md, not 1;"
  echo "    the selftest itself is void (the citation moved, or the wording changed)"
  FAILED=1
  PROBE1_LINE=""
fi
cp "$REPO/docs/threat-model.md" "$TMP/threat-model.md"
if [ -n "$PROBE1_LINE" ]; then
  sed -i '' "${PROBE1_LINE}s/\`:180\`/\`:1\`/" "$REPO/docs/threat-model.md"
  if ! sed -n "${PROBE1_LINE}p" "$REPO/docs/threat-model.md" | grep -q -F '`:1`'; then
    echo "  ✘ probe 1 could not modify the document; the selftest itself is void"
    FAILED=1
  fi
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
echo "Probe 6 (W35): an extensionless citation must be attributed to"
echo "  its own file."
echo "  docs/install.md:188 writes \`.gitignore:15\`. The parser used to"
echo "  recognise a path only when its extension was in CITED_EXTS, so"
echo "  \`:15\` read as a *continuation* and inherited"
echo "  apps/extension/package.json — a file the sentence never names,"
echo "  anchored to content that had nothing to do with the claim."
echo ""
echo "  The probe cites \`.gitignore:15\` from the contract document — a"
echo "  citation the lockfile already holds, word for word — and asks"
echo "  two things of the run: it must stay green (so the citation is"
echo "  answered by .gitignore's own lines, which have not changed), and"
echo "  --list must attribute it to .gitignore and to nothing else."
echo "  A parser that does not see .gitignore as a path reads the bare"
echo "  \`:15\` instead, and there is no citation in that sentence for it"
echo "  to inherit: red, and no such anchor in --list."
echo "=============================================================="
PROBE6_CITE='W35 probe: see `.gitignore:15`.'
PROBE6_DOC_ANCHOR='`.gitignore:15`'
PROBE6_DOC_HITS="$(grep -c -F "$PROBE6_DOC_ANCHOR" "$REPO/docs/install.md")"
echo "  Target: contracts/nativehost-protocol.md, citing ${PROBE6_DOC_ANCHOR}"
if [ "$PROBE6_DOC_HITS" != "1" ]; then
  echo "  ✘ probe 6's anchor is on ${PROBE6_DOC_HITS} line(s) of docs/install.md, not 1;"
  echo "    the selftest itself is void (the citation moved, or the wording changed)"
  FAILED=1
  PROBE6_CITE=""
fi
if ! grep -q '^\.gitignore:15  ' "$REPO/docs/citations.lock"; then
  echo "  ✘ .gitignore:15 is not a locked anchor; the probe would prove nothing"
  FAILED=1
  PROBE6_CITE=""
fi
cp "$REPO/contracts/nativehost-protocol.md" "$TMP/nativehost-protocol.md"
if [ -n "$PROBE6_CITE" ]; then
  printf '\n%s\n' "$PROBE6_CITE" >> "$REPO/contracts/nativehost-protocol.md"
  if ! grep -q -F "$PROBE6_CITE" "$REPO/contracts/nativehost-protocol.md"; then
    echo "  ✘ probe 6 could not modify the contract document; the selftest itself is void"
    FAILED=1
  fi
fi
$CHECK >"$TMP/probe6.out" 2>&1
rc=$?
expect 0 "$rc" "an extensionless citation of an unchanged, locked range must be green"
if $CHECK --list 2>/dev/null | grep -q '^\.gitignore:15  '; then
  echo "  ✔ --list attributes the citation to .gitignore:15"
else
  echo "  ✘ --list has no .gitignore:15 anchor; the citation was attributed elsewhere:"
  sed 's/^/      /' "$TMP/probe6.out"
  FAILED=1
fi
cp "$TMP/nativehost-protocol.md" "$REPO/contracts/nativehost-protocol.md"
echo

echo "=============================================================="
echo "Probe 7 (W35): a continuation whose own path token cannot be"
echo "  resolved must fail instead of inheriting the citation before"
echo "  it."
echo "  The probe appends one line to the real contract document: a"
echo "  citation of a range the lockfile holds, then a bare range"
echo "  written behind a token that is not a file in the repository."
echo "  Hereditary reading of that bare range points at the locked"
echo "  anchor, so an inheriting parser stays green — which is the bug."
echo "  The locked anchor is read from docs/citations.lock at run time,"
echo "  so it cannot rot into a copy of a range that no longer exists."
echo "=============================================================="
PROBE7_ANCHOR="$(awk '!/^#/ && NF>=3 && $1 ~ /\// {print $1; exit}' "$REPO/docs/citations.lock")"
PROBE7_RANGE="${PROBE7_ANCHOR##*:}"
echo "  Target: contracts/nativehost-protocol.md, anchor ${PROBE7_ANCHOR:-none}"
if [ -z "$PROBE7_ANCHOR" ] || ! grep -q -F "$PROBE7_ANCHOR  " "$REPO/docs/citations.lock"; then
  echo "  ✘ probe 7 found no path-shaped anchor in docs/citations.lock; the selftest itself is void"
  FAILED=1
  PROBE7_ANCHOR=""
fi
cp "$REPO/contracts/nativehost-protocol.md" "$TMP/nativehost-protocol.md"
if [ -n "$PROBE7_ANCHOR" ]; then
  printf '\nW35 probe: see `%s`, `W35-PROBE-NOT-A-FILE:%s`.\n' "$PROBE7_ANCHOR" "$PROBE7_RANGE" \
    >> "$REPO/contracts/nativehost-protocol.md"
  if ! grep -q 'W35-PROBE-NOT-A-FILE' "$REPO/contracts/nativehost-protocol.md"; then
    echo "  ✘ probe 7 could not modify the contract document; the selftest itself is void"
    FAILED=1
  fi
fi
$CHECK >"$TMP/probe7.out" 2>&1
rc=$?
expect 1 "$rc" "a continuation behind an unresolvable path token must be red"
if grep -q -F 'W35-PROBE-NOT-A-FILE' "$TMP/probe7.out"; then
  echo "  ✔ the failure names the token it could not resolve"
else
  echo "  ✘ the failure does not name W35-PROBE-NOT-A-FILE:"
  sed 's/^/      /' "$TMP/probe7.out"
  FAILED=1
fi
cp "$TMP/nativehost-protocol.md" "$REPO/contracts/nativehost-protocol.md"
echo

echo "=============================================================="
echo "After: the working tree should hold only the intended new files"
echo "=============================================================="
git status --porcelain
echo

if [ "$FAILED" -eq 0 ]; then
  echo "SELFTEST PASS: all seven probes returned the exit code they must."
  exit 0
fi
echo "SELFTEST FAIL: a probe returned the wrong exit code."
exit 1
