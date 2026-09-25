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
# 🔴 The tool relocates on **exact shifts only**: the block a citation named must
#    still be one unchanged run of lines, in exactly one place. Every probe
#    below that used to expect a *grown* range to be relocated now expects a
#    refusal instead, because the grown heuristic is gone — it walked the file
#    for the cited lines in order and took the earliest alignment each step
#    allowed, and three review rounds each built an input where the earliest
#    alignment was not the cited one and the run wrote a range nobody had cited
#    while exiting 0. A refusal that leaves the range stale is recoverable; a
#    wrong range that validates is not. So each of those probes is kept, and
#    asserts the refusal's reason, not just the exit code.
#
# Fixtures, because the shapes do not fit in one repository:
#
#   A  everything relocates — two shifts, a continuation, a two-range span in
#      one token, and one block that grew and is therefore refused beside them
#   B  nothing relocates — a duplicated snippet, a deleted snippet, and one
#      certain citation alongside them to prove a refusal does not stop the run
#   C  two parents — a citation written by side A's document and one written by
#      side B's, moved by a single invocation that declares both
#   D  the shapes that are not an exact shift — a cited line deleted from a
#      repeat, a deleted line whose text still exists further down, a block
#      whose lines also occur earlier, a cited line that is blank, a block that
#      grew, and one certain shift beside them all as the positive control
#   F  two parents that disagree — the same range written by both sides with
#      different text, a reworded prose line, and a range only another file's
#      document ever wrote
#   G  a citation that is the suffix of a longer token, in a document that is
#      CRLF and has no final newline
#   H  a document that cannot be written, after an earlier one has been
#   I  another worktree's copy of the tool, run from this worktree, and a
#      directory that is in no git repository at all
#   J  a block whose last line the merge's insertion left behind — the `}` of an
#      inner `if`, and the two-line block that has no interior line at all —
#      plus one certain shift so that a run refusing everything cannot pass
#   K  a bare `:N` continuation and a bare file name that resolves to neither
#      of the two files sharing it
#   L  a comma list one of whose spans cannot be placed
#   M  a block whose two middle lines read the same, and which grew anyway
#   N  a document that cannot be put back after a later write failed
#   O  the three inputs three review rounds built to make the grown heuristic
#      write a wrong range and exit 0: a `);` that closes an inner call, a `}`
#      written inside a `// }` comment, and a bare name whose file the merge
#      deleted while another file with that name remained
#   P  a read-back restore that cannot be written
#   Q  the four constructions R66d built where a bare name now names a
#      different file and the citation is no longer on the document line it sat
#      on at --old, so the line-keyed comparison saw nothing: one parent with a
#      second citation of the file supplying the ownership, two parents with
#      side B supplying it, the file that never moved (reported `right`), and a
#      bare name that resolved to nothing at --old — plus a continuation whose
#      sentence named one file at --old and another after the merge, and a path
#      the parent's own document writes and its own tree does not have
#   R  the two symlink shapes R66e found, each beside a certain shift: a cited
#      file that is a symlink in the working tree (git show reads the link's
#      target text, the working tree follows the link), and one that is a
#      symlink only at --old, replaced by a regular file the merge left the old
#      link text in
#   S  a `:N` that is part of another side's `name:N`: the merged tree drops the
#      token (`Makefile` deleted, so it is no longer path-shaped), the colon
#      reads as a continuation, and the range is called "already right" against
#      a file the sentence never named
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
# 🔴 Fixtures D, F, G and O are the same hazard in the case where the answer
#    *looks* unique. Every one of them was, before this selftest existed, a run
#    that edited the document and exited 0: the range it wrote still pointed at
#    real lines, so nothing downstream could tell. Fixtures H and P are the same
#    idea for the write itself: a run that fails partway must either leave the
#    documents it already rewrote as it found them, or say plainly which ones it
#    could not.

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
  mkdir -p "$1/scripts" "$1/docs-dev" "$1/src"
  cp "$ROOT/scripts/check-citation-drift.py" "$1/scripts/"
  cp "$ROOT/scripts/relocate-citations.py" "$1/scripts/"
  cat > "$1/README.md" <<'MD'
# Fixture

A minimal document so that the citations in docs-dev/ have a project around them.
MD
  cat > "$1/SECURITY.md" <<'MD'
# Security

Nothing to report.
MD
  cat > "$1/CONTRIBUTING.md" <<'MD'
# Contributing

Run the checks.
MD
  cat > "$1/docs-dev/threat-model.md" <<'MD'
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
echo "Fixture A: a file the merge moves, re-indents and grows around"
echo "=============================================================="
FIXA="$TMP/a"
seed_repo "$FIXA"
cat > "$FIXA/docs-dev/install.md" <<'MD'
# The relocatable cases

The body of alpha is `src/a.ts:2-4`, and beta is `src/a.ts:7-10`.

Two ranges in one span: `src/a.ts:2-4,15-17`.

Alpha again, and then gamma without repeating the file name: `src/a.ts:2-4`, `:15-17`.
MD
cat > "$FIXA/docs-dev/privacy.md" <<'MD'
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
digest_2_4="$(cd "$FIXA" && awk '$1=="src/a.ts:2-4"{print $2}' docs-dev/citations.lock)"
digest_15_17="$(cd "$FIXA" && awk '$1=="src/a.ts:15-17"{print $2}' docs-dev/citations.lock)"
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
echo "  Fixture A's plan is not clean — beta's block grew, so it is"
echo "  refused — and a dry run over it must say so with the same"
echo "  non-zero exit the real run gives. A clean plan's exit 0 is"
echo "  probe 9's job, where both citations really do relocate."
echo "=============================================================="
before="$(shasum -a 256 docs-dev/install.md | cut -d' ' -f1)"
python3 scripts/relocate-citations.py --old "$OLDA" --dry-run >"$TMP/dry.out" 2>&1
rc=$?
expect 1 "$rc" "a plan with a refusal in it must be an error"
if [ "$before" = "$(shasum -a 256 docs-dev/install.md | cut -d' ' -f1)" ]; then
  echo "  ✔ docs-dev/install.md is byte-identical after --dry-run"
else
  echo "  ✘ --dry-run modified the document"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 2: the real run moves each citation to the text it named"
echo "  alpha  2-4   -> 6-8    (three lines inserted above it)"
echo "  beta   7-10  -> REFUSED (one line inserted inside it)"
echo "  gamma  15-17 -> 20-22  (shifted by alpha's three lines)"
echo "  The run relocates the two exact shifts and refuses beta, so"
echo "  it exits non-zero — and beta's line keeps the text it had."
echo "=============================================================="
python3 scripts/relocate-citations.py --old "$OLDA" >"$TMP/run1.out" 2>&1
rc=$?
expect 1 "$rc" "a block with a line inserted inside it is refused, so the run is not clean"
contains docs-dev/install.md '`src/a.ts:6-8`' "alpha's citation shifted to 6-8"
contains docs-dev/install.md '`src/a.ts:6-8,20-22`' "the two-range span was rewritten as one token"
contains docs-dev/install.md '`:20-22`' "the continuation kept its bare form and moved"
absent docs-dev/install.md '`src/a.ts:2-4`' "no stale range survived"
refused "$TMP/run1.out" 'REFUSE.*src/a\.ts:7-10 .*not in the merged file at all' \
  "beta's grown block is refused as text that is no longer one run of lines"
contains docs-dev/install.md 'and beta is `src/a.ts:7-10`' \
  "beta's citation is left with the text it had, for a human"
absent "$TMP/run1.out" 'GROWN' "and nothing is reported as a growth any more"
absent docs-dev/install.md 'src/a.ts:11-15' "the grown range was not invented for it"

echo
echo "=============================================================="
echo "Probe 3: the anchor moved, the pinned content did not"
echo "  Each relocated range must hash to what the old range hashed"
echo "  to: the tool moved where the citation points, it did not"
echo "  change what the sentence is a claim about."
echo ""
echo "  The refusal in probe 2 left beta's range stale on purpose,"
echo "  and a document in that state is not one --update may be run"
echo "  over: pinning a refused anchor locks in a range the run just"
echo "  said it could not place. Repairing it is the human step the"
echo "  refusal asks for, so it is done here, explicitly — the"
echo "  '11-15' below is beta's construct in the merged file, the"
echo "  same one the old '7-10' named."
echo "=============================================================="
python3 - <<'PY'
import pathlib

