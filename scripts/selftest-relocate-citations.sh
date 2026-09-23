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
# Fixtures, because the shapes do not fit in one repository:
#
#   A  everything relocates — a shift, a grown range, a continuation, a
#      two-range span in one token
#   B  nothing relocates — a duplicated snippet, a deleted snippet, and one
#      certain citation alongside them to prove a refusal does not stop the run
#   C  two parents — a citation written by side A's document and one written by
#      side B's, moved by a single invocation that declares both
#   D  the grown range that is not the right alignment — a cited line deleted
#      from a repeat, a deleted line whose text still exists further down, a
#      block whose lines also occur earlier, a cited line that is blank, and one
#      genuine grown range beside them all as the positive control
#   F  two parents that disagree — the same range written by both sides with
#      different text, a reworded prose line, and a range only another file's
#      document ever wrote
#   G  a citation that is the suffix of a longer token, in a document that is
#      CRLF and has no final newline
#   H  a document that cannot be written, after an earlier one has been
#   I  another worktree's copy of the tool, run from this worktree, and a
#      directory that is in no git repository at all
#   J  a grown range whose last line is an inner `}` the merge inserted, the
#      two-line block that has no interior line to check, and one that really
#      does grow
#   K  a bare `:N` continuation and a bare file name that resolves to neither
#      of the two files sharing it
#   L  a comma list one of whose spans cannot be placed
#   M  a block whose two middle lines read the same, so the block's own second
#      copy is not an alternative alignment
#   N  a document that cannot be put back after a later write failed
#
# 🔴 Probes 4, 5 and 6 are the point of the tool, not decoration. A relocation
#    that guesses when the answer is not unique is worse than the hand work it
#    saves: the range it invents still points at *a* real range, the sentence
#    around it still reads as if it were checked, and the next --update locks it
#    in. Probe 4 is the same hazard one step later — run the tool twice and the
#    second run reads a document that is no longer in anybody's coordinate
#    system, which is how a tool with the best rules in the world corrupts a
#    document.
#
# 🔴 Fixtures D, F and G are the same hazard in the one case where the answer
#    *looks* unique. Every one of them was, before this selftest existed, a run
#    that edited the document and exited 0: the range it wrote still pointed at
#    real lines, so nothing downstream could tell. Each probe below therefore
#    asserts the refusal's reason, not just the exit code — "the text is gone"
#    and "the file offers this block in two alignments" are different repairs.
#    Fixture H is the same idea for the write itself: a run that fails partway
#    must leave the documents it already rewrote as it found them.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
FAILED=0

# Fixture H makes a directory unwritable to see the write fail, and rm cannot
# empty a directory it may not write to. The mode is put back here as well as in
# the probe, so an early exit does not leave the scratch directory behind.
trap 'chmod -R u+rwX "$TMP" 2>/dev/null; rm -rf "$TMP"' EXIT

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

refused() { # refused <transcript> <ERE> <description>
  # The ERE has to match the whole refusal: the range and the reason. A refusal
  # for the right range with the wrong reason sends the reader to the wrong
  # repair, and the outcomes exist to be told apart.
  if grep -E -- "$2" "$1" >/dev/null; then
    echo "  ✔ $3"
  else
    echo "  ✘ $3"
    echo "      wanted a refusal line matching: $2"
    grep -E 'REFUSE' "$1" | sed 's/^/      /'
    FAILED=1
  fi
}

same_bytes() { # same_bytes <before-copy> <file> <description>
  if diff -q "$1" "$2" >/dev/null; then
    echo "  ✔ $3"
  else
    echo "  ✘ $3"
    diff "$1" "$2" | sed 's/^/      /'
    FAILED=1
  fi
}

file_mode() { # file_mode <file> — the permission bits, octal
  python3 -c 'import os, sys; print(oct(os.stat(sys.argv[1]).st_mode & 0o777))' "$1"
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
echo "Fixture D: a grown range, and the four shapes where the"
echo "  alignment only looks forced"
echo "  The file offers this cited block more than one alignment,"
echo "  or none, in each of these. A walk that takes the earliest"
echo "  copy of each line answers anyway: it shrinks a two-line"
echo "  claim to one, or stitches a range across two constructs"
echo "  and calls it a growth. There is one citation here that"
echo "  really did grow, in the other document, so that a run"
echo "  which refuses everything cannot pass either."
echo "=============================================================="
FIXD="$TMP/d"
seed_repo "$FIXD"
cat > "$FIXD/docs/install.md" <<'MD'
# The alignments that are not forced

The function is `src/a.ts:1-4`.

The block is `src/a.ts:5-7`.

The repeats are `src/a.ts:9-10`.

The blank line is `src/blank.ts:2`.
MD
cat > "$FIXD/docs/privacy.md" <<'MD'
# The one that does grow

The function is `src/a.ts:11-14`.
MD
cat > "$FIXD/src/a.ts" <<'TS'
fn unique_name() {
  step();
  helper();
}
// ANCHOR-LINE
// MIDDLE-LINE
// TAIL-LINE
filler
repeat();
repeat();
export function beta(): number {
  const c = 3;
  return c;
}
TS
# Line 2 is three spaces. The drift checker pins it like any other range — the
# digest of a range with no non-blank line is the digest of the empty string.
printf '// alpha\n   \n// gamma\n' > "$FIXD/src/blank.ts"
(
  cd "$FIXD" || exit 2
  git add -A
  git commit -qm "fixture D: the state the document's line numbers describe"
  python3 scripts/check-citation-drift.py --update >/dev/null
)
OLDD="$(cd "$FIXD" && git rev-parse HEAD)"
echo "  fixture D at ${OLDD:0:7}"
cat > "$FIXD/src/a.ts" <<'TS'
fn unique_name() {
  step();
}
other();
helper();
}
// ANCHOR-LINE
// MIDDLE-LINE
// UNRELATED
// TAIL-LINE
filler
// MIDDLE-LINE
// TAIL-LINE
repeat();
export function beta(): number {
  const c = 3;
  // merged: one new line inside beta
  return c;
}
TS
printf '// gamma\n// delta\n   \n' > "$FIXD/src/blank.ts"
if ! grep -q 'UNRELATED' "$FIXD/src/a.ts" || [ "$(grep -c 'repeat();' "$FIXD/src/a.ts")" -ne 1 ]; then
  echo "  ✘ fixture D's merged file is not what this selftest means to write; it is void"
  FAILED=1
