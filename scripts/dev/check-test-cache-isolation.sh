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
#
# the same spellings `scanner::user_cache_dirs_on` writes down, which is the
# same function `store::rustic_cache_roots` uses to say where the cache
# lives — so the guard watches what the product would walk, not a second
# spelling that could drift.
#
# Three changes are each a failure: entries that appeared, entries that
# vanished (a test that deletes from the real cache is worse than one that
# writes to it), and — the subtle one — an unchanged entry set with a
# changed directory mtime, which is what a create-then-remove leaves behind.
# The last one is why the mtime is compared at all: the pre-W289 tests
# created their cache directory and then deleted it in the same run, so a
# plain name diff would have called that run clean.
#
# The guard cannot see a run that only writes files *inside* a per-repo
# directory that already existed before the run. That needs the same
# repository to be opened twice, once before and once during the run; test
# repositories are created with a fresh random repository id per run, so no
# test can have a pre-existing directory here. Stated, not hidden.
#
# Windows (stated gap, not a silent skip): `dirs::cache_dir()` on Windows
# comes from the Known Folder API (`SHGetKnownFolderPath`), which reads the
# user profile, not the environment — a shell script cannot name that
# directory portably and cannot redirect it either. On a Windows host the
# run is therefore NOT guarded: the script says so in one loud line and then
# runs the command unguarded, with its true exit code. The CI Windows cell
# inherits that same one-line statement. A real Windows guard needs a
# product-level cache-dir override (a config knob or argument every spawned
# test can be given), which is why the in-process and config-knob half of
# W289 was applied cross-platform while the environment half could not be.
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
  Darwin*) ;;
  Linux*) ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "$TAG NOT GUARDED: Windows dir names come from the Known Folder API," \
      "which a shell cannot redirect or portably read; see the header of this" \
      "script for what that covers and what the fix would be. Running unguarded."
    "${COMMAND[@]}"
    exit $?
    ;;
  *)
    echo "$TAG refusing: unsupported platform $(uname -s)" >&2
    exit 1
    ;;
esac

if [ -z "${HOME:-}" ]; then
  echo "$TAG refusing: no \$HOME, so the real cache root cannot be named" >&2
  exit 1
fi

# The cache root this platform really uses, mirroring `user_cache_dirs_on`:
# Linux reads $XDG_CACHE_HOME first (only when absolute — `dirs` ignores a
# relative value), every other *nix pins $HOME/Library/Caches. The watched
# directory is `<root>/rustic`, i.e. `store::rustic_cache_roots()`'s first
# entry on this platform.
case "$(uname -s)" in
  Darwin*) ROOT=$HOME/Library/Caches ;;
  *)
    if [ -n "${XDG_CACHE_HOME:-}" ] && [ "${XDG_CACHE_HOME#\/}" != "$XDG_CACHE_HOME" ]; then
      ROOT=$XDG_CACHE_HOME
    else
      ROOT=$HOME/.cache
    fi
    ;;
esac
WATCH="$ROOT/rustic"

# Snapshot: sorted entry names plus the directory's own (mtime, size). Both
# halves of the pre/post comparison are textual, taken with the same tool on
# the same machine, so no span of formats is assumed — only stability.
SNAP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cs-cache-isolation.XXXXXX") || exit 1
cleanup() { rm -rf "$SNAP_DIR"; }
trap cleanup EXIT

snapshot() {
  # $1: output file. Absent WATCH directory is recorded as the literal
  # "ABSENT" line so a run that creates the root is a diff, not a surprise.
  : >"$1"
  if [ -e "$WATCH" ]; then
    # -print0/sort -z keep names with spaces intact, and the sort is not
    # cosmetic: readdir order is not guaranteed to match between two
    # snapshots, and a reorder would read as a spurious diff on a directory
    # with tens of thousands of entries. find's -mindepth/-maxdepth are
    # BSD-find compatible for the macOS half. A find that fails is a guard
    # that cannot see, which is a red guard, never a green one.
    if ! find "$WATCH" -mindepth 1 -maxdepth 1 -print0 2>"$SNAP_DIR/find.err" |
      LC_ALL=C sort -z >"$SNAP_DIR/names.raw"; then
      echo "$TAG refusing: could not list $WATCH" >&2
      cat "$SNAP_DIR/find.err" >&2
      exit 1
    fi
    while IFS= read -r -d '' entry; do
      printf '%s\n' "entry ${entry#"$WATCH"/}"
    done <"$SNAP_DIR/names.raw" >>"$1"
    if ! stat_meta >>"$1" 2>"$SNAP_DIR/stat.err" || [ -s "$SNAP_DIR/stat.err" ]; then
      echo "$TAG refusing: could not stat $WATCH" >&2
      cat "$SNAP_DIR/stat.err" >&2
      exit 1
    fi
  else
    printf 'ABSENT\n' >>"$1"
  fi
}

# mtime and size of the watched directory in one line. Deliberately not
# normalised across `stat` dialects: both snapshots are taken by the same
# call on the same machine, so only stability matters, and probing for a
# "portable" format (the trap release-gate.sh once fell into with `stat -f
# %z`) would add a second way to be wrong.
stat_meta() {
  stat -f 'meta mtime=%m size=%z' "$WATCH" 2>/dev/null ||
    stat -c 'meta mtime=%Y size=%s' "$WATCH" 2>/dev/null
}

before=$SNAP_DIR/before
after=$SNAP_DIR/after
snapshot "$before"

# The run itself. Everything is timed so the report can say what the guard
# cost next to it — on a machine whose cache root still holds tens of
# thousands of test leftovers, each snapshot is another full directory walk
# (see scripts/dev/prune-test-rustic-cache.sh for reclaiming them; with the
# leftovers gone the snapshot is instant).
started=$(date +%s)
"${COMMAND[@]}"
cmd_status=$?
elapsed=$(( $(date +%s) - started ))

snapshot "$after"

if ! diff -q "$before" "$after" >/dev/null; then
  echo "$TAG FAIL: this run changed the real user cache root $WATCH" 1>&2
  diff "$before" "$after" 1>&2 | head -40 || true
  echo "$TAG FAIL: exit code of the wrapped command was $cmd_status — the guard" \
    "fails on the cache change regardless; fix the test that opened a repository" \
    "without an isolated cache_dir (W289)" 1>&2
  exit 1
fi

if [ "$cmd_status" -ne 0 ]; then
  echo "$TAG FAIL: wrapped command exited $cmd_status (real cache root untouched)"
  exit 1
fi

echo "$TAG PASS: ${COMMAND[*]} left the real user cache root untouched (root=$WATCH, wrapped runtime=${elapsed}s)"