doc = pathlib.Path("docs-dev/install.md")
text = doc.read_text(encoding="utf-8")
repaired = text.replace("`src/a.ts:7-10`", "`src/a.ts:11-15`")
if repaired == text:
    raise SystemExit("the refused citation is not where this repair expects it")
doc.write_text(repaired, encoding="utf-8")
PY
rc=$?
expect 0 "$rc" "the human repair the refusal asks for is applied to the fixture"
python3 scripts/check-citation-drift.py --update >/dev/null 2>&1
rc=$?
expect 0 "$rc" "--update must accept the relocated documents"
moved_2_4="$(awk '$1=="src/a.ts:6-8"{print $2}' docs-dev/citations.lock)"
moved_15_17="$(awk '$1=="src/a.ts:20-22"{print $2}' docs-dev/citations.lock)"
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
cp docs-dev/install.md "$TMP/install.after1"
python3 scripts/relocate-citations.py --old "$OLDA" >"$TMP/run2.out" 2>&1
rc=$?
expect 1 "$rc" "a document that is in no declared side's coordinates is an error"
if diff -q "$TMP/install.after1" docs-dev/install.md >/dev/null; then
  echo "  ✔ the second run left the document byte-identical"
else
  echo "  ✘ the second run rewrote the document:"
  diff "$TMP/install.after1" docs-dev/install.md | sed 's/^/      /'
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
cat > "$FIXB/docs-dev/install.md" <<'MD'
# The relocatable case

The body of alpha is `src/a.ts:2-4`.
MD
cat > "$FIXB/docs-dev/privacy.md" <<'MD'
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
cp docs-dev/privacy.md "$TMP/refuse.before"
python3 scripts/relocate-citations.py --old "$OLDB" >"$TMP/run3.out" 2>&1
rc=$?
expect 1 "$rc" "a citation with more than one possible answer is an error"
if diff -q "$TMP/refuse.before" docs-dev/privacy.md >/dev/null; then
  echo "  ✔ the refused document was left byte-identical"
else
  echo "  ✘ the refused document was edited anyway:"
  diff "$TMP/refuse.before" docs-dev/privacy.md | sed 's/^/      /'
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
contains docs-dev/install.md '`src/a.ts:6-8`' "the certain citation was relocated despite the refusals"

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
cat > "$FIXC/docs-dev/install.md" <<'MD'
# Nothing cited yet
MD
cat > "$FIXC/docs-dev/privacy.md" <<'MD'
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
cat > "$FIXC/docs-dev/install.md" <<'MD'
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
cat > "$FIXC/docs-dev/privacy.md" <<'MD'
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
  git checkout -q "$OLDC_B" -- docs-dev/privacy.md
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
contains docs-dev/install.md '`src/two.ts:6-7`' "side A's document was rewritten"
contains docs-dev/privacy.md '`src/two.ts:6-7`' "side B's document was rewritten"

echo
echo "=============================================================="
echo "Fixture D: five shapes that are not an exact shift, and one"
echo "  that is"
echo "  None of these blocks is still one unchanged run of lines,"
echo "  so none of them has a placement the merged file forces. The"
echo "  old tool walked the file for the cited lines in order and"
echo "  answered anyway: it shrank a two-line claim to one, or"
echo "  stitched a range across two constructs and called it a"
echo "  growth. There is one citation here that really did shift,"
echo "  in the other document, so that a run which refuses"
echo "  everything cannot pass either."
echo "=============================================================="
FIXD="$TMP/d"
seed_repo "$FIXD"
cat > "$FIXD/docs-dev/install.md" <<'MD'
# The shapes that are not an exact shift

The function is `src/a.ts:1-4`.

The block is `src/a.ts:5-7`.

The repeats are `src/a.ts:9-10`.

The blank line is `src/blank.ts:2`.
MD
cat > "$FIXD/docs-dev/privacy.md" <<'MD'
# The block that grew, and the one that merely moved

The function is `src/a.ts:11-14`.

The certain one is `src/mover.ts:1-2`.
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
printf 'm1\nm2\nm3\n' > "$FIXD/src/mover.ts"
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
printf 'NEW1\nNEW2\nm1\nm2\nm3\n' > "$FIXD/src/mover.ts"
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
cp docs-dev/install.md "$TMP/d-install.before"
cp docs-dev/privacy.md "$TMP/d-privacy.before"
python3 scripts/relocate-citations.py --old "$OLDD" >"$TMP/run7.out" 2>&1
rc=$?
expect 1 "$rc" "a block that is not still one run of lines is an error, not a relocation"
contains docs-dev/privacy.md '`src/mover.ts:3-4`' "the exact shift beside them still relocated"
refused "$TMP/run7.out" 'REFUSE.*src/a\.ts:11-14 .*not in the merged file at all' \
  "the block the merge inserted a line into is refused, not grown"
contains docs-dev/privacy.md '`src/a.ts:11-14`' "and its line keeps the text it had"
if [ "$(diff "$TMP/d-privacy.before" docs-dev/privacy.md | grep -c '^[<>]')" = "2" ]; then
  echo "  ✔ and it is the only line of privacy.md that changed"
else
  echo "  ✘ privacy.md changed in more than the one certain citation:"
  diff "$TMP/d-privacy.before" docs-dev/privacy.md | sed 's/^/      /'
  FAILED=1
fi

echo
echo "=============================================================="
echo "Probe 13: two cited lines that are the same, one of them"
echo "  deleted, is not a one-line range"
echo "  src/a.ts:9-10 at --old is \`repeat();\` twice; the merge"
echo "  deletes one, so the block is not one run of lines any"
echo "  more. A walk that advanced past the anchor and matched the"
echo "  anchor itself instead would shrink the claim to"
echo "  \`src/a.ts:14\` — one line, from a block that named two."
echo "=============================================================="
refused "$TMP/run7.out" 'REFUSE.*src/a\.ts:9-10.*not in the merged file at all' \
  "the deleted repeat is refused, not shrunk to one line"
contains docs-dev/install.md '`src/a.ts:9-10`' "it is still the range the document carries"
absent docs-dev/install.md 'src/a.ts:14' "no one-line range was invented for it"

echo
echo "=============================================================="
echo "Probe 14: a deleted line whose text still exists further"
echo "  down the file is not stitched into the range"
echo "  The cited function lost \`helper();\` and its \`}\`. Both"
echo "  still exist further down, after \`other();\`. A walk that"
echo "  takes the earliest later copy of each line builds"
echo "  src/a.ts:1-6 — a window covering three lines that belong"
echo "  to other code — and the block it started from is not one"
echo "  run of lines at all."
echo "=============================================================="
refused "$TMP/run7.out" 'REFUSE.*src/a\.ts:1-4.*not in the merged file at all' \
  "the block whose tail repeats further down is refused"
absent docs-dev/install.md '`src/a.ts:1-6`' "the stitched range was not written"

echo
echo "=============================================================="
echo "Probe 15: a block whose lines also occur earlier is refused"
echo "  src/a.ts:5-7 is three marker comments. All three also"
echo "  occur further down with the real pair at the end, and the"
echo "  merge broke the cited run with \`// UNRELATED\`, so the"
echo "  block is contiguous nowhere — in particular not at the"
echo "  alignment that stops short of the very text that survived."
echo "=============================================================="
refused "$TMP/run7.out" 'REFUSE.*src/a\.ts:5-7.*not in the merged file at all' \
  "the block whose run the merge broke is refused"
absent docs-dev/install.md '`src/a.ts:7-10`' "no alignment was picked for it"

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
contains docs-dev/install.md '`src/blank.ts:2`' "its line in the document is untouched"

