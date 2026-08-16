#!/usr/bin/env bash
#
# release-gate.sh — one command that runs the whole chat-stasher chain and
# answers "ready to release? PASS / FAIL" with exit code 0 / 1.
#
# Chain exercised: 造夹具(封口分片) -> push -> push(幂等) -> read --all-machines
#                -> verify l1 -> verify l3 -> doctor
#
# Negative self-check (--selftest): injects 1 byte into a sealed shard in the
# *staging source tree* (never the pack), re-pushes, and re-runs verify l3
# against the ORIGINAL (pre-corruption) golden. The gate MUST end GATE: FAIL.
# A selftest that still passes means the gate is broken.
#
# Privacy line: only ids / counts / bytes / sha256 prefixes are ever printed.
# Real conversation payloads are read as opaque fixture bytes and never shown.
#
# Usage:
#   bash scripts/release-gate.sh            # happy path, must end GATE: PASS
#   bash scripts/release-gate.sh --selftest # negative path, must end GATE: FAIL

set -euo pipefail

# --------------------------------------------------------------------- config
BIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/debug/chat-stasher"
SRC_ROOT="${CLAUDE_PROJECTS_DIR:-$HOME/.claude/projects}"
MACHINE="gate-mbp"        # fixed path partition + snapshot host
N_SESSIONS=3              # real sessions to archive
SHARDS=4                  # sealed shards per session
TARGET_BYTES=$((1500 * 1024))   # ~1.5 MiB cap per session -> ~4.5 MiB total

SELFTEST="${1:-}"

START=$(date +%s)
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

STAGE="$TMP/stage"        # sealed staging tree (fed to push)
REPO="$TMP/repo"          # brand-new local rustic repository
KEY="$TMP/masterkey.json" # fresh masterkey (persisted on repo init)

gate()   { echo "[gate] $*"; }
elapsed(){ echo "$(( $(date +%s) - START ))s"; }

fail_gate() {
  echo "[gate] elapsed $(elapsed)"
  echo "GATE: FAIL"
  exit 1
}

[ -x "$BIN" ] || { echo "[gate] binary not found: $BIN (run cargo build first)"; fail_gate; }

# ------------------------------------------------------------------ helpers
# sha256 of a session's concatenated shards (seq order = lexicographic, 6-pad).
session_sha() {
  local dir="$1"
  cat "$dir"/[0-9]*.jsonl | shasum -a 256 | awk '{print $1}'
}
session_bytes() {
  local dir="$1"
  cat "$dir"/[0-9]*.jsonl | wc -c | tr -d ' '
}
shard_count() {
  ls "$1"/[0-9]*.jsonl 2>/dev/null | wc -l | tr -d ' '
}

# ------------------------------------------------------------------- step 1
gate "step 1/7 · build fixtures (cp real jsonl -> sealed shards)"
mkdir -p "$STAGE"
sources=()
while IFS= read -r p; do sources+=("$p"); done < <(
  find "$SRC_ROOT" -name '*.jsonl' -type f -exec stat -f '%z %N' {} \; 2>/dev/null \
    | sort -rn | head -n "$N_SESSIONS" | awk '{print $2}'
)
if [ "${#sources[@]}" -lt "$N_SESSIONS" ]; then
  echo "[gate] only ${#sources[@]} candidate sessions found under $SRC_ROOT"
  fail_gate
fi

