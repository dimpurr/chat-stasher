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
# Every pid this test starts is recorded here, one per line. The CDP mock server
# is the only child, and it is started in this shell rather than inside a command
# substitution: a pid set in a subshell never reaches the parent, which is how a
# `port="$(start_mock …)"` call once leaked a mock on every run — the parent's
# pid stayed empty, so neither stop_mock nor this trap killed anything. A file is
# used rather than a variable for the same reason: it is written by whichever
# shell starts the child and read by the one that cleans up.
MOCK_PIDS_FILE="$SCRATCH/mock-pids"
: > "$MOCK_PIDS_FILE"
cleanup() {
  # Kill every mock this run started, not just the most recent one.
  if [ -s "$MOCK_PIDS_FILE" ]; then
    while read -r pid; do
      [ -n "$pid" ] || continue
      kill "$pid" 2>/dev/null || true
    done < "$MOCK_PIDS_FILE"
  fi
  # Backstop for a mock whose pid never reached the file — the failure this
  # block exists for. It is still identifiable by the scratch path it was run
  # from, which no other run of this test shares.
  pkill -f "$SCRATCH/cdp-mock.mjs" 2>/dev/null || true
  rm -rf "$SCRATCH"
}
trap cleanup EXIT
# A signal has to clean up too, and then actually stop the script: exiting is
# what fires the EXIT trap, so each handler only sets the status. Without this,
# `trap cleanup TERM` would clean up and then carry on from where it was
# interrupted, which is not what a killed run should do.
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# --- a minimal committed repo so `git worktree add` has a ref to build ------
REPO="$SCRATCH/repo"
mkdir -p "$REPO/apps/extension"
printf '%s\n' '{"name":"chat-stasher-ext","version":"0.1.0","dependencies":{"left-pad":"1.3.0"}}' > "$REPO/apps/extension/package.json"
git -C "$REPO" init -q
git -C "$REPO" config user.email test@example.invalid
git -C "$REPO" config user.name test
git -C "$REPO" add -A
git -C "$REPO" commit -qm fixture
# A ref from before the extension declared ajv, so case 18 can build a tree
# whose manifest the checkout's node_modules does satisfy.
OLD_REF="$(git -C "$REPO" rev-parse HEAD)"
printf '%s\n' '{"name":"chat-stasher-ext","version":"0.1.0","dependencies":{"left-pad":"1.3.0","ajv":"8.20.0"}}' > "$REPO/apps/extension/package.json"
git -C "$REPO" commit -qam 'declare ajv'

# The checkout's node_modules, complete for what the committed package.json
# declares. It is created after the commits, so it stays untracked — the real
# setup, where node_modules is never committed and the script symlinks the
# checkout's copy into the throwaway worktree. The staleness cases below break
# it on purpose, the way W932 found it: a ref that declares a dependency the
# checkout's install does not link.
mkdir -p "$REPO/apps/extension/node_modules/left-pad" "$REPO/apps/extension/node_modules/ajv"
printf '%s\n' '{"name":"left-pad","version":"1.3.0"}' > "$REPO/apps/extension/node_modules/left-pad/package.json"
printf '%s\n' '{"name":"ajv","version":"8.20.0"}' > "$REPO/apps/extension/node_modules/ajv/package.json"

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

