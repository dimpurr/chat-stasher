#!/usr/bin/env bash
# Exercise the derived-file merge drivers (MERGE-1).
#
# This guards the git-facing contract: that a registered driver is actually
# invoked by `git merge` AND by `git rebase`, that neither stops on the derived
# files (they resolve to the current side), and that the fix is inert but never
# wrong on a clone that has not run setup-merge-drivers.sh — a plain text merge,
# a loud conflict, not a silent skip.
#
# What a driver-resolved merge leaves committed is a DISPOSABLE value (the
# current side), so the contract has a second half: the integration step must
# regenerate both files from the merged tree. That half is not testable here —
# it needs the real generator against a merged source tree — and is covered by
# test-rebase-onto-main.sh, which drives rebase-onto-main.sh with the real
# scripts/output-inventory.py and asserts the committed file passes
# `output-inventory.py --check`. The gates (check-citation-drift.py,
# output-inventory.py --check) are what turn a skipped regeneration into a red
# run rather than a stale value; see CONTRIBUTING.md, "Resolving a merge".
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="$(mktemp -d)"
FAILED=0
trap 'rm -rf "$TMP"' EXIT

ok()   { echo "  ✔ $*"; }
bad()  { echo "  ✘ $*"; FAILED=1; }

# run the setup script *inside* the scratch repo (it resolves its own root).
setup() { (cd "$1" && bash scripts/dev/setup-merge-drivers.sh) >/dev/null 2>&1; }

seed() {
  dir=$1
  mkdir -p "$dir/scripts/dev" "$dir/docs-dev" "$dir/src"
  cp "$ROOT/.gitattributes" "$dir/.gitattributes"
  cp "$ROOT/scripts/dev/rebase-onto-main.sh" "$dir/scripts/dev/"
  cp "$ROOT/scripts/dev/setup-merge-drivers.sh" "$dir/scripts/dev/"
  cp "$ROOT/scripts/dev/regenerate-merge-drivers.py" "$dir/scripts/dev/"
  chmod +x "$dir/scripts/dev/regenerate-merge-drivers.py"
  git -C "$dir" init -q
  git -C "$dir" config user.name fixture
  git -C "$dir" config user.email fixture@example.invalid
  echo 'base' > "$dir/src/v.txt"
  printf 'citations.lock base\n' > "$dir/docs-dev/citations.lock"
  printf 'output-inventory base\n' > "$dir/docs-dev/output-inventory.txt"
  git -C "$dir" add -A
  git -C "$dir" commit -qm base
  git -C "$dir" branch mainline
}

fork_regen() { # dir brand — both sides rewrite the derived files, sources don't clash
  D=$1
  git -C "$D" checkout -q mainline
  echo 'ml'   > "$D/src/v.txt"
  printf 'citations.lock base\nMAIN\n'   > "$D/docs-dev/citations.lock"
  printf 'output-inventory base\nMAIN\n' > "$D/docs-dev/output-inventory.txt"
  git -C "$D" add -A && git -C "$D" commit -qm mainline
  git -C "$D" checkout -qb topic HEAD~1
  echo 'topic' > "$D/src/t.txt"
  printf 'citations.lock base\nTOPIC\n'   > "$D/docs-dev/citations.lock"
  printf 'output-inventory base\nTOPIC\n' > "$D/docs-dev/output-inventory.txt"
  git -C "$D" add -A && git -C "$D" commit -qm topic
  git -C "$D" checkout -q mainline
}

echo "=================================================================="
echo "Case 1: drivers registered -> derived files merge without stopping"
echo "=================================================================="
D="$TMP/resolve"
seed "$D"
setup "$D"
[ -n "$(git -C "$D" config --get merge.regenerate-citations.driver)" ] \
  && ok "setup registered citations driver" || bad "citations driver not registered"
[ -n "$(git -C "$D" config --get merge.regenerate-inventory.driver)" ] \
  && ok "setup registered inventory driver" || bad "inventory driver not registered"