# session metadata: (id, shard_count, bytes, sha) — the golden manifest.
declare -a SESS_IDS SESS_SHA
n=0
for src in "${sources[@]}"; do
  n=$((n + 1))
  sid="gate-${n}"
  sdir="$STAGE/sessions/$MACHINE/$sid"
  mkdir -p "$sdir"

  # cap the real session to TARGET_BYTES, keeping whole lines.
  cap="$TMP/cap.$n"
  head -c "$TARGET_BYTES" "$src" > "$cap"
  lines=$(wc -l < "$cap")
  per=$(( (lines + SHARDS - 1) / SHARDS ))
  if [ "$per" -lt 1 ]; then per=1; fi

  seq=1
  for ((s = 1; s <= SHARDS; s++)); do
    start=$(( (s - 1) * per + 1 ))
    end=$(( s * per ))
    if (( start > lines )); then break; fi
    if (( end > lines )); then end="$lines"; fi
    sed -n "${start},${end}p" "$cap" > "$(printf '%s/%06d.jsonl' "$sdir" "$seq")"
    seq=$((seq + 1))
  done

  sha=$(session_sha "$sdir")
  SESS_IDS+=("$sid")
  SESS_SHA+=("$sha")
  bytes=$(session_bytes "$sdir")
  shards=$(shard_count "$sdir")
  gate "  fixture $sid  shards=$shards  bytes=$bytes  sha256=${sha:0:12}"
done
gate "step 1 OK · ${#sources[@]} real sessions, ~$(du -sk "$STAGE" | awk '{print $1}') KiB staging"

# ------------------------------------------------------------------- step 2
gate "step 2/7 · push -> fresh local repo"
if ! out=$("$BIN" push --stage "$STAGE" --repo "$REPO" --key-file "$KEY" \
              --machine "$MACHINE" --no-reap 2>&1); then
  echo "$out"; echo "[gate] push failed"; fail_gate
fi
echo "$out" | sed 's/^/  /'
if ! echo "$out" | grep -q 'INIT'; then
  echo "[gate] first push did not init a fresh repository"; fail_gate
fi
gate "step 2 OK · repo created, masterkey persisted"

# ------------------------------------------------------------------- step 3
gate "step 3/7 · repeat push -> assert data_added == 0 (idempotent)"
if ! out=$("$BIN" push --stage "$STAGE" --repo "$REPO" --key-file "$KEY" \
              --machine "$MACHINE" --no-reap 2>&1); then
  echo "$out"; echo "[gate] repeat push failed"; fail_gate
fi
echo "$out" | sed 's/^/  /'
data_added=$(echo "$out" | sed -n 's/.*data_added=\([0-9]*\).*/\1/p' | tail -1)
if [ "$data_added" != "0" ]; then
  echo "[gate] repeat push added $data_added bytes (expected 0) — idempotency broken"
  fail_gate
fi
gate "step 3 OK · data_added=0"

# ------------------------------------------------------------------- step 4
gate "step 4/7 · read --all-machines -> assert per-session sha == source"
if ! out=$("$BIN" read --all-machines --repo "$REPO" --key-file "$KEY" --no-reap 2>&1); then
  echo "$out"; echo "[gate] read --all-machines failed"; fail_gate
fi
echo "$out" | sed 's/^/  /'
seen=0
for i in "${!SESS_IDS[@]}"; do
  sid="${SESS_IDS[$i]}"
  want="${SESS_SHA[$i]}"
  got=$(echo "$out" | awk -v s="$sid" '$1=="session" && $2==s {print $NF}' | sed 's/^sha256=//')
  if [ -z "$got" ]; then
    echo "[gate] session $sid missing from read --all-machines"; fail_gate
  fi
  if [ "$got" != "$want" ]; then
    echo "[gate] session $sid sha mismatch: read=$got expected=$want"; fail_gate
  fi
  gate "  $sid readback sha256=${got:0:12} == source (${want:0:12})"
  seen=$((seen + 1))
done
[ "$seen" -eq "$N_SESSIONS" ] || { echo "[gate] not all sessions verified"; fail_gate; }
gate "step 4 OK · ${seen}/${N_SESSIONS} sessions sha256 match source"

# ------------------------------------------------------------------- step 5
gate "step 5/7 · verify --level l1"
if ! out=$("$BIN" verify --level l1 --stage "$STAGE" --repo "$REPO" --key-file "$KEY" \
              --machine "$MACHINE" --no-reap 2>&1); then
  echo "$out"; echo "[gate] verify l1 FAILED"; fail_gate
