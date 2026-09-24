#!/usr/bin/env bash
# reload-extension.sh — build the browser extension from a git ref and swap it
# into the directory Chrome loads unpacked from.
#
# Why it exists: real-browser acceptance runs against an unpacked build, and
# Chrome only re-reads a manifest when the version changes. The reload cycle is
# therefore: build a fresh extension from committed source, bump the 4th
# version component, and drop it into the load directory. Chrome then keeps
# running the old service worker until the extension is reloaded, and a browser
# restart does not fix that: it re-reads the manifest but can leave the old
# worker running. With --cdp-port the script reloads the worker itself over the
# DevTools Protocol and checks the version it comes back on, which removes the
# manual toggle; without it, it prints the toggle as before. Reloading the
# platform tabs stays manual in both cases.
#
# It builds from a throwaway git worktree of the ref so uncommitted edits in
# the current checkout can never leak into the build.
#
# The swap is two renames, not one atomic step: the build is staged in a
# sibling temp directory, the previous build is renamed aside to
# <load-dir>.prev, and then the staged directory is renamed into place. Each
# rename is atomic, so a half-copied tree is never visible under <load-dir>;
# the pair is not, so between the two renames <load-dir> does not exist. That
# window is not left for the operator to find: if the second rename fails the
# script renames .prev back and exits non-zero, and if the run is interrupted
# inside the window (or the rename back fails too) the next run refuses to
# touch <load-dir> and asks for --recover, which puts .prev back.
#
# A --load-dir that is a symlink is renamed as a link: the link is swapped, and
# the directory it pointed at is left where it is.
#
# >>> usage
# Usage:
#   reload-extension.sh --load-dir <dir> [--ref <git-ref>] [--build-number <n>]
#                        [--cdp-port <port>] [--init] [--recover] [--dry-run]
#
#   --load-dir <dir>    Directory Chrome loads unpacked from (required).
#   --ref <ref>         Git ref to build (default: HEAD).
#   --build-number <n>  Build number for the 4th version component. Defaults to
#                       the previous load-dir build's 4th component + 1, or 1.
#   --cdp-port <port>   After the swap, reload the running extension over the
#                       DevTools Protocol on 127.0.0.1:<port> (the port given to
#                       Chrome's --remote-debugging-port) and fail unless the
#                       worker comes back on the version just built. Off by
#                       default, in which case the manual toggle is printed.
#                       Requires node on PATH.
#   --init              Allow a load dir that is absent or empty, so a first
#                       build can seed it. A non-empty directory that is not a
#                       chat-stasher build is refused even with --init: --init
#                       on, say, a projects directory would otherwise rename
#                       that whole directory to <dir>.prev and put a build in
#                       its place.
#   --recover           Restore <load-dir>.prev after a run that was interrupted
#                       between the two renames (the load dir is missing while
#                       .prev exists), then reload.
#   --dry-run           Print the plan and change nothing.
# <<< usage
#
# Tests only: when CS_RELOAD_BUILD_CMD is set it is used instead of the real
# pnpm build. It is invoked as "$CS_RELOAD_BUILD_CMD" <extension-dir> <n> and
# must leave a built extension (including manifest.json) under
# <extension-dir>/.output/chrome-mv3/. When CS_RELOAD_TEST_FAIL_SWAP=1 the
# rename of the staged build into place is forced to fail, so the recovery path
# can be exercised without a real filesystem failure. Both exist so that the
# bash test needs no toolchain.

set -euo pipefail

# Tests only (see the header). Read once here so the swap below stays plain.
CS_RELOAD_TEST_FAIL_SWAP="${CS_RELOAD_TEST_FAIL_SWAP:-0}"

# The help text is the block between the two markers in the header above: one
# copy, printed by --help, so --help cannot drift from what the script
# documents. A hardcoded line range here would rot the next time the header
# gains a line.
usage() {
  sed -n '/^# >>> usage$/,/^# <<< usage$/p' "$0" \
    | sed -e 's/^# \{0,1\}//' -e '/^>>> usage$/d' -e '/^<<< usage$/d' >&2
  exit 2
}

# True for anything that occupies the path, including a symlink whose target is
# gone (which `[ -e ]` alone reports as absent).
path_exists() { [ -e "$1" ] || [ -L "$1" ]; }

# True only for an existing directory with no entries at all. A directory that
# cannot be read returns false, because "we could not look" must not be read as
# "there is nothing there" when the answer decides whether a directory gets
# renamed away.
dir_is_empty() {
  local found
  [ -d "$1" ] || return 1
  found="$(find "$1" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" || return 1
  [ -z "$found" ]
}

REF=HEAD
LOAD_DIR=""
BUILD_NUMBER=""
CDP_PORT=""
MODE=install
INIT=0
RECOVER=0

