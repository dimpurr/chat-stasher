#!/usr/bin/env bash
#
# The selftest for scripts/dev/check-test-cache-isolation.sh (W289): proof that
# the guard turns each way a test run can touch the real rustic cache into a
# red run — and that the two regressions W289's review found stay fixed.
#
# It drives the real guard against a throwaway cache root, with the wrapped
# command supplied per probe, the way scripts/selftest-npm-latest-tag.sh drives
# its script with `npm` shimmed on PATH. A `uname` shim reports a Windows
# platform so the Windows branch can be exercised from macOS/Linux; the cache
# root is a temp directory in every probe, so nothing here touches the host's
# real cache.
#
# The probes that matter most, and what each would catch:
#
#   · **A create-then-delete inside the run window is caught.** The pre-W289
#     guard compared the watched directory's mtime at whole-second resolution,
#     so a probe that creates an entry and removes it within the same second
#     left an identical entry set, an identical second-resolution mtime, and a
#     green guard (measured: marker and directory both `…707`). The current
#     guard compares against a run-boundary marker with `find -newer`, at full
#     filesystem precision, and this probe is red for exactly that reason.
#   · **Windows is guarded, not skipped.** The guard watches
#     `%LOCALAPPDATA%\rustic` (and `%USERPROFILE%\AppData\Local\rustic`), so a
#     leak there fails the run instead of printing a NOT-GUARDED line.
#   · **A root that cannot be named is a refusal, not a silent pass.** With no
#     `$HOME` (or, on Windows, no `%LOCALAPPDATA%`/`%USERPROFILE%`) the guard
#     exits 1 before running anything.
#   · **A green suite that dirtied the cache is still red**, and a red wrapped
#     command does not bypass the verdict.
#
# Exit codes: 0 = every probe behaved · 1 = at least one did not · 2 = usage.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GUARD="$ROOT/scripts/dev/check-test-cache-isolation.sh"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cs-cache-guard-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

SHIM="$TMP/shim"
SANDBOX="$TMP/sandbox"
mkdir -p "$SHIM" "$SANDBOX"

# A 64-hex per-repository directory name, the shape rustic writes.
rep64() {
  local c=$1 out="" i=0
  while [ "$i" -lt 64 ]; do
    out="$out$c"
    i=$(( i + 1 ))
  done
  printf '%s' "$out"
}
SEED="$(rep64 a)"

# The host's real shell the guard is run *with* (so `uname` is shimmed), and a
# host platform the guard is run *against* for the non-Windows probes.
case "$(uname -s)" in
  Darwin*) HOST_WATCH="$SANDBOX/home/Library/Caches/rustic" ;;
  *) HOST_WATCH="$SANDBOX/home/.cache/rustic" ;;
esac
WIN_WATCH="$SANDBOX/win/AppData/Local/rustic"
WIN_USERPROFILE="$SANDBOX/win-profile"

# `uname -s` reports MINGW so the guard takes its Windows branch without a
# Windows host. Every other argument passes through to the real `uname`.
cat >"$SHIM/uname" <<'SH'
#!/bin/sh
case "$1" in
  -s) echo "MINGW64_NT-10.0-19045" ;;
  *) exec /usr/bin/uname "$@" ;;
esac
SH
chmod +x "$SHIM/uname"

PROBES=0
FAILED=0

record() { # $1 label · $2 expected rc
  PROBES=$(( PROBES + 1 ))
  if [ "$RC" -eq "$2" ]; then
    echo "  ok   $1 (rc=$RC)"
  else
    FAILED=$(( FAILED + 1 ))
    echo "  FAIL $1: expected rc=$2, got $RC" >&2
    printf '%s\n' "$OUT" | sed 's/^/       /' >&2
  fi
}

contains() { # $1 substring the guard's output must carry
  PROBES=$(( PROBES + 1 ))
  case "$OUT" in
    *"$1"*) echo "  ok   output mentions: $1" ;;
    *)
      FAILED=$(( FAILED + 1 ))
      echo "  FAIL output does not mention: $1" >&2
      printf '%s\n' "$OUT" | sed 's/^/       /' >&2
      ;;
  esac
}

run_guard() { # runs the guard with the caller's environment; sets OUT and RC
  OUT=$(bash "$GUARD" "$@" 2>&1)
  RC=$?
}

seed_host_root() {
  rm -rf "$HOST_WATCH"
  mkdir -p "$HOST_WATCH/$SEED"
}

echo "== non-Windows platform =="