fork_regen "$D"
git -C "$D" merge --no-ff topic >"$TMP/merge1.log" 2>&1
merge_rc=$?
[ "$merge_rc" -eq 0 ] && ok "merge of two regenerations exits 0" || bad "merge rc=$merge_rc"
u=$(git -C "$D" diff --name-only --diff-filter=U)
[ -z "$u" ] && ok "no unmerged files after the merge" || bad "still unmerged: $u"
[ "$(cat "$D/docs-dev/citations.lock")" = "citations.lock base
MAIN" ] && ok "citations.lock kept the current side (regeneration is integration's job)" \
         || bad "citations.lock unexpected: $(cat "$D/docs-dev/citations.lock")"
[ -f "$D/src/t.txt" ] && ok "merged-in source file present => merge really completed" || bad "src/t.txt missing"

echo "=================================================================="
echo "Case 2: no setup (fresh clone) -> inert fallback, a loud conflict"
echo "=================================================================="
D="$TMP/safe"
seed "$D"   # copy setup script but do NOT run it
fork_regen "$D"
git -C "$D" merge --no-ff topic >"$TMP/merge2.log" 2>&1
merge_rc=$?
[ "$merge_rc" -ne 0 ] && ok "without the driver the derived files still conflict (rc=$merge_rc)" \
                       || bad "unexpectedly clean without setup (driver would be skipped silently?)"
grep -qi "CONFLICT" "$TMP/merge2.log" && ok "conflict is loud (textual fallback), not a silent skip" \
                                          || bad "no CONFLICT line in fallback path"
u=$(git -C "$D" diff --name-only --diff-filter=U)
echo "  unmerged as expected: $(echo $u | tr '\n' ' ')"

# The integration tool rebases rather than merges, so the same contract has to
# hold for `git rebase`: it drives the same merge machinery, and whether the
# driver is consulted there is a property of the git, not of this script's
# intent. Both states are asserted against ONE fixture, so "it passed with the
# driver" cannot be a fixture that happened not to conflict.
echo "=================================================================="
echo "Case 3: drivers registered -> a rebase does not stop either"
echo "=================================================================="
D="$TMP/rebase"
seed "$D"
fork_regen "$D"
git -C "$D" checkout -q topic
git -C "$D" rebase mainline >"$TMP/rebase3-before.log" 2>&1
rebase_rc=$?
u=$(git -C "$D" diff --name-only --diff-filter=U)
[ "$rebase_rc" -ne 0 ] && [ -n "$u" ] \
  && ok "unregistered driver leaves the same rebase stopped (rc=$rebase_rc, unmerged: $(echo $u | tr '\n' ' '))" \
  || bad "rebase without the driver did not conflict (rc=$rebase_rc, unmerged: $u) — Case 3 would prove nothing"
git -C "$D" rebase --abort >/dev/null 2>&1
setup "$D"
git -C "$D" rebase mainline >"$TMP/rebase3-after.log" 2>&1
rebase_rc=$?
[ "$rebase_rc" -eq 0 ] && ok "rebase of two regenerations exits 0 once the driver is registered" \
                       || bad "rebase rc=$rebase_rc with the driver registered"
u=$(git -C "$D" diff --name-only --diff-filter=U)
[ -z "$u" ] && ok "no unmerged files after the rebase" || bad "still unmerged: $u"
[ "$(cat "$D/docs-dev/citations.lock")" = "citations.lock base
MAIN" ] && ok "citations.lock kept the onto side (regeneration is integration's job)" \
         || bad "citations.lock unexpected: $(cat "$D/docs-dev/citations.lock")"
[ -f "$D/src/t.txt" ] && ok "replayed commit present => rebase really completed" || bad "src/t.txt missing"

echo "=================================================================="
if [ "$FAILED" -eq 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILED"; fi
exit "$FAILED"
