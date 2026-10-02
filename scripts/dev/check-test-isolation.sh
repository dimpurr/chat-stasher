#!/usr/bin/env bash
# Fail a test run that touches this machine's real chat-stasher user data
# (W306).
#
#     bash scripts/dev/check-test-isolation.sh -- cargo test
#
# runs <command> (default: `cargo test -p chat-stasher`) and exits non-zero if
# the run created, removed, renamed or modified anything under the machine's
# real
#
#   * data root     `$XDG_DATA_HOME/chat-stasher` else `~/.local/share/chat-stasher`
#   * config root   `$XDG_CONFIG_HOME/chat-stasher` else `~/.config/chat-stasher`
#   * state home    `$XDG_STATE_HOME` else `~/.local/state`
#   * inbox         `~/Downloads/chat-stasher/inbox`
#   * the native-messaging manifest directories
#                   `~/Library/Application Support/*/NativeMessagingHosts` on
#                   macOS, `$HOME/.config/*/NativeMessagingHosts` and
#                   `$HOME/.mozilla/native-messaging-hosts` on Linux,
#                   `%LOCALAPPDATA%\chat-stasher\NativeMessagingHosts` on Windows
#
# This is the W289 cache guard's sibling, and it exists for the same reason.
# W289 pinned the *rustic metadata cache*; the run that wrote into this
# machine's real data on 2026-10-02 proved the pin was one directory too narrow.
# A test resolved the real `$XDG_DATA_HOME/chat-stasher` root and planted
# `stage/sessions/<machine>/chatgpt.synthetic-session` and a `synthetic-install`
# row in `state/extension-coordination.sqlite3`. Those are the author's real
# archive and coordination state. The environment fix (W306) points every test
# at a temp root, and `src/test_identity_guard.rs` refuses a fixture identity
# aimed outside temp; *this* guard is the check that a leak is red on the run
# that introduces it, on every platform, rather than discovered later.
#
# What it watches, and why each root is the product's own spelling:
#
#   * data/config/state/inbox are the exact fallbacks `config::config_path`,
#     `config::default_data_root`, `collect::default_state_dir` and the inbox
#     argument document.
#   * the manifest directories are read from the filesystem, not hard-coded per
#     browser: the set that exists now is snapshotted, so a test that writes a
#     manifest into a browser it found is caught, and a browser directory that
#     *appears* during the run is a new root and therefore a diff.
#
# Three changes are each a failure: entries that appeared, entries that
# vanished (a test that deletes from the real data is worse than one that
# writes to it), and a change that left the entry set identical — a
# create-then-delete, caught with a run-boundary marker and `find -newer`, at
# whatever precision the filesystem keeps (the whole-second comparison the
# pre-W289 guard used false-passed exactly that case).
#
# A root that cannot be named is a refusal (exit 1), never a skip: a guard that
# cannot see is red, not absent.
#
# The recursive walk over the data and config roots is the expensive part; on a
# machine with a large archive it is bounded by the archive's own size, and the
# two snapshots are the price of the guarantee. `~/.local/state` is watched one
# level deep only: the product never writes its state there (its state lives
# under the data root), so a deep change is not a leak this guard is built to
# catch, while the shallow listing still catches a test creating or removing a
# top-level entry there.
#
# Exit codes: 0 = the run left every watched root untouched; 1 = it did not (or
# the snapshot itself failed — a guard that errors is red, never absent);
# 2 = usage. The wrapped command's own exit code is reported as part of the
# verdict and does not bypass it: a green suite that dirtied the real data is a
# red guard run.

set -uo pipefail

TAG="[test-isolation]"

