#!/usr/bin/env bash
# Exercise citation conflict resolution, prose refusal, and clean code rollback,
# plus the two derived-file paths MERGE-1 added: a registered merge driver that
# resolves docs-dev/output-inventory.txt during the rebase, and the textual
# conflict that path falls back to on a clone that never ran the setup — both
# ending in a value regenerated from the MERGED tree by the real generator.
#
# Every edit here goes through python3 rather than `sed -i ''`: the BSD spelling
# of in-place editing is a syntax error under GNU sed, which is what a Linux
# runner has, and a suite that silently cannot run on the machine CI uses is a
# suite that stops covering the fix. python3 is already a hard dependency of
# everything under test.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="$(mktemp -d)"
FAILED=0
trap 'rm -rf "$TMP"' EXIT

expect() {
  if [ "$1" -eq "$2" ]; then
    echo "  ✔ rc=$1 — $3"
  else
    echo "  ✘ wanted rc=$1, got rc=$2 — $3"
    FAILED=1
  fi
}

# edit <file> <old> <new> — every occurrence, like sed 's/old/new/'.
# A pattern that is not there is a broken fixture, not a no-op: without the
# guard the case would go on to assert something it never set up.
edit() {
  python3 - "$@" <<'PY'
import pathlib, sys
path, old, new = sys.argv[1:4]
p = pathlib.Path(path)
text = p.read_text()
if old not in text:
    raise SystemExit(f"edit: {old!r} not in {path}")
p.write_text(text.replace(old, new))
PY
}

# edit_line1 <file> <old> <new> — first line only, like sed '1s/old/new/'.
edit_line1() {
  python3 - "$@" <<'PY'
import pathlib, sys
path, old, new = sys.argv[1:4]
p = pathlib.Path(path)
lines = p.read_text().split("\n")
if old not in lines[0]:
    raise SystemExit(f"edit_line1: {old!r} not in the first line of {path}")
lines[0] = lines[0].replace(old, new)
p.write_text("\n".join(lines))
PY
}

seed() {
  dir=$1
  mkdir -p "$dir/scripts/dev" "$dir/docs-dev" "$dir/src"
  cp "$ROOT/scripts/dev/rebase-onto-main.sh" "$dir/scripts/dev/"
  cat > "$dir/scripts/relocate-citations.py" <<'PY'
#!/usr/bin/env python3
from pathlib import Path
Path('docs-dev/citations.lock').write_text(Path('docs-dev/citations.lock').read_text() + '# relocated\n')
PY
  cat > "$dir/scripts/check-citation-drift.py" <<'PY'
#!/usr/bin/env python3
raise SystemExit(0)
PY
  # rebase-onto-main.sh regenerates the output inventory before committing; the
  # fixture stubs the generator deterministically, like the other two.
  cat > "$dir/scripts/output-inventory.py" <<'PY'
#!/usr/bin/env python3
from pathlib import Path
Path('docs-dev/output-inventory.txt').write_text('regenerated\n')
PY
  chmod +x "$dir/scripts/relocate-citations.py" "$dir/scripts/check-citation-drift.py" "$dir/scripts/output-inventory.py"
  cat > "$dir/README.md" <<'MD'
Anchor `src/a.rs:1`.
MD
  echo 'src/a.rs:1 deadbeef lines=1' > "$dir/docs-dev/citations.lock"
  printf 'base\ntail\n' > "$dir/src/a.rs"
  git -C "$dir" init -q
  git -C "$dir" config user.name fixture
  git -C "$dir" config user.email fixture@example.invalid
  git -C "$dir" add README.md docs-dev src scripts
  git -C "$dir" commit -qm base
  git -C "$dir" branch onto
}

echo "=============================================================="
echo "Case 1: citation-only document conflicts take the onto side"
echo "=============================================================="
FIX="$TMP/citation"
seed "$FIX"
git -C "$FIX" checkout -q onto
edit "$FIX/README.md" 'src/a.rs:1' 'src/a.rs:2'
edit "$FIX/docs-dev/citations.lock" 'src/a.rs:1' 'src/a.rs:2'
printf 'onto\n' >> "$FIX/src/a.rs"
git -C "$FIX" add README.md docs-dev/citations.lock src/a.rs
git -C "$FIX" commit -qm onto
git -C "$FIX" checkout -qb topic HEAD~1
edit "$FIX/README.md" 'src/a.rs:1' 'src/a.rs:3'
edit "$FIX/docs-dev/citations.lock" 'src/a.rs:1' 'src/a.rs:3'
printf 'topic\n' | cat - "$FIX/src/a.rs" > "$FIX/src/a.rs.new"
mv "$FIX/src/a.rs.new" "$FIX/src/a.rs"
git -C "$FIX" add README.md docs-dev/citations.lock src/a.rs
git -C "$FIX" commit -qm topic
(cd "$FIX" && bash scripts/dev/rebase-onto-main.sh --onto onto)
rc=$?
expect 0 "$rc" "citation-only conflicts rebase and commit"
if grep -q 'src/a.rs:2' "$FIX/README.md" && [ "$(git -C "$FIX" log -1 --pretty=%s)" = "Relocate citations after rebasing onto main" ]; then
  echo "  ✔ onto citation retained and relocation commit created"
