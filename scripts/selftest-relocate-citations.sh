#!/usr/bin/env bash
# The selftest for scripts/relocate-citations.py: proof that it moves a citation
# to where the cited text went, that it refuses when there is more than one
# possible answer or no answer at all, and that a second run over a document it
# has already rewritten is a refusal rather than a second relocation.
#
# It runs against throwaway repositories built here, the way
# scripts/dev/test-reload-extension.sh drives its script against a throwaway
# copy — not against this checkout. Editing the real documents to test a tool
# whose whole job is to edit the real documents is how a test leaves the tree
# dirty when it fails.
#
# Three fixtures, because the shapes do not fit in one repository:
#
#   A  everything relocates — a shift, a grown range, a continuation, a
#      two-range span in one token
#   B  nothing relocates — a duplicated snippet, a deleted snippet, and one
#      certain citation alongside them to prove a refusal does not stop the run
#   C  two parents — a citation written by side A's document and one written by
#      side B's, moved by a single invocation that declares both
#
# 🔴 Probes 4, 5 and 6 are the point of the tool, not decoration. A relocation
#    that guesses when the answer is not unique is worse than the hand work it
#    saves: the range it invents still points at *a* real range, the sentence
#    around it still reads as if it were checked, and the next --update locks it
#    in. Probe 4 is the same hazard one step later — run the tool twice and the
#    second run reads a document that is no longer in anybody's coordinate
#    system, which is how a tool with the best rules in the world corrupts a
#    document.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
FAILED=0

trap 'rm -rf "$TMP"' EXIT

expect() { # expect <wanted rc> <actual rc> <description>
  if [ "$1" -eq "$2" ]; then
    echo "  ✔ rc=$1 as expected — $3"
  else
    echo "  ✘ wanted rc=$1, got rc=$2 — $3"
    FAILED=1
  fi
}

contains() { # contains <file> <literal> <description>
  if grep -q -F -- "$2" "$1"; then
    echo "  ✔ $3"
  else
    echo "  ✘ $3"
    echo "      wanted: $2"
    grep -n 'a\.ts\|two\.ts' "$1" | sed 's/^/      /'
    FAILED=1
  fi
}

absent() { # absent <file> <literal> <description>
  if grep -q -F -- "$2" "$1"; then
    echo "  ✘ $3"
    echo "      still there: $2"
    FAILED=1
  else
    echo "  ✔ $3"
  fi
}

# The drift checker treats a missing document as a finding, not as an empty file
# — "we could not look" must not read as "nothing to find" — and the relocation
# tool refuses to work on a tree it cannot fully parse. So its whole scan set has
# to exist in a fixture, or every run below refuses on the fixture itself.
seed_repo() { # seed_repo <dir>
  mkdir -p "$1/scripts" "$1/docs" "$1/src"
  cp "$ROOT/scripts/check-citation-drift.py" "$1/scripts/"
  cp "$ROOT/scripts/relocate-citations.py" "$1/scripts/"
  cat > "$1/README.md" <<'MD'
# Fixture

A minimal document so that the citations in docs/ have a project around them.
MD
  cat > "$1/SECURITY.md" <<'MD'
# Security

Nothing to report.
MD
  cat > "$1/CONTRIBUTING.md" <<'MD'
# Contributing

Run the checks.
MD
  cat > "$1/docs/threat-model.md" <<'MD'
# Threat model

The fixture's threat model.
MD
  (
    cd "$1" || exit 2
    git init -q .
    git config user.email "fixture@example.invalid"
    git config user.name "fixture"
  )
}

echo "=============================================================="
echo "Fixture A: a file the merge moves, grows and re-indents around"
echo "=============================================================="
FIXA="$TMP/a"
seed_repo "$FIXA"
cat > "$FIXA/docs/install.md" <<'MD'
# The relocatable cases

The body of alpha is `src/a.ts:2-4`, and beta is `src/a.ts:7-10`.

Two ranges in one span: `src/a.ts:2-4,15-17`.

Alpha again, and then gamma without repeating the file name: `src/a.ts:2-4`, `:15-17`.
MD
cat > "$FIXA/docs/privacy.md" <<'MD'
# Privacy

This document cites nothing.
MD
cat > "$FIXA/src/a.ts" <<'TS'
export function alpha(): number {
  const a = 1;
  const b = 2;
  return a + b;
}

export function beta(): number {
  const c = 3;
  return c;
}

