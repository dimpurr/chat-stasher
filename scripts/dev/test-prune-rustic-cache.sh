#!/usr/bin/env bash
#
# The selftest for scripts/dev/prune-test-rustic-cache.sh (W289): proof that the
# prune keeps every cache directory that belongs to a repository the user's real
# config declares, and deletes only the leftovers — including when the keep-set
# cannot be completed.
#
# It drives the real script against a throwaway cache root, with the
# repository-id helper shimmed: the script runs
# `cargo run --example repo-config-id`, which needs a real config and would open
# the user's real repositories, so a `cargo` shim on PATH prints a canned report
# instead. Nothing here touches the host's real cache or config.
#
# The probes that matter most, and what each would catch:
#
#   · **An unresolved repository refuses --apply, with no override.** W289's
#     review found `--apply --allow-unresolved` could delete the cache of a
#     configured repository whose id could not be read, breaking the absolute
#     keep guarantee. The escape hatch is gone: the flag is a usage error now,
#     and an `error` line in the report refuses --apply outright. Both probes
#     are red against the pre-fix script (which deleted under the flag).
#   · **--apply deletes exactly the reported set, and nothing else.** The
#     keep-set directories, a `CACHEDIR.TAG`, a non-hex name, and a *file* whose
#     name is 64 hex digits all survive.
#   · **A keep-set that cannot be built refuses too** — an unloadable config or
#     a report with no derivable id deletes nothing.
#
# Exit codes: 0 = every probe behaved · 1 = at least one did not · 2 = usage.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$ROOT/scripts/dev/prune-test-rustic-cache.sh"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cs-prune-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

BIN="$TMP/bin"
HOME_DIR="$TMP/home"
mkdir -p "$BIN" "$HOME_DIR"

case "$(uname -s)" in
  Darwin*) WATCH="$HOME_DIR/Library/Caches/rustic" ;;
  *) WATCH="$HOME_DIR/.cache/rustic" ;;
esac

# The `cargo` shim: print the canned report PRUNE_SELFTEST_REPORT points at and
# exit with PRUNE_SELFTEST_RC (default 0), exactly the two outcomes the helper
# has — a report, or a config that could not be loaded (rc 1).
cat >"$BIN/cargo" <<'SH'
#!/usr/bin/env bash
cat "${PRUNE_SELFTEST_REPORT:?PRUNE_SELFTEST_REPORT must be set}"
exit "${PRUNE_SELFTEST_RC:-0}"
SH
chmod +x "$BIN/cargo"

rep64() {
  local c=$1 out="" i=0
  while [ "$i" -lt 64 ]; do
    out="$out$c"
    i=$(( i + 1 ))
  done
  printf '%s' "$out"
}
KEEP_A="$(rep64 a)" # the local repository — must survive every probe
KEEP_B="$(rep64 b)" # a configured destination — must survive too
LEFT_1="$(rep64 1)" # a leftover — removable
LEFT_2="$(rep64 2)" # a leftover — removable
HEXFILE="$(rep64 3)" # a *file* with a 64-hex name — never deleted

reset_fixture() {
  rm -rf "$WATCH"
  mkdir -p "$WATCH/$KEEP_A" "$WATCH/$KEEP_B" "$WATCH/$LEFT_1" "$WATCH/$LEFT_2" "$WATCH/nothex"
  : >"$WATCH/CACHEDIR.TAG"
  : >"$WATCH/$HEXFILE"
}

# Reports the helper can print.
R_FULL="$TMP/r-full.txt"
printf 'id local %s\nid dest %s\nend\n' "$KEEP_A" "$KEEP_B" >"$R_FULL"
R_UNRESOLVED="$TMP/r-unresolved.txt"
printf 'id local %s\nerror dest remote unreachable\nend\n' "$KEEP_A" >"$R_UNRESOLVED"
R_NOEND="$TMP/r-noend.txt"
printf 'id local %s\n' "$KEEP_A" >"$R_NOEND"
R_CONFIG_ERR="$TMP/r-config-error.txt"
printf 'config-error read the chat-stasher config\nend\n' >"$R_CONFIG_ERR"
R_NOKEEP="$TMP/r-nokeep.txt"
printf 'absent local\nend\n' >"$R_NOKEEP"

PROBES=0
FAILED=0

run_prune() { # caller sets PRUNE_SELFTEST_REPORT; sets OUT and RC
  OUT=$(PATH="$BIN:$PATH" HOME="$HOME_DIR" bash "$SCRIPT" "$@" 2>&1)
  RC=$?
}

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

contains() { # $1 substring the script's output must carry
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

exists() { # $1 path · $2 label
  PROBES=$(( PROBES + 1 ))
  if [ -e "$1" ]; then
    echo "  ok   kept $2"
  else
    FAILED=$(( FAILED + 1 ))
    echo "  FAIL $2 should still exist: $1" >&2
  fi
}

gone() { # $1 path · $2 label
  PROBES=$(( PROBES + 1 ))
  if [ ! -e "$1" ]; then
    echo "  ok   removed $2"
  else
    FAILED=$(( FAILED + 1 ))
    echo "  FAIL $2 should be deleted: $1" >&2
  fi
}

if case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) true ;; *) false ;; esac; then
  # The prune refuses on Windows (the Known Folder root cannot be enumerated
  # safely); the selftest asserts that refusal rather than skipping silently.
  reset_fixture
  export PRUNE_SELFTEST_REPORT="$R_FULL"
  run_prune --apply
  record "windows: prune refuses" 1
  contains "refusing"
  exists "$WATCH/$LEFT_1" "windows refusal deleted nothing"
  echo
  if [ "$FAILED" -eq 0 ]; then
    echo "test-prune-rustic-cache: $PROBES probes passed"
    exit 0
  fi
  echo "test-prune-rustic-cache: $FAILED of $PROBES probes FAILED" >&2
  exit 1
fi

export PRUNE_SELFTEST_REPORT="$R_FULL"
unset PRUNE_SELFTEST_RC 2>/dev/null || true

echo "== dry run =="

reset_fixture
run_prune
record "dry run exits 0" 0
contains "dry run complete"
exists "$WATCH/$LEFT_1" "dry run leftover 1"
exists "$WATCH/$LEFT_2" "dry run leftover 2"
exists "$WATCH/$KEEP_A" "dry run keep A"

echo "== --apply with a complete keep-set =="

reset_fixture
run_prune --apply
record "--apply exits 0" 0
contains "deleted 2 directories"
gone "$WATCH/$LEFT_1" "leftover 1"
gone "$WATCH/$LEFT_2" "leftover 2"
exists "$WATCH/$KEEP_A" "configured local repository"
exists "$WATCH/$KEEP_B" "configured destination"
exists "$WATCH/CACHEDIR.TAG" "CACHEDIR.TAG"
exists "$WATCH/nothex" "non-hex directory"
exists "$WATCH/$HEXFILE" "64-hex *file*"

echo "== the removed escape hatch =="

# W289 review HIGH 2: --allow-unresolved used to let --apply delete the cache of
# a configured repository whose id could not be read. The flag must not exist.
reset_fixture
export PRUNE_SELFTEST_REPORT="$R_UNRESOLVED"
run_prune --apply --allow-unresolved
record "--apply --allow-unresolved is a usage error" 2
exists "$WATCH/$LEFT_1" "rejected flag deleted nothing"

echo "== an unresolved repository blocks --apply =="

reset_fixture
export PRUNE_SELFTEST_REPORT="$R_UNRESOLVED"
run_prune --apply
record "--apply refuses while a repository is unresolved" 1
contains "REFUSED"
exists "$WATCH/$LEFT_1" "unresolved refusal deleted nothing"
exists "$WATCH/$KEEP_A" "unresolved refusal kept the id it could read"

# A dry run still reports (it deletes nothing), so the refusal is not a wall
# against ever seeing the list.
reset_fixture
export PRUNE_SELFTEST_REPORT="$R_UNRESOLVED"
run_prune
record "dry run with a repository unresolved still reports" 0
contains "REFUSED"
exists "$WATCH/$LEFT_1" "dry run with an unresolved id deleted nothing"

echo "== a keep-set that cannot be built =="

reset_fixture
export PRUNE_SELFTEST_REPORT="$R_CONFIG_ERR"
export PRUNE_SELFTEST_RC=1
run_prune --apply
record "unloadable config refuses --apply" 1
exists "$WATCH/$LEFT_1" "unloadable config deleted nothing"
unset PRUNE_SELFTEST_RC

reset_fixture
export PRUNE_SELFTEST_REPORT="$R_NOEND"
run_prune --apply
record "report missing the end marker refuses --apply" 1
exists "$WATCH/$LEFT_1" "truncated report deleted nothing"

reset_fixture
export PRUNE_SELFTEST_REPORT="$R_NOKEEP"
run_prune --apply
record "no derivable id refuses --apply" 1
exists "$WATCH/$LEFT_1" "empty keep-set deleted nothing"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "test-prune-rustic-cache: $PROBES probes passed"
  exit 0
fi
echo "test-prune-rustic-cache: $FAILED of $PROBES probes FAILED" >&2
exit 1