fi

cd "$FIXD" || exit 2

echo
echo "=============================================================="
echo "Probe 12: the run refuses, and still relocates the one that"
echo "  is certain"
echo "=============================================================="
cp docs/install.md "$TMP/d-install.before"
python3 scripts/relocate-citations.py --old "$OLDD" >"$TMP/run7.out" 2>&1
rc=$?
expect 1 "$rc" "citations whose alignment is not forced are an error, not a relocation"
contains docs/privacy.md '`src/a.ts:15-19`' "the genuine grown range beside them still relocated"

echo
echo "=============================================================="
echo "Probe 13: two cited lines that are the same, one of them"
echo "  deleted, is not a one-line range"
echo "  src/a.ts:9-10 at --old is \`repeat();\` twice; the merge"
echo "  deletes one. The walk has to advance past the anchor"
echo "  before looking for the block's second line, or the anchor"
echo "  matches itself and the claim shrinks to \`src/a.ts:14\`"
echo "  with a negative count of inserted lines."
echo "=============================================================="
refused "$TMP/run7.out" 'REFUSE.*src/a\.ts:9-10.*not in the merged file at all' \
  "the deleted repeat is refused as absent, not shrunk to one line"
contains docs/install.md '`src/a.ts:9-10`' "it is still the range the document carries"
absent docs/install.md 'src/a.ts:14' "no one-line range was invented for it"

echo
echo "=============================================================="
echo "Probe 14: a deleted line whose text still exists further"
echo "  down the file is not stitched into the range"
echo "  The cited function lost \`helper();\` and its \`}\`. Both"
echo "  still exist further down, after \`other();\`. Taking the"
echo "  earliest later copy of each line builds src/a.ts:1-6 — a"
echo "  window covering three lines that belong to other code."
echo "=============================================================="
refused "$TMP/run7.out" 'REFUSE.*src/a\.ts:1-4.*more than one alignment.*sits between two of the others' \
  "the block whose tail repeats further down is refused"
absent docs/install.md '`src/a.ts:1-6`' "the stitched range was not written"

echo
echo "=============================================================="
echo "Probe 15: a block whose lines also occur earlier is refused"
echo "  src/a.ts:5-7 is three marker comments. All three also"
echo "  occur further down with the real pair at the end, so the"
echo "  earliest alignment is not the cited one — it stops at"
echo "  \`// UNRELATED\` and short of the very text that survived."
echo "=============================================================="
refused "$TMP/run7.out" 'REFUSE.*src/a\.ts:5-7.*more than one alignment.*occurs again later' \
  "the block with an alignment that is not forced is refused"
absent docs/install.md '`src/a.ts:7-10`' "no alignment was picked for it"

echo
echo "=============================================================="
echo "Probe 16: a cited line that is whitespace is refused"
echo "  Lines are compared after strip(), so a range of spaces is"
echo "  the empty string — and the empty string is 'found' at"
echo "  every blank line in the file. Writing one of them into"
echo "  the document moves a citation onto a line that was never"
echo "  cited, at a range nothing distinguishes from any other."
echo "=============================================================="
refused "$TMP/run7.out" 'REFUSE.*src/blank\.ts:2.*is blank at --old' \
  "the whitespace-only range is refused, not matched to a blank line"
contains docs/install.md '`src/blank.ts:2`' "its line in the document is untouched"

echo
echo "=============================================================="
echo "Probe 17: the document with the four refusals is byte-identical"
echo "=============================================================="
same_bytes "$TMP/d-install.before" docs/install.md \
  "docs/install.md is exactly as the run found it"

echo
echo "=============================================================="
echo "Fixture F: two sides that write the same range and disagree"
echo "  Both sides' documents write src/disagree.ts:3-4 and"
echo "  src/reword.ts:3-4. In the first case the resolution kept a"
echo "  sentence both sides' documents carry, so the prose cannot"
echo "  break the tie; in the second the resolution reworded the"
echo "  sentence so neither side's document carries it. Either"
echo "  way one side's text survived the merge and the other's"
echo "  was deleted, and a run that follows the surviving side"
echo "  reports a relocation for a sentence it cannot place."
echo "  The third citation names a file only one side's document"
echo "  ever wrote about — under different numbers — and that"
echo "  side's text for it is not where the numbers say."
echo "=============================================================="
FIXF="$TMP/f"
seed_repo "$FIXF"
printf 'd1\nd2\nd3\nd4\nd5\nd6\n' > "$FIXF/src/disagree.ts"
printf 'w1\nw2\nw3\nw4\nw5\nw6\n' > "$FIXF/src/reword.ts"
printf 'c1\nc2\nc3\nc4\nc5\nc6\n' > "$FIXF/src/cross.ts"
printf 'a1\na2\na3\na4\na5\na6\n' > "$FIXF/src/alpha.ts"
printf '# base\n\nnothing\n' > "$FIXF/docs/install.md"
printf '# base\n\nnothing\n' > "$FIXF/docs/privacy.md"
(
  cd "$FIXF" || exit 2
  git add -A
  git commit -qm "fixture F: the common ancestor"
  git checkout -q -b side-a
)
OLDF_BASE="$(cd "$FIXF" && git rev-parse HEAD)"
printf 'd1\nd2\nA3\nA4\nd5\nd6\n' > "$FIXF/src/disagree.ts"
printf 'w1\nw2\nRA3\nRA4\nw5\nw6\n' > "$FIXF/src/reword.ts"
printf 'a1\na2\nX3\nX4\na5\na6\n' > "$FIXF/src/alpha.ts"
cat > "$FIXF/docs/install.md" <<'MD'
# Side A