usage() {
  echo "Usage: bash scripts/dev/check-test-isolation.sh [-- <command...>]" >&2
  echo "Runs <command> (default: cargo test -p chat-stasher) and fails when it" >&2
  echo "touches this machine's real chat-stasher data, config, state, inbox or" >&2
  echo "native-messaging manifest directories." >&2
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

to_unix_path() {
  local converted=""
  if command -v cygpath >/dev/null 2>&1 &&
    converted=$(cygpath -u "$1" 2>/dev/null) && [ -n "$converted" ]; then
    printf '%s' "$converted"
  else
    printf '%s' "$1" | tr '\\' '/'
  fi
}

SNAP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cs-test-isolation.XXXXXX") || exit 1
cleanup() { rm -rf "$SNAP_DIR"; }
trap cleanup EXIT

DATA_ROOT=""
CONFIG_ROOT=""
STATE_HOME=""
INBOX_ROOT=""
LOCALAPPDATA_UNIX=""

case "$PLATFORM" in
  macos)
    [ -n "${HOME:-}" ] || refuse "no \$HOME, so the real data root cannot be named"
    if [ -n "${XDG_DATA_HOME:-}" ] && [ "${XDG_DATA_HOME#\/}" != "$XDG_DATA_HOME" ]; then
      DATA_ROOT="$XDG_DATA_HOME/chat-stasher"
    else
      DATA_ROOT="$HOME/.local/share/chat-stasher"
    fi
    if [ -n "${XDG_CONFIG_HOME:-}" ] && [ "${XDG_CONFIG_HOME#\/}" != "$XDG_CONFIG_HOME" ]; then
      CONFIG_ROOT="$XDG_CONFIG_HOME/chat-stasher"
    else
      CONFIG_ROOT="$HOME/.config/chat-stasher"
    fi
    STATE_HOME="${XDG_STATE_HOME:-$HOME/.local/state}"
    INBOX_ROOT="$HOME/Downloads/chat-stasher/inbox"
    ;;
  linux)
    [ -n "${HOME:-}" ] || refuse "no \$HOME, so the real data root cannot be named"
    if [ -n "${XDG_DATA_HOME:-}" ] && [ "${XDG_DATA_HOME#\/}" != "$XDG_DATA_HOME" ]; then
      DATA_ROOT="$XDG_DATA_HOME/chat-stasher"
    else
      DATA_ROOT="$HOME/.local/share/chat-stasher"
    fi
    if [ -n "${XDG_CONFIG_HOME:-}" ] && [ "${XDG_CONFIG_HOME#\/}" != "$XDG_CONFIG_HOME" ]; then
      CONFIG_ROOT="$XDG_CONFIG_HOME/chat-stasher"
    else
      CONFIG_ROOT="$HOME/.config/chat-stasher"
    fi
    STATE_HOME="${XDG_STATE_HOME:-$HOME/.local/state}"
    INBOX_ROOT="$HOME/Downloads/chat-stasher/inbox"
    ;;
  windows)
    if [ -n "${LOCALAPPDATA:-}" ]; then
      LOCALAPPDATA_UNIX="$(to_unix_path "$LOCALAPPDATA")"
      DATA_ROOT="$LOCALAPPDATA_UNIX/chat-stasher"
      CONFIG_ROOT="$LOCALAPPDATA_UNIX/chat-stasher"
    fi
    if [ -n "${USERPROFILE:-}" ]; then
      local_profile="$(to_unix_path "$USERPROFILE")"
      [ -n "$DATA_ROOT" ] || DATA_ROOT="$local_profile/AppData/Local/chat-stasher"
      [ -n "$CONFIG_ROOT" ] || CONFIG_ROOT="$local_profile/AppData/Local/chat-stasher"
      STATE_HOME="$local_profile/AppData/Local"
      INBOX_ROOT="$USERPROFILE/Downloads/chat-stasher/inbox"
    fi
    [ -n "$DATA_ROOT" ] || refuse \
      "neither %LOCALAPPDATA% nor %USERPROFILE% is set, so the real data root" \
      "cannot be named; the run would be unguarded"
    ;;
esac

# Every root the guard watches, one per line: the four fixed roots plus every
# native-messaging manifest directory that currently exists. Re-derived before
# and after the run, so a manifest directory that appears or disappears is part
# of the diff rather than invisible to it.
collect_roots() {
  printf '%s\n' "$DATA_ROOT" "$CONFIG_ROOT" "$STATE_HOME" "$INBOX_ROOT"
  # `find -name`, not a `*/NativeMessagingHosts` glob: the Chrome path is
  # `Google/Chrome/NativeMessagingHosts` (and Arc's is `Arc/User Data/…`), two
  # components deep, and `*` does not cross `/` — the glob silently missed the
  # one directory the incident's own browser uses. Depth 4 covers every layout
  # in `nativehost.rs`'s table.
  case "$PLATFORM" in
    macos)
      find "$HOME/Library/Application Support" -maxdepth 4 -type d \
        -name NativeMessagingHosts 2>/dev/null
      ;;
    linux)
      find "$HOME/.config" -maxdepth 3 -type d -name NativeMessagingHosts 2>/dev/null
      [ -d "$HOME/.mozilla/native-messaging-hosts" ] &&
        printf '%s\n' "$HOME/.mozilla/native-messaging-hosts"
      ;;
    windows)
      [ -n "$LOCALAPPDATA_UNIX" ] &&
        find "$LOCALAPPDATA_UNIX" -maxdepth 3 -type d -name NativeMessagingHosts 2>/dev/null
      ;;
  esac
}

