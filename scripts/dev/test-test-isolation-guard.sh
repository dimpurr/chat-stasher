#!/usr/bin/env bash
#
# The selftest for scripts/dev/check-test-isolation.sh (W306, redesigned by
# W306b): proof that the guard turns each way a *test* can leave its trace in a
# real chat-stasher user-data location into a red run, and — the other half of
# the contract — proof that a *product*-shaped write there does not.
#
# Same shape as scripts/dev/test-cache-isolation-guard.sh — the guard is driven
# against a throwaway HOME with the wrapped command supplied per probe, and a
# `uname` shim reports a Windows platform so the Windows branch is exercised
# from macOS/Linux. Every watched root is a temp directory in the probes, so
# nothing here touches the host's real data.
#
# The probes that matter most, and what each would catch:
#
#   · **A realistic write into the real data root is NOT red.** This is the
#     W306b change: the machine the guard gates also runs the live product,
#     which writes `stage/ext-status/<machine>/<uuid>.json` every few minutes.
#     A guard that reds on that is one people learn to re-run. The probe writes
#     exactly that shape and must stay green.
#   · **A fixture identity is red** — by name, and for the coordination row, by
#     *content* of the `<data root>/state` store (the incident's second half) —
#     including the *adjacent-bytes* shape a real SQLite record has, which the
#     W306b boundary rule could not see (W306c).
#   · **Pre-existing debris is not red, and new writes still are** (W306c): the
#     per-run baseline subtracts a fixture identity or fixture-named path that
#     was already on the machine before the run, so the live product rewriting
#     the file that holds an earlier leak does not red every run — while an
#     identity the run introduces, or an entry it adds under residue, is red.
#   · **The per-run marker is red**, in a filename and in a file's bytes, so a
#     leak that carries only a sandbox-derived value is still caught.
#   · **The product cache root is watched** (W306b): a fixture written under
#     `~/Library/Caches/chat-stasher` is red, and the realistic body-cache
#     shape under it is not.
#   · **The state home is marker-only**: a dependency's scratch write there is
#     not red (the ubuntu-latest failure of run 37023093243), a run-marked
#     write there is.
#   · **The inbox and the manifest directories are structure roots**, where any
#     change — including a create-then-delete caught by the run-boundary
#     marker — is red.
#   · **Windows is guarded, not skipped**, at the roots the *product* resolves
#     there (`%USERPROFILE%\.local\share\chat-stasher`, not `%LOCALAPPDATA%`),
#     and a machine whose root cannot be named refuses.
#   · **The shell's reserved-token list cannot drift** from
#     `test_identity_guard::FIXTURE_IDENTITY_TOKENS`.
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
  Darwin*)
    MANIFEST_DIR="$SANDBOX/home/Library/Application Support/Google/Chrome/NativeMessagingHosts"
    CACHE_ROOT="$SANDBOX/home/Library/Caches/chat-stasher"
    ;;
  *)
    MANIFEST_DIR="$SANDBOX/home/.config/google-chrome/NativeMessagingHosts"
    CACHE_ROOT="$SANDBOX/home/.cache/chat-stasher"
    ;;
esac
DATA_ROOT="$SANDBOX/home/.local/share/chat-stasher"
CONFIG_ROOT="$SANDBOX/home/.config/chat-stasher"
STATE_HOME="$SANDBOX/home/.local/state"
INBOX_ROOT="$SANDBOX/home/Downloads/chat-stasher/inbox"

# The Windows sandbox: home is `%USERPROFILE%` (the product's second home
# spelling), and the cache root hangs off `%LOCALAPPDATA%` — *not* off home.
WIN_HOME="$SANDBOX/win/home"
WIN_LOCAL="$SANDBOX/win/AppData/Local"
WIN_DATA_ROOT="$WIN_HOME/.local/share/chat-stasher"
WIN_CACHE_ROOT="$WIN_LOCAL/chat-stasher"

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
  OUT=$(env -u XDG_DATA_HOME -u XDG_CONFIG_HOME -u XDG_STATE_HOME -u XDG_CACHE_HOME \
    HOME="$SANDBOX/home" bash "$GUARD" "$@" 2>&1)
  RC=$?
}

run_win() { # runs the guard's Windows branch; sets OUT and RC
  OUT=$(env PATH="$SHIM:$PATH" HOME="$WIN_HOME" USERPROFILE="$WIN_HOME" \
    LOCALAPPDATA="$WIN_LOCAL" bash "$GUARD" "$@" 2>&1)
  RC=$?
}

