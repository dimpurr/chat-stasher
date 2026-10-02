#!/usr/bin/env bash
#
# The selftest for scripts/dev/check-test-isolation.sh (W306): proof that the
# guard turns each way a test run can touch a real chat-stasher user-data
# location into a red run, on every platform branch the guard has.
#
# Same shape as scripts/dev/test-cache-isolation-guard.sh — the guard is driven
# against a throwaway HOME with the wrapped command supplied per probe, and a
# `uname` shim reports a Windows platform so the Windows branch is exercised
# from macOS/Linux. Every watched root is a temp directory in the probes, so
# nothing here touches the host's real data.
#
# The probes that matter most, and what each would catch:
#
#   · **The inbox and the manifest directories are watched, not just the data
#     root.** W289 watched the rustic cache; the 2026-10-02 leak used the data
#     root, and the other user-data locations in the same class have to fail a
#     run too.
#   · **A manifest directory that appears during the run is a diff**, because
#     the root set is re-derived on both sides rather than hard-coded.
#   · **A create-then-delete inside the run window is caught** by the
#     run-boundary marker and `find -newer`, at full filesystem precision.
#   · **Windows is guarded, not skipped**, and a machine whose root cannot be
#     named refuses rather than passing unguarded.
#   · **A green suite that dirtied a root is still red**, and a red wrapped
#     command does not bypass the verdict.
#
# Exit codes: 0 = every probe behaved · 1 = at least one did not · 2 = usage.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GUARD="$ROOT/scripts/dev/check-test-isolation.sh"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cs-test-isolation-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

SHIM="$TMP/shim"
SANDBOX="$TMP/sandbox"
mkdir -p "$SHIM" "$SANDBOX/home"

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

# The host platform's manifest path, so the manifest probes run on the host.
case "$(uname -s)" in
  Darwin*) MANIFEST_DIR="$SANDBOX/home/Library/Application Support/Google/Chrome/NativeMessagingHosts" ;;
  *) MANIFEST_DIR="$SANDBOX/home/.config/google-chrome/NativeMessagingHosts" ;;
esac
DATA_ROOT="$SANDBOX/home/.local/share/chat-stasher"
CONFIG_ROOT="$SANDBOX/home/.config/chat-stasher"
INBOX_ROOT="$SANDBOX/home/Downloads/chat-stasher/inbox"

WIN_LOCAL="$SANDBOX/win/AppData/Local"
WIN_WATCH="$WIN_LOCAL/chat-stasher"

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

run_host() { # runs the guard against the sandbox HOME; sets OUT and RC
  OUT=$(env -u XDG_DATA_HOME -u XDG_CONFIG_HOME -u XDG_STATE_HOME \
    HOME="$SANDBOX/home" bash "$GUARD" "$@" 2>&1)
  RC=$?
}

seed_data_root() {
  rm -rf "$DATA_ROOT"
  mkdir -p "$DATA_ROOT/stage"
}

echo "== host platform =="

# A clean run: nothing under any root changes, the wrapped command succeeds.
seed_data_root
run_host -- true
record "clean run passes" 0
contains "left the real data"

# A run that writes into the real data root is red.
seed_data_root
run_host -- bash -c 'mkdir -p "$HOME/.local/share/chat-stasher/stage/sessions/leak"'
record "data-root leak is red" 1
contains "changed a real chat-stasher user-data location"

# A run that removes an existing entry is red — deleting from real data is
# worse than writing to it.
seed_data_root
run_host -- bash -c 'rm -rf "$HOME/.local/share/chat-stasher/stage"'
record "removed data entry is red" 1

# A run that writes into the real config root is red.
rm -rf "$CONFIG_ROOT"; mkdir -p "$CONFIG_ROOT"
run_host -- bash -c 'echo x > "$HOME/.config/chat-stasher/config.toml"'
record "config-root leak is red" 1

# A run that writes into the real inbox is red.
rm -rf "$INBOX_ROOT"; mkdir -p "$INBOX_ROOT"
run_host -- bash -c 'echo x > "$HOME/Downloads/chat-stasher/inbox/bundle.json"'
record "inbox leak is red" 1

# A run that writes a native-messaging manifest into a browser the guard found
# is red.
rm -rf "$MANIFEST_DIR"; mkdir -p "$MANIFEST_DIR"
run_host -- bash -c 'echo "{}" > "$1/com.chat_stasher.host.json"' _ "$MANIFEST_DIR"
record "manifest-dir leak is red" 1

# A manifest directory that *appears* during the run is a new root, so the root
# set itself differs.
rm -rf "$MANIFEST_DIR"
run_host -- bash -c 'mkdir -p "$1"' _ "$MANIFEST_DIR"
record "manifest dir appearing is red" 1

# The regression: create then delete inside the run window leaves the entry set
# identical. The run-boundary marker must catch it.
seed_data_root
run_host -- bash -c 'd="$HOME/.local/share/chat-stasher/stage/gone"
  mkdir -p "$d"
  rmdir "$d"'
record "create-then-delete is red" 1
contains "create-then-delete"

# A wrapped command that fails does not bypass the verdict.
seed_data_root
run_host -- bash -c 'exit 7'
record "failing wrapped command is red (clean roots)" 1
contains "wrapped command exited 7"

# No $HOME: the root cannot be named, so the guard refuses before running.
OUT=$(env -u HOME -u XDG_DATA_HOME -u XDG_CONFIG_HOME -u XDG_STATE_HOME \
  bash "$GUARD" -- true 2>&1)
RC=$?
record "no HOME refuses" 1
contains "refusing"

echo "== Windows platform =="

rm -rf "$WIN_WATCH"; mkdir -p "$WIN_WATCH/stage"
OUT=$(env PATH="$SHIM:$PATH" LOCALAPPDATA="$WIN_LOCAL" \
  bash "$GUARD" -- true 2>&1)
RC=$?
record "windows clean run passes" 0

rm -rf "$WIN_WATCH"; mkdir -p "$WIN_WATCH/stage"
OUT=$(env PATH="$SHIM:$PATH" LOCALAPPDATA="$WIN_LOCAL" \
  bash "$GUARD" -- bash -c \
  'mkdir -p "$LOCALAPPDATA/chat-stasher/stage/sessions/leak"' 2>&1)
RC=$?
record "windows data leak is red" 1

OUT=$(env -u LOCALAPPDATA -u USERPROFILE PATH="$SHIM:$PATH" \
  bash "$GUARD" -- true 2>&1)
RC=$?
record "windows with no root variables refuses" 1
contains "neither %LOCALAPPDATA% nor %USERPROFILE%"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "test-test-isolation-guard: $PROBES probes passed"
  exit 0
fi
echo "test-test-isolation-guard: $FAILED of $PROBES probes FAILED" >&2
exit 1
