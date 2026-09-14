#!/usr/bin/env bash
# reload-extension.sh — build the browser extension from a git ref and swap it
# into the directory Chrome loads unpacked from.
#
# Why it exists: real-browser acceptance runs against an unpacked build, and
# Chrome only re-reads a manifest when the version changes. The reload cycle is
# therefore: build a fresh extension from committed source, bump the 4th
# version component, and drop it into the load directory. This script automates
# every step except the two a browser offers no supported API for (toggling the
# extension off/on in chrome://extensions and reloading the platform tabs),
# which it prints so the operator knows the one remaining manual action.
#
# It builds from a throwaway git worktree of the ref so uncommitted edits in
# the current checkout can never leak into the build. The swap is atomic in the
# sense that the load directory is replaced by renaming a fully-built sibling
# temp directory into place (rename(2)); the previous build is kept as
# <load-dir>.prev so a bad reload can be reverted by hand.
#
# Usage:
#   reload-extension.sh --load-dir <dir> [--ref <git-ref>] [--build-number <n>]
#                        [--init] [--dry-run]
#
#   --load-dir <dir>    Directory Chrome loads unpacked from (required).
#   --ref <ref>         Git ref to build (default: HEAD).
#   --build-number <n>  Build number for the 4th version component. Defaults to
#                       the previous load-dir build's 4th component + 1, or 1.
#   --init              Allow a load dir that has never held a build (or does
#                       not exist yet). Without it, a load dir that does not
#                       look like a previous chat-stasher build is refused.
#   --dry-run           Print the plan and change nothing.
#
# Tests only: when CS_RELOAD_BUILD_CMD is set it is used instead of the real
# pnpm build. It is invoked as "$CS_RELOAD_BUILD_CMD" <extension-dir> <n> and
# must leave a built extension (including manifest.json) under
# <extension-dir>/.output/chrome-mv3/. This lets the bash test use a stub build
# and assert on the reload mechanics without a real toolchain.

set -euo pipefail

usage() {
  sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

REF=HEAD
LOAD_DIR=""
BUILD_NUMBER=""
MODE=install
INIT=0

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
    --init)
      INIT=1; shift ;;
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

if [ "$INIT" -eq 0 ] && [ "$LOAD_IS_BUILD" -eq 0 ]; then
  echo "reload-extension.sh: $LOAD_DIR does not look like a previous chat-stasher build" >&2
  echo "  (expected a manifest.json whose name is __MSG_extName__). Pass --init to seed it." >&2
  exit 1
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
  echo "                  output into a sibling temp dir, then swap"
  echo "                  old build -> $LOAD_DIR_PREV and temp dir -> $LOAD_DIR"
  echo "  manual step:    $MANUAL_STEP"
  exit 0
fi

# --- build from a throwaway worktree ----------------------------------------
STAGING="$(mktemp -d "$(dirname "$LOAD_DIR")/.$(basename "$LOAD_DIR").reload.XXXXXX")"
WT="$(mktemp -d /tmp/chat-stasher-reload.XXXXXX)"

cleanup() {
  # Every step guarded: this trap must never change the script's exit status
  # (a failing command here under `set -e` would turn a successful reload into
  # a reported failure, which is exactly the wrong signal).
  git worktree remove --force "$WT" >/dev/null 2>&1 || true
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

if [ -e "$LOAD_DIR" ]; then
  rm -rf "$LOAD_DIR_PREV"
  mv "$LOAD_DIR" "$LOAD_DIR_PREV"
fi
# STAGING has been renamed into place, so the path no longer exists and the
# EXIT trap (which only removes the staging path) becomes a harmless no-op.
mv "$STAGING" "$LOAD_DIR"

# --- done -------------------------------------------------------------------
# The throwaway worktree is removed by the EXIT trap (works for the failure
# path too); this body only reflects on what it just did.
echo "reload-extension.sh: $PLAN_MSG"
echo "reload-extension.sh: previous build kept at $LOAD_DIR_PREV"
echo "reload-extension.sh: remaining manual step: $MANUAL_STEP"
echo "reload-extension.sh: done."
exit 0