# A clean run: nothing under the root changes, the wrapped command succeeds.
seed_host_root
OUT=$(env -u XDG_CACHE_HOME HOME="$SANDBOX/home" bash "$GUARD" -- true 2>&1)
RC=$?
record "clean run passes" 0
contains "left the real user cache root untouched"

# A run that creates a per-repository directory is red.
seed_host_root
OUT=$(env -u XDG_CACHE_HOME HOME="$SANDBOX/home" bash "$GUARD" -- bash -c \
  'mkdir -p "$HOME/Library/Caches/rustic/'"$SEED"'2"; mkdir -p "$HOME/.cache/rustic/'"$SEED"'2"' 2>&1)
RC=$?
record "created entry is red" 1
contains "changed the real user cache root"

# A run that removes an existing entry is red — deleting from the real cache
# is worse than writing to it.
seed_host_root
OUT=$(env -u XDG_CACHE_HOME HOME="$SANDBOX/home" bash "$GUARD" -- bash -c \
  'rm -rf "$HOME/Library/Caches/rustic/'"$SEED"'" "$HOME/.cache/rustic/'"$SEED"'"' 2>&1)
RC=$?
record "removed entry is red" 1

# The regression: create then delete inside the run window leaves the entry set
# identical. Whole-second mtime comparison called this clean; the run-boundary
# marker must not.
seed_host_root
OUT=$(env -u XDG_CACHE_HOME HOME="$SANDBOX/home" bash "$GUARD" -- bash -c \
  'for r in "$HOME/Library/Caches/rustic" "$HOME/.cache/rustic"; do
     d="$r/'"$SEED"'3"; mkdir -p "$d"; rmdir "$d";
   done' 2>&1)
RC=$?
record "create-then-delete is red" 1
contains "create-then-delete"

# A wrapped command that fails does not bypass the verdict, and a clean cache
# does not mask the failure.
seed_host_root
OUT=$(env -u XDG_CACHE_HOME HOME="$SANDBOX/home" bash "$GUARD" -- bash -c 'exit 7' 2>&1)
RC=$?
record "failing wrapped command is red (clean cache)" 1
contains "wrapped command exited 7"

# No $HOME: the root cannot be named, so the guard refuses before running.
seed_host_root
OUT=$(env -u HOME -u XDG_CACHE_HOME bash "$GUARD" -- bash -c \
  'mkdir -p "$HOME/Library/Caches/rustic/should-not-happen"' 2>&1)
RC=$?
record "no HOME refuses" 1
contains "refusing"

echo "== Windows platform =="

# Clean Windows run.
rm -rf "$WIN_WATCH"
mkdir -p "$WIN_WATCH/$SEED"
OUT=$(env PATH="$SHIM:$PATH" LOCALAPPDATA="$SANDBOX/win/AppData/Local" \
  bash "$GUARD" -- true 2>&1)
RC=$?
record "windows clean run passes" 0
contains "left the real user cache root untouched"

# A leak into %LOCALAPPDATA%\rustic fails the run instead of being skipped.
rm -rf "$WIN_WATCH"
mkdir -p "$WIN_WATCH/$SEED"
OUT=$(env PATH="$SHIM:$PATH" LOCALAPPDATA="$SANDBOX/win/AppData/Local" \
  bash "$GUARD" -- bash -c \
  'mkdir -p "$LOCALAPPDATA/rustic/'"$SEED"'4"' 2>&1)
RC=$?
record "windows leak is red" 1
contains "changed the real user cache root"

# %USERPROFILE%\AppData\Local is the documented second candidate the product
# keeps for the Known Folder; a leak there is watched too.
rm -rf "$WIN_USERPROFILE"
mkdir -p "$WIN_USERPROFILE/AppData/Local/rustic/$SEED"
OUT=$(env -u LOCALAPPDATA PATH="$SHIM:$PATH" USERPROFILE="$WIN_USERPROFILE" \
  bash "$GUARD" -- bash -c \
  'mkdir -p "$USERPROFILE/AppData/Local/rustic/'"$SEED"'5"' 2>&1)
RC=$?
record "windows USERPROFILE leak is red" 1

# Neither variable set: refuse, never run unguarded.
OUT=$(env -u LOCALAPPDATA -u USERPROFILE PATH="$SHIM:$PATH" \
  bash "$GUARD" -- true 2>&1)
RC=$?
record "windows with no root variables refuses" 1
contains "neither %LOCALAPPDATA% nor %USERPROFILE%"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "test-cache-isolation-guard: $PROBES probes passed"
  exit 0
fi
echo "test-cache-isolation-guard: $FAILED of $PROBES probes FAILED" >&2
exit 1
