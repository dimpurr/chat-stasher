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

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/test-reload-ext.XXXXXX")"
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
# The reload script must build from a throwaway worktree of the ref, never from
# the checkout it was invoked in. An untracked marker in that checkout is
# therefore invisible here; if the build can see it, the build did not come from
# a worktree. Cases 8 and 9 lean on this.
if [ -e "$extdir/UNCOMMITTED-MARKER" ]; then
  echo "stub build: uncommitted edits leaked into the build" >&2
  exit 98
fi
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

# 8. uncommitted edits in the invoking checkout never reach the build. The
#    script builds from a throwaway worktree of the ref, so a file that exists
#    only in the working tree is absent there; the stub exits 98 if it sees one,
#    which makes this case pass only when the build really came from a worktree.
printf 'uncommitted\n' > "$REPO/apps/extension/UNCOMMITTED-MARKER"
before="$(manifest_version "$LOAD/manifest.json")"
if ! bash "$RELOAD" --load-dir "$LOAD" --build-number 60 >"$SCRATCH/o8" 2>&1; then
  fail "a reload must succeed with uncommitted edits in the invoking checkout"
  cat "$SCRATCH/o8" >&2
fi
rm -f "$REPO/apps/extension/UNCOMMITTED-MARKER"
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.60" ] || fail "expected 0.1.0.60"
grep -q "old -> new: $before -> 0.1.0.60" "$SCRATCH/o8" || fail "should print old->new"
note "uncommitted edits never reach the build (it runs from a worktree of the ref)"

# 9. the same holds for --ref: a ref other than HEAD is what gets built, so a
#    marker added after the ref's commit is still absent from the build.
printf 'uncommitted\n' > "$REPO/apps/extension/UNCOMMITTED-MARKER"
if ! bash "$RELOAD" --load-dir "$LOAD" --ref HEAD --build-number 61 >"$SCRATCH/o9" 2>&1; then
  fail "--ref HEAD reload failed with an uncommitted marker present"; cat "$SCRATCH/o9" >&2
fi
rm -f "$REPO/apps/extension/UNCOMMITTED-MARKER"
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.61" ] || fail "expected 0.1.0.61"
[ "$(manifest_version "$LOAD.prev/manifest.json")" = "0.1.0.60" ] || fail ".prev should hold 0.1.0.60"
note "--ref builds the ref, not the working tree"

# 10. --init must not move an unrelated directory aside. A non-empty directory
#     that is not a chat-stasher build is refused even with --init: "--init" on,
#     say, a projects directory would otherwise rename that whole directory to
#     <dir>.prev and drop a build in its place. Nothing in the decoy may change.
DECOY="$SCRATCH/decoy"
mkdir -p "$DECOY/sub"
printf 'not ours\n' > "$DECOY/keep-me.txt"
if bash "$RELOAD" --load-dir "$DECOY" --init >"$SCRATCH/o10" 2>&1; then
  fail "--init must refuse a non-empty directory that is not a build"
fi
grep -q "refusing to --init" "$SCRATCH/o10" || fail "the refusal should say it is about --init"
grep -q "point --load-dir at an empty or new directory" "$SCRATCH/o10" || fail "the refusal should say what to point at instead"
[ ! -e "$DECOY.prev" ] || fail "the decoy directory must not have been renamed to .prev"
[ -f "$DECOY/keep-me.txt" ] || fail "the decoy directory's file must be untouched"
[ -d "$DECOY/sub" ] || fail "the decoy directory's subdirectory must be untouched"
note "--init refuses a non-empty non-build directory and leaves it untouched"

# 11. --init still seeds a directory that exists but is empty, which is the case
#     it exists for (a load dir made ahead of the first build).
EMPTY="$SCRATCH/empty-load"
mkdir -p "$EMPTY"
if ! bash "$RELOAD" --load-dir "$EMPTY" --init >"$SCRATCH/o11" 2>&1; then
  fail "--init must seed an empty existing directory"; cat "$SCRATCH/o11" >&2
fi
[ "$(manifest_version "$EMPTY/manifest.json")" = "0.1.0.1" ] || fail "an empty load dir should build 0.1.0.1"
note "--init seeds an empty existing directory"