// DUPLICATED-NOTE
export const dup = 1;

export function gamma(): number {
  return 0;
}

// GONE-NOTE
export function removed(): number {
  return -1;
}
TS
(
  cd "$FIXA" || exit 2
  git add -A
  git commit -qm "fixture A: the state the document's line numbers describe"
)
OLDA="$(cd "$FIXA" && git rev-parse HEAD)"
(cd "$FIXA" && python3 scripts/check-citation-drift.py --update >/dev/null)
echo "  fixture A at ${OLDA:0:7}"

# The digests the lock pins for the ranges about to move. The tool moves an
# anchor; it must not change what content that anchor pins, so these have to
# reappear unchanged under the new keys.
digest_2_4="$(cd "$FIXA" && awk '$1=="src/a.ts:2-4"{print $2}' docs/citations.lock)"
digest_15_17="$(cd "$FIXA" && awk '$1=="src/a.ts:15-17"{print $2}' docs/citations.lock)"
if [ -z "$digest_2_4" ] || [ -z "$digest_15_17" ]; then
  echo "  ✘ fixture A's own lock has no src/a.ts:2-4 / :15-17 anchor; the selftest is void"
  FAILED=1
fi

echo
echo "  The merge: three lines above alpha, one line inside beta."
cat > "$FIXA/src/a.ts" <<'TS'
// merged: three new lines at the top
import type { X } from "./x";
const initialised = true;

export function alpha(): number {
  const a = 1;
  const b = 2;
  return a + b;
}

export function beta(): number {
  const c = 3;
  // merged: one new line inside beta
  return c;
}

// DUPLICATED-NOTE
export const dup = 1;

export function gamma(): number {
  return 0;
}
TS
if ! grep -q 'initialised' "$FIXA/src/a.ts"; then
  echo "  ✘ the merged fixture is not the file this selftest means to write; it is void"
  FAILED=1
fi

cd "$FIXA" || exit 2

echo
echo "=============================================================="
echo "Probe 1: --dry-run prints the plan and changes nothing"
echo "=============================================================="
before="$(shasum -a 256 docs/install.md | cut -d' ' -f1)"
python3 scripts/relocate-citations.py --old "$OLDA" --dry-run >"$TMP/dry.out" 2>&1
rc=$?
expect 0 "$rc" "a plan that can be carried out must not be an error"
if [ "$before" = "$(shasum -a 256 docs/install.md | cut -d' ' -f1)" ]; then
  echo "  ✔ docs/install.md is byte-identical after --dry-run"
else
  echo "  ✘ --dry-run modified the document"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 2: the real run moves each citation to the text it named"
echo "  alpha  2-4   -> 6-8    (three lines inserted above it)"
echo "  beta   7-10  -> 11-15  (one line inserted inside it: GROWN)"
echo "  gamma  15-17 -> 20-22  (shifted by alpha's three lines and beta's one)"
echo "=============================================================="
python3 scripts/relocate-citations.py --old "$OLDA" >"$TMP/run1.out" 2>&1
rc=$?
expect 0 "$rc" "every citation in fixture A can be relocated"
contains docs/install.md '`src/a.ts:6-8`' "alpha's citation shifted to 6-8"
contains docs/install.md '`src/a.ts:11-15`' "beta's citation grew to 11-15"
contains docs/install.md '`src/a.ts:6-8,20-22`' "the two-range span was rewritten as one token"
contains docs/install.md '`:20-22`' "the continuation kept its bare form and moved"
absent docs/install.md '`src/a.ts:2-4`' "no stale range survived"
if grep -q -F 'GROWN' "$TMP/run1.out"; then
  echo "  ✔ the grown range is reported as its own class"
else
  echo "  ✘ the grown range was not reported as GROWN:"
  sed 's/^/      /' "$TMP/run1.out"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 3: the anchor moved, the pinned content did not"
echo "  Each relocated range must hash to what the old range hashed"
echo "  to: the tool moved where the citation points, it did not"
echo "  change what the sentence is a claim about."
echo "=============================================================="
python3 scripts/check-citation-drift.py --update >/dev/null 2>&1
rc=$?
expect 0 "$rc" "--update must accept the relocated documents"
moved_2_4="$(awk '$1=="src/a.ts:6-8"{print $2}' docs/citations.lock)"
moved_15_17="$(awk '$1=="src/a.ts:20-22"{print $2}' docs/citations.lock)"
if [ -n "$digest_2_4" ] && [ "$moved_2_4" = "$digest_2_4" ]; then
  echo "  ✔ src/a.ts:2-4 and src/a.ts:6-8 pin the same content ($digest_2_4)"