Disagree is `src/disagree.ts:3-4`.

Reword is `src/reword.ts:3-4`.

Alpha is `src/alpha.ts:3-4`.
MD
(
  cd "$FIXF" || exit 2
  git add -A
  git commit -qm "fixture F side A: three ranges, one of them alpha's"
)
OLDF_A="$(cd "$FIXF" && git rev-parse HEAD)"
(
  cd "$FIXF" || exit 2
  git checkout -q "$OLDF_BASE"
  git checkout -q -b side-b
)
printf 'd1\nd2\nB3\nB4\nd5\nd6\n' > "$FIXF/src/disagree.ts"
printf 'w1\nw2\nRB3\nRB4\nw5\nw6\n' > "$FIXF/src/reword.ts"
cat > "$FIXF/docs/install.md" <<'MD'
# Side B

Disagree is `src/disagree.ts:3-4`.

Reword is `src/reword.ts:3-4`.
MD
(
  cd "$FIXF" || exit 2
  git add -A
  git commit -qm "fixture F side B: the same two ranges, different text"
)
OLDF_B="$(cd "$FIXF" && git rev-parse HEAD)"
# The merge: side A's tree and document, with each cited pair moved down one
# line, and a document that carries one sentence from each side's prose plus a
# range no side's document writes.
(
  cd "$FIXF" || exit 2
  git checkout -q side-a
  git checkout -q "$OLDF_B" -- docs/install.md
)
printf 'd1\nd2\nZZ\nA3\nA4\nd5\nd6\n' > "$FIXF/src/disagree.ts"
printf 'w1\nw2\nYY\nRA3\nRA4\nw5\nw6\n' > "$FIXF/src/reword.ts"
printf 'c1\nc2\nQQ\nc3\nc4\nc5\nc6\n' > "$FIXF/src/cross.ts"
cat > "$FIXF/docs/install.md" <<'MD'
# Merged

Disagree is `src/disagree.ts:3-4`.

A reworded line `src/reword.ts:3-4`.

No side cites this `src/cross.ts:3-4`.
MD
echo "  side A ${OLDF_A:0:7}, side B ${OLDF_B:0:7}, ancestor ${OLDF_BASE:0:7}"

cd "$FIXF" || exit 2

echo
echo "=============================================================="
echo "Probe 18: a side that cannot place the range vetoes one that can"
echo "  Side A's text for src/disagree.ts:3-4 survived the merge"
echo "  (it is at 4-5 now); side B's was deleted. Both sides'"
echo "  documents carry the sentence, so the prose cannot say whose"
echo "  it is. The surviving side is half the evidence, and the"
echo "  other half says the text this sentence names is gone."
echo "=============================================================="
cp docs/install.md "$TMP/f-install.before"
python3 scripts/relocate-citations.py --old "$OLDF_A" --old "$OLDF_B" >"$TMP/run8.out" 2>&1
rc=$?
expect 1 "$rc" "sides that disagree about a range are an error"
absent "$TMP/run8.out" 'cannot resolve every citation' \
  "and it got as far as the citations — a fixture the parser cannot read would refuse for the wrong reason"
refused "$TMP/run8.out" 'REFUSE.*src/disagree\.ts:3-4.*do not agree about it.*not in the merged file at all' \
  "the disagreement is reported, naming the side whose text is gone"
absent docs/install.md 'src/disagree.ts:4-5' "the surviving side's range was not written"

echo
echo "=============================================================="
echo "Probe 19: a reworded sentence does not hand the range to the"
echo "  side whose text happens to have survived"
echo "  The merged sentence is in neither side's document, so the"
echo "  prose breaks no tie and both sides stay owners — which is"
echo "  the same disagreement as probe 18, reached a different way."
echo "=============================================================="
refused "$TMP/run8.out" 'REFUSE.*src/reword\.ts:3-4.*do not agree about it' \
  "the reworded sentence's range is refused"
absent docs/install.md 'src/reword.ts:4-5' "no range was picked for it"

echo
echo "=============================================================="
echo "Probe 20: the same line numbers in another file are not a claim"
echo "  Side A's document writes src/alpha.ts:3-4, never"
echo "  src/cross.ts:3-4. A claim keyed on the numbers alone makes"
echo "  side A an owner of the cross.ts citation, and side A's old"
echo "  cross.ts bytes are still in the merged file one line down —"
echo "  so the run relocates a citation of a file no side's"
echo "  document ever cited, using alpha.ts's coordinates for it."
echo "=============================================================="
refused "$TMP/run8.out" 'REFUSE.*src/cross\.ts:3-4.*no --old document writes this range' \
  "the range only another file's document wrote is unclaimed"
absent docs/install.md 'src/cross.ts:4-5' "and the citation was left where it was"
same_bytes "$TMP/f-install.before" docs/install.md \
  "docs/install.md is exactly as the run found it"

