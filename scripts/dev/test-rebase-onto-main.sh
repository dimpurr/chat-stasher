#!/usr/bin/env bash
# Exercise citation conflict resolution, prose refusal, and clean code rollback.
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
  chmod +x "$dir/scripts/relocate-citations.py" "$dir/scripts/check-citation-drift.py"
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
sed -i '' 's/src\/a.rs:1/src\/a.rs:2/' "$FIX/README.md"
sed -i '' 's/src\/a.rs:1/src\/a.rs:2/' "$FIX/docs-dev/citations.lock"
printf 'onto\n' >> "$FIX/src/a.rs"
git -C "$FIX" add README.md docs-dev/citations.lock src/a.rs
git -C "$FIX" commit -qm onto
git -C "$FIX" checkout -qb topic HEAD~1
sed -i '' 's/src\/a.rs:1/src\/a.rs:3/' "$FIX/README.md"
sed -i '' 's/src\/a.rs:1/src\/a.rs:3/' "$FIX/docs-dev/citations.lock"
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
sed -i '' 's/Anchor/Updated/' "$FIX/README.md"
git -C "$FIX" add README.md
git -C "$FIX" commit -qm onto-prose
onto_tip=$(git -C "$FIX" rev-parse HEAD)
git -C "$FIX" checkout -qb topic HEAD~1
sed -i '' 's/Anchor/Branch/' "$FIX/README.md"
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
sed -i '' '1s/base/onto/' "$FIX/src/a.rs"
git -C "$FIX" add src/a.rs
git -C "$FIX" commit -qm onto-code
onto_tip=$(git -C "$FIX" rev-parse HEAD)
git -C "$FIX" checkout -qb topic HEAD~1
sed -i '' '1s/base/topic/' "$FIX/src/a.rs"
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

if [ "$FAILED" -eq 0 ]; then
  echo "SELFTEST: PASS"
else
  echo "SELFTEST: FAIL"
fi
exit "$FAILED"