else
  echo "  ✘ digest changed with the range: was $digest_2_4, now $moved_2_4"
  FAILED=1
fi
if [ -n "$digest_15_17" ] && [ "$moved_15_17" = "$digest_15_17" ]; then
  echo "  ✔ src/a.ts:15-17 and src/a.ts:20-22 pin the same content ($digest_15_17)"
else
  echo "  ✘ digest changed with the range: was $digest_15_17, now $moved_15_17"
  FAILED=1
fi
python3 scripts/check-citation-drift.py >/dev/null 2>&1
rc=$?
expect 0 "$rc" "the drift check is green again after the relocation and --update"

echo
echo "=============================================================="
echo "Probe 4: a second run with the same --old is refused"
echo "  The document now carries the working tree's numbers, so"
echo "  'these numbers are side A's' is no longer a true statement"
echo "  about it. The relocated range src/a.ts:6-8 is not a range"
echo "  side A's document ever wrote — so the citation belongs to no"
echo "  declared side, and the run must refuse rather than re-read"
echo "  A's line 6 and relocate the citation to whatever that is."
echo "=============================================================="
cp docs/install.md "$TMP/install.after1"
python3 scripts/relocate-citations.py --old "$OLDA" >"$TMP/run2.out" 2>&1
rc=$?
expect 1 "$rc" "a document that is in no declared side's coordinates is an error"
if diff -q "$TMP/install.after1" docs/install.md >/dev/null; then
  echo "  ✔ the second run left the document byte-identical"
else
  echo "  ✘ the second run rewrote the document:"
  diff "$TMP/install.after1" docs/install.md | sed 's/^/      /'
  FAILED=1
fi
if grep -q -F 'no --old document writes this range' "$TMP/run2.out"; then
  echo "  ✔ the refusal says the range is in no declared side's coordinates"
else
  echo "  ✘ the refusal does not name the reason:"
  grep -F 'REFUSE' "$TMP/run2.out" | sed 's/^/      /'
  FAILED=1
fi

echo
echo "=============================================================="
echo "Fixture B: a duplicated snippet and a deleted one"
echo "=============================================================="
FIXB="$TMP/b"
seed_repo "$FIXB"
cat > "$FIXB/docs/install.md" <<'MD'
# The relocatable case

The body of alpha is `src/a.ts:2-4`.
MD
cat > "$FIXB/docs/privacy.md" <<'MD'
# The refusals

The duplicated snippet is `src/a.ts:12-13`.

The deleted function is `src/a.ts:19-21`.

The ambiguous head is `src/a.ts:24-26`.
MD
cat > "$FIXB/src/a.ts" <<'TS'
export function alpha(): number {
  const a = 1;
  const b = 2;
  return a + b;
}

export function beta(): number {
  const c = 3;
  return c;
}

// DUPLICATED-NOTE
export const dup = 1;

export function gamma(): number {
  return 0;
}

// GONE-NOTE
export function removed(): number {
  return -1;
}

// SHARED-NOTE
export const shared = 1;
export const tailMarker = 9;
TS
(
  cd "$FIXB" || exit 2
  git add -A
  git commit -qm "fixture B: the state the document's line numbers describe"
)
OLDB="$(cd "$FIXB" && git rev-parse HEAD)"
echo "  fixture B at ${OLDB:0:7}"
cat > "$FIXB/src/a.ts" <<'TS'
// merged: three new lines at the top
import type { X } from "./x";
const initialised = true;

export function alpha(): number {
  const a = 1;
  const b = 2;
  return a + b;
}

export function beta(): number {
  const c = 3;
  return c;
}

// DUPLICATED-NOTE
export const dup = 1;

// DUPLICATED-NOTE
export const dup = 1;

export function gamma(): number {
  return 0;
}

// SHARED-NOTE
export const shared = 1;

// SHARED-NOTE
export const shared = 1;

export const tailMarker = 9;
TS
if ! grep -q 'DUPLICATED-NOTE' "$FIXB/src/a.ts" || grep -q 'GONE-NOTE' "$FIXB/src/a.ts"; then
  echo "  ✘ fixture B's merged file is not what this selftest means to write; it is void"
  FAILED=1
fi

cd "$FIXB" || exit 2