echo
echo "=============================================================="
echo "Fixture G: a citation that is the suffix of a longer path"
echo "  token, in a CRLF document with no final newline"
echo "  \`pkg/src/a.ts:12\` and \`src/a.ts:12\` are two files, and"
echo "  only src/a.ts moved. The search for the shorter token also"
echo "  matches inside the longer one, so the document ends up"
echo "  citing a line of a file the sentence did not name — and"
echo "  the read-back check cannot see it, because the damaged"
echo "  citation has exactly the numbers the plan expected."
echo "=============================================================="
FIXG="$TMP/g"
seed_repo "$FIXG"
mkdir -p "$FIXG/pkg/src"
cat > "$FIXG/src/a.ts" <<'TS'
const x1 = "src-1";
const x2 = "src-2";
const x3 = "src-3";
const x4 = "src-4";
const x5 = "src-5";
const x6 = "src-6";
const x7 = "src-7";
const x8 = "src-8";
const x9 = "src-9";
const x10 = "src-10";
const x11 = "src-11";
const moved = "A12";
const x13 = "src-13";
const x14 = "src-14";
TS
cat > "$FIXG/pkg/src/a.ts" <<'TS'
const x1 = "pkg-1";
const x2 = "pkg-2";
const x3 = "pkg-3";
const x4 = "pkg-4";
const x5 = "pkg-5";
const x6 = "pkg-6";
const x7 = "pkg-7";
const x8 = "pkg-8";
const x9 = "pkg-9";
const x10 = "pkg-10";
const x11 = "pkg-11";
const kept = "PKG12";
const x13 = "pkg-13";
const x14 = "pkg-14";
TS
# CRLF, and the last line has no newline after it. Neither is what the tool
# writes by default, and both are properties of a document the tool must keep.
printf '# Fixture G\r\n\r\nStay `pkg/src/a.ts:12` and move `src/a.ts:12`.' \
  > "$FIXG/docs/install.md"
printf '# Fixture G\n\nPrivacy cites nothing.\n' > "$FIXG/docs/privacy.md"
if ! od -c "$FIXG/docs/install.md" | grep -q '\\r'; then
  echo "  ✘ this printf does not write the CR fixture G needs; the selftest is void"
  FAILED=1
fi
(
  cd "$FIXG" || exit 2
  git add -A
  git commit -qm "fixture G: two files with the same line numbers"
)
OLDG="$(cd "$FIXG" && git rev-parse HEAD)"
echo "  fixture G at ${OLDG:0:7}"
cat > "$FIXG/src/a.ts" <<'TS'
// inserted 1
// inserted 2
// inserted 3
// inserted 4
// inserted 5
const x1 = "src-1";
const x2 = "src-2";
const x3 = "src-3";
const x4 = "src-4";
const x5 = "src-5";
const x6 = "src-6";
const x7 = "src-7";
const x8 = "src-8";
const x9 = "src-9";
const x10 = "src-10";
const x11 = "src-11";
const moved = "A12";
const x13 = "src-13";
const x14 = "src-14";
TS

cd "$FIXG" || exit 2

echo
echo "=============================================================="
echo "Probe 21: only the citation of the file that moved was rewritten"
echo "=============================================================="
mode_before="$(file_mode docs/install.md)"
python3 scripts/relocate-citations.py --old "$OLDG" >"$TMP/run9.out" 2>&1
rc=$?
expect 0 "$rc" "one citation moved, the other is already right"
contains docs/install.md '`pkg/src/a.ts:12` and move `src/a.ts:17`' \
  "the shorter token was rewritten where it starts, not inside the longer one"
absent docs/install.md 'pkg/src/a.ts:17' "the longer token's file was left alone"
if [ "$mode_before" = "$(file_mode docs/install.md)" ]; then
  echo "  ✔ docs/install.md kept its permission bits (${mode_before#0} → the same)"
else
  echo "  ✘ the rewrite changed the document's permissions: $mode_before → $(file_mode docs/install.md)"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 22: the document kept its CRLF endings and its missing"
echo "  final newline"
echo "  Rewriting a document as \\n-joined lines plus a trailing"
echo "  \\n converts every ending and adds one that was not there."
echo "  The expected bytes are written out in full: the content,"
echo "  the \\r\\n after each line, and the absent final newline"
echo "  are one assertion, because a document is one file."
echo "=============================================================="
printf '# Fixture G\r\n\r\nStay `pkg/src/a.ts:12` and move `src/a.ts:17`.' \
  > "$TMP/g-expected"
same_bytes "$TMP/g-expected" docs/install.md \
  "docs/install.md is byte for byte the planned rewrite"

echo
echo "=============================================================="
echo "Fixture H: a document that cannot be written"
echo "  README.md is written before contracts/api.md, and"
echo "  contracts/ is made unwritable so the second write fails."
echo "  A write loop that truncates each document in turn leaves"
echo "  README.md rewritten by a run that then failed — the run"
echo "  has to put back what it already changed."
echo "=============================================================="
FIXH="$TMP/h"
seed_repo "$FIXH"
mkdir -p "$FIXH/contracts"
cat > "$FIXH/docs/install.md" <<'MD'
# Fixture H

Install cites nothing.
MD
cat > "$FIXH/README.md" <<'MD'
# Fixture H

Readme cites `src/a.ts:2-3`.
MD
cat > "$FIXH/contracts/api.md" <<'MD'
# Contract

Contract cites `src/a.ts:2-3`.
MD
printf '# Fixture H\n\nPrivacy cites nothing.\n' > "$FIXH/docs/privacy.md"
printf 'AA\nBB\nCC\nDD\n' > "$FIXH/src/a.ts"
(
  cd "$FIXH" || exit 2
  git add -A
  git commit -qm "fixture H: two documents that relocate"
)
OLDH="$(cd "$FIXH" && git rev-parse HEAD)"
echo "  fixture H at ${OLDH:0:7}"
printf 'PP\nQQ\nAA\nBB\nCC\nDD\n' > "$FIXH/src/a.ts"
cp "$FIXH/README.md" "$TMP/h-readme.before"
# Both the file and its directory, so that neither the truncate-and-write this
# replaced nor the temporary file beside it can be created. (If these modes do
# not take effect — a run as root, say — the probe fails loudly below, which is
# the direction a probe should fail in.)
chmod 500 "$FIXH/contracts"
chmod 400 "$FIXH/contracts/api.md"