# --- a stale or missing node_modules for the ref being built -----------------
# The script symlinks the checkout's node_modules into the throwaway worktree so
# the build runs offline and fast, but a tree that cannot serve the ref is worse
# than none: the build dies inside the bundle with "Cannot find module …", which
# reads as a source bug and is not one (W932: origin/main declared ajv 8.20.0,
# the operator checkout linked nothing). The script must detect that and install
# inside the worktree instead, and must never write to the operator checkout,
# which may be the tree the user's own browser loads from.
#
# The manifest that decides "can this tree serve the build" is the *worktree's*,
# not the checkout's, because the worktree is what gets built — case 18 is the
# check that pins which of the two the script consulted.
#
# CS_RELOAD_INSTALL_CMD (test-only, documented in the script header) stands in
# for `pnpm install`: it completes the worktree's node_modules and records that
# it was called. The build stub used here fails unless the worktree's
# node_modules really carries ajv, so a pass means the install ran there — and
# the checkout's own copy must still lack ajv afterwards, which is what "never
# in the operator checkout" means observably.
STUB2="$SCRATCH/stub-build2.sh"
cat > "$STUB2" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[ $# -eq 2 ] || exit 99
extdir="$1"; n="$2"
if [ -e "$extdir/UNCOMMITTED-MARKER" ]; then
  echo "stub build: uncommitted edits leaked into the build" >&2
  exit 98
fi
# The worktree's node_modules must carry ajv: a stale symlink from the checkout
# would be missing it, and the build must not run from such a tree.
if [ ! -e "$extdir/node_modules/ajv/package.json" ]; then
  echo "stub build: the worktree's node_modules is incomplete (no ajv) — a stale symlink was used" >&2
  exit 97
fi
# .wxt is generated build state that belongs to whichever node_modules the build
# uses. Recorded here so a case can assert it travelled with that copy: a
# worktree-local install must not hand it the checkout's, because writing the
# build's generated files into the operator checkout is what the whole path is
# there to avoid.
if [ -L "$extdir/.wxt" ]; then
  echo "wxt=symlink" >> "$WXT_LOG"
elif [ -d "$extdir/.wxt" ]; then
  echo "wxt=dir" >> "$WXT_LOG"
else
  echo "wxt=none" >> "$WXT_LOG"
fi
out="$extdir/.output/chrome-mv3"
mkdir -p "$out"
printf '{"name":"__MSG_extName__","version":"0.1.0.%s"}\n' "$n" > "$out/manifest.json"
EOF
chmod +x "$STUB2"
INSTALL_STUB="$SCRATCH/stub-install.sh"
cat > "$INSTALL_STUB" <<EOF
#!/usr/bin/env bash
set -euo pipefail
extdir="\$1"
echo "install \$extdir" >> "$SCRATCH/install-log"
mkdir -p "\$extdir/node_modules/ajv"
printf '%s\n' '{"name":"ajv","version":"8.20.0"}' > "\$extdir/node_modules/ajv/package.json"
EOF
chmod +x "$INSTALL_STUB"
# Exported, because the stub build runs as a subprocess of the reload script:
# an unexported variable would reach it empty and the append would fail there,
# not here.
export WXT_LOG="$SCRATCH/wxt-log"

# The checkout generates .wxt as a real directory; give it one holding a marker,
# so the cases below can tell "symlinked to the checkout" from "the worktree's
# own".
mkdir -p "$REPO/apps/extension/.wxt"
printf '%s\n' 'prepared by an earlier build' > "$REPO/apps/extension/.wxt/types.d.ts"

# 16. a complete node_modules is symlinked, no install runs, and the worktree
#     reuses the checkout's .wxt — the fast path, unchanged.
: > "$SCRATCH/install-log"
: > "$WXT_LOG"
if ! CS_RELOAD_BUILD_CMD="$STUB2" bash "$RELOAD" --load-dir "$LOAD" --build-number 100 >"$SCRATCH/o100" 2>&1; then
  fail "a reload with a complete node_modules should succeed"; cat "$SCRATCH/o100" >&2
fi
[ ! -s "$SCRATCH/install-log" ] || fail "a complete node_modules must not trigger an install"
[ "$(cat "$WXT_LOG")" = "wxt=symlink" ] || fail "the complete fast path should reuse the checkout's .wxt, got: $(tr '\n' ' ' < "$WXT_LOG")"
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.100" ] || fail "expected 0.1.0.100"
note "a complete node_modules is symlinked and triggers no install"

# 17. the ref declares ajv and the checkout's node_modules does not link it:
#     detected by name, installed inside the worktree, checkout left untouched.
rm -rf "$REPO/apps/extension/node_modules/ajv"
: > "$SCRATCH/install-log"
: > "$WXT_LOG"
if ! CS_RELOAD_INSTALL_CMD="$INSTALL_STUB" CS_RELOAD_BUILD_CMD="$STUB2" bash "$RELOAD" --load-dir "$LOAD" --build-number 101 >"$SCRATCH/o101" 2>&1; then
  fail "a reload with a stale node_modules should install in the worktree and succeed"; cat "$SCRATCH/o101" >&2
fi
grep -q "cannot serve HEAD (missing: ajv" "$SCRATCH/o101" || fail "the stale node_modules should be reported by which dep is missing"
grep -q "installing inside the throwaway worktree" "$SCRATCH/o101" || fail "the worktree install should be reported"
[ -s "$SCRATCH/install-log" ] || fail "the install stub should have been called"
[ ! -e "$REPO/apps/extension/node_modules/ajv" ] || fail "the install must not write into the operator checkout"
[ "$(cat "$WXT_LOG")" = "wxt=none" ] || fail "a worktree-local install must not symlink the checkout's .wxt, got: $(tr '\n' ' ' < "$WXT_LOG")"
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.101" ] || fail "expected 0.1.0.101"
note "a node_modules that cannot serve the ref is detected and installed inside the worktree"

# 18. the manifest that decides is the *ref's*, not the checkout's. Here the
#     two disagree: the checkout's own (uncommitted) package.json no longer
#     declares ajv and its node_modules does not link ajv, so a check that
#     asked the checkout would call that tree complete and symlink it — and the
#     build of HEAD, which does declare ajv, would then die inside the bundle.
#     The script must ask the ref instead and install in the worktree.
printf '%s\n' '{"name":"chat-stasher-ext","version":"0.1.0","dependencies":{"left-pad":"1.3.0"}}' > "$REPO/apps/extension/package.json"
: > "$SCRATCH/install-log"
: > "$WXT_LOG"
if ! CS_RELOAD_INSTALL_CMD="$INSTALL_STUB" CS_RELOAD_BUILD_CMD="$STUB2" bash "$RELOAD" --load-dir "$LOAD" --build-number 102 >"$SCRATCH/o102" 2>&1; then
  fail "a reload of a ref that declares a dep the checkout's node_modules lacks should install in the worktree"; cat "$SCRATCH/o102" >&2
fi
[ -s "$SCRATCH/install-log" ] || fail "the install should have run: the ref, not the checkout, decides what the build needs"
[ ! -e "$REPO/apps/extension/node_modules/ajv" ] || fail "the install must not write into the operator checkout"
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.102" ] || fail "expected 0.1.0.102"
note "the ref's manifest, not the checkout's, decides whether node_modules is stale"

# 19. and the other direction: a ref that predates the dependency does not need
#     the link the checkout is missing, so the symlinked tree is the right one
#     and no install must run. Without this, "install whenever ajv is absent"
#     would pass as "detects staleness".
: > "$SCRATCH/install-log"
if ! CS_RELOAD_INSTALL_CMD="$INSTALL_STUB" CS_RELOAD_BUILD_CMD="$STUB" bash "$RELOAD" --load-dir "$LOAD" --ref "$OLD_REF" --build-number 103 >"$SCRATCH/o103" 2>&1; then
  fail "a reload of a ref whose deps the checkout does link should succeed"; cat "$SCRATCH/o103" >&2
fi
[ ! -s "$SCRATCH/install-log" ] || fail "a ref that declares no ajv must not trigger an install"
[ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.103" ] || fail "expected 0.1.0.103"
note "a ref that does not need the missing link is served by the checkout's node_modules"
printf '%s\n' '{"name":"chat-stasher-ext","version":"0.1.0","dependencies":{"left-pad":"1.3.0","ajv":"8.20.0"}}' > "$REPO/apps/extension/package.json"

# 20. an absent node_modules installs inside the worktree too.
mv "$REPO/apps/extension/node_modules" "$SCRATCH/nm-backup"
: > "$SCRATCH/install-log"
: > "$WXT_LOG"
if ! CS_RELOAD_INSTALL_CMD="$INSTALL_STUB" CS_RELOAD_BUILD_CMD="$STUB2" bash "$RELOAD" --load-dir "$LOAD" --build-number 104 >"$SCRATCH/o104" 2>&1; then
  fail "a reload with no node_modules should install in the worktree and succeed"; cat "$SCRATCH/o104" >&2
fi
grep -q "cannot serve HEAD" "$SCRATCH/o104" || fail "the missing node_modules should be reported"
[ -s "$SCRATCH/install-log" ] || fail "the install stub should have been called"
[ ! -e "$SCRATCH/nm-backup/ajv" ] || fail "the install must not write into the operator checkout"
mv "$SCRATCH/nm-backup" "$REPO/apps/extension/node_modules"
# Put back what case 17 removed, so the checkout's node_modules is complete
# again for the CDP cases below.
mkdir -p "$REPO/apps/extension/node_modules/ajv"
printf '%s\n' '{"name":"ajv","version":"8.20.0"}' > "$REPO/apps/extension/node_modules/ajv/package.json"
note "an absent node_modules installs inside the worktree, not the checkout"

# --- CDP reload (--cdp-port) ------------------------------------------------
# These cases mock Chrome's DevTools endpoint: a tiny HTTP + WebSocket server
# that lists one chat-stasher service worker and answers Runtime.evaluate the
# way a worker would. No real browser is involved. The helper is Node, so the
# whole block is skipped (loudly, not silently) when node is absent; CI and a
# development machine have it.
if ! command -v node >/dev/null 2>&1; then
  echo "note: node not found; skipping the --cdp-port cases"
else
  # How long start_mock waits for the mock to report its port, in 50ms steps.
  # Sized for the slowest machine the suite runs on rather than the fastest: a
  # shared CI runner starting a cold node under load took longer than the 5s this
  # was before, and the run failed with a message that did not say which of the
  # two ways it failed it was. 30s is still bounded, and a mock that dies is
  # detected at once (see start_mock), so this ceiling is only reached by a mock
  # that is alive and slow.
  MOCK_START_TRIES=600
  cat > "$SCRATCH/cdp-mock.mjs" <<'EOF'
// A fake Chrome DevTools endpoint for test-reload-extension.sh. Test-only.
import http from 'node:http';
import crypto from 'node:crypto';
import fs from 'node:fs';

function arg(name, fallback) {
  const i = process.argv.indexOf(name);
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : fallback;
}

const mode = arg('--mode', 'ok');                 // ok | notfound | stuck
const expectedVersion = arg('--expected-version', '0.1.0.2');
const portFile = arg('--port-file');
let running = arg('--old-version', '0.1.0.1');

const EXT_ID = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
const server = http.createServer((req, res) => {
  if (req.url === '/json/list') {
    res.setHeader('content-type', 'application/json');
    if (mode === 'notfound') { res.end('[]'); return; }
    res.end(JSON.stringify([{
      id: 'mock-sw',
      type: 'service_worker',
      url: `chrome-extension://${EXT_ID}/service-worker.js`,
      webSocketDebuggerUrl: `ws://127.0.0.1:${server.address().port}/devtools/page/mock-sw`,
    }]));
    return;
  }
  res.statusCode = 404;
  res.end();
});

// Minimal unmasked protocol plumbing: the test only exchanges one small text
// frame in each direction, so a general implementation would be untested weight.
function decodeFrame(buf) {
  const opcode = buf[0] & 0x0f;
  const masked = (buf[1] & 0x80) !== 0;
  let len = buf[1] & 0x7f;
  let offset = 2;
  if (len === 126) { len = buf.readUInt16BE(2); offset = 4; }
  else if (len === 127) { len = Number(buf.readBigUInt64BE(2)); offset = 10; }
  let mask = null;
  if (masked) { mask = buf.subarray(offset, offset + 4); offset += 4; }
  const payload = Buffer.from(buf.subarray(offset, offset + len));
  if (mask) for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4];
  return { opcode, text: payload.toString('utf8') };
}

function encodeText(str) {
  const data = Buffer.from(str, 'utf8');
  if (data.length < 126) return Buffer.concat([Buffer.from([0x81, data.length]), data]);
  const header = Buffer.alloc(4);
  header[0] = 0x81;
  header[1] = 126;
  header.writeUInt16BE(data.length, 2);
  return Buffer.concat([header, data]);
}

server.on('upgrade', (req, socket) => {
  const key = String(req.headers['sec-websocket-key'] || '');
  const accept = crypto.createHash('sha1')
    .update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11')
    .digest('base64');
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\n'
    + 'Upgrade: websocket\r\n'
    + 'Connection: Upgrade\r\n'
    + `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );
  socket.on('data', (buf) => {
    const frame = decodeFrame(buf);
    // Answer the close handshake. Without this the client's socket stays in
    // CLOSING, which keeps its event loop alive and hangs the helper process
    // after it has already printed its success line.
    if (frame.opcode === 0x8) {
      socket.write(Buffer.from([0x88, 0x00]));
      socket.end();
      return;
    }
    if (frame.opcode !== 0x1) return;
    let message;
    try { message = JSON.parse(frame.text); } catch { return; }
    if (message.method !== 'Runtime.evaluate') return;
    const expression = String((message.params && message.params.expression) || '');
    let value = null;
    if (expression.includes('chrome.runtime.reload')) {
      // 'stuck' keeps the old version: the reload is acknowledged but does not
      // take effect, which is the case the version check has to catch.
      if (mode === 'ok') running = expectedVersion;
      value = 'reload-sent';
    } else if (expression.includes('getManifest')) {
      value = JSON.stringify({ name: '__MSG_extName__', dn: 'Chat Stasher', version: running });
    }
    socket.write(encodeText(JSON.stringify({ id: message.id, result: { result: { type: 'string', value } } })));
  });
});

server.listen(0, '127.0.0.1', () => {
  const port = server.address().port;
  // The caller reads the port from a file, so it never parses a log line.
  if (portFile) fs.writeFileSync(portFile, String(port));
  console.log(`test-cdp-mock: listening on 127.0.0.1:${port} (mode ${mode})`);
});
EOF

  # start_mock <mode> <old-version> <expected-version> — starts the mock in THIS
  # shell and prints nothing. The port is read afterwards with mock_port, never
  # returned through a command substitution: `$(start_mock …)` runs the function
  # in a subshell, so a pid it set there is invisible to the parent that has to
  # kill it, and every mock this test started outlived the run because of it.
  # The pid goes to MOCK_PIDS_FILE, which start_mock writes and cleanup reads.
  start_mock() {
    rm -f "$SCRATCH/cdp-port"
    node "$SCRATCH/cdp-mock.mjs" --mode "$1" --old-version "$2" \
      --expected-version "$3" --port-file "$SCRATCH/cdp-port" >"$SCRATCH/mock.log" 2>&1 &
    mock_pid="$!"
    echo "$mock_pid" >> "$MOCK_PIDS_FILE"
    # Two ways to stop waiting, and the budget is neither of them. A mock that
    # has died cannot come back, so its exit ends the wait at once instead of
    # spending the whole budget discovering it; a mock that is merely slow gets
    # the budget, which is generous because a loaded CI runner can take several
    # seconds to start node at all. The 5s this replaced was sized for an idle
    # developer machine and expired on the runner, and because both causes
    # reported the same one-line message it was impossible to tell a slow start
    # from a broken mock.
    for _ in $(seq 1 "$MOCK_START_TRIES"); do
      [ -s "$SCRATCH/cdp-port" ] && break
      kill -0 "$mock_pid" 2>/dev/null || break
      sleep 0.05
    done
    if [ ! -s "$SCRATCH/cdp-port" ]; then
      # The mock's own output, because "did not report a port" on its own says
      # nothing about which of the two causes above it was.
      echo "--- the CDP mock's output ---" >&2
      cat "$SCRATCH/mock.log" >&2 || true
      echo "------------------------------" >&2
      if kill -0 "$mock_pid" 2>/dev/null; then
        fail "the CDP mock did not report a port within $((MOCK_START_TRIES / 20))s (still running)"
      else
        fail "the CDP mock exited before reporting a port"
      fi
    fi
  }
  mock_port() { cat "$SCRATCH/cdp-port"; }
  stop_mock() {
    mock_pid="$(tail -n 1 "$MOCK_PIDS_FILE")"
    kill "$mock_pid" 2>/dev/null || true
    wait "$mock_pid" 2>/dev/null || true
  }

  # 21. --cdp-port reloads the worker and verifies the version. The mock starts
  #     on 0.1.0.89 and moves to the expected version only when it sees the
  #     reload call, so a pass means the call really arrived.
  start_mock ok 0.1.0.89 0.1.0.90
  port="$(mock_port)"
  if ! bash "$RELOAD" --load-dir "$LOAD" --build-number 90 --cdp-port "$port" >"$SCRATCH/o16" 2>&1; then
    cat "$SCRATCH/o16" >&2
    fail "--cdp-port should reload and verify"
  fi
  grep -q "reloaded and verified version 0.1.0.90" "$SCRATCH/o16" || fail "the verification should be reported"
  grep -q "running version is 0.1.0.90" "$SCRATCH/o16" || fail "the script should report the reloaded version"
  grep -q "reload the platform tabs" "$SCRATCH/o16" || fail "reloading the tabs is still manual and should be said"
  [ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.90" ] || fail "the swap should have happened"
  stop_mock
  note "--cdp-port reloads the worker and verifies the new version"

  # 22. a reachable CDP port with no chat-stasher worker fails, and the swap
  #     that already happened stays (it is not rolled back by a CDP failure).
  start_mock notfound 0.1.0.90 0.1.0.91
  port="$(mock_port)"
  if CS_CDP_TIMEOUT_MS=800 bash "$RELOAD" --load-dir "$LOAD" --build-number 91 --cdp-port "$port" >"$SCRATCH/o17" 2>&1; then
    fail "no matching worker should exit non-zero"
  fi
  grep -q "no chat-stasher service worker found" "$SCRATCH/o17" || fail "the failure should name the missing worker"
  grep -q "remaining manual step" "$SCRATCH/o17" || fail "the manual fallback should be printed"
  [ "$(manifest_version "$LOAD/manifest.json")" = "0.1.0.91" ] || fail "the swap must not be rolled back on a CDP failure"
  stop_mock
  note "a CDP port with no matching worker fails and prints the manual step"

  # 23. the reload call can be sent while the worker never comes back on the
  #     new version. The helper must wait out its budget and fail, otherwise the
  #     version check would be decoration. CS_CDP_TIMEOUT_MS (test-only, see the
  #     helper header) keeps this under a second.
  start_mock stuck 0.1.0.91 0.1.0.92
  port="$(mock_port)"
  if CS_CDP_TIMEOUT_MS=800 bash "$RELOAD" --load-dir "$LOAD" --build-number 92 --cdp-port "$port" >"$SCRATCH/o18" 2>&1; then
    fail "a worker that never reaches the new version should exit non-zero"
  fi
  grep -q "chrome.runtime.reload() sent" "$SCRATCH/o18" || fail "the reload should still have been attempted"
  grep -q "did not come back on 0.1.0.92" "$SCRATCH/o18" || fail "the version mismatch should be reported"
  stop_mock
  note "a worker stuck on the old version fails verification"

  # 24. an unreachable CDP port fails fast with the transport error. Port 1 is
  #     privileged, so nothing can be listening on it.
  if bash "$RELOAD" --load-dir "$LOAD" --build-number 93 --cdp-port 1 >"$SCRATCH/o19" 2>&1; then
    fail "an unreachable CDP port should exit non-zero"
  fi
  grep -q "no chat-stasher service worker found" "$SCRATCH/o19" || fail "the unreachable port should be reported"
  note "an unreachable CDP port fails and prints the manual step"

  # 25. --cdp-port is validated before any build: a non-port is a usage error.
  if bash "$RELOAD" --load-dir "$LOAD" --cdp-port not-a-port >"$SCRATCH/o20" 2>&1; then
    fail "a bad --cdp-port should fail"
  else
    rc=$?
    [ "$rc" = "2" ] || fail "a bad --cdp-port should exit 2 (usage), got $rc"
  fi
  grep -q "cdp-port must be a TCP port number" "$SCRATCH/o20" || fail "the usage error should name --cdp-port"
  note "--cdp-port is validated up front as a usage error"

  # 26. a dry run with --cdp-port plans the reload and touches nothing.
  if ! bash "$RELOAD" --load-dir "$LOAD" --cdp-port "$port" --dry-run >"$SCRATCH/o21" 2>&1; then
    fail "a dry run with --cdp-port should exit 0"
  fi
  grep -q "reload over CDP on 127.0.0.1:" "$SCRATCH/o21" || fail "the dry run should plan the CDP reload"
  note "a dry run with --cdp-port plans the reload"
fi

# The run must not leave a mock behind. A leaked mock keeps listening on a port
# and keeps a node process alive after the suite has already reported success,
# which is how one run at a time accumulated hundreds of orphans. This is
# asserted rather than assumed: the traps are the mechanism, and this is the
# check that they worked, so an edit that drops one fails here loudly instead of
# leaking quietly. It runs before the EXIT trap, so a mock still alive now is one
# the traps did not account for.
leftover=""
if command -v pgrep >/dev/null 2>&1; then
  # Matching on the scratch path is what catches a mock whose pid was never
  # recorded — the exact shape of the bug this guards — which checking the
  # recorded pids alone would miss. The pattern is not `-c`: macOS pgrep has no
  # count flag, and a `-c` here would fail and read as "nothing left behind".
  leftover="$(pgrep -f "$SCRATCH/cdp-mock.mjs" 2>/dev/null || true)"
else
  # Without pgrep the sweep cannot run, so fall back to the recorded pids and
  # say so: a check that could not run must not look like one that passed.
  echo "note: pgrep not found; the leftover check examined only this run's recorded pids"
  while read -r pid; do
    [ -n "$pid" ] || continue
    if kill -0 "$pid" 2>/dev/null; then leftover="$leftover $pid"; fi
  done < "$MOCK_PIDS_FILE"
fi
if [ -n "${leftover// /}" ]; then
  echo "FAIL: this run left cdp-mock processes running (pids: $(printf '%s' "$leftover" | tr '\n' ' '))" >&2
  exit 1
fi

echo
echo "test-reload-extension: ${PASS} cases passed"