echo
echo "=============================================================="
echo "Probe 5: a snippet that occurs twice is refused"
echo "  The two-line snippet is written twice in the merged file."
echo "  Only one of them is what the sentence was about and the"
echo "  tool cannot tell which — so it must leave the line alone."
echo "=============================================================="
cp docs/privacy.md "$TMP/refuse.before"
python3 scripts/relocate-citations.py --old "$OLDB" >"$TMP/run3.out" 2>&1
rc=$?
expect 1 "$rc" "a citation with more than one possible answer is an error"
if diff -q "$TMP/refuse.before" docs/privacy.md >/dev/null; then
  echo "  ✔ the refused document was left byte-identical"
else
  echo "  ✘ the refused document was edited anyway:"
  diff "$TMP/refuse.before" docs/privacy.md | sed 's/^/      /'
  FAILED=1
fi
if grep -q -F 'src/a.ts:12-13' "$TMP/run3.out" && grep -q -F 'places in the merged file' "$TMP/run3.out"; then
  echo "  ✔ the duplicate is reported, and the count of places is named"
else
  echo "  ✘ the duplicate refusal is not reported with its count:"
  grep -F 'REFUSE' "$TMP/run3.out" | sed 's/^/      /'
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 6: a snippet that is gone is refused, and is not"
echo "  reported as the duplicate above — 'the text is not there"
echo "  any more' and 'the text is there twice' are different facts"
echo "  and lead to different repairs."
echo "=============================================================="
if grep -q -F 'src/a.ts:19-21' "$TMP/run3.out" \
  && grep -q -F 'not in the merged file at all' "$TMP/run3.out"; then
  echo "  ✔ the deleted snippet is reported as absent, not as ambiguous"
else
  echo "  ✘ the deleted snippet was not reported as absent:"
  grep -F 'REFUSE' "$TMP/run3.out" | sed 's/^/      /'
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 7: a block whose first line is not unique is refused"
echo "  src/a.ts:24-26 at --old is \`// SHARED-NOTE\`, a line,"
echo "  \`export const tailMarker = 9;\`. In the merged file"
echo "  \`// SHARED-NOTE\` is written twice, the line under the first"
echo "  copy is the block's second line, and the block's third line"
echo "  is six lines further down. So the block is contiguous"
echo "  nowhere, and a tool that anchors on \`anchors[0]\` instead of"
echo "  requiring the first line to occur exactly once builds a"
echo "  7-line range out of lines that have nothing to do with each"
echo "  other and writes it into the document."
echo "=============================================================="
if grep -E 'REFUSE.*src/a\.ts:24-26' "$TMP/run3.out" >/dev/null; then
  echo "  ✔ the non-unique-anchor block is reported, not relocated"
else
  echo "  ✘ the non-unique-anchor block was not reported as refused:"
  grep -E 'src/a\.ts:24-26' "$TMP/run3.out" | sed 's/^/      /'
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 8: a refusal does not stop the rest of the run"
echo "  The tool must still be useful on a document where two"
echo "  citations need a human: it re-locates what is certain and"
echo "  exits non-zero so the run is not mistaken for a clean one."
echo "=============================================================="
contains docs/install.md '`src/a.ts:6-8`' "the certain citation was relocated despite the refusals"

echo
echo "=============================================================="
echo "Fixture C: two sides, each writing its own range of one file"
echo "  side A inserts two lines and its document cites the range"
echo "  that results; side B inserts three lines and its document"
echo "  cites the range that results. The merge keeps both lines of"
echo "  prose, so the document carries one range from each side."
echo "=============================================================="
FIXC="$TMP/c"
seed_repo "$FIXC"
cat > "$FIXC/src/two.ts" <<'TS'
P
Q
R
S
T
U
TS
cat > "$FIXC/docs/install.md" <<'MD'
# Nothing cited yet
MD
cat > "$FIXC/docs/privacy.md" <<'MD'
# Nothing cited yet
MD
(
  cd "$FIXC" || exit 2
  git add -A
  git commit -qm "fixture C: the common ancestor"
  git checkout -q -b side-a
)
OLDC_BASE="$(cd "$FIXC" && git rev-parse HEAD)"
cat > "$FIXC/src/two.ts" <<'TS'
a1
a2
P
Q
R
S
T
U
TS
cat > "$FIXC/docs/install.md" <<'MD'
# Side A