fi
echo "$out" | sed 's/^/  /'
echo "$out" | grep -q 'RESULT         : OK' || { echo "[gate] verify l1 not OK"; fail_gate; }
gate "step 5 OK · l1 structure check passed"

# ------------------------------------------------------------------- step 6
gate "step 6/7 · verify --level l3"
if ! out=$("$BIN" verify --level l3 --stage "$STAGE" --repo "$REPO" --key-file "$KEY" \
              --machine "$MACHINE" --no-reap 2>&1); then
  echo "$out"; echo "[gate] verify l3 FAILED"; fail_gate
fi
echo "$out" | sed 's/^/  /'
echo "$out" | grep -q 'L3 verdict       : OK' || { echo "[gate] verify l3 not OK"; fail_gate; }
gate "step 6 OK · l3 reconcile: archived content == sealed originals"

# ------------------------------------------------------------------- step 7
gate "step 7/7 · doctor (runs without error)"
if ! out=$("$BIN" doctor 2>&1); then
  echo "$out"; echo "[gate] doctor errored"; fail_gate
fi
gate "step 7 OK · doctor ran clean"

# ---------------------------------------------------------------- selftest
if [ "$SELFTEST" = "--selftest" ]; then
  echo
  gate "SELFTEST · injecting 1 byte into a sealed shard (staging SOURCE, not pack)"
  # Snapshot the pristine staging tree as the ORIGINAL golden manifest.
  GOLDEN="$TMP/golden"
  mkdir -p "$GOLDEN"
  cp -r "$STAGE/sessions" "$GOLDEN/sessions"

  # Pick one sealed shard in the LIVE staging tree and XOR one byte at its
  # midpoint (guarantees the byte changes -> sha256 must change).
  target=$(find "$STAGE/sessions" -name '*.jsonl' | sort | head -1)
  size=$(stat -f %z "$target")
  pos=$(( size / 2 ))
  orig=$(dd if="$target" bs=1 skip="$pos" count=1 2>/dev/null | od -An -tu1 | tr -d ' ')
  new=$(( orig ^ 0x01 ))
  printf "\\$(printf '%03o' "$new")" | dd of="$target" bs=1 seek="$pos" count=1 conv=notrunc 2>/dev/null
  gate "  corrupted staging source shard: ${target#$STAGE/}"
  gate "  byte at pos $pos (of $size): 0x$(printf '%02x' "$orig") -> 0x$(printf '%02x' "$new")"

  gate "SELFTEST · re-push (corrupted shard -> newest snapshot)"
  if ! out=$("$BIN" push --stage "$STAGE" --repo "$REPO" --key-file "$KEY" \
                --machine "$MACHINE" --no-reap 2>&1); then
    echo "$out"; echo "[gate] selftest push failed"; fail_gate
  fi
  echo "$out" | sed 's/^/  /'

  gate "SELFTEST · verify --level l3 against ORIGINAL golden (pristine staging)"
  if out=$("$BIN" verify --level l3 --stage "$GOLDEN" --repo "$REPO" --key-file "$KEY" \
             --machine "$MACHINE" --no-reap 2>&1); then
    rc=0
  else
    rc=$?
  fi
  echo "$out" | sed 's/^/  /'
  echo "[gate] selftest verify exit code = $rc (expect non-zero)"
  if [ "$rc" -eq 0 ]; then
    echo "[gate] SELFTEST BROKEN: verify l3 still PASS despite corrupted shard — gate is a no-op"
    fail_gate
  fi
  gate "SELFTEST OK · corruption detected, verify l3 failed as required"
  echo "[gate] elapsed $(elapsed)"
  echo "GATE: FAIL"
  exit 1
fi

# --------------------------------------------------------------------- pass
echo "[gate] elapsed $(elapsed)"
echo "GATE: PASS"
exit 0