# Snapshot every watched root, recursively, keyed by path so a changed root is
# a readable diff. `state` is the shallow exception documented above. An absent
# root is recorded as `ABSENT <path>` so a run that creates it is a diff, not a
# surprise.
snapshot_all() {
  # $1 = output file
  : >"$1"
  collect_roots | LC_ALL=C sort -u >"$SNAP_DIR/roots"
  while IFS= read -r root; do
    [ -n "$root" ] || continue
    if [ ! -e "$root" ]; then
      printf 'ABSENT %s\n' "$root" >>"$1"
      continue
    fi
    depth=()
    if [ "$root" = "$STATE_HOME" ]; then
      depth=(-maxdepth 1)
    fi
    # `${depth[@]+…}` is the empty-array-safe expansion: macOS ships bash 3.2,
    # where a bare `"${depth[@]}"` on an empty array is an unbound variable
    # under `set -u` and would abort the walk.
    if ! find "$root" ${depth[@]+"${depth[@]}"} -print0 2>"$SNAP_DIR/find.err" |
      LC_ALL=C sort -z >"$SNAP_DIR/names.raw"; then
      echo "$TAG refusing: could not list $root" >&2
      cat "$SNAP_DIR/find.err" >&2
      exit 1
    fi
    while IFS= read -r -d '' entry; do
      # inode, mtime, size — a rename or an in-place rewrite is a diff even
      # when the name set is unchanged.
      stat -c '%i %Y %s' "$entry" 2>/dev/null ||
        stat -f '%i %m %z' "$entry" 2>/dev/null ||
        printf 'stat-unavailable'
      printf ' %s\n' "$entry"
    done <"$SNAP_DIR/names.raw" >>"$1"
  done <"$SNAP_DIR/roots"
  LC_ALL=C sort -o "$1" "$1"
}

snapshot_all "$SNAP_DIR/before"

# The run boundary: everything the run does happens with a timestamp after
# this, so `find -newer` on a root's own directory catches a create-then-delete
# whose entry names cancel out.
MARKER="$SNAP_DIR/boundary"
: >"$MARKER"

started=$(date +%s)
"${COMMAND[@]}"
cmd_status=$?
elapsed=$(( $(date +%s) - started ))

snapshot_all "$SNAP_DIR/after"

failed=0
if ! diff -q "$SNAP_DIR/before" "$SNAP_DIR/after" >/dev/null; then
  failed=1
  echo "$TAG FAIL: this run changed a real chat-stasher user-data location" >&2
  diff "$SNAP_DIR/before" "$SNAP_DIR/after" 2>&1 | head -60 || true
fi

# Unchanged entry set, but a watched directory was touched during the run: a
# create-then-delete. Reported only when the snapshot diff was clean, so one
# change is never announced twice. The state home is excluded: the run only
# reads it, and reading does not change its own mtime.
if [ "$failed" -eq 0 ]; then
  collect_roots | LC_ALL=C sort -u >"$SNAP_DIR/roots.after"
  while IFS= read -r root; do
    [ -n "$root" ] || continue
    [ -e "$root" ] || continue
    [ "$root" = "$STATE_HOME" ] && continue
    depth=()
    if [ "$root" = "$STATE_HOME" ]; then
      depth=(-maxdepth 1)
    fi
    # Whole-tree, not `-maxdepth 0`: the create-then-delete can happen in a
    # subdirectory (`stage/<session>`), and only the subdirectory's mtime moves,
    # which a check of the root's own timestamp would miss.
    if [ -n "$(find "$root" ${depth[@]+"${depth[@]}"} -newer "$MARKER" -print -quit 2>/dev/null)" ]; then
      echo "$TAG FAIL: this run modified $root during the run (an entry is newer" \
        "than the run boundary) while leaving the entry set unchanged —" \
        "a create-then-delete the snapshot diff cannot see" >&2
      failed=1
    fi
  done <"$SNAP_DIR/roots.after"
fi

if [ "$failed" -ne 0 ]; then
  echo "$TAG FAIL: exit code of the wrapped command was $cmd_status — the guard" \
    "fails on the change regardless. A test resolved a real user-data location;" \
    "give it a per-test temp root through tests' shared fixture (W306)." >&2
  exit 1
fi

if [ "$cmd_status" -ne 0 ]; then
  echo "$TAG FAIL: wrapped command exited $cmd_status (real user data untouched)"
  exit 1
fi

echo "$TAG PASS: ${COMMAND[*]} left the real data, config, state, inbox and" \
  "native-messaging manifest directories untouched (runtime=${elapsed}s)"