reset_host() {
  rm -rf "$DATA_ROOT" "$CONFIG_ROOT" "$STATE_HOME" "$INBOX_ROOT" "$CACHE_ROOT" "$MANIFEST_DIR"
}

seed_data_root() {
  rm -rf "$DATA_ROOT"
  mkdir -p "$DATA_ROOT/stage"
}

echo "== host platform =="

# A clean run: nothing under any root changes, the wrapped command succeeds.
reset_host
seed_data_root
run_host -- true
record "clean run passes" 0
contains "free of test fingerprints"

# The W306b regression: the live product writes the real stage every few
# minutes, in exactly this shape, with a realistic identity. It must stay green.
reset_host
seed_data_root
run_host -- bash -c 'd="$HOME/.local/share/chat-stasher/stage/ext-status/dims-macbook-pro-max-2"
  mkdir -p "$d"
  printf "{\"machine\":\"dims-macbook-pro-max-2\",\"stage\":\"/Users/dimpurr/stage\"}" \
    > "$d/9f3c1e2a-5b81-4d0a-9e11-2f0c4a7b6d88.json"'
record "live-product-shaped write is not red" 0

# The incident's first half: a fixture-named shard in the real stage is red.
reset_host
seed_data_root
run_host -- bash -c 'mkdir -p "$HOME/.local/share/chat-stasher/stage/sessions/leak"
  echo x > "$HOME/.local/share/chat-stasher/stage/sessions/leak/chatgpt.synthetic-session"'
record "fixture-named data leak is red" 1
contains "fixture-named entry appeared"

# A fixture identity in a file *name* under the real state directory is red
# (this is where the incident's coordination row landed).
reset_host
seed_data_root
run_host -- bash -c 'mkdir -p "$HOME/.local/share/chat-stasher/state"
  echo x > "$HOME/.local/share/chat-stasher/state/synthetic-install.json"'
record "fixture-named state file is red" 1

# The incident's second half: a fixture *row* inside the coordination store,
# whose file name carries no token. The state-store content scan catches it.
reset_host
seed_data_root
run_host -- bash -c 'mkdir -p "$HOME/.local/share/chat-stasher/state"
  printf "create table t(x); insert into t values(0x%08x);\n" 1 > "$HOME/.local/share/chat-stasher/state/extension-coordination.sqlite3"
  printf "CREATE TABLE ext_install_v2(install_id TEXT); INSERT INTO ext_install_v2 VALUES(\"synthetic-install\");\n" \
    > "$HOME/.local/share/chat-stasher/state/extension-coordination.sqlite3"'
record "fixture row in the coordination store is red" 1
contains "carrying a fixture identity"

# The byte shape a *real* SQLite store has (W306c): a record lays its columns
# down with no separator, so the incident's row is `chatgptsynthetic-install` —
# the `synthetic` is glued to the `chatgpt` before it, with no boundary the
# whole-token rule could match. The quoted SQL text above is the *weaker* case;
# this is the one the guard missed and the reason the content rule is a
# substring search.
reset_host
seed_data_root
run_host -- bash -c 'mkdir -p "$HOME/.local/share/chat-stasher/state"
  printf "SQLite format 3\000\022\007chatgptsynthetic-install" \
    > "$HOME/.local/share/chat-stasher/state/extension-coordination.sqlite3"'
record "sqlite-adjacent fixture row is red" 1
contains "carrying a fixture identity"

# The per-run baseline (W306c): an identity that was already in the store
# before the run — the residue an earlier leak left on this machine — is not a
# leak, even though the wrapped command rewrites the file that holds it (what
# the live product's heartbeat does every few minutes). Without the baseline the
# substring rule would red on the same bytes every run.
reset_host
seed_data_root
mkdir -p "$DATA_ROOT/state"
printf "SQLite format 3\000\022\007chatgptsynthetic-install" \
  >"$DATA_ROOT/state/extension-coordination.sqlite3"
run_host -- bash -c 'printf "2026-10-02T18:00:00Z" \
  >> "$HOME/.local/share/chat-stasher/state/extension-coordination.sqlite3"'
record "pre-existing fixture identity rewritten is not red" 0

