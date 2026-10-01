#!/usr/bin/env bash
# Fail a test run that creates anything in the real user cache directory
# (W289).
#
#     bash scripts/dev/check-test-cache-isolation.sh -- cargo test
#
# snaps the per-repository entries of this machine's real rustic cache root
# before and after the command it is given (default: `cargo test -p
# chat-stasher`), and exits non-zero when the run created, removed or renamed
# anything there. CI's Test step runs through this wrapper, so a test that
# starts opening repositories without an isolated `cache_dir` again goes red
# in the run itself, not months later when someone looks at the disk.
#
# The history it exists for: every repository open with default options makes
# rustic create a per-repository metadata cache directory under
# `dirs::cache_dir()/rustic` — the *user's* cache directory, on the machine
# running the tests. The suite has opened thousands of throwaway repositories
# over its life; the bill was 36,174 directories in ~/Library/Caches/rustic
# on one machine, which the cache-walking tests then had to sweep, and which
# only `scripts/dev/prune-test-rustic-cache.sh` can safely reclaim. The fix
# (W289) was to point every test at a cache under its own temp directory.
# This guard is what makes that fix a rule rather than a one-time cleanup: a
# "cache_dir: None" that slips back into a test helper shows up here as a new
# directory in the user's cache, on the very run that introduced it.
#
# What it watches, and how it mirrors the product:
#
#   * macOS:   $HOME/Library/Caches/rustic            (dirs-6.0.0 src/mac.rs:9)
#   * Linux:   $XDG_CACHE_HOME/rustic, else ~/.cache  (dirs-6.0.0 src/lin.rs:8)
#   * Windows: %LOCALAPPDATA%\rustic, else
#              %USERPROFILE%\AppData\Local\rustic    (dirs-6.0.0 src/win.rs:10)
#
# the same spellings `scanner::user_cache_dirs_on` writes down, which is the
# same function `store::rustic_cache_roots` uses to say where the cache
# lives — so the guard watches what the product would walk, not a second
# spelling that could drift.
#
# Windows, guarded rather than skipped: `dirs::cache_dir()` there comes from
# the Known Folder API (`SHGetKnownFolderPath`), which reads the user profile
# rather than the environment, so no shell can *redirect* that root and an
# `XDG_CACHE_HOME` override is a no-op — that is why every spawned test child
# is relocated through the product's own `rustic_cache_dir` config knob
# (W289), which rustic honours on Windows because the *product* resolves the
# path, not `dirs`. The guard cannot redirect the root either, but it can
# *read* it: `%LOCALAPPDATA%` is the documented spelling of that Known Folder
# and the second candidate `user_cache_dirs_on` keeps for it, so the Windows
# cells watch `%LOCALAPPDATA%\rustic` (and `%USERPROFILE%\AppData\Local
# \rustic`) and fail a leaking run exactly as the other platforms do. When
# neither variable is set the guard refuses (exit 1) rather than running
# unguarded: a guard that cannot see is red, never absent.
#
# Three changes are each a failure: entries that appeared, entries that
# vanished (a test that deletes from the real cache is worse than one that
# writes to it), and — the subtle one — a change that left the entry set
# identical, which is what a create-then-remove leaves behind. That last one
# is caught with a run-boundary marker: the guard records the watched
# directory's own modification time against a marker file taken the instant
# before the run, compared with `find -newer`, which uses the filesystem's
# full timestamp precision and never truncates it. Comparing whole seconds was
# the earlier spelling, and it false-passed a create-then-delete that happened
# inside one second — which is exactly what a fast test does.
#
# The guard cannot see a run that only writes files *inside* a per-repo
# directory that already existed before the run. That needs the same
# repository to be opened twice, once before and once during the run; test
# repositories are created with a fresh random repository id per run, so no
# test can have a pre-existing directory here. Stated, not hidden.
#
# Exit codes: 0 = the run left the real cache untouched; 1 = it did not (or
# the snapshot itself failed — a guard that errors is red, never absent);
# 2 = usage. The wrapped command's own exit code is reported as part of the
# verdict and does not bypass it: a green suite that dirtied the cache is a
# red guard run.