echo
echo "=============================================================="
echo "Probe 17: the document with the four refusals is byte-identical"
echo "=============================================================="
same_bytes "$TMP/d-install.before" docs-dev/install.md \
  "docs-dev/install.md is exactly as the run found it"

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
printf '# base\n\nnothing\n' > "$FIXF/docs-dev/install.md"
printf '# base\n\nnothing\n' > "$FIXF/docs-dev/privacy.md"
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
cat > "$FIXF/docs-dev/install.md" <<'MD'
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
cat > "$FIXF/docs-dev/install.md" <<'MD'
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
  git checkout -q "$OLDF_B" -- docs-dev/install.md
)
printf 'd1\nd2\nZZ\nA3\nA4\nd5\nd6\n' > "$FIXF/src/disagree.ts"
printf 'w1\nw2\nYY\nRA3\nRA4\nw5\nw6\n' > "$FIXF/src/reword.ts"
printf 'c1\nc2\nQQ\nc3\nc4\nc5\nc6\n' > "$FIXF/src/cross.ts"
cat > "$FIXF/docs-dev/install.md" <<'MD'
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
cp docs-dev/install.md "$TMP/f-install.before"
python3 scripts/relocate-citations.py --old "$OLDF_A" --old "$OLDF_B" >"$TMP/run8.out" 2>&1
rc=$?
expect 1 "$rc" "sides that disagree about a range are an error"
absent "$TMP/run8.out" 'cannot resolve every citation' \
  "and it got as far as the citations — a fixture the parser cannot read would refuse for the wrong reason"
refused "$TMP/run8.out" 'REFUSE.*src/disagree\.ts:3-4.*do not agree about it.*not in the merged file at all' \
  "the disagreement is reported, naming the side whose text is gone"
absent docs-dev/install.md 'src/disagree.ts:4-5' "the surviving side's range was not written"

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
absent docs-dev/install.md 'src/reword.ts:4-5' "no range was picked for it"

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
absent docs-dev/install.md 'src/cross.ts:4-5' "and the citation was left where it was"
same_bytes "$TMP/f-install.before" docs-dev/install.md \
  "docs-dev/install.md is exactly as the run found it"

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
  > "$FIXG/docs-dev/install.md"
printf '# Fixture G\n\nPrivacy cites nothing.\n' > "$FIXG/docs-dev/privacy.md"
if ! od -c "$FIXG/docs-dev/install.md" | grep -q '\\r'; then
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
mode_before="$(file_mode docs-dev/install.md)"
python3 scripts/relocate-citations.py --old "$OLDG" >"$TMP/run9.out" 2>&1
rc=$?
expect 0 "$rc" "one citation moved, the other is already right"
contains docs-dev/install.md '`pkg/src/a.ts:12` and move `src/a.ts:17`' \
  "the shorter token was rewritten where it starts, not inside the longer one"
absent docs-dev/install.md 'pkg/src/a.ts:17' "the longer token's file was left alone"
if [ "$mode_before" = "$(file_mode docs-dev/install.md)" ]; then
  echo "  ✔ docs-dev/install.md kept its permission bits (${mode_before#0} → the same)"
else
  echo "  ✘ the rewrite changed the document's permissions: $mode_before → $(file_mode docs-dev/install.md)"
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
same_bytes "$TMP/g-expected" docs-dev/install.md \
  "docs-dev/install.md is byte for byte the planned rewrite"

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
cat > "$FIXH/docs-dev/install.md" <<'MD'
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
printf '# Fixture H\n\nPrivacy cites nothing.\n' > "$FIXH/docs-dev/privacy.md"
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
cat > "$FIXI/docs-dev/install.md" <<'MD'
# Fixture I

Alpha is `src/a.ts:2-3`.
MD
cat > "$FIXI/docs-dev/privacy.md" <<'MD'
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
cp "$FIXI_OTHER/docs-dev/install.md" "$TMP/i-other-install.before"

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
contains "$FIXI/docs-dev/install.md" '`src/a.ts:4-5`' "this worktree's document was the one rewritten"
same_bytes "$TMP/i-other-install.before" "$FIXI_OTHER/docs-dev/install.md" \
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
echo "Fixture J: the blocks whose last line the insertion moved"
echo "  The cited block's last line is a closing brace, and the"
echo "  merge inserts a construct *inside* the block, so the cited"
echo "  run is broken. A walk that took the brace closing the"
echo "  insertion wrote a range ending inside the cited construct"
echo "  — an inner \`}\` for the first, and for the two-line block"
echo "  there is not even an interior line for the old checks to"
echo "  look at. Both are refusals now. The shift beside them"
echo "  really does relocate, so a run that refuses everything"
echo "  cannot pass."
echo "=============================================================="
FIXJ="$TMP/j"
seed_repo "$FIXJ"
cat > "$FIXJ/docs-dev/install.md" <<'MD'
# The blocks the insertion broke

The inner brace is `src/inner.ts:1-4`.

The pair is `src/pair.ts:1-2`.
MD
cat > "$FIXJ/docs-dev/privacy.md" <<'MD'
# The block that grew, and the one that merely moved

The function is `src/control.ts:1-4`.

The certain one is `src/mover.ts:1-2`.
MD
printf 'm1\nm2\nm3\n' > "$FIXJ/src/mover.ts"
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
printf 'NEW1\nNEW2\nm1\nm2\nm3\n' > "$FIXJ/src/mover.ts"
if ! grep -q 'guard' "$FIXJ/src/inner.ts" || ! grep -q 'tail();' "$FIXJ/src/inner.ts" \
  || [ "$(wc -l < "$FIXJ/src/inner.ts")" -ne 7 ]; then
  echo "  ✘ fixture J's merged file is not what this selftest means to write; it is void"
  FAILED=1
fi

cd "$FIXJ" || exit 2

echo
echo "=============================================================="
echo "Probe 27: a block whose insertion moved its closing brace is"
echo "  refused"
echo "  src/inner.ts:1-4 is the whole of fn unique_name. The merge"
echo "  put an \`if\` inside it, so the cited \`}\` is no longer the"
echo "  line after \`helper();\`. The \`}\` that closes the \`if\`"
echo "  (line 5) is an earlier candidate the old walk took, and"
echo "  the function's own \`}\` is line 7."
echo "=============================================================="
cp docs-dev/install.md "$TMP/j-install.before"
cp docs-dev/privacy.md "$TMP/j-privacy.before"
python3 scripts/relocate-citations.py --old "$OLDJ" >"$TMP/run13.out" 2>&1
rc=$?
expect 1 "$rc" "a block that is not one unchanged run of lines is an error"
refused "$TMP/run13.out" 'REFUSE.*src/inner\.ts:1-4 .*not in the merged file at all' \
  "the inner-brace block is refused as text that is no longer one run"
absent docs-dev/install.md 'src/inner.ts:1-5' "the range that ends inside the function was not written"

echo
echo "=============================================================="
echo "Probe 28: and so is the two-line block, which has no interior"
echo "  line for any check to look at"
echo "  src/pair.ts:1-2 is fn unique_only_here and its \`}\`. Both"
echo "  lines are cited; an inserted \`if\` breaks the run, and the"
echo "  \`}\` of the \`if\` sits on line 3 with the function's own"
echo "  \`}\` still on line 5."
echo "=============================================================="
refused "$TMP/run13.out" 'REFUSE.*src/pair\.ts:1-2 .*not in the merged file at all' \
  "the two-line block is refused by the same rule"
absent docs-dev/install.md 'src/pair.ts:1-3' "the range that ends on the inserted brace was not written"

echo
echo "=============================================================="
echo "Probe 29: the refusals do not stop the run, and the block that"
echo "  grew is left for a human"
echo "  The control.ts citation is a block the merge inserted a line"
echo "  into, so it is refused too; mover.ts is a plain shift and"
echo "  must still relocate, with the run exiting non-zero for the"
echo "  three refusals beside it."
echo "=============================================================="
contains docs-dev/privacy.md '`src/mover.ts:3-4`' "the exact shift relocated"
contains docs-dev/privacy.md '`src/control.ts:1-4`' "the grown block keeps the text it had"
refused "$TMP/run13.out" 'REFUSE.*src/control\.ts:1-4 .*not in the merged file at all' \
  "and the grown block is refused, not grown"
if [ "$(diff "$TMP/j-privacy.before" docs-dev/privacy.md | grep -c '^[<>]')" = "2" ]; then
  echo "  ✔ and it is the only line of privacy.md that changed"
else
  echo "  ✘ privacy.md changed in more than the one certain citation:"
  diff "$TMP/j-privacy.before" docs-dev/privacy.md | sed 's/^/      /'
  FAILED=1