while [ $# -gt 0 ]; do
  case "$1" in
    --load-dir)
      [ $# -ge 2 ] || usage
      LOAD_DIR="$2"; shift 2 ;;
    --ref)
      [ $# -ge 2 ] || usage
      REF="$2"; shift 2 ;;
    --build-number)
      [ $# -ge 2 ] || usage
      BUILD_NUMBER="$2"; shift 2 ;;
    --cdp-port)
      [ $# -ge 2 ] || usage
      CDP_PORT="$2"; shift 2 ;;
    --init)
      INIT=1; shift ;;
    --recover)
      RECOVER=1; shift ;;
    --dry-run)
      MODE=dry-run; shift ;;
    -h|--help)
      usage ;;
    *)
      echo "reload-extension.sh: unknown argument: $1" >&2
      usage ;;
  esac
done

[ -n "$LOAD_DIR" ] || { echo "reload-extension.sh: --load-dir is required" >&2; usage; }

# Validate an explicit build number up front so we never begin a build that is
# doomed. The same rule the manifest enforces: a non-negative base-10 integer.
if [ -n "$BUILD_NUMBER" ] && ! [[ "$BUILD_NUMBER" =~ ^(0|[1-9][0-9]*)$ ]]; then
  echo "reload-extension.sh: --build-number must be a non-negative integer, got: $BUILD_NUMBER" >&2
  exit 2
fi

# Validate the CDP port up front for the same reason: a typo must fail before a
# build, not after the load dir has already been swapped.
if [ -n "$CDP_PORT" ]; then
  if ! [[ "$CDP_PORT" =~ ^[0-9]{1,5}$ ]] || [ "$CDP_PORT" -lt 1 ] || [ "$CDP_PORT" -gt 65535 ]; then
    echo "reload-extension.sh: --cdp-port must be a TCP port number (1-65535), got: $CDP_PORT" >&2
    exit 2
  fi
fi

# --recover moves the previous build back, and --dry-run promises to change
# nothing, so the combination is refused rather than resolved one way silently.
if [ "$MODE" = dry-run ] && [ "$RECOVER" -eq 1 ]; then
  echo "reload-extension.sh: --recover changes the load dir, so it cannot be combined with --dry-run" >&2
  exit 2
fi

# --- locate the repository and the extension source -------------------------
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "reload-extension.sh: must be run inside a git checkout of chat-stasher" >&2
  exit 2
}
EXT_DIR="$REPO_ROOT/apps/extension"
[ -f "$EXT_DIR/package.json" ] || {
  echo "reload-extension.sh: no apps/extension under $REPO_ROOT — is this the chat-stasher checkout?" >&2
  exit 2
}

BASE_VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$EXT_DIR/package.json")"

# --- resolve the load dir to an absolute sibling-based path -----------------
LOAD_DIR="$(cd "$(dirname "$LOAD_DIR")" && pwd)/$(basename "$LOAD_DIR")"
LOAD_DIR_PREV="$LOAD_DIR.prev"

# --- an interrupted earlier run ---------------------------------------------
# The swap is two renames (see the header). A run that died between them left
# the load dir gone and the previous build in .prev. From the load dir alone
# that state is indistinguishable from "the user named a directory that is not
# there yet", and the two want opposite things — one must be restored, the
# other built fresh, and building fresh would also delete .prev. So it is never
# resolved silently.
if ! path_exists "$LOAD_DIR" && path_exists "$LOAD_DIR_PREV"; then
  if [ "$RECOVER" -eq 0 ]; then
    echo "reload-extension.sh: $LOAD_DIR is missing but $LOAD_DIR_PREV exists — an earlier run stopped between its two renames." >&2
    echo "  Pass --recover to move $LOAD_DIR_PREV back to $LOAD_DIR, or move it back yourself." >&2
    exit 1
  fi
  mv "$LOAD_DIR_PREV" "$LOAD_DIR"
  echo "reload-extension.sh: recovered $LOAD_DIR from $LOAD_DIR_PREV"
elif [ "$RECOVER" -eq 1 ]; then
  echo "reload-extension.sh: --recover is for an interrupted run: it needs $LOAD_DIR missing while $LOAD_DIR_PREV exists" >&2
  exit 2
fi

# --- inspect the current load dir -------------------------------------------
OLD_VERSION=""
OLD_BUILD=""
LOAD_IS_BUILD=0
if [ -f "$LOAD_DIR/manifest.json" ]; then
  if python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("name") == "__MSG_extName__" else 1)' "$LOAD_DIR/manifest.json" 2>/dev/null; then
    LOAD_IS_BUILD=1
    OLD_VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("version",""))' "$LOAD_DIR/manifest.json")"
    if [ -n "$OLD_VERSION" ]; then
      parts="$(printf '%s' "$OLD_VERSION" | awk -F. '{print NF}')"
      last="$(printf '%s' "$OLD_VERSION" | awk -F. '{print $NF}')"
      if [ "$parts" -ge 4 ] && [[ "$last" =~ ^[0-9]+$ ]]; then
        OLD_BUILD="$last"
      fi
    fi
  fi