# 12. a rename that fails between the two steps of the swap is recoverable. The
#     hook CS_RELOAD_TEST_FAIL_SWAP (test-only, documented in the script header)
#     fails the second rename; the script must put .prev back, say so, and exit
#     non-zero rather than leaving the load dir missing.
before_swap="$(manifest_version "$LOAD/manifest.json")"
if CS_RELOAD_TEST_FAIL_SWAP=1 bash "$RELOAD" --load-dir "$LOAD" >"$SCRATCH/o12" 2>&1; then
  fail "a failed swap must exit non-zero"
fi
grep -q "could not move the staged build into" "$SCRATCH/o12" || fail "the failed rename should be named"
grep -q "restored the previous build" "$SCRATCH/o12" || fail "the restore should be reported"
[ "$(manifest_version "$LOAD/manifest.json")" = "$before_swap" ] || fail "the load dir must hold the previous build again"
[ ! -e "$LOAD.prev" ] || fail ".prev should have been moved back, not left behind"
note "a failed swap restores the previous build and exits non-zero"

# 13. a run interrupted between the two renames leaves the load dir missing and
#     .prev in place. That must not be read as "a fresh directory": the next run
#     refuses and asks for --recover, and --recover puts the build back.
interrupted="$(manifest_version "$LOAD/manifest.json")"
rm -rf "$LOAD.prev"
mv "$LOAD" "$LOAD.prev"
if bash "$RELOAD" --load-dir "$LOAD" >"$SCRATCH/o13a" 2>&1; then
  fail "an interrupted state must be refused, not built over"
fi
grep -q "an earlier run stopped between its two renames" "$SCRATCH/o13a" || fail "the refusal should name the interrupted state"
grep -q -- "--recover" "$SCRATCH/o13a" || fail "the refusal should point at --recover"
[ ! -e "$LOAD" ] || fail "the refusal must not create the load dir"
[ "$(manifest_version "$LOAD.prev/manifest.json")" = "$interrupted" ] || fail "the refusal must leave .prev alone"
if ! bash "$RELOAD" --load-dir "$LOAD" --recover --build-number 70 >"$SCRATCH/o13b" 2>&1; then
  fail "--recover failed"; cat "$SCRATCH/o13b" >&2
fi
grep -q "recovered" "$SCRATCH/o13b" || fail "--recover should report the restore"
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.70" ] || fail "expected 0.1.0.70 after recovery"
[ "$(manifest_version "$LOAD.prev/manifest.json")" = "$interrupted" ] || fail ".prev should hold the recovered build"
note "an interrupted swap is refused, and --recover restores it"

# 14. --recover on a load dir that is not in the interrupted state is a usage
#     error (exit 2), not a silent no-op: there is nothing to recover there.
if bash "$RELOAD" --load-dir "$LOAD" --recover >"$SCRATCH/o14" 2>&1; then
  fail "--recover without an interrupted state should fail"
else
  rc=$?
  [ "$rc" = "2" ] || fail "--recover without an interrupted state should exit 2 (usage), got $rc"
fi
note "--recover outside the interrupted state is a usage error"

# 15. a throwaway worktree that `git worktree remove` cannot delete is reported,
#     not swallowed, and does not stay behind in `git worktree list`. The stub
#     git fails only that one subcommand and delegates everything else.
REAL_GIT="$(command -v git)"
GITSTUB="$SCRATCH/gitstub"
mkdir -p "$GITSTUB"
cat > "$GITSTUB/git" <<EOF
#!/usr/bin/env bash
if [ "\$1" = "worktree" ] && [ "\$2" = "remove" ]; then
  echo "gitstub: refusing to remove a worktree" >&2
  exit 1
fi
exec "$REAL_GIT" "\$@"
EOF
chmod +x "$GITSTUB/git"
if ! PATH="$GITSTUB:$PATH" bash "$RELOAD" --load-dir "$LOAD" --build-number 80 >"$SCRATCH/o15" 2>&1; then
  fail "a worktree-removal failure must not fail the reload itself"; cat "$SCRATCH/o15" >&2
fi
grep -q "warning: could not remove the throwaway worktree" "$SCRATCH/o15" || fail "the cleanup failure must be reported"
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.80" ] || fail "the reload itself should still have happened"
stale="$(git -C "$REPO" worktree list | grep "chat-stasher-reload" || true)"
[ -z "$stale" ] || fail "a stale worktree entry was left in git worktree list: $stale"
note "an undeletable worktree is reported and left no stale entry"

echo
echo "test-reload-extension: ${PASS} cases passed"