fi
same_bytes "$TMP/j-install.before" docs-dev/install.md \
  "docs-dev/install.md is exactly as the run found it"

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
cat > "$FIXK/docs-dev/install.md" <<'MD'
# Side A

Alpha is `src/alpha.ts:1-2`, and more of it is `:3-4`.

The basename is `a.ts:1-2`.

Gamma is `src/gamma.ts:1-2`.
MD
cat > "$FIXK/docs-dev/privacy.md" <<'MD'
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
cat > "$FIXK/docs-dev/install.md" <<'MD'
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
cp docs-dev/install.md "$TMP/k-install.before"
python3 scripts/relocate-citations.py --old "$OLDK" >"$TMP/run14.out" 2>&1
rc=$?
expect 1 "$rc" "an unclaimed range is an error, not a relocation"
refused "$TMP/run14.out" 'REFUSE.*src/cross\.ts:3-4 .*no --old document writes this range' \
  "the file the continuation never named is unclaimed"
absent docs-dev/install.md 'src/cross.ts:4-5' "and the citation was left where it was"

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
absent docs-dev/install.md 'pkg/two/a.ts:2-3' "and its citation was left where it was"

echo
echo "=============================================================="
echo "Probe 32: the citation the side did name is still relocated"
echo "  The refusals above must not turn the run into one that"
echo "  refuses everything: side A's document writes"
echo "  src/gamma.ts:1-2 outright, and gamma's lines moved down one."
echo "=============================================================="
contains docs-dev/install.md '`src/gamma.ts:2-3`' "the named citation was relocated"

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
cat > "$FIXL/docs-dev/install.md" <<'MD'
# The comma list

The spans are `src/a.ts:2-4,15-17`.
MD
cat > "$FIXL/docs-dev/privacy.md" <<'MD'
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
cp docs-dev/install.md "$TMP/l-install.before"
python3 scripts/relocate-citations.py --old "$OLDL" >"$TMP/run15.out" 2>&1
rc=$?
expect 1 "$rc" "a token only half of which can be placed is an error"
contains docs-dev/install.md '`src/a.ts:2-4,15-17`' "the token is still the one the document had"
absent docs-dev/install.md 'src/a.ts:6-8,15-17' "no half-rewritten token was written"
same_bytes "$TMP/l-install.before" docs-dev/install.md \
  "docs-dev/install.md is exactly as the run found it"

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
echo "  and the closing brace. The merge inserts \`extra();\` before"
echo "  the brace, so the cited run is broken. A walk that matched"
echo "  the block's lines in order placed all four and reported a"
echo "  grown range to the whole construct; the lines it matched"
echo "  are in order, but the block is not one run of lines, which"
echo "  is the only shape that forces a range."
echo "=============================================================="
FIXM="$TMP/m"
seed_repo "$FIXM"
cat > "$FIXM/docs-dev/install.md" <<'MD'
# The repeated line inside the block

The function is `src/a.ts:1-4`.
MD
cat > "$FIXM/docs-dev/privacy.md" <<'MD'
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
echo "Probe 35: the block that grew is refused, whatever the walk"
echo "  could have matched"
echo "=============================================================="
cp docs-dev/install.md "$TMP/m-install.before"
python3 scripts/relocate-citations.py --old "$OLDM" >"$TMP/run16.out" 2>&1
rc=$?
expect 1 "$rc" "a block with a line inserted inside it is a refusal, not a relocation"
refused "$TMP/run16.out" 'REFUSE.*src/a\.ts:1-4 .*not in the merged file at all' \
  "the grown block is refused as text that is no longer one run of lines"
absent docs-dev/install.md 'src/a.ts:1-5' "the whole-construct range was not written"
same_bytes "$TMP/m-install.before" docs-dev/install.md \
  "docs-dev/install.md is exactly as the run found it"

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
cat > "$FIXN/docs-dev/install.md" <<'MD'
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
printf '# Fixture N\n\nPrivacy cites nothing.\n' > "$FIXN/docs-dev/privacy.md"
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
echo "Fixture O: the three inputs three review rounds built to make"
echo "  the grown heuristic write a wrong range and exit 0"
echo "  Each is a block the merge broke, in the one shape where the"
echo "  old walk still found an alignment that looked forced:"
echo "    * a \`);\` that closes an inner call rather than the cited"
echo "      one — no braces at all, so the brace check had nothing"
echo "      to say, for both a three-line and a two-line citation;"
echo "    * a \`}\` written inside a \`// }\` comment, which cancels"
echo "      the function's own opening brace so the walk stopped on"
echo "      the \`if\`'s brace instead;"
echo "    * a bare name whose file the merge deleted while another"
echo "      file with that name remained, so the same token named"
echo "      one file at --old and a different one in the merged"
echo "      tree."
echo "  The shift in the other document is the control: refusing"
echo "  everything is not a way to pass this fixture."
echo "=============================================================="
FIXO="$TMP/o"
seed_repo "$FIXO"
mkdir -p "$FIXO/pkg"
cat > "$FIXO/src/call.ts" <<'TS'
uniqueCall(
  arg
);
TS
printf 'uniqueCall(\n);\n' > "$FIXO/src/twoline.ts"
cat > "$FIXO/src/fn.ts" <<'TS'
fn unique_name() {
  step();
  helper();
}
TS
printf 'R1\nR2\n' > "$FIXO/a.ts"
printf 'T1\nT2\n' > "$FIXO/pkg/a.ts"
printf 's1\ns2\ns3\n' > "$FIXO/src/shift.ts"
cat > "$FIXO/docs-dev/install.md" <<'MD'
# The inputs the grown heuristic answered wrongly

The call is `src/call.ts:1-3`.

The two-line call is `src/twoline.ts:1-2`.

The function is `src/fn.ts:1-4`.

The basename is `a.ts:1-2`.
MD
cat > "$FIXO/docs-dev/privacy.md" <<'MD'
# The one that only moved

The certain one is `src/shift.ts:1-2`.
MD
(
  cd "$FIXO" || exit 2
  git add -A
  git commit -qm "fixture O: the state the document's line numbers describe"
)
OLDO="$(cd "$FIXO" && git rev-parse HEAD)"
echo "  fixture O at ${OLDO:0:7}"
cat > "$FIXO/src/call.ts" <<'TS'
uniqueCall(
  arg
  inner(
  );
  more
);
TS
printf 'uniqueCall(\n  arg\n  inner(\n  );\n);\n' > "$FIXO/src/twoline.ts"
cat > "$FIXO/src/fn.ts" <<'TS'
fn unique_name() {
  step();
  // }
  if (guard) {
    helper();
  }
  tail();
}
TS
# The merge deleted the root a.ts and left pkg/a.ts, which then gained a line —
# so the token `a.ts:1-2` now names a file the sentence never named.
(cd "$FIXO" && git rm -q a.ts)
printf 'YY\nT1\nT2\n' > "$FIXO/pkg/a.ts"
printf 'NEW1\nNEW2\ns1\ns2\ns3\n' > "$FIXO/src/shift.ts"
if [ -e "$FIXO/a.ts" ] || ! grep -q 'inner(' "$FIXO/src/call.ts"; then
  echo "  ✘ fixture O's merged tree is not what this selftest means to write; it is void"
  FAILED=1
fi

cd "$FIXO" || exit 2

echo
echo "=============================================================="
echo "Probe 37: neither the inner \`);\` nor the commented \`}\` is"
echo "  taken as the end of a block the merge broke"
echo "  All three blocks are refused, and none of the ranges the"
echo "  old walk wrote is in the document."
echo "=============================================================="
cp docs-dev/install.md "$TMP/o-install.before"
python3 scripts/relocate-citations.py --old "$OLDO" >"$TMP/run18.out" 2>&1
rc=$?
expect 1 "$rc" "blocks the merge broke are errors, not relocations"
refused "$TMP/run18.out" 'REFUSE.*src/call\.ts:1-3 .*not in the merged file at all' \
  "the outer call is refused rather than ended on the inner \`);\`"
refused "$TMP/run18.out" 'REFUSE.*src/twoline\.ts:1-2 .*not in the merged file at all' \
  "the two-line call is refused by the same rule"
refused "$TMP/run18.out" 'REFUSE.*src/fn\.ts:1-4 .*not in the merged file at all' \
  "the function is refused rather than ended on the \`// }\` brace"