fi

# A load dir that does not look like a previous build is refused. --init is the
# one exception, and only for a directory that is absent or empty: --init must
# never rename something the user already had there out of the way, because the
# whole directory would become the .prev of a build and the operator's own
# files would come back only by hand.
if [ "$LOAD_IS_BUILD" -eq 0 ]; then
  if [ "$INIT" -eq 0 ]; then
    echo "reload-extension.sh: $LOAD_DIR does not look like a previous chat-stasher build" >&2
    echo "  (expected a manifest.json whose name is __MSG_extName__). Pass --init to seed an absent or empty directory." >&2
    exit 1
  fi
  if path_exists "$LOAD_DIR" && ! dir_is_empty "$LOAD_DIR"; then
    echo "reload-extension.sh: refusing to --init $LOAD_DIR: it is not empty and does not look like a chat-stasher build" >&2
    echo "  point --load-dir at an empty or new directory; this script does not move its contents aside." >&2
    exit 1
  fi
fi

# --- determine the build number ---------------------------------------------
if [ -n "$BUILD_NUMBER" ]; then
  N="$BUILD_NUMBER"
elif [ -n "$OLD_BUILD" ]; then
  N="$((OLD_BUILD + 1))"
else
  N=1
fi

NEW_VERSION="$BASE_VERSION.$N"
NEW_VERSION_NAME="$BASE_VERSION+build.$N"

MANUAL_STEP="toggle the extension off and on in chrome://extensions, then reload the platform tabs"
PLAN_MSG="old -> new: ${OLD_VERSION:-(none)} -> $NEW_VERSION ($NEW_VERSION_NAME)"

if [ "$MODE" = dry-run ]; then
  echo "Dry run — no changes. Would:"
  echo "  ref:            $REF"
  echo "  build number:   $N"
  echo "  plan:           $PLAN_MSG"
  echo "  load dir:       $LOAD_DIR"
  echo "  steps:          build from a throwaway worktree of $REF, stage the"
  echo "                  output into a sibling temp dir, then swap: rename"
  echo "                  old build -> $LOAD_DIR_PREV (absent for a new load"
  echo "                  dir), then rename the temp dir -> $LOAD_DIR"
  if [ -n "$CDP_PORT" ]; then
    echo "  cdp:            reload over CDP on 127.0.0.1:$CDP_PORT and verify $NEW_VERSION"
  else
    echo "  manual step:    $MANUAL_STEP"
  fi
  exit 0
fi

# --- build from a throwaway worktree ----------------------------------------
STAGING="$(mktemp -d "$(dirname "$LOAD_DIR")/.$(basename "$LOAD_DIR").reload.XXXXXX")"
WT="$(mktemp -d "${TMPDIR:-/tmp}/chat-stasher-reload.XXXXXX")"

cleanup() {
  # Every step guarded: this trap must never change the script's exit status
  # (a failing command here under `set -e` would turn a successful reload into
  # a reported failure, which is exactly the wrong signal).
  if ! git worktree remove --force "$WT" >/dev/null 2>&1; then
    # Not swallowed: a worktree git could not delete is state the operator has
    # to clean up, and staying quiet about it is how it stays behind.
    echo "reload-extension.sh: warning: could not remove the throwaway worktree at $WT" >&2
    rm -rf "$WT" 2>/dev/null || true
    # The directory is gone now, so prune drops the entry it left in
    # `git worktree list`; otherwise the checkout would keep a worktree that
    # no longer exists.
    if ! git worktree prune >/dev/null 2>&1; then
      echo "reload-extension.sh: warning: 'git worktree prune' failed; a stale entry for $WT may remain in 'git worktree list'" >&2
    fi
  fi
  rm -rf "$WT" 2>/dev/null || true
  rm -rf "$STAGING" 2>/dev/null || true
}
trap cleanup EXIT

git worktree add --detach "$WT" "$REF" || {
  echo "reload-extension.sh: could not create a worktree at $REF" >&2
  exit 1
}

# The throwaway worktree shares only git history, not node_modules or the
# generated .wxt. Symlink whichever the current checkout has so the build runs
# offline and fast without re-running pnpm install.
CUR_NM="$EXT_DIR/node_modules"
CUR_WXT="$EXT_DIR/.wxt"
[ -d "$CUR_NM" ] && ln -s "$CUR_NM" "$WT/apps/extension/node_modules"
[ -d "$CUR_WXT" ] && ln -s "$CUR_WXT" "$WT/apps/extension/.wxt"