cd "$FIXH" || exit 2

echo
echo "=============================================================="
echo "Probe 23: a failed write is an error"
echo "=============================================================="
python3 scripts/relocate-citations.py --old "$OLDH" >"$TMP/run10.out" 2>&1
rc=$?
expect 1 "$rc" "a document that could not be written must not exit 0"
absent "$TMP/run10.out" 'cannot resolve every citation' \
  "and it got as far as the citations — the refusal is the write, not the parse"

echo
echo "=============================================================="
echo "Probe 24: the document already rewritten was put back"
echo "=============================================================="
chmod 700 "$FIXH/contracts"
chmod 600 "$FIXH/contracts/api.md"
same_bytes "$TMP/h-readme.before" README.md \
  "README.md is exactly as the run found it"

echo
echo
echo "=============================================================="
echo "Fixture I: the repository is the working directory's"
echo "  The tool left in another worktree is invoked from this"
echo "  one. Its own copy of the script sits in a tree that is a"
echo "  whole other repository, with the same commits, so a run"
echo "  that resolves the repo from __file__ edits *that* tree —"
echo "  and prints an ordinary-looking plan while doing it."
echo "=============================================================="
FIXI="$TMP/i"
seed_repo "$FIXI"
cat > "$FIXI/docs/install.md" <<'MD'
# Fixture I

Alpha is `src/a.ts:2-3`.
MD
cat > "$FIXI/docs/privacy.md" <<'MD'
# Fixture I

Privacy cites nothing.
MD
printf 'A1\nA2\nA3\nA4\n' > "$FIXI/src/a.ts"
(
  cd "$FIXI" || exit 2
  git add -A
  git commit -qm "fixture I: the state the document's line numbers describe"
)
OLDI="$(cd "$FIXI" && git rev-parse HEAD)"
echo "  fixture I at ${OLDI:0:7}"
# The merge moves the cited pair down two lines.
printf 'NEW1\nNEW2\nA1\nA2\nA3\nA4\n' > "$FIXI/src/a.ts"
# The other worktree: the same commits, so the same --old resolves there too —
# which is what makes the wrong tree answer the question instead of erroring.
FIXI_OTHER="$TMP/i-other"
cp -R "$FIXI" "$FIXI_OTHER"
cp "$FIXI_OTHER/docs/install.md" "$TMP/i-other-install.before"

echo
echo "=============================================================="
echo "Probe 25: the run rewrites the tree it was run in, not the"
echo "  one its script file lives in"
echo "=============================================================="
(
  cd "$FIXI" || exit 2
  python3 "$FIXI_OTHER/scripts/relocate-citations.py" --old "$OLDI"
) >"$TMP/run11.out" 2>&1
rc=$?
expect 0 "$rc" "another worktree's copy can still relocate in this one"
contains "$FIXI/docs/install.md" '`src/a.ts:4-5`' "this worktree's document was the one rewritten"
same_bytes "$TMP/i-other-install.before" "$FIXI_OTHER/docs/install.md" \
  "the worktree the script file lives in was not touched"

echo
echo "=============================================================="
echo "Probe 26: outside a git repository it refuses"
echo "  --dry-run so that a wrong answer cannot edit a document on"
echo "  the way to failing this probe."
echo "=============================================================="
mkdir -p "$TMP/not-a-repo"
(
  cd "$TMP/not-a-repo" || exit 2
  python3 "$FIXI/scripts/relocate-citations.py" --old "$OLDI" --dry-run
) >"$TMP/run12.out" 2>&1
rc=$?
expect 2 "$rc" "a directory that is in no repository is a usage error, not a guess"
contains "$TMP/run12.out" 'not inside a git repository' \
  "and it says which question it could not answer"

echo
echo "=============================================================="
echo "Fixture J: a grown range that ends on the wrong line"
echo "  The cited block's last line is a closing brace. The merge"
echo "  inserts a construct *inside* the block, and the walk takes"
echo "  the brace that closes the insertion — an earlier copy than"
echo "  the one the citation named — so the range it writes ends"
echo "  inside the cited construct and still points at real lines."
echo "  Fixture D's alignments are caught by the interior lines;"
echo "  these two are the shapes where the last step is the only"
echo "  wrong one, including the two-line block that has no"
echo "  interior line at all. The control beside them really does"
echo "  grow, so a run that refuses everything cannot pass."
echo "=============================================================="
FIXJ="$TMP/j"
seed_repo "$FIXJ"
cat > "$FIXJ/docs/install.md" <<'MD'
# The grown ranges that end on the wrong line

The inner brace is `src/inner.ts:1-4`.

The pair is `src/pair.ts:1-2`.
MD
cat > "$FIXJ/docs/privacy.md" <<'MD'
# The one that really does grow

The function is `src/control.ts:1-4`.
MD
cat > "$FIXJ/src/inner.ts" <<'TS'
fn unique_name() {
  step();
  helper();
}
TS
cat > "$FIXJ/src/pair.ts" <<'TS'
fn unique_only_here() {
}
TS
cat > "$FIXJ/src/control.ts" <<'TS'
export function beta(): number {
  const c = 3;
  return c;
}
TS
(
  cd "$FIXJ" || exit 2
  git add -A
  git commit -qm "fixture J: the state the document's line numbers describe"
)
OLDJ="$(cd "$FIXJ" && git rev-parse HEAD)"
echo "  fixture J at ${OLDJ:0:7}"
cat > "$FIXJ/src/inner.ts" <<'TS'
fn unique_name() {
  step();
  if (guard) {
    helper();
  }
  tail();
}
TS
cat > "$FIXJ/src/pair.ts" <<'TS'
fn unique_only_here() {
  if (guard) {
  }
  tail();
}
TS
cat > "$FIXJ/src/control.ts" <<'TS'
export function beta(): number {
  const c = 3;
  // merged: one new line inside beta
  return c;
}
TS
if ! grep -q 'guard' "$FIXJ/src/inner.ts" || ! grep -q 'tail();' "$FIXJ/src/inner.ts" \
  || [ "$(wc -l < "$FIXJ/src/inner.ts")" -ne 7 ]; then
  echo "  ✘ fixture J's merged file is not what this selftest means to write; it is void"
  FAILED=1