# ...but an identity the run *introduces* into that same store is still a leak,
# so the baseline subtracts the residue without blinding the scan.
reset_host
seed_data_root
mkdir -p "$DATA_ROOT/state"
printf "SQLite format 3\000\022\007chatgptsynthetic-install" \
  >"$DATA_ROOT/state/extension-coordination.sqlite3"
run_host -- bash -c 'printf "\007fixture-newrow" \
  >> "$HOME/.local/share/chat-stasher/state/extension-coordination.sqlite3"'
record "new fixture identity in a baselined store is red" 1
contains "carrying a fixture identity"

# A file whose *content* carries the run marker is red even when neither its
# name nor its location says anything about a test.
reset_host
seed_data_root
run_host -- bash -c 'mkdir -p "$HOME/.local/share/chat-stasher/stage"
  printf "stage=%s" "$CHAT_STASHER_TEST_ISOLATION_MARKER" \
    > "$HOME/.local/share/chat-stasher/stage/4f1a2b3c.json"'
record "run-marker content is red" 1
contains "carrying this run's marker"

# A fixture-named entry *removed* during the run is red — deleting from the
# real archive is worse than writing to it.
reset_host
seed_data_root
mkdir -p "$DATA_ROOT/stage/sessions/m"
echo x >"$DATA_ROOT/stage/sessions/m/w292.synthetic-session-one"
run_host -- bash -c 'rm -f "$HOME/.local/share/chat-stasher/stage/sessions/m/w292.synthetic-session-one"'
record "removed fixture name is red" 1
contains "disappeared during the run"

# The per-run baseline for *names* (W306c): a fixture-named path that already
# existed before the run is residue, so a run that touches it is not a leak.
# This is the incident's own `chatgpt.synthetic-session` shard, still on the
# real machine; without the baseline its every mtime touch would red.
reset_host
seed_data_root
mkdir -p "$DATA_ROOT/stage/sessions/m/chatgpt.synthetic-session"
run_host -- bash -c 'touch "$HOME/.local/share/chat-stasher/stage/sessions/m/chatgpt.synthetic-session"'
record "pre-existing fixture-named residue touched is not red" 0

# ...but subtracting the path must not blind the scan to what the run adds
# inside it: a new shard under the residue directory is a new write, caught by
# the ancestor rule even though its own name carries no token.
reset_host
seed_data_root
mkdir -p "$DATA_ROOT/stage/sessions/m/chatgpt.synthetic-session"
run_host -- bash -c 'echo x > "$HOME/.local/share/chat-stasher/stage/sessions/m/chatgpt.synthetic-session/newshard"'
record "new entry under pre-existing residue is red" 1
contains "under fixture-named residue"

# A fixture written into the real config root is red.
reset_host
mkdir -p "$CONFIG_ROOT"
run_host -- bash -c 'echo x > "$HOME/.config/chat-stasher/fixture-config.toml"'
record "config-root fixture leak is red" 1

# A fixture written into the real *cache* root is red — W306b now watches the
# root the product's body cache and activity index live under.
reset_host
mkdir -p "$CACHE_ROOT/body"
run_host -- bash -c 'echo x > "$HOME/Library/Caches/chat-stasher/body/dummy-entry" 2>/dev/null ||
  echo x > "$HOME/.cache/chat-stasher/body/dummy-entry"'
record "cache-root fixture leak is red" 1

# ...while the realistic cache shape — a hash-named body entry — is not.
reset_host
mkdir -p "$CACHE_ROOT/body"
run_host -- bash -c 'd="$HOME/Library/Caches/chat-stasher/body"
  [ -d "$d" ] || d="$HOME/.cache/chat-stasher/body"
  mkdir -p "$d/02930116e96fa33c690441f521e27202a75da347e27a745015d22fd997387277"
  echo body > "$d/02930116e96fa33c690441f521e27202a75da347e27a745015d22fd997387277/0-879"'
record "realistic cache write is not red" 0

# A dependency's scratch write into the state home is not red: the product
# never writes there, and this is what made ubuntu-latest fail in run
# 37023093243.
reset_host
mkdir -p "$STATE_HOME"
run_host -- bash -c 'mkdir -p "$HOME/.local/state/opendal-dep"
  touch "$HOME/.local/state/opendal-dep/master.sock"'
record "dependency scratch in state home is not red" 0