Side A cites `src/two.ts:3-4`.
MD
(
  cd "$FIXC" || exit 2
  git add -A
  git commit -qm "fixture C side A: two lines inserted, citing the result"
)
OLDC_A="$(cd "$FIXC" && git rev-parse HEAD)"
(
  cd "$FIXC" || exit 2
  git checkout -q "$OLDC_BASE"
  git checkout -q -b side-b
)
cat > "$FIXC/src/two.ts" <<'TS'
b1
b2
b3
P
Q
R
S
T
U
TS
cat > "$FIXC/docs/privacy.md" <<'MD'
# Side B

Side B cites `src/two.ts:4-5`.
MD
(
  cd "$FIXC" || exit 2
  git add -A
  git commit -qm "fixture C side B: three lines inserted, citing the result"
)
OLDC_B="$(cd "$FIXC" && git rev-parse HEAD)"
# The merge: side A's tree, plus side B's document, plus the union of the code.
(
  cd "$FIXC" || exit 2
  git checkout -q side-a
  git checkout -q "$OLDC_B" -- docs/privacy.md
)
cat > "$FIXC/src/two.ts" <<'TS'
a1
a2
b1
b2
b3
P
Q
R
S
T
U
TS
echo "  side A ${OLDC_A:0:7}, side B ${OLDC_B:0:7}, common ancestor ${OLDC_BASE:0:7}"

cd "$FIXC" || exit 2

echo
echo "=============================================================="
echo "Probe 9: one run that declares both sides moves both citations"
echo "  install.md's range is written only by side A's document,"
echo "  privacy.md's only by side B's. Both point at P,Q, which the"
echo "  merge left at lines 6-7."
echo "=============================================================="
python3 scripts/relocate-citations.py --old "$OLDC_A" --old "$OLDC_B" --dry-run >"$TMP/run4.out" 2>&1
rc=$?
expect 0 "$rc" "a citation each side wrote is relocatable in one run"
if grep -q -F 'src/two.ts:3-4 -> src/two.ts:6-7' "$TMP/run4.out" \
  && grep -q -F 'src/two.ts:4-5 -> src/two.ts:6-7' "$TMP/run4.out"; then
  echo "  ✔ both sides' citations are planned for 6-7"
else
  echo "  ✘ the plan does not move both citations:"
  sed 's/^/      /' "$TMP/run4.out"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 10: one side alone does not silently relocate the other's"
echo "  With only --old <side A>, the range side B's document writes"
echo "  is in nobody's declared coordinates. Moving it would mean"
echo "  reading it in A's line numbers, which is a different range."
echo "=============================================================="
python3 scripts/relocate-citations.py --old "$OLDC_A" --dry-run >"$TMP/run5.out" 2>&1
rc=$?
expect 1 "$rc" "an undeclared side's citation must be an error, not a silent no-op"
if grep -q -F 'no --old document writes this range' "$TMP/run5.out"; then
  echo "  ✔ side B's range is reported as unclaimed, not relocated"
else
  echo "  ✘ side B's range was not reported as unclaimed:"
  sed 's/^/      /' "$TMP/run5.out"
  FAILED=1
fi
if grep -q -F 'src/two.ts:3-4 -> src/two.ts:6-7' "$TMP/run5.out"; then
  echo "  ✔ and side A's range is still relocated"
else
  echo "  ✘ side A's range was not relocated:"
  sed 's/^/      /' "$TMP/run5.out"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 11: the real run writes both documents"
echo "=============================================================="
python3 scripts/relocate-citations.py --old "$OLDC_A" --old "$OLDC_B" >"$TMP/run6.out" 2>&1
rc=$?
expect 0 "$rc" "the multi-side run succeeds"
contains docs/install.md '`src/two.ts:6-7`' "side A's document was rewritten"
contains docs/privacy.md '`src/two.ts:6-7`' "side B's document was rewritten"

echo
echo "=============================================================="
echo "After: each fixture's own git status"
echo "=============================================================="
for d in "$FIXA" "$FIXB" "$FIXC"; do
  echo "  $(basename "$d"):"
  (cd "$d" && git status --porcelain | sed 's/^/    /')
done
echo

if [ "$FAILED" -eq 0 ]; then
  echo "SELFTEST PASS: relocation, growth, continuation, multi-span, both refusals,"
  echo "  the stale-rerun refusal and the two-sided run all behave."
  exit 0
fi
echo "SELFTEST FAIL: a probe did not behave as the tool's contract says."
exit 1