absent docs-dev/install.md 'src/call.ts:1-4' "no range ending on the inner \`);\` was written"
absent docs-dev/install.md 'src/call.ts:1-5' "and no longer growth of it either"
absent docs-dev/install.md 'src/twoline.ts:1-3' "no range ending on the inserted \`);\` was written"
absent docs-dev/install.md 'src/fn.ts:1-6' "no range ending on the \`if\`'s brace was written"

echo
echo "=============================================================="
echo "Probe 38: the token that names a different file on each side"
echo "  is refused by name"
echo "  \`a.ts:1-2\` named the root a.ts at --old. The merge deleted"
echo "  that file and left pkg/a.ts, which is one line further down,"
echo "  so the merged tree resolves the same token to pkg/a.ts —"
echo "  and pkg/a.ts's old first two lines are still there, one"
echo "  line down, waiting for a run that answers with numbers"
echo "  alone. Every number in the citation means something"
echo "  different on the two sides, so no range is forced."
echo "=============================================================="
refused "$TMP/run18.out" 'REFUSE.*pkg/a\.ts:1-2 .*resolves to a different file' \
  "the token that re-resolves to another file is refused, and the other file is named"
absent docs-dev/install.md 'a.ts:2-3' "no range was written against pkg/a.ts's old lines"
contains docs-dev/install.md '`a.ts:1-2`' "and the citation keeps the text it had"
same_bytes "$TMP/o-install.before" docs-dev/install.md \
  "docs-dev/install.md is exactly as the run found it"

echo
echo "=============================================================="
echo "Probe 39: the shift beside the three refusals still relocated"
echo "  The refusals must not turn the run into one that refuses"
echo "  everything: src/shift.ts:1-2 is one unchanged run of lines"
echo "  two lines further down, and it must move."
echo "=============================================================="
contains docs-dev/privacy.md '`src/shift.ts:3-4`' "the exact shift relocated"

echo
echo "=============================================================="
echo "Fixture P: a read-back restore that cannot be written"
echo "  README.md and contracts/api.md both relocate, so both are"
echo "  written. The read-back parse is then made to disagree,"
echo "  which puts the run on its restore path, and the restore of"
echo "  every document is made to raise OSError. An OSError"
echo "  escaping there used to be a traceback out of main(): one"
echo "  document put back, another still carrying the rewrite, and"
echo "  nothing on stdout saying which. The summary has to name the"
echo "  documents it could not restore and say the tree is not as"
echo "  the run found it."
echo "=============================================================="
FIXP="$TMP/p"
seed_repo "$FIXP"
mkdir -p "$FIXP/contracts"
cat > "$FIXP/docs-dev/install.md" <<'MD'
# Fixture P

Install cites nothing.
MD
cat > "$FIXP/README.md" <<'MD'
# Fixture P

Readme cites `src/a.ts:2-3`.
MD
cat > "$FIXP/contracts/api.md" <<'MD'
# Contract

Contract cites `src/a.ts:2-3`.
MD
printf '# Fixture P\n\nPrivacy cites nothing.\n' > "$FIXP/docs-dev/privacy.md"
printf 'AA\nBB\nCC\nDD\n' > "$FIXP/src/a.ts"
(
  cd "$FIXP" || exit 2
  git add -A
  git commit -qm "fixture P: two documents that relocate"
)
OLDP="$(cd "$FIXP" && git rev-parse HEAD)"
echo "  fixture P at ${OLDP:0:7}"
printf 'PP\nQQ\nAA\nBB\nCC\nDD\n' > "$FIXP/src/a.ts"

echo
echo "=============================================================="
echo "Probe 40: a restore that raises is reported, not traced back"
echo "=============================================================="
python3 - "$FIXP" "$OLDP" >"$TMP/run19.out" 2>&1 <<'PY'
import importlib.util
import os
import sys

fixture, old = sys.argv[1], sys.argv[2]
os.chdir(fixture)
spec = importlib.util.spec_from_file_location(
    "relocate_citations_readback",
    os.path.join(fixture, "scripts", "relocate-citations.py"),
)
mod = importlib.util.module_from_spec(spec)
sys.modules["relocate_citations_readback"] = mod
spec.loader.exec_module(mod)

state = {"restoring": False}
real_write_atomic = mod.write_atomic


def write_atomic_that_fails_the_restore(doc, text):
    if state["restoring"]:
        # From here on every write is the run putting a document back.
        raise OSError(13, "injected: the read-back restore cannot be written")
    real_write_atomic(doc, text)


mod.write_atomic = write_atomic_that_fails_the_restore

# The drift module is loaded inside main(), not at import, so the read-back
# parse is wrapped where it is created rather than on a module attribute that
# does not exist yet.
real_load_drift_module = mod.load_drift_module
calls = {"n": 0}


def load_drift_module_whose_readback_disagrees():
    drift_module = real_load_drift_module()
    real_parse_docs = drift_module.parse_docs

    def parse_docs_that_disagrees(basenames):
        calls["n"] += 1
        if calls["n"] > 1:
            # The second parse is the read-back. Making it disagree is what
            # puts the run on the restore path at all; the writes above were
            # the real ones.
            state["restoring"] = True
            return [], ["injected: the read-back does not agree with the plan"]
        return real_parse_docs(basenames)

    drift_module.parse_docs = parse_docs_that_disagrees
    return drift_module


mod.load_drift_module = load_drift_module_whose_readback_disagrees
sys.argv = ["relocate-citations.py", "--old", old]
sys.exit(mod.main())
PY
rc=$?
expect 1 "$rc" "a read-back restore that fails must not exit 0"
if grep -q -F 'COULD NOT RESTORE: README.md' "$TMP/run19.out"; then
  echo "  ✔ the summary names the document it could not put back"
else
  echo "  ✘ the summary does not name the unrestored document:"
  sed 's/^/      /' "$TMP/run19.out"
  FAILED=1
fi
contains "$TMP/run19.out" 'the tree is NOT as the run found it' \
  "and it says the tree is not as the run found it"
absent "$TMP/run19.out" 'nothing was changed' \
  "it does not claim the tree is as the run found it"
absent "$TMP/run19.out" 'Traceback' \
  "and no OSError escapes as a traceback"

echo
echo "=============================================================="
echo "Fixture Q: the four constructions R66d built where the"
echo "  citation is no longer on the document line it sat on at"
echo "  --old, so a lookup keyed by (document, line) sees nothing,"
echo "  while the token now names a different file than it did"
echo "  The token is a bare name in all four, and which file a bare"
echo "  name means is a fact about a tree, not about the token: the"
echo "  merge changed the tree and left the token. Each one below"
echo "  rewrote — or called \"right\" — a range against a file the"
echo "  sentence never named, and exited 0."
echo "    Q1 one parent, and a second citation of the newly resolved"
echo "       file supplies the ownership the old run used:"
echo "       \`a.ts:1-2\` became \`a.ts:2-3\`, which is pkg/a.ts's old"
echo "       first two lines."
echo "    Q2 two parents, and it is side B's document that supplies"
echo "       it, so the run reports a relocation via B."
echo "    Q3 no second citation and the file never moved: the run"
echo "       reported the citation \`right\` for pkg/a.ts:1-2 — a file"
echo "       the token does not name — and exited 0."
echo "    Q4 the bare name resolved to nothing at --old (two files"
echo "       shared it), so no citation of it was in that side's"
echo "       coordinates at all, and it was still moved."
echo "  Every one must be refused by name, the range the old run"
echo "  invented must not be in the document, and each fixture"
echo "  carries one certain shift that must still be relocated:"
echo "  refusing everything is not a way to pass this fixture."
echo "=============================================================="
FIXQ1="$TMP/q1"
seed_repo "$FIXQ1"
cat > "$FIXQ1/docs-dev/install.md" <<'MD'
# Install

This document cites nothing.
MD
cat > "$FIXQ1/docs-dev/privacy.md" <<'MD'
# Privacy

This document cites nothing.
MD
mkdir -p "$FIXQ1/pkg"
printf 'R1\nR2\n' > "$FIXQ1/a.ts"
printf 'T1\nT2\n' > "$FIXQ1/pkg/a.ts"
printf 'm1\nm2\nm3\n' > "$FIXQ1/src/moved.ts"
cat > "$FIXQ1/README.md" <<'MD'
# Q1