else
  echo "  ✘ onto citation or relocation commit missing"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Case 2: prose conflict is identified before rebase"
echo "=============================================================="
FIX="$TMP/prose"
seed "$FIX"
git -C "$FIX" checkout -q onto
edit "$FIX/README.md" 'Anchor' 'Updated'
git -C "$FIX" add README.md
git -C "$FIX" commit -qm onto-prose
onto_tip=$(git -C "$FIX" rev-parse HEAD)
git -C "$FIX" checkout -qb topic HEAD~1
edit "$FIX/README.md" 'Anchor' 'Branch'
git -C "$FIX" add README.md
git -C "$FIX" commit -qm topic-prose
original=$(git -C "$FIX" rev-parse HEAD)
(cd "$FIX" && bash scripts/dev/rebase-onto-main.sh --onto "$onto_tip")
rc=$?
expect 1 "$rc" "branch prose changes stop for manual re-application"
if [ "$(git -C "$FIX" rev-parse HEAD)" = "$original" ] && [ -z "$(git -C "$FIX" status --porcelain)" ]; then
  echo "  ✔ original SHA and clean tree preserved"
else
  echo "  ✘ original SHA or clean tree not preserved"
  FAILED=1
fi

echo
echo "=============================================================="
echo "Case 3: code conflict aborts and restores original SHA"
echo "=============================================================="
FIX="$TMP/code"
seed "$FIX"
git -C "$FIX" checkout -q onto
edit_line1 "$FIX/src/a.rs" 'base' 'onto'
git -C "$FIX" add src/a.rs
git -C "$FIX" commit -qm onto-code
onto_tip=$(git -C "$FIX" rev-parse HEAD)
git -C "$FIX" checkout -qb topic HEAD~1
edit_line1 "$FIX/src/a.rs" 'base' 'topic'
git -C "$FIX" add src/a.rs
git -C "$FIX" commit -qm topic-code
original=$(git -C "$FIX" rev-parse HEAD)
(cd "$FIX" && bash scripts/dev/rebase-onto-main.sh --onto "$onto_tip")
rc=$?
expect 1 "$rc" "code conflict aborts"
if [ "$(git -C "$FIX" rev-parse HEAD)" = "$original" ] && [ -z "$(git -C "$FIX" status --porcelain)" ]; then
  echo "  ✔ original SHA and clean tree restored"
else
  echo "  ✘ original SHA or clean tree not restored"
  FAILED=1
fi

# Cases 4 and 5 drive the real generator. `docs-dev/output-inventory.txt` is
# keyed by `file:line`, so both sides regenerate it to different text at the
# same position: it conflicts as text, and the tool must take a side and then
# overwrite it with a regeneration of the MERGED tree. The three source strings
# make the three possible values distinguishable — base has neither side's
# string, the onto side has only its own, and only a regeneration of the merge
# has both.
seed_regen() { # dir — derived inventory produced by the REAL generator
  dir=$1
  mkdir -p "$dir/scripts/dev" "$dir/docs-dev" "$dir/crates/chat-stasher/src"
  cp "$ROOT/scripts/dev/rebase-onto-main.sh" "$dir/scripts/dev/"
  cp "$ROOT/scripts/output-inventory.py" "$ROOT/scripts/_user_strings.py" "$dir/scripts/"
  # The citation side stays stubbed: relocation and drift are Cases 1-3's
  # subject and selftest-relocate-citations.sh's, not this fixture's.
  cat > "$dir/scripts/relocate-citations.py" <<'PY'
#!/usr/bin/env python3
from pathlib import Path
Path('docs-dev/citations.lock').write_text(Path('docs-dev/citations.lock').read_text() + '# relocated\n')
PY
  cat > "$dir/scripts/check-citation-drift.py" <<'PY'
#!/usr/bin/env python3
raise SystemExit(0)
PY
  chmod +x "$dir/scripts/relocate-citations.py" "$dir/scripts/check-citation-drift.py"
  printf '# fixture\n' > "$dir/README.md"
  cat > "$dir/crates/chat-stasher/src/x.rs" <<'RS'
fn main() {
    println!("base line");
}
RS
  printf 'src/a.rs:1 deadbeef lines=1\n' > "$dir/docs-dev/citations.lock"
  git -C "$dir" init -q
  git -C "$dir" config user.name fixture
  git -C "$dir" config user.email fixture@example.invalid
  regen "$dir"
  git -C "$dir" add -A
  git -C "$dir" commit -qm base
  git -C "$dir" branch onto
}

regen() { (cd "$1" && python3 scripts/output-inventory.py) >/dev/null; }