fi

cd "$FIXJ" || exit 2

echo
echo "=============================================================="
echo "Probe 27: a window that stops on an inner brace is refused"
echo "  src/inner.ts:1-4 is the whole of fn unique_name. The merge"
echo "  put an \`if\` inside it, and the walk's last step lands on"
echo "  the \`}\` that closes the \`if\` — line 5 — while the"
echo "  function's own \`}\` is line 7."
echo "=============================================================="
cp docs/install.md "$TMP/j-install.before"
python3 scripts/relocate-citations.py --old "$OLDJ" >"$TMP/run13.out" 2>&1
rc=$?
expect 1 "$rc" "a window whose end is not forced is an error, not a relocation"
refused "$TMP/run13.out" 'REFUSE.*src/inner\.ts:1-4 .*is not the only candidate' \
  "the inner-brace window is refused, naming the end line"
absent docs/install.md 'src/inner.ts:1-5' "the range that ends inside the function was not written"

echo
echo "=============================================================="
echo "Probe 28: a two-line block has no interior check to fall back on"
echo "  src/pair.ts:1-2 is fn unique_only_here and its \`}\`. Both"
echo "  lines are cited, so the interior rule has nothing to test,"
echo "  and the walk takes the \`}\` of an inserted \`if\` on line 3"
echo "  as the end — with the function's own \`}\` still on line 5."
echo "=============================================================="
refused "$TMP/run13.out" 'REFUSE.*src/pair\.ts:1-2 .*is not the only candidate' \
  "the two-line window is refused by the same rule"
absent docs/install.md 'src/pair.ts:1-3' "the range that ends on the inserted brace was not written"

echo
echo "=============================================================="
echo "Probe 29: the block that really grows still relocates"
echo "  lines inserted inside a cited block that opens and closes"
echo "  its own construct leave the window's balance where the"
echo "  block's was, so this is still a relocation and the run"
echo "  still exits non-zero for the two refusals beside it."
echo "=============================================================="
contains docs/privacy.md '`src/control.ts:1-5`' "the genuine grown range relocated"
same_bytes "$TMP/j-install.before" docs/install.md \
  "docs/install.md is exactly as the run found it"

echo
echo "=============================================================="
echo "Fixture K: a continuation and a bare file name"
echo "  A bare \`:3-4\` continues the file its own sentence named —"
echo "  src/alpha.ts — and not every file with those line numbers."
echo "  A token of \`a.ts\` names one file, and this repository has"
echo "  two; a name that resolves to neither claims neither. Both"
echo "  shapes used to make a side an owner of a citation it never"
echo "  wrote, and both times the cited file's old bytes were still"
echo "  in the merged tree, so the run moved the citation and"
echo "  exited 0."
echo "=============================================================="
FIXK="$TMP/k"
seed_repo "$FIXK"
mkdir -p "$FIXK/pkg/one" "$FIXK/pkg/two"
printf 'k1\nk2\nk3\nk4\nk5\nk6\n' > "$FIXK/src/alpha.ts"
printf 'c1\nc2\nc3\nc4\nc5\nc6\n' > "$FIXK/src/cross.ts"
printf 'g1\ng2\ng3\ng4\n' > "$FIXK/src/gamma.ts"
printf 'o1\no2\no3\no4\n' > "$FIXK/pkg/one/a.ts"
printf 't1\nt2\nt3\nt4\n' > "$FIXK/pkg/two/a.ts"
cat > "$FIXK/docs/install.md" <<'MD'
# Side A

Alpha is `src/alpha.ts:1-2`, and more of it is `:3-4`.

The basename is `a.ts:1-2`.

Gamma is `src/gamma.ts:1-2`.
MD
cat > "$FIXK/docs/privacy.md" <<'MD'
# Fixture K

Privacy cites nothing.
MD
(
  cd "$FIXK" || exit 2
  git add -A
  git commit -qm "fixture K: the side that wrote the continuation and the bare name"
)
OLDK="$(cd "$FIXK" && git rev-parse HEAD)"
echo "  fixture K at ${OLDK:0:7}"
# The merge: the sentence that was about alpha now names cross, the bare name
# now names one of the two files it could mean, and each cited file gained a
# line at the top so the old bytes are still there, one line down.
printf 'ZZ\nk1\nk2\nk3\nk4\nk5\nk6\n' > "$FIXK/src/alpha.ts"
printf 'QQ\nc1\nc2\nc3\nc4\nc5\nc6\n' > "$FIXK/src/cross.ts"
printf 'ZZ\ng1\ng2\ng3\ng4\n' > "$FIXK/src/gamma.ts"
printf 'YY\nt1\nt2\nt3\nt4\n' > "$FIXK/pkg/two/a.ts"
cat > "$FIXK/docs/install.md" <<'MD'
# Merged

Alpha is `src/cross.ts:3-4`.

The basename is `pkg/two/a.ts:1-2`.

Gamma is `src/gamma.ts:1-2`.
MD

cd "$FIXK" || exit 2

