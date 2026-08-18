#!/usr/bin/env bash
#
# gate-selftest-semantic.sh — meta-test for release-gate step 8
# (scripts/check-semantic-defaults.py, the "semantic defaults must carry a
# // reason:" gate).
#
# WHY THIS EXISTS
# ---------------
# B92 shipped that gate with a blind spot: `in_test` was set once and never
# reset, so everything after the *first* `#[cfg(test)]` in a file went
# unchecked. It was not caught, because the red-test injection that "proved"
# the gate worked was placed at line 1 of a file — the one position that
# happened to still be scanned. Move the same probe to the end of the file and
# the gate reported PASS over a real violation.
#
# The lesson is not "that was a bug". The lesson is: when the thing being
# tested chooses its own test point, it will choose the one it passes.
# So this meta-test takes that choice away — it injects the SAME violation at
# several structurally different positions and demands a FAIL at every one.
#
# WHAT IT ASSERTS
#   P1  violation at the top of the file (before any test module)      -> FAIL
#   P2  violation at the end of the file (after the first #[cfg(test)]) -> FAIL   <- the B92 blind spot
#   P3  violation after a closed #[cfg(test)] module, mid-file          -> FAIL
#   P4  violation with a token reason (`// reason: ok`)                 -> FAIL
#   P5  violation with a real reason on the line above                  -> pass
#   P6  the SAME violation inside a #[cfg(test)] module                 -> pass   (reverse assertion)
#   P7  clean tree, nothing injected                                    -> pass
# plus: release-gate.sh actually calls this checker (no drifted second copy).
#
# WHEN TO RUN IT
#   - after touching scripts/check-semantic-defaults.py or step 8 of
#     release-gate.sh (it is the regression test for that checker);
#   - in CI, as its own job / on a schedule.
#   NOT wired into release-gate.sh's normal path on purpose: this script
#   mutates a tracked source file, and a release pipeline must never do that.
#
# SAFETY
#   Exactly one tracked file is touched (PROBE_FILE). A pristine copy is taken
#   before the first injection and restored by an EXIT trap, so an interrupt,
#   an error, or a failed assertion all still leave the work tree clean. The
#   script verifies the restore itself (byte-identical + `git status` clean)
#   and fails if the restore did not happen.
#
# Usage:  bash scripts/gate-selftest-semantic.sh
# Exit:   0 = every assertion held (the gate is not blind)
#         1 = an assertion failed (the gate has a blind spot, or is over-eager)
#         2 = harness problem (missing file, restore failed, ...)

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECKER="$ROOT/scripts/check-semantic-defaults.py"
PROBE_FILE="$ROOT/crates/chat-stasher/src/id.rs"

# The probe line. Deliberately a bare, reason-less semantic default — the exact
# shape the gate exists to catch.
PROBE='    let _gate_probe: u64 = "x".parse::<u64>().unwrap_or(0);'

pass_count=0
fail_count=0
harness_rc=0

say()  { echo "[selftest] $*"; }
ok()   { pass_count=$((pass_count + 1)); echo "[selftest]   PASS · $*"; }
bad()  { fail_count=$((fail_count + 1)); echo "[selftest]   FAIL · $*"; }

[ -f "$CHECKER" ]    || { echo "[selftest] checker not found: $CHECKER"; exit 2; }
[ -f "$PROBE_FILE" ] || { echo "[selftest] probe file not found: $PROBE_FILE"; exit 2; }

TMP="$(mktemp -d)"
PRISTINE="$TMP/id.rs.pristine"
cp "$PROBE_FILE" "$PRISTINE"

restore() {
  # Unconditional restore. Runs on success, on failure, and on Ctrl-C.
  cp "$PRISTINE" "$PROBE_FILE" 2>/dev/null || true
}
trap 'restore; rm -rf "$TMP"' EXIT

# --- probe file facts we build injections from -----------------------------
TOTAL_LINES=$(grep -c "" "$PRISTINE")
FIRST_TEST_LINE=$(grep -n '^\s*#\[cfg(test)\]' "$PRISTINE" | head -n 1 | cut -d: -f1)
if [ -z "${FIRST_TEST_LINE:-}" ]; then
  echo "[selftest] probe file has no #[cfg(test)] module — positions 2/3/6 cannot be built."
  echo "[selftest] pick a different PROBE_FILE."
  exit 2
fi
say "probe file: ${PROBE_FILE#$ROOT/} ($TOTAL_LINES lines, first #[cfg(test)] at line $FIRST_TEST_LINE)"
echo

# inject_at <line-no> <text...>   — insert given lines BEFORE <line-no>
# (line-no = TOTAL_LINES+1 means append at end of file)
inject_at() {
  local at="$1"; shift
  local payload
  payload="$(printf '%s\n' "$@")"
  AT="$at" PAYLOAD="$payload" SRC="$PRISTINE" DST="$PROBE_FILE" python3 - <<'PY'
import os
at = int(os.environ["AT"])
lines = open(os.environ["SRC"], encoding="utf-8").read().splitlines(keepends=True)
payload = [l + "\n" for l in os.environ["PAYLOAD"].split("\n")]
idx = max(0, min(at - 1, len(lines)))
open(os.environ["DST"], "w", encoding="utf-8").writelines(lines[:idx] + payload + lines[idx:])
PY
}