fork_inventory() { # dir — both sides add a source string, left on the topic branch
  D=$1
  git -C "$D" checkout -q onto
  cat > "$D/crates/chat-stasher/src/a.rs" <<'RS'
fn extra() {
    println!("onto line");
}
RS
  cat > "$D/crates/chat-stasher/src/x.rs" <<'RS'
fn main() {
    // onto
    println!("base line");
}
RS
  regen "$D"
  git -C "$D" add -A && git -C "$D" commit -qm onto
  git -C "$D" checkout -qb topic HEAD~1
  cat > "$D/crates/chat-stasher/src/b.rs" <<'RS'
fn extra() {
    println!("topic line");
}
RS
  regen "$D"
  git -C "$D" add -A && git -C "$D" commit -qm topic
}

# The end state both paths must reach: the relocation commit, a clean tree, a
# HEAD that really is the merge (both added source files in it), and an
# inventory that IS the real regeneration of that tree — which the generator's
# own gate form decides, rather than a grep that a stale file could satisfy.
# "topic line" is the tell: only the merged tree has it, so an onto-side value
# (or a textual mix) fails this and --check both. Every check but --check reads
# HEAD, not the working tree: a run that rolled back would otherwise be judged
# on the branch's own pre-rebase files, which match each other and would pass.
expect_regenerated() { # dir label
  D=$1; label=$2
  if [ "$(git -C "$D" log -1 --pretty=%s)" = "Relocate citations after rebasing onto main" ]; then
    echo "  ✔ $label: relocation commit created"
  else
    echo "  ✘ $label: no relocation commit (HEAD is $(git -C "$D" log -1 --pretty=%s))"
    FAILED=1
  fi
  if [ -z "$(git -C "$D" status --porcelain)" ]; then
    echo "  ✔ $label: tree clean after the commit"
  else
    echo "  ✘ $label: tree dirty: $(git -C "$D" status --porcelain | tr '\n' ' ')"
    FAILED=1
  fi
  if [ -n "$(git -C "$D" show HEAD:crates/chat-stasher/src/a.rs 2>/dev/null)" ] \
     && [ -n "$(git -C "$D" show HEAD:crates/chat-stasher/src/b.rs 2>/dev/null)" ]; then
    echo "  ✔ $label: HEAD holds the merged source tree (both sides' files)"
  else
    echo "  ✘ $label: HEAD is not the merge — a rollback or an aborted replay is being judged"
    FAILED=1
  fi
  committed=$(git -C "$D" show HEAD:docs-dev/output-inventory.txt)
  if printf '%s\n' "$committed" | grep -q 'crates/chat-stasher/src/a.rs:2  "onto line"' \
     && printf '%s\n' "$committed" | grep -q 'crates/chat-stasher/src/b.rs:2  "topic line"'; then
    echo "  ✔ $label: committed inventory holds BOTH sides' strings (the merged tree's value)"
  else
    echo "  ✘ $label: committed inventory holds only one side's strings — a side was kept, not regenerated"
    FAILED=1
  fi
  (cd "$D" && python3 scripts/output-inventory.py --check) >"$TMP/check.log" 2>&1
  if [ "$?" -eq 0 ]; then
    echo "  ✔ $label: output-inventory.py --check passes on the committed file"
  else
    echo "  ✘ $label: --check rejects the committed inventory:"
    sed 's/^/      /' "$TMP/check.log"
    FAILED=1
  fi
}

echo
echo "=============================================================="
echo "Case 4: registered driver + real generator -> regenerated"
echo "=============================================================="
FIX="$TMP/regen-driver"
seed_regen "$FIX"
cp "$ROOT/.gitattributes" "$FIX/.gitattributes"
cp "$ROOT/scripts/dev/setup-merge-drivers.sh" "$ROOT/scripts/dev/regenerate-merge-drivers.py" "$FIX/scripts/dev/"
chmod +x "$FIX/scripts/dev/regenerate-merge-drivers.py"
(cd "$FIX" && bash scripts/dev/setup-merge-drivers.sh) >/dev/null 2>&1
fork_inventory "$FIX"
(cd "$FIX" && bash scripts/dev/rebase-onto-main.sh --onto onto) >"$TMP/regen4.log" 2>&1
rc=$?
expect 0 "$rc" "rebase completes with the driver registered"
expect_regenerated "$FIX" "driver"

echo
echo "=============================================================="
echo "Case 5: no driver -> the conflict is accepted, then regenerated"
echo "=============================================================="
FIX="$TMP/regen-fallback"
seed_regen "$FIX"   # setup script is never copied and never run
fork_inventory "$FIX"
(cd "$FIX" && bash scripts/dev/rebase-onto-main.sh --onto onto) >"$TMP/regen5.log" 2>&1
rc=$?
expect 0 "$rc" "rebase completes with no driver registered"
if grep -qi "CONFLICT.*output-inventory" "$TMP/regen5.log"; then
  echo "  ✔ the derived file really conflicted (acceptance path exercised, not a silent skip)"
else
  echo "  ✘ no conflict on the derived file — the acceptance path was not exercised"
  FAILED=1
fi
expect_regenerated "$FIX" "fallback"

if [ "$FAILED" -eq 0 ]; then
  echo "SELFTEST: PASS"
else
  echo "SELFTEST: FAIL"
fi
exit "$FAILED"