echo
echo "=============================================================="
echo "Probe 30: a bare continuation does not own another file"
echo "  Side A's document writes src/alpha.ts:1-2 and then a bare"
echo "  \`:3-4\` continuing it. src/cross.ts:3-4 is a citation no"
echo "  declared side's document ever wrote: the continuation is a"
echo "  claim about alpha, not about everything with a line 3."
echo "=============================================================="
cp docs/install.md "$TMP/k-install.before"
python3 scripts/relocate-citations.py --old "$OLDK" >"$TMP/run14.out" 2>&1
rc=$?
expect 1 "$rc" "an unclaimed range is an error, not a relocation"
refused "$TMP/run14.out" 'REFUSE.*src/cross\.ts:3-4 .*no --old document writes this range' \
  "the file the continuation never named is unclaimed"
absent docs/install.md 'src/cross.ts:4-5' "and the citation was left where it was"

echo
echo "=============================================================="
echo "Probe 31: a bare file name does not own every file with that"
echo "  name"
echo "  \`a.ts:1-2\` in side A's document resolves to no single file"
echo "  here — pkg/one/a.ts and pkg/two/a.ts share the name — so it"
echo "  is a claim about neither, and the citation of pkg/two/a.ts"
echo "  cannot be moved on the strength of it."
echo "=============================================================="
refused "$TMP/run14.out" 'REFUSE.*pkg/two/a\.ts:1-2 .*no --old document writes this range' \
  "the same-named file the bare token never resolved to is unclaimed"
absent docs/install.md 'pkg/two/a.ts:2-3' "and its citation was left where it was"

echo
echo "=============================================================="
echo "Probe 32: the citation the side did name is still relocated"
echo "  The refusals above must not turn the run into one that"
echo "  refuses everything: side A's document writes"
echo "  src/gamma.ts:1-2 outright, and gamma's lines moved down one."
echo "=============================================================="
contains docs/install.md '`src/gamma.ts:2-3`' "the named citation was relocated"

echo
echo "=============================================================="
echo "Fixture L: one span of a comma list cannot be placed"
echo "  A comma list is one token in the document and N citations"
echo "  out of the parser. Rewriting it with the spans that could"
echo "  be placed and the ones that could not left as they were"
echo "  produced \`src/a.ts:6-8,15-17\`: half the token in the"
echo "  merged tree's numbers and half in the parent's, pointing"
echo "  at a range nobody wrote. The token stays as it was, and"
echo "  every span of it is reported, because half a token is not"
echo "  something a reader can act on."
echo "=============================================================="
FIXL="$TMP/l"
seed_repo "$FIXL"
cat > "$FIXL/docs/install.md" <<'MD'
# The comma list

The spans are `src/a.ts:2-4,15-17`.
MD
cat > "$FIXL/docs/privacy.md" <<'MD'
# Fixture L

Privacy cites nothing.
MD
cat > "$FIXL/src/a.ts" <<'TS'
export function alpha(): number {
  const a = 1;
  const b = 2;
  return a + b;
}

export function beta(): number {
  const c = 3;
  return c;
}

export function gamma(): number {
  return 0;
}
// DELETED-HEAD
export const doomed = 1;
export const doomedToo = 2;

export function removed(): number {
  return -1;
}
TS
(
  cd "$FIXL" || exit 2
  git add -A
  git commit -qm "fixture L: the two spans the one token names"
)
OLDL="$(cd "$FIXL" && git rev-parse HEAD)"
echo "  fixture L at ${OLDL:0:7}"
cat > "$FIXL/src/a.ts" <<'TS'
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

export function gamma(): number {
  return 0;
}

export function removed(): number {
  return -1;
}
TS
cd "$FIXL" || exit 2

echo
echo "=============================================================="
echo "Probe 33: the token is left whole, and is still the token"
echo "  The first span moved with the file; the second names three"
echo "  lines the merge deleted. There is no rewrite of this token"
echo "  that is not half old and half new."
echo "=============================================================="
cp docs/install.md "$TMP/l-install.before"
python3 scripts/relocate-citations.py --old "$OLDL" >"$TMP/run15.out" 2>&1
rc=$?
expect 1 "$rc" "a token only half of which can be placed is an error"
contains docs/install.md '`src/a.ts:2-4,15-17`' "the token is still the one the document had"
absent docs/install.md 'src/a.ts:6-8,15-17' "no half-rewritten token was written"
same_bytes "$TMP/l-install.before" docs/install.md \
  "docs/install.md is exactly as the run found it"

echo
echo "=============================================================="
echo "Probe 34: and both spans of it are reported"
echo "  The span that could be placed is a refusal too: it is not"
echo "  being relocated, and a summary that counted it as one would"
echo "  describe a document that is not on disk."
echo "=============================================================="
refused "$TMP/run15.out" 'REFUSE.*src/a\.ts:2-4 .*left whole' \
  "the placeable span is reported as part of a token left whole"
refused "$TMP/run15.out" 'REFUSE.*src/a\.ts:15-17 .*not in the merged file at all' \
  "and the unplaceable span is reported as the text that is gone"

echo
echo "=============================================================="
echo "Fixture M: a block whose two middle lines read the same"
echo "  The citation names fn unique_name, \`step();\`, \`step();\`"
echo "  and the closing brace. The two identical lines are the"
echo "  block's own content, not two alignments to choose between —"
echo "  the walk placed both, and the range it produced is the one"
echo "  the sentence is about. Counting the block's own second copy"
echo "  as an alternative refused it and left the citation stale."
echo "=============================================================="
FIXM="$TMP/m"
seed_repo "$FIXM"
cat > "$FIXM/docs/install.md" <<'MD'
# The repeated line inside the block

The function is `src/a.ts:1-4`.
MD
cat > "$FIXM/docs/privacy.md" <<'MD'
# Fixture M