# ...but a run-marked file there is.
reset_host
mkdir -p "$STATE_HOME"
run_host -- bash -c 'printf "%s" "$CHAT_STASHER_TEST_ISOLATION_MARKER" > "$HOME/.local/state/marked"'
record "run-marked state-home file is red" 1

# A run that writes into the real inbox is red.
reset_host
mkdir -p "$INBOX_ROOT"
run_host -- bash -c 'echo x > "$HOME/Downloads/chat-stasher/inbox/bundle.json"'
record "inbox leak is red" 1
contains "changed a real inbox"

# A run that writes a native-messaging manifest into a browser the guard found
# is red.
reset_host
mkdir -p "$MANIFEST_DIR"
run_host -- bash -c 'echo "{}" > "$1/com.chat_stasher.host.json"' _ "$MANIFEST_DIR"
record "manifest-dir leak is red" 1

# A manifest directory that *appears* during the run is a new root, so the root
# set itself differs.
reset_host
run_host -- bash -c 'mkdir -p "$1"' _ "$MANIFEST_DIR"
record "manifest dir appearing is red" 1

# The structure regression: create then delete inside the run window leaves the
# entry set identical. The run-boundary marker must catch it.
reset_host
mkdir -p "$INBOX_ROOT"
run_host -- bash -c 'd="$HOME/Downloads/chat-stasher/inbox/gone"
  mkdir -p "$d"
  rmdir "$d"'
record "structure create-then-delete is red" 1
contains "create-then-delete"

# A wrapped command that fails does not bypass the verdict.
reset_host
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

# The shell's reserved-token list is the Rust one, read back out of the source.
rust_tokens=$(grep -oE 'FIXTURE_IDENTITY_TOKENS: &\[&str\] = &\[[^]]*\]' \
  "$ROOT/crates/chat-stasher/src/test_identity_guard.rs" |
  grep -oE '"[a-z]+"' | tr -d '"' | LC_ALL=C sort | tr '\n' ' ')
shell_tokens=$(grep -E '^FIXTURE_TOKENS=' "$GUARD" | sed 's/^[^"]*"//; s/"$//' |
  tr ' ' '\n' | LC_ALL=C sort | tr '\n' ' ')
PROBES=$(( PROBES + 1 ))
if [ -n "$rust_tokens" ] && [ "$rust_tokens" = "$shell_tokens" ]; then
  echo "  ok   reserved-token list matches test_identity_guard.rs ($rust_tokens)"
else
  FAILED=$(( FAILED + 1 ))
  echo "  FAIL reserved-token list drifted: rust=[$rust_tokens] shell=[$shell_tokens]" >&2
fi

echo "== Windows platform =="

# Roots are the product's real Windows resolutions: home is `%USERPROFILE%`,
# so the data root is `%USERPROFILE%\.local\share\chat-stasher` — NOT
# `%LOCALAPPDATA%\chat-stasher`, which is the cache root.
rm -rf "$WIN_HOME" "$WIN_LOCAL"; mkdir -p "$WIN_DATA_ROOT/stage" "$WIN_CACHE_ROOT/body"
run_win -- true
record "windows clean run passes" 0

run_win -- bash -c 'mkdir -p "$USERPROFILE/.local/share/chat-stasher/stage/sessions/leak"
  echo x > "$USERPROFILE/.local/share/chat-stasher/stage/sessions/leak/chatgpt.synthetic-session"'
record "windows data-root fixture leak is red" 1

# The realistic body-cache shape under `%LOCALAPPDATA%\chat-stasher` — the
# write that made the old Windows branch red — must not be.
rm -rf "$WIN_DATA_ROOT" "$WIN_CACHE_ROOT"; mkdir -p "$WIN_DATA_ROOT/stage" "$WIN_CACHE_ROOT/body"
run_win -- bash -c 'd="$LOCALAPPDATA/chat-stasher/body/02930116e96fa33c690441f521e27202a75da347e27a745015d22fd997387277"
  mkdir -p "$d"
  echo body > "$d/0-879"'
record "windows realistic cache write is not red" 0

OUT=$(env -u HOME -u USERPROFILE -u LOCALAPPDATA PATH="$SHIM:$PATH" \
  bash "$GUARD" -- true 2>&1)
RC=$?
record "windows with no root variables refuses" 1
contains "neither \$HOME nor"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "test-test-isolation-guard: $PROBES probes passed"
  exit 0
fi
echo "test-test-isolation-guard: $FAILED of $PROBES probes FAILED" >&2
exit 1