set -uo pipefail

TAG="[cache-isolation]"

usage() {
  echo "Usage: bash scripts/dev/check-test-cache-isolation.sh [-- <command...>]" >&2
  echo "Runs <command> (default: cargo test -p chat-stasher) and fails when it" >&2
  echo "creates, removes or renames anything under this machine's real rustic cache root." >&2
  exit 2
}

refuse() {
  echo "$TAG refusing: $*" >&2
  exit 1
}

COMMAND=("cargo" "test" "-p" "chat-stasher")
while [ "$#" -gt 0 ]; do
  case "$1" in
    --)
      shift
      if [ "$#" -gt 0 ]; then
        COMMAND=("$@")
      fi
      break
      ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
done

case "$(uname -s)" in
  Darwin*) PLATFORM=macos ;;
  Linux*) PLATFORM=linux ;;
  MINGW*|MSYS*|CYGWIN*) PLATFORM=windows ;;
  *) refuse "unsupported platform $(uname -s)" ;;
esac

# A native Windows path (`C:\Users\x`) names nothing to the MSYS shell's
# tools; cygpath converts it when it is present. `cygpath` ships with the
# MSYS2 runtime, but the fallback keeps the guard working without it: MSYS
# tools also accept a drive-letter path with forward slashes, so `C:\Users\x`
# becomes `C:/Users/x`. On the other platforms neither variable is set.
to_unix_path() {
  local converted=""
  if command -v cygpath >/dev/null 2>&1 &&
    converted=$(cygpath -u "$1" 2>/dev/null) && [ -n "$converted" ]; then
    printf '%s' "$converted"
  else
    printf '%s' "$1" | tr '\\' '/'
  fi
}

SNAP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cs-cache-isolation.XXXXXX") || exit 1
cleanup() { rm -rf "$SNAP_DIR"; }
trap cleanup EXIT

# The rustic cache root(s) this platform really uses, mirroring
# `user_cache_dirs_on`: Linux reads $XDG_CACHE_HOME first (only when absolute
# — `dirs` ignores a relative value), macOS pins $HOME/Library/Caches, Windows
# is %LOCALAPPDATA% with %USERPROFILE%\AppData\Local as the documented second
# candidate.
ROOTS_FILE="$SNAP_DIR/roots"
: >"$ROOTS_FILE"
add_root() {
  if ! grep -qxF "$1" "$ROOTS_FILE"; then
    printf '%s\n' "$1" >>"$ROOTS_FILE"
  fi
}

case "$PLATFORM" in
  macos)
    [ -n "${HOME:-}" ] || refuse "no \$HOME, so the real cache root cannot be named"
    add_root "$HOME/Library/Caches/rustic"
    ;;
  linux)
    if [ -n "${XDG_CACHE_HOME:-}" ] && [ "${XDG_CACHE_HOME#\/}" != "$XDG_CACHE_HOME" ]; then
      add_root "$XDG_CACHE_HOME/rustic"
    elif [ -n "${HOME:-}" ]; then
      add_root "$HOME/.cache/rustic"
    else
      refuse "no absolute \$XDG_CACHE_HOME and no \$HOME, so the real cache root cannot be named"
    fi
    ;;
  windows)
    if [ -n "${LOCALAPPDATA:-}" ]; then
      add_root "$(to_unix_path "$LOCALAPPDATA")/rustic"
    fi
    if [ -n "${USERPROFILE:-}" ]; then
      add_root "$(to_unix_path "$USERPROFILE")/AppData/Local/rustic"
    fi
    [ -s "$ROOTS_FILE" ] || refuse \
      "neither %LOCALAPPDATA% nor %USERPROFILE% is set, so the Known Folder" \
      "cache root cannot be named; the run would be unguarded"
    ;;
esac