Privacy cites nothing.
MD
cat > "$FIXM/src/a.ts" <<'TS'
fn unique_name() {
  step();
  step();
}
TS
(
  cd "$FIXM" || exit 2
  git add -A
  git commit -qm "fixture M: the state the document's line numbers describe"
)
OLDM="$(cd "$FIXM" && git rev-parse HEAD)"
echo "  fixture M at ${OLDM:0:7}"
cat > "$FIXM/src/a.ts" <<'TS'
fn unique_name() {
  step();
  step();
  extra();
}
TS
cd "$FIXM" || exit 2

echo
echo "=============================================================="
echo "Probe 35: the range relocates, and is reported as grown"
echo "=============================================================="
python3 scripts/relocate-citations.py --old "$OLDM" >"$TMP/run16.out" 2>&1
rc=$?
expect 0 "$rc" "a block whose embedding is unique is a relocation, not a refusal"
contains docs/install.md '`src/a.ts:1-5`' "the function's range grew to the whole construct"
absent docs/install.md '`src/a.ts:1-4`' "no stale range survived"
if grep -q -F 'GROWN' "$TMP/run16.out"; then
  echo "  ✔ and the growth is reported as GROWN"
else
  echo "  ✘ the growth was not reported as GROWN:"
  sed 's/^/      /' "$TMP/run16.out"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Fixture N: a document that cannot be put back"
echo "  The write that fails is real, exactly as in fixture H:"
echo "  contracts/ is made unwritable, so contracts/api.md fails"
echo "  after README.md has been written. The failing *restore*"
echo "  cannot be staged from outside — the same mode that would"
echo "  refuse the restore refuses the original write first — so"
echo "  it is injected, and only it: the first write of README.md"
echo "  is the real one, and the second call is the run putting it"
echo "  back. A summary that says every document was put back"
echo "  reports a document that is in the wrong state as untouched."
echo "=============================================================="
FIXN="$TMP/n"
seed_repo "$FIXN"
mkdir -p "$FIXN/contracts"
cat > "$FIXN/docs/install.md" <<'MD'
# Fixture N

Install cites nothing.
MD
cat > "$FIXN/README.md" <<'MD'
# Fixture N

Readme cites `src/a.ts:2-3`.
MD
cat > "$FIXN/contracts/api.md" <<'MD'
# Contract

Contract cites `src/a.ts:2-3`.
MD
printf '# Fixture N\n\nPrivacy cites nothing.\n' > "$FIXN/docs/privacy.md"
printf 'AA\nBB\nCC\nDD\n' > "$FIXN/src/a.ts"
(
  cd "$FIXN" || exit 2
  git add -A
  git commit -qm "fixture N: two documents that relocate"
)
OLDN="$(cd "$FIXN" && git rev-parse HEAD)"
echo "  fixture N at ${OLDN:0:7}"
printf 'PP\nQQ\nAA\nBB\nCC\nDD\n' > "$FIXN/src/a.ts"
chmod 500 "$FIXN/contracts"
chmod 400 "$FIXN/contracts/api.md"

echo
echo "=============================================================="
echo "Probe 36: a restore that fails is reported as a failure"
echo "=============================================================="
python3 - "$FIXN" "$OLDN" >"$TMP/run17.out" 2>&1 <<'PY'
import importlib.util
import os
import sys

fixture, old = sys.argv[1], sys.argv[2]
os.chdir(fixture)
spec = importlib.util.spec_from_file_location(
    "relocate_citations_under_test",
    os.path.join(fixture, "scripts", "relocate-citations.py"),
)
mod = importlib.util.module_from_spec(spec)
sys.modules["relocate_citations_under_test"] = mod
spec.loader.exec_module(mod)

real_write_atomic = mod.write_atomic
seen = {"README.md": 0}


def write_atomic_whose_restore_fails(doc, text):
    if doc == "README.md":
        seen[doc] += 1
        if seen[doc] > 1:
            # The second call for this document is the run putting it back.
            raise OSError(13, "injected: the restore cannot be written")
    real_write_atomic(doc, text)


mod.write_atomic = write_atomic_whose_restore_fails
sys.argv = ["relocate-citations.py", "--old", old]
sys.exit(mod.main())
PY
rc=$?
chmod 700 "$FIXN/contracts"
chmod 600 "$FIXN/contracts/api.md"
expect 1 "$rc" "a document left rewritten must not be reported as a clean run"
if grep -q -F 'COULD NOT RESTORE: README.md' "$TMP/run17.out"; then
  echo "  ✔ the summary names the document it could not put back"
else
  echo "  ✘ the summary does not name the unrestored document:"
  sed 's/^/      /' "$TMP/run17.out"
  FAILED=1
fi
absent "$TMP/run17.out" 'nothing was changed' \
  "and it does not claim the tree is as the run found it"

echo
echo "=============================================================="
echo "After: each fixture's own git status"
echo "=============================================================="
for d in "$FIXA" "$FIXB" "$FIXC" "$FIXD" "$FIXF" "$FIXG" "$FIXH" \
         "$FIXI" "$FIXI_OTHER" "$FIXJ" "$FIXK" "$FIXL" "$FIXM"; do
  echo "  $(basename "$d"):"
  (cd "$d" && git status --porcelain | sed 's/^/    /')
done
echo

if [ "$FAILED" -eq 0 ]; then
  echo "SELFTEST PASS: relocation, growth, continuation, multi-span, both refusals,"
  echo "  the stale-rerun refusal and the two-sided run all behave — as do the"
  echo "  grown ranges that are not the right alignment, the whitespace-only range,"
  echo "  the disagreeing sides, the range only another file's document wrote, the"
  echo "  citation that is a suffix of a longer token, the document that keeps its"
  echo "  own newlines, and the failed write that is put back."
  exit 0
fi
echo "SELFTEST FAIL: a probe did not behave as the tool's contract says."
exit 1