See root `a.ts:1-2`.

See package `pkg/a.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ1" || exit 2
  git add -A
  git commit -qm "fixture Q1: the tree and the numbers the document describes"
)
OLDQ1="$(cd "$FIXQ1" && git rev-parse HEAD)"
# The merge: a NOTE line above the sentence, so it is not on the line it sat on
# at --old; the root a.ts gone; pkg/a.ts carrying the cited block one line down;
# and the control file moved down as well.
cat > "$FIXQ1/README.md" <<'MD'
# Q1

NOTE

See root `a.ts:1-2`.

See package `pkg/a.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(cd "$FIXQ1" && git rm -q a.ts)
printf 'INSERTED\nT1\nT2\n' > "$FIXQ1/pkg/a.ts"
printf 'X\nm1\nm2\nm3\n' > "$FIXQ1/src/moved.ts"
if [ -e "$FIXQ1/a.ts" ] || ! grep -q 'INSERTED' "$FIXQ1/pkg/a.ts"; then
  echo "  ✘ fixture Q1's merged tree is not what this selftest means to write; it is void"
  FAILED=1
fi

FIXQ2="$TMP/q2"
seed_repo "$FIXQ2"
cat > "$FIXQ2/docs-dev/install.md" <<'MD'
# Install

This document cites nothing.
MD
cat > "$FIXQ2/docs-dev/privacy.md" <<'MD'
# Privacy

This document cites nothing.
MD
mkdir -p "$FIXQ2/pkg"
printf 'R1\nR2\n' > "$FIXQ2/a.ts"
printf 'T1\nT2\n' > "$FIXQ2/pkg/a.ts"
printf 'm1\nm2\nm3\n' > "$FIXQ2/src/moved.ts"
cat > "$FIXQ2/README.md" <<'MD'
# Q2 base

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ2" || exit 2
  git add -A
  git commit -qm "fixture Q2: the common ancestor"
  git checkout -q -b side-a
)
OLDQ2_BASE="$(cd "$FIXQ2" && git rev-parse HEAD)"
cat > "$FIXQ2/README.md" <<'MD'
# Q2 side A

See root `a.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ2" || exit 2
  git add -A
  git commit -qm "fixture Q2 side A: the sentence about the root file"
)
OLDQ2_A="$(cd "$FIXQ2" && git rev-parse HEAD)"
(
  cd "$FIXQ2" || exit 2
  git checkout -q "$OLDQ2_BASE"
  git checkout -q -b side-b
)
cat > "$FIXQ2/README.md" <<'MD'
# Q2 side B

See package `pkg/a.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ2" || exit 2
  git add -A
  git commit -qm "fixture Q2 side B: the sentence about the package file"
)
OLDQ2_B="$(cd "$FIXQ2" && git rev-parse HEAD)"
# The merge: side A's sentence, under a NOTE line that is in neither side's
# document, with the root a.ts deleted.
(
  cd "$FIXQ2" || exit 2
  git checkout -q side-a
)
cat > "$FIXQ2/README.md" <<'MD'
# Q2 merged

NOTE

See root `a.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(cd "$FIXQ2" && git rm -q a.ts)
printf 'INSERTED\nT1\nT2\n' > "$FIXQ2/pkg/a.ts"
printf 'X\nm1\nm2\nm3\n' > "$FIXQ2/src/moved.ts"
echo "  Q2 side A ${OLDQ2_A:0:7}, side B ${OLDQ2_B:0:7}"

FIXQ3="$TMP/q3"
seed_repo "$FIXQ3"
cat > "$FIXQ3/docs-dev/install.md" <<'MD'
# Install

This document cites nothing.
MD
cat > "$FIXQ3/docs-dev/privacy.md" <<'MD'
# Privacy

This document cites nothing.
MD
mkdir -p "$FIXQ3/pkg"
printf 'R1\nR2\n' > "$FIXQ3/a.ts"
printf 'T1\nT2\n' > "$FIXQ3/pkg/a.ts"
printf 'm1\nm2\nm3\n' > "$FIXQ3/src/moved.ts"
cat > "$FIXQ3/README.md" <<'MD'
# Q3

See root `a.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ3" || exit 2
  git add -A
  git commit -qm "fixture Q3: the tree and the numbers the document describes"
)
OLDQ3="$(cd "$FIXQ3" && git rev-parse HEAD)"
# The merge: the same NOTE line, the root a.ts gone — and pkg/a.ts left exactly
# where it was, so there is nothing to relocate and nothing to refuse either,
# unless the token is asked which file it names.
cat > "$FIXQ3/README.md" <<'MD'
# Q3

NOTE

See root `a.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(cd "$FIXQ3" && git rm -q a.ts)
printf 'X\nm1\nm2\nm3\n' > "$FIXQ3/src/moved.ts"

FIXQ4="$TMP/q4"
seed_repo "$FIXQ4"
cat > "$FIXQ4/docs-dev/install.md" <<'MD'
# Install

This document cites nothing.
MD
cat > "$FIXQ4/docs-dev/privacy.md" <<'MD'
# Privacy

This document cites nothing.
MD
mkdir -p "$FIXQ4/a" "$FIXQ4/b"
printf 'A1\nA2\n' > "$FIXQ4/a/foo.ts"
printf 'B1\nB2\n' > "$FIXQ4/b/foo.ts"
printf 'm1\nm2\nm3\n' > "$FIXQ4/src/moved.ts"
cat > "$FIXQ4/README.md" <<'MD'
# Q4

The bare name is `foo.ts:1-2`.

The path is `a/foo.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ4" || exit 2
  git add -A
  git commit -qm "fixture Q4: two files share the name, so the bare one names neither"
)
OLDQ4="$(cd "$FIXQ4" && git rev-parse HEAD)"
# The merge: b/foo.ts deleted, so the bare name resolves to a/foo.ts in the
# merged tree and to nothing at all at --old; a/foo.ts carries its cited block
# one line down.
(cd "$FIXQ4" && git rm -q b/foo.ts)
printf 'INSERTED\nA1\nA2\n' > "$FIXQ4/a/foo.ts"
printf 'X\nm1\nm2\nm3\n' > "$FIXQ4/src/moved.ts"

FIXQ5="$TMP/q5"
seed_repo "$FIXQ5"
cat > "$FIXQ5/docs-dev/install.md" <<'MD'
# Install

This document cites nothing.
MD
cat > "$FIXQ5/docs-dev/privacy.md" <<'MD'
# Privacy

This document cites nothing.
MD
printf 'k1\nk2\nk3\nk4\nk5\nk6\n' > "$FIXQ5/src/alpha.ts"
printf 'c1\nc2\nc3\nc4\nc5\nc6\n' > "$FIXQ5/src/cross.ts"
printf 'm1\nm2\nm3\n' > "$FIXQ5/src/moved.ts"
cat > "$FIXQ5/README.md" <<'MD'
# Q5

Alpha is `src/alpha.ts:1-2`, and more of it is `:3-4`.

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ5" || exit 2
  git add -A
  git commit -qm "fixture Q5: a continuation whose sentence names alpha"
)
OLDQ5="$(cd "$FIXQ5" && git rev-parse HEAD)"
# The merge: the sentence's own token now names cross.ts and the continuation
# still inherits whatever that sentence names. A continuation writes no name of
# its own, so the token-shape rule has nothing to say about it; its file is the
# one its two sides' parses have to agree on. As in fixture Q, the sentence is
# not left on the line it sat on at --old.
cat > "$FIXQ5/README.md" <<'MD'
# Q5

NOTE

Alpha is `src/cross.ts:1-2`, and more of it is `:3-4`.

Moved is `src/moved.ts:1-2`.
MD
printf 'ZZ\nk1\nk2\nk3\nk4\nk5\nk6\n' > "$FIXQ5/src/alpha.ts"
printf 'QQ\nc1\nc2\nc3\nc4\nc5\nc6\n' > "$FIXQ5/src/cross.ts"
printf 'X\nm1\nm2\nm3\n' > "$FIXQ5/src/moved.ts"

FIXQ6="$TMP/q6"
seed_repo "$FIXQ6"
cat > "$FIXQ6/docs-dev/install.md" <<'MD'
# Install

This document cites nothing.
MD
cat > "$FIXQ6/docs-dev/privacy.md" <<'MD'
# Privacy