# Snapshot: the sorted entry names directly under one root. Textual, taken
# with the same tools on the same machine for both halves, so no span of
# formats is assumed — only stability.
snapshot_root() {
  # $1: root directory. $2: output file. An absent root is recorded as the
  # literal "ABSENT" line so a run that creates the root is a diff, not a
  # surprise.
  : >"$2"
  if [ -e "$1" ]; then
    # -print0/sort -z keep names with spaces intact, and the sort is not
    # cosmetic: readdir order is not guaranteed to match between two
    # snapshots, and a reorder would read as a spurious diff on a directory
    # with tens of thousands of entries. find's -mindepth/-maxdepth are
    # portable across the BSD and GNU halves. A find that fails is a guard
    # that cannot see, which is a red guard, never a green one.
    if ! find "$1" -mindepth 1 -maxdepth 1 -print0 2>"$SNAP_DIR/find.err" |
      LC_ALL=C sort -z >"$SNAP_DIR/names.raw"; then
      echo "$TAG refusing: could not list $1" >&2
      cat "$SNAP_DIR/find.err" >&2
      exit 1
    fi
    while IFS= read -r -d '' entry; do
      printf 'entry %s\n' "${entry#"$1"/}"
    done <"$SNAP_DIR/names.raw" >>"$2"
  else
    printf 'ABSENT\n' >>"$2"
  fi
}

idx=0
while IFS= read -r root; do
  snapshot_root "$root" "$SNAP_DIR/before.$idx"
  idx=$((idx + 1))
done <"$ROOTS_FILE"

# The run boundary. Everything the run does to a watched directory happens
# after this marker's timestamp, so `find -newer` on the directory itself
# catches a create-then-delete the entry names cannot. `: >` creates it with
# the current time at whatever precision the filesystem keeps.
MARKER="$SNAP_DIR/boundary"
: >"$MARKER"

# The run itself. Everything is timed so the report can say what the guard
# cost next to it — on a machine whose cache root still holds tens of
# thousands of test leftovers, each snapshot is another full directory walk
# (see scripts/dev/prune-test-rustic-cache.sh for reclaiming them; with the
# leftovers gone the snapshot is instant).
started=$(date +%s)
"${COMMAND[@]}"
cmd_status=$?
elapsed=$(( $(date +%s) - started ))

failed=0
idx=0
while IFS= read -r root; do
  after="$SNAP_DIR/after.$idx"
  before="$SNAP_DIR/before.$idx"
  snapshot_root "$root" "$after"

  set_changed=0
  if ! diff -q "$before" "$after" >/dev/null; then
    set_changed=1
    failed=1
    echo "$TAG FAIL: this run changed the real user cache root $root" >&2
    diff "$before" "$after" 2>&1 | head -40 || true
  fi

  # Unchanged entry set, but the directory itself was touched during the run:
  # a create-then-delete within the run window, which the name diff alone
  # cannot see. Reported only when the name diff was clean, so one change is
  # never announced twice.
  if [ "$set_changed" -eq 0 ] && [ -e "$root" ] &&
    [ -n "$(find "$root" -maxdepth 0 -newer "$MARKER" -print 2>/dev/null)" ]; then
    failed=1
    echo "$TAG FAIL: this run modified the real user cache root $root during the" \
      "run (its own timestamp is newer than the run boundary) while leaving the" \
      "entry set unchanged — a create-then-delete an entry-name diff cannot see" >&2
  fi
  idx=$((idx + 1))
done <"$ROOTS_FILE"

if [ "$failed" -ne 0 ]; then
  echo "$TAG FAIL: exit code of the wrapped command was $cmd_status — the guard" \
    "fails on the cache change regardless; fix the test that opened a repository" \
    "without an isolated cache_dir (W289)" >&2
  exit 1
fi

if [ "$cmd_status" -ne 0 ]; then
  echo "$TAG FAIL: wrapped command exited $cmd_status (real cache root untouched)"
  exit 1
fi

roots_shown=$(paste -sd ', ' "$ROOTS_FILE")
echo "$TAG PASS: ${COMMAND[*]} left the real user cache root untouched (root=$roots_shown, wrapped runtime=${elapsed}s)"