# run_checker: sets $LAST_REPORT and $LAST_RC.
# NOT via `$(run_checker)` — a command substitution is a subshell and the
# report would never make it back out.
LAST_REPORT=""
LAST_RC=0
run_checker() {
  LAST_REPORT="$(python3 "$CHECKER" "$ROOT" 2>&1)"
  LAST_RC=$?
}

# expect <name> <expect: fail|pass> <injected-line-no>
expect() {
  local name="$1" want="$2" line_no="$3"
  run_checker
  local rc="$LAST_RC"
  local head1; head1="$(printf '%s' "$LAST_REPORT" | head -n 1)"
  local hit=""
  if [ -n "$line_no" ]; then
    hit="$(printf '%s' "$LAST_REPORT" | grep -c "id\.rs:${line_no} " || true)"
  fi
  say "$name -> checker exit code = $rc, first line = '$head1'"
  if [ "$want" = "fail" ]; then
    if [ "$rc" -ne 1 ]; then
      bad "$name: expected exit 1 (violation reported), got $rc — THE GATE IS BLIND AT THIS POSITION"
      printf '%s\n' "$LAST_REPORT" | sed 's/^/[selftest]     /'
      return
    fi
    if [ -n "$line_no" ] && [ "${hit:-0}" -lt 1 ]; then
      bad "$name: exit 1, but the report does not name the injected line $line_no"
      printf '%s\n' "$LAST_REPORT" | sed 's/^/[selftest]     /'
      return
    fi
    ok "$name: violation caught at the injected position"
  else
    if [ "$rc" -ne 0 ]; then
      bad "$name: expected exit 0 (no violation), got $rc — the gate is over-eager here"
      printf '%s\n' "$LAST_REPORT" | sed 's/^/[selftest]     /'
      return
    fi
    ok "$name: correctly stayed quiet"
  fi
}

# ---------------------------------------------------------------- position 1
say "P1 · violation at the TOP of the file (line 1, before any test module)"
inject_at 1 "$PROBE"
expect "P1 top-of-file" fail 1
echo

# ---------------------------------------------------------------- position 2
say "P2 · violation at the END of the file (after the first #[cfg(test)]) — the B92 blind spot"
inject_at $((TOTAL_LINES + 1)) "$PROBE"
expect "P2 end-of-file" fail $((TOTAL_LINES + 1))
echo

# ---------------------------------------------------------------- position 3
say "P3 · production code AFTER a closed #[cfg(test)] module, mid-file"
inject_at "$FIRST_TEST_LINE" \
  "#[cfg(test)]" \
  "mod gate_probe_mod { fn _p() {} }" \
  "$PROBE"
expect "P3 after-closed-test-module" fail $((FIRST_TEST_LINE + 2))
echo

# ---------------------------------------------------------------- position 4
say "P4 · violation with a token reason (\`// reason: ok\`) — must STILL fail"
inject_at 1 "    // reason: ok" "$PROBE"
expect "P4 token-reason" fail 2
echo

# ------------------------------------------------------- positive control (5)
say "P5 · violation with a real reason above it — must pass"
inject_at 1 \
  "    // reason: the probe parses a literal that is known-good at compile" \
  "    // time; 0 here is unreachable, not a swallowed failure." \
  "$PROBE"
expect "P5 real-reason" pass ""
echo

# ------------------------------------------------- reverse assertion (6)
say "P6 · the SAME violation INSIDE the #[cfg(test)] module — must NOT trigger"
# +2: past the `#[cfg(test)]` line and the `mod tests {` line, i.e. inside.
inject_at $((FIRST_TEST_LINE + 2)) "$PROBE"
expect "P6 inside-test-module" pass ""
echo

# ---------------------------------------------------------------- clean tree
say "P7 · restore, then re-check the untouched tree — must pass"
restore
if ! cmp -s "$PRISTINE" "$PROBE_FILE"; then
  echo "[selftest] restore did not produce a byte-identical file"; harness_rc=2
fi
expect "P7 clean-tree" pass ""
echo

# ------------------------------------------------------- wiring / no-drift
say "W1 · release-gate.sh step 8 must call THIS checker (not an inline copy)"
if grep -q "check-semantic-defaults.py" "$ROOT/scripts/release-gate.sh"; then
  ok "W1: release-gate.sh invokes scripts/check-semantic-defaults.py"
else
  bad "W1: release-gate.sh does not reference the checker — step 8 may be a drifted copy, and this meta-test would then be testing nothing"
fi
echo

# ----------------------------------------------------------- clean work tree
say "C1 · work tree must be clean after the run (no probe left behind)"
if command -v git >/dev/null 2>&1 && git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
  dirty="$(git -C "$ROOT" status --porcelain -- "crates/chat-stasher/src/id.rs")"
  if [ -z "$dirty" ]; then
    ok "C1: git status --porcelain is empty for the probe file"
  else
    bad "C1: probe file still modified: $dirty"
    harness_rc=2
  fi
else
  say "  (not a git work tree — relying on the cmp check above)"
fi
echo

# --------------------------------------------------------------------- verdict
say "assertions: $pass_count passed, $fail_count failed"
if [ "$harness_rc" -ne 0 ]; then
  echo "SELFTEST: HARNESS ERROR"
  exit 2
fi
if [ "$fail_count" -ne 0 ]; then
  echo "SELFTEST: FAIL — the semantic-defaults gate does not hold at every position"
  exit 1
fi
echo "SELFTEST: PASS — the semantic-defaults gate catches the probe at every injected position, and stays out of test code"
exit 0