This document cites nothing.
MD
printf 'm1\nm2\nm3\n' > "$FIXQ6/src/moved.ts"
cat > "$FIXQ6/README.md" <<'MD'
# Q6

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ6" || exit 2
  git add -A
  git commit -qm "fixture Q6: the document before the sentence is written"
)
OLDQ6="$(cd "$FIXQ6" && git rev-parse HEAD)"
# The state a side is left in when a file it cites is removed on that side and
# the sentence citing it stays: its own document writes a path its own tree does
# not have. The merge brings the file back, so the merged tree can resolve the
# token — and the numbers in that sentence were never read against any file.
cat > "$FIXQ6/README.md" <<'MD'
# Q6

Added later is `src/new.ts:1-2`.

Moved is `src/moved.ts:1-2`.
MD
(
  cd "$FIXQ6" || exit 2
  git add -A
  git commit -qm "fixture Q6: this side's document cites a path this side's tree lacks"
)
OLDQ6_BAD="$(cd "$FIXQ6" && git rev-parse HEAD)"
printf 'n1\nn2\n' > "$FIXQ6/src/new.ts"
printf 'X\nm1\nm2\nm3\n' > "$FIXQ6/src/moved.ts"
echo "  Q6 parent ${OLDQ6_BAD:0:7}, the sound one before it ${OLDQ6:0:7}"

echo
echo "=============================================================="
echo "Probe 41: the one-parent move is refused, and the file the"
echo "  token does name is not the file the range is read against"
echo "  \`a.ts:1-2\` named the root a.ts at --old and names pkg/a.ts"
echo "  in the merged tree. A second citation of pkg/a.ts makes the"
echo "  old run an owner of it and it wrote \`a.ts:2-3\` — pkg/a.ts's"
echo "  old first two lines, which are not what the sentence named."
echo "=============================================================="
cd "$FIXQ1" || exit 2
cp README.md "$TMP/q1-readme.before"
python3 scripts/relocate-citations.py --old "$OLDQ1" >"$TMP/run20.out" 2>&1
rc=$?
expect 1 "$rc" "a token that names a different file on each side is an error"
refused "$TMP/run20.out" 'REFUSE.*pkg/a\.ts:1-2 .*bare name' \
  "the citation whose token re-resolved is refused, and named by the file it resolved to"
absent README.md '`a.ts:2-3`' "the range the old run invented is not in the document"
contains README.md '`a.ts:1-2`' "and the citation keeps the text it had"
contains README.md '`pkg/a.ts:2-3`' "the path token beside it still relocated"

echo
echo "=============================================================="
echo "Probe 42: the two-parent move is refused, and no side's"
echo "  numbers are used to write it"
echo "  Side A wrote the sentence about the root file; side B's"
echo "  document is the second citation of pkg/a.ts, and the old run"
echo "  relocated A's sentence in B's numbers and reported [via B]."
echo "=============================================================="
cd "$FIXQ2" || exit 2
python3 scripts/relocate-citations.py --old "$OLDQ2_A" --old "$OLDQ2_B" >"$TMP/run21.out" 2>&1
rc=$?
expect 1 "$rc" "a bare name that changed files is an error on both sides' evidence"
refused "$TMP/run21.out" 'REFUSE.*pkg/a\.ts:1-2 .*resolves to a different file on each side.*--old' \
  "the refusal names the side whose parse put the token on another file"
absent README.md '`a.ts:2-3`' "the range the old run invented is not in the document"
contains README.md '`src/moved.ts:2-3`' "the control shift beside it still relocated"
absent "$TMP/run21.out" 'shifted  README.md:  pkg/a.ts:1-2' \
  "and nothing was relocated onto the file the token re-resolved to"

echo
echo "=============================================================="
echo "Probe 43: the citation reported \`right\` against a file the"
echo "  token does not name is refused, not accepted"
echo "  pkg/a.ts never moved, so the numbers happen to be correct —"
echo "  for pkg/a.ts. The sentence says \`a.ts\`, and there is no"
echo "  a.ts in the merged tree: a run that reports this as right is"
echo "  reporting a file the citation does not name as correct."
echo "=============================================================="
cd "$FIXQ3" || exit 2
python3 scripts/relocate-citations.py --old "$OLDQ3" >"$TMP/run22.out" 2>&1
rc=$?
expect 1 "$rc" "an already-right range is still not a citation of the file it names"
refused "$TMP/run22.out" 'REFUSE.*pkg/a\.ts:1-2 .*bare name' \
  "the citation is refused by name rather than reported"
absent "$TMP/run22.out" 'right    README.md:  pkg/a.ts:1-2' \
  "and it is not listed among the citations that are already right"
contains README.md '`a.ts:1-2`' "the citation keeps the text it had"
contains README.md '`src/moved.ts:2-3`' "the control shift beside it still relocated"

echo
echo "=============================================================="
echo "Probe 44: the bare name that resolved to nothing at --old is"
echo "  refused too, and the path token in the same sentence is not"
echo "  \`foo.ts\` was shared by a/foo.ts and b/foo.ts at --old, so"
echo "  it resolved to neither and no side ever claimed the citation"
echo "  of a/foo.ts it now reads as. The old run moved it anyway —"
echo "  a/foo.ts's old first two lines were one line down, in the"
echo "  coordinates of the path token beside it."
echo "=============================================================="
cd "$FIXQ4" || exit 2
python3 scripts/relocate-citations.py --old "$OLDQ4" >"$TMP/run23.out" 2>&1
rc=$?
expect 1 "$rc" "a token that resolved to no file at --old is an error"
refused "$TMP/run23.out" 'REFUSE.*a/foo\.ts:1-2 .*bare name' \
  "the citation whose token never resolved at --old is refused"
absent README.md '`foo.ts:2-3`' "the range the old run invented is not in the document"
contains README.md '`a/foo.ts:2-3`' "the path token beside it still relocated"

echo
echo "=============================================================="
echo "Probe 45: a continuation is refused when the two sides'"
echo "  sentences name different files"
echo "  A continuation writes no name, so the token-shape rule has"
echo "  nothing to say about it — and it must not be relocated on"
echo "  numbers that were read against the file its sentence named"
echo "  at --old. Both sides' parses have to agree on the file."
echo "  The old run refused this one too, but only because the"
echo "  sentence happened to be on the line it sat on at --old;"
echo "  with a NOTE above it the refusal came back as \"no --old"
echo "  document writes this range\", which sends the reader to"
echo "  the wrong repair."
echo "=============================================================="
cd "$FIXQ5" || exit 2
python3 scripts/relocate-citations.py --old "$OLDQ5" >"$TMP/run24.out" 2>&1
rc=$?
expect 1 "$rc" "a continuation whose file changed is an error"
refused "$TMP/run24.out" 'REFUSE.*src/cross\.ts:3-4 .*resolves to a different file on each side' \
  "the continuation is refused by name, not left unclaimed"
absent README.md '`:2-3`' "no range was read against the file the sentence named at --old"
contains README.md '`src/moved.ts:2-3`' "the control shift beside it still relocated"

echo
echo "=============================================================="
echo "Probe 46: a path the parent's own document writes and its own"
echo "  tree does not have is refused by name"
echo "  The refusal says which side and which path, rather than"
echo "  leaving the reader with \"these numbers are in nobody's"
echo "  coordinate system\" — the repairs are not the same one."
echo "=============================================================="
cd "$FIXQ6" || exit 2
python3 scripts/relocate-citations.py --old "$OLDQ6_BAD" >"$TMP/run25.out" 2>&1
rc=$?
expect 1 "$rc" "a token that does not resolve at --old is an error"
refused "$TMP/run25.out" 'REFUSE.*src/new\.ts:1-2 .*does not resolve at --old' \
  "the refusal names the side whose tree has no such path"
absent "$TMP/run25.out" 'no --old document writes this range' \
  "it is not reported as a range in nobody's coordinate system"
contains README.md '`src/moved.ts:2-3`' "the control shift beside it still relocated"

echo
echo "=============================================================="
echo "Fixture R: a cited file that is a symlink"
echo "  A symlink is one path name for two different byte"
echo "  sequences: git show prints the link's target text, while the"
echo "  working-tree read follows the link to the target's contents."
echo "  When the link text happens to appear in the target, the search"
echo "  finds it and \"relocates\" the citation onto that line, exit 0"
echo "  (R66e). The first case below has the link on both sides; the"
echo "  second has it only at --old, replaced by a regular file the"
echo "  merge left the old link text in. A certain shift sits beside"
echo "  them, so refusing everything is not a way to pass."
echo "=============================================================="
FIXR="$TMP/r"
seed_repo "$FIXR"
mkdir -p "$FIXR/pkg"
printf 'KEEP_A\nK2\nK3\n' > "$FIXR/pkg/real.ts"
ln -s ../pkg/real.ts "$FIXR/src/link.ts"
printf 'OLDLINK\n' > "$FIXR/pkg/target.ts"
ln -s ../pkg/target.ts "$FIXR/src/old.ts"
printf 'm1\nm2\nm3\n' > "$FIXR/src/mover.ts"
cat > "$FIXR/docs-dev/install.md" <<'MD'
# The symlink cases

The working-tree link is `src/link.ts:1`.

The --old link is `src/old.ts:1`.

The certain one is `src/mover.ts:1-2`.
MD
cat > "$FIXR/docs-dev/privacy.md" <<'MD'
# Fixture R

Privacy cites nothing.
MD
(
  cd "$FIXR" || exit 2
  git add -A
  git commit -qm "fixture R: a symlink in the working tree and one only at --old"
)
OLDR="$(cd "$FIXR" && git rev-parse HEAD)"
echo "  fixture R at ${OLDR:0:7}"
# The merge: each target gains the link text, so an unchecked run finds the
# link text in the target and shifts the citation onto it; mover.ts shifts for
# real.
printf 'KEEP_A\n../pkg/real.ts\nK2\nK3\n' > "$FIXR/pkg/real.ts"
rm -f "$FIXR/src/old.ts"
printf 'OLDLINK\n../pkg/target.ts\n' > "$FIXR/src/old.ts"
printf 'NEW1\nNEW2\nm1\nm2\nm3\n' > "$FIXR/src/mover.ts"
if [ ! -L "$FIXR/src/link.ts" ] || [ -L "$FIXR/src/old.ts" ] \
  || ! grep -q 'real.ts' "$FIXR/pkg/real.ts"; then
  echo "  ✘ fixture R's merged tree is not what this selftest means to write; it is void"
  FAILED=1
fi

cd "$FIXR" || exit 2

echo
echo "=============================================================="
echo "Probe 47: a symlink in the working tree is refused, and the"
echo "  link's target text is not taken as the cited line"
echo "=============================================================="
cp docs-dev/install.md "$TMP/r-install.before"
python3 scripts/relocate-citations.py --old "$OLDR" >"$TMP/run26.out" 2>&1
rc=$?
expect 1 "$rc" "a cited symlink is an error, not a relocation"
refused "$TMP/run26.out" 'REFUSE.*src/link\.ts:1-1 .*symlink in the working tree' \
  "the working-tree symlink is refused and named"
absent docs-dev/install.md 'src/link.ts:2' "the link's target text was not written for it"
contains docs-dev/install.md '`src/link.ts:1`' "the citation keeps the text it had"

echo
echo "=============================================================="
echo "Probe 48: a symlink only at --old is refused by the side it"
echo "  is a link on, not by the merged file it is now"
echo "=============================================================="
refused "$TMP/run26.out" 'REFUSE.*src/old\.ts:1-1 .*symlink at --old' \
  "the --old symlink is refused and named"
absent docs-dev/install.md 'src/old.ts:2' "the old link text was not written for it"
contains docs-dev/install.md '`src/old.ts:1`' "the citation keeps the text it had"
contains docs-dev/install.md '`src/mover.ts:3-4`' "the control shift beside the two links still relocated"

echo
echo "=============================================================="
echo "Fixture S: a colon that belongs to another side's token"
echo "  README cites \`src/a.ts:1\` and \`Makefile:10\`. The merge"
echo "  deletes Makefile, so in the merged tree the token is no"
echo "  longer path-shaped, the parser drops it, and \`:10\` inherits"
echo "  src/a.ts from the sentence. src/a.ts did not move, so the"
echo "  citation used to be reported \`right\` as src/a.ts:10-10 and"
echo "  the run exited 0 while the sentence still says Makefile."
echo "  A certain shift sits beside it, so refusing everything is"
echo "  not a way to pass."
echo "=============================================================="
FIXS="$TMP/s"
seed_repo "$FIXS"
printf 'K1\nK2\nK3\nK4\nK5\nK6\nK7\nK8\nK9\ncode10\nK11\n' > "$FIXS/src/a.ts"
printf 'm1\nm2\nm3\n' > "$FIXS/src/mover.ts"
cat > "$FIXS/Makefile" <<'MK'
m1
m2
m3
m4
m5
m6
m7
m8
m9
m10
MK
cat > "$FIXS/docs-dev/install.md" <<'MD'
# The odd continuation

See `src/a.ts:1` and `Makefile:10`.

The certain one is `src/mover.ts:1-2`.
MD
cat > "$FIXS/docs-dev/privacy.md" <<'MD'
# Fixture S

Privacy cites nothing.
MD
(
  cd "$FIXS" || exit 2
  git add -A
  git commit -qm "fixture S: a citation of an extensionless file"
)
OLDS="$(cd "$FIXS" && git rev-parse HEAD)"
echo "  fixture S at ${OLDS:0:7}"
(cd "$FIXS" && git rm -q Makefile)
printf 'NEW1\nNEW2\nm1\nm2\nm3\n' > "$FIXS/src/mover.ts"

cd "$FIXS" || exit 2

echo
echo "=============================================================="
echo "Probe 49: the deleted file's citation is refused, not reported"
echo "  right against the file the merged parse inherited"
echo "=============================================================="
cp docs-dev/install.md "$TMP/s-install.before"
python3 scripts/relocate-citations.py --old "$OLDS" >"$TMP/run27.out" 2>&1
rc=$?
expect 1 "$rc" "a colon that is part of another side's token is not a continuation"
refused "$TMP/run27.out" 'REFUSE.*src/a\.ts:10-10 .*is written `Makefile:10`' \
  "the refusal names the other side's token at that range"
absent "$TMP/run27.out" 'right    docs-dev/install.md:  src/a.ts:10-10' \
  "and it is not listed among the citations that are already right"
contains docs-dev/install.md 'and `Makefile:10`' "the citation keeps the text it had"
contains docs-dev/install.md '`src/mover.ts:3-4`' "the control shift beside it still relocated"

echo
echo "=============================================================="
echo "After: each fixture's own git status"
echo "=============================================================="
for d in "$FIXA" "$FIXB" "$FIXC" "$FIXD" "$FIXF" "$FIXG" "$FIXH" \
         "$FIXI" "$FIXI_OTHER" "$FIXJ" "$FIXK" "$FIXL" "$FIXM" "$FIXO" \
         "$FIXQ1" "$FIXQ2" "$FIXQ3" "$FIXQ4" "$FIXQ5" "$FIXQ6" "$FIXR" "$FIXS"; do
  echo "  $(basename "$d"):"
  (cd "$d" && git status --porcelain | sed 's/^/    /')
done
echo "  p (fixture P is deliberately left as its injected failure left it):"
(cd "$FIXP" && git status --porcelain | sed 's/^/    /')
echo

if [ "$FAILED" -eq 0 ]; then
  echo "SELFTEST PASS: exact shifts, continuations, multi-span tokens and the"
  echo "  two-sided run all behave — as do every shape that is not an exact shift"
  echo "  (a grown block, a shrunk one, a reworded one, a duplicated one, a deleted"
  echo "  one, a blank range, a name that resolves to a different file), the"
  echo "  whitespace-only range, the disagreeing sides, the range only another"
  echo "  file's document wrote, the citation that is a suffix of a longer token,"
  echo "  a cited file that is a symlink (in the working tree or only at --old), a"
  echo "  colon that is part of another side's token rather than a continuation,"
  echo "  the document that keeps its own newlines, the failed write that is put"
  echo "  back, and the failed read-back restore that is reported instead of"
  echo "  traced back."
  exit 0
fi
echo "SELFTEST FAIL: a probe did not behave as the tool's contract says."
exit 1
