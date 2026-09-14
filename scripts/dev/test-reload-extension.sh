#!/usr/bin/env bash
# test-reload-extension.sh — bash tests for scripts/dev/reload-extension.sh.
#
# Runs against a throwaway temp git repo and a stub build command, so no real
# toolchain or network is involved. The stub (CS_RELOAD_BUILD_CMD, a
# test-only override documented in reload-extension.sh) writes a manifest with
# the build number passed to it; the mechanics under test are the reload swaps,
# version increments, dry-run, refusal and failure-safety.
#
# Run from the repository root (or anywhere): bash scripts/dev/test-reload-extension.sh

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RELOAD="$here/reload-extension.sh"

SCRATCH="$(mktemp -d /tmp/test-reload-ext.XXXXXX)"
trap 'rm -rf "$SCRATCH"' EXIT

# --- a minimal committed repo so `git worktree add` has a ref to build ------
REPO="$SCRATCH/repo"
mkdir -p "$REPO/apps/extension"
printf '%s\n' '{"name":"chat-stasher-ext","version":"0.1.0"}' > "$REPO/apps/extension/package.json"
git -C "$REPO" init -q
git -C "$REPO" config user.email test@example.invalid
git -C "$REPO" config user.name test
git -C "$REPO" add -A
git -C "$REPO" commit -qm fixture

# --- a stub build command (invoked as <cmd> <extension-dir> <build-number>) --
STUB="$SCRATCH/stub-build.sh"
cat > "$STUB" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[ $# -eq 2 ] || exit 99
extdir="$1"; n="$2"
if [ "${CS_STUB_FAIL:-0}" = "1" ]; then
  echo "stub build: configured to fail" >&2
  exit 1
fi
out="$extdir/.output/chrome-mv3"
mkdir -p "$out"
printf '{"name":"__MSG_extName__","version":"0.1.0.%s"}\n' "$n" > "$out/manifest.json"
EOF
chmod +x "$STUB"
export CS_RELOAD_BUILD_CMD="$STUB"

LOAD="$SCRATCH/ext-load"
PASS=0
fail() { echo "FAIL: $1" >&2; exit 1; }
note() { echo "ok: $1"; PASS=$((PASS + 1)); }

manifest_version() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("version",""))' "$1" 2>/dev/null || true
}

cd "$REPO"

# 1. dry-run on a not-yet-existing load dir changes nothing.
if ! bash "$RELOAD" --load-dir "$LOAD" --init --dry-run >"$SCRATCH/o1" 2>&1; then
  fail "dry-run should exit 0"
fi
[ ! -e "$LOAD" ] && [ ! -e "$LOAD.prev" ] || fail "dry-run must not create the load dir or its .prev"
grep -q "old -> new: (none) -> 0.1.0.1" "$SCRATCH/o1" || fail "dry-run should print the planned version"
note "dry-run changes nothing and prints the plan"

# 2. refuses a load dir that does not look like a previous build, unless --init.
mkdir -p "$SCRATCH/refuse"
printf '%s\n' '{"name":"not-ours","version":"9.9.9"}' > "$SCRATCH/refuse/manifest.json"
if bash "$RELOAD" --load-dir "$SCRATCH/refuse" >"$SCRATCH/o2" 2>&1; then
  fail "should refuse a non-chat-stasher load dir"
fi
grep -q "does not look like a previous chat-stasher build" "$SCRATCH/o2" || fail "refusal message missing"
note "refuses a load dir that is not a previous build"

# 3. refuses a load dir that does not exist, unless --init.
if bash "$RELOAD" --load-dir "$LOAD" >"$SCRATCH/o3" 2>&1; then
  fail "should refuse a nonexistent load dir without --init"
fi
note "refuses a nonexistent load dir without --init"

# 4. --init seeds a fresh load dir at build 1.
if ! bash "$RELOAD" --load-dir "$LOAD" --init >"$SCRATCH/o4" 2>&1; then
  fail "init reload failed"; cat "$SCRATCH/o4" >&2
fi
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.1" ] || fail "first build should be 0.1.0.1"
[ ! -e "$LOAD.prev" ] || fail "first build must have no .prev"
grep -q "old -> new: (none) -> 0.1.0.1" "$SCRATCH/o4" || fail "init should print old->new"
note "--init builds 0.1.0.1 and keeps no .prev"

# 5. a second reload increments the build number and keeps the previous copy.
if ! bash "$RELOAD" --load-dir "$LOAD" >"$SCRATCH/o5" 2>&1; then
  fail "second reload failed"; cat "$SCRATCH/o5" >&2
fi
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.2" ] || fail "second build should be 0.1.0.2"
[ "$(manifest_version "$LOAD.prev/manifest.json")" = "0.1.0.1" ] || fail ".prev should hold the prior 0.1.0.1 build"
grep -q "old -> new: 0.1.0.1 -> 0.1.0.2" "$SCRATCH/o5" || fail "second reload should print 0.1.0.1 -> 0.1.0.2"
grep -q "previous build kept at" "$SCRATCH/o5" || fail "should mention the kept .prev"
note "second reload increments to 0.1.0.2 and keeps .prev"

# 6. an explicit --build-number wins over the auto-increment.
if ! bash "$RELOAD" --load-dir "$LOAD" --build-number 50 >"$SCRATCH/o6" 2>&1; then
  fail "explicit build-number reload failed"; cat "$SCRATCH/o6" >&2
fi
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.50" ] || fail "explicit build number 50 not applied"
note "--build-number overrides the auto-increment"

# 7. a failed build leaves the load dir (and .prev) untouched.
before="$(manifest_version "$LOAD/manifest.json")"
before_prev="$(manifest_version "$LOAD.prev/manifest.json")"
if CS_STUB_FAIL=1 bash "$RELOAD" --load-dir "$LOAD" >"$SCRATCH/o7" 2>&1; then
  fail "a failed build should exit non-zero"
fi
[ "$(manifest_version "$LOAD/manifest.json")" = "$before" ] || fail "load dir changed after a failed build"
[ "$(manifest_version "$LOAD.prev/manifest.json")" = "$before_prev" ] || fail ".prev changed after a failed build"
note "a failed build leaves the load dir and .prev untouched"

echo
echo "test-reload-extension: ${PASS} cases passed"