echo "reload-extension.sh: building $REF (build $N) -> $NEW_VERSION"

if [ -n "${CS_RELOAD_BUILD_CMD:-}" ]; then
  # Test-only stub build: the script leaves ./output/chrome-mv3 behind.
  "$CS_RELOAD_BUILD_CMD" "$WT/apps/extension" "$N"
else
  ( cd "$WT/apps/extension" && CS_BUILD_NUMBER="$N" pnpm -s build )
fi

NEW_MANIFEST="$WT/apps/extension/.output/chrome-mv3/manifest.json"
[ -f "$NEW_MANIFEST" ] || {
  echo "reload-extension.sh: build produced no manifest at $NEW_MANIFEST" >&2
  exit 1
}

# Sanity: the built manifest must carry the version we planned.
STAGED_VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("version",""))' "$NEW_MANIFEST")"
if [ "$STAGED_VERSION" != "$NEW_VERSION" ]; then
  echo "reload-extension.sh: built version is $STAGED_VERSION, expected $NEW_VERSION (build aborted; load dir untouched)" >&2
  exit 1
fi

# --- stage into a sibling temp dir, then swap -------------------------------
cp -R "$(dirname "$NEW_MANIFEST")/." "$STAGING/"

if path_exists "$LOAD_DIR"; then
  rm -rf "$LOAD_DIR_PREV"
  mv "$LOAD_DIR" "$LOAD_DIR_PREV"
fi

# Two renames, so the load dir is briefly absent between them (see the header).
# If the second one fails the previous build goes straight back, so a failed
# run never leaves the operator pointing Chrome at a directory that is not
# there. Either way the status is non-zero: the reload did not happen.
SWAP_FAILED=0
if [ "$CS_RELOAD_TEST_FAIL_SWAP" = "1" ]; then
  echo "reload-extension.sh: (test hook CS_RELOAD_TEST_FAIL_SWAP) failing the swap rename" >&2
  SWAP_FAILED=1
elif ! mv "$STAGING" "$LOAD_DIR"; then
  SWAP_FAILED=1
fi

if [ "$SWAP_FAILED" -eq 1 ]; then
  echo "reload-extension.sh: could not move the staged build into $LOAD_DIR" >&2
  if ! path_exists "$LOAD_DIR_PREV"; then
    echo "reload-extension.sh: there was no previous build to restore; $LOAD_DIR did not exist before this run" >&2
  elif mv "$LOAD_DIR_PREV" "$LOAD_DIR" 2>/dev/null; then
    echo "reload-extension.sh: restored the previous build to $LOAD_DIR; nothing was reloaded" >&2
  else
    echo "reload-extension.sh: the previous build is still at $LOAD_DIR_PREV and $LOAD_DIR is missing — re-run with --recover" >&2
  fi
  exit 1
fi

# --- done -------------------------------------------------------------------
# The throwaway worktree is removed by the EXIT trap (works for the failure
# path too); this body only reflects on what it just did.
echo "reload-extension.sh: $PLAN_MSG"
echo "reload-extension.sh: previous build kept at $LOAD_DIR_PREV"

if [ -z "$CDP_PORT" ]; then
  echo "reload-extension.sh: remaining manual step: $MANUAL_STEP"
  echo "reload-extension.sh: done."
  exit 0
fi

# --- optional CDP reload ----------------------------------------------------
# The swap is already committed at this point, so a CDP failure is not rolled
# back: the new build is on disk and Chrome will pick it up on the next reload
# however that happens. But the operator asked for the reload to be done, so a
# failure exits non-zero and still prints the manual step as the fallback.
# The helper sits next to this script, not at the root of the checkout being
# built: the script may be run from any checkout, and in tests the "repo" is a
# throwaway whose only content is the fixture.
CDP_HELPER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/cdp-reload-extension.mjs"
if ! command -v node >/dev/null 2>&1; then
  echo "reload-extension.sh: --cdp-port needs node on PATH, and none was found" >&2
  echo "reload-extension.sh: remaining manual step: $MANUAL_STEP" >&2
  exit 1
fi
if ! node "$CDP_HELPER" --port "$CDP_PORT" --expected-version "$NEW_VERSION"; then
  echo "reload-extension.sh: the CDP reload did not complete (the file swap is already done)" >&2
  echo "reload-extension.sh: remaining manual step: $MANUAL_STEP" >&2
  exit 1
fi
echo "reload-extension.sh: extension reloaded over CDP; running version is $NEW_VERSION"
echo "reload-extension.sh: remaining manual step: reload the platform tabs"
echo "reload-extension.sh: done."
exit 0
