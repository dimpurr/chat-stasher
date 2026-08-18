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
#   bash scripts/release-gate.sh                   # synthetic happy path
#   bash scripts/release-gate.sh --selftest        # synthetic negative path
#   bash scripts/release-gate.sh --real-data       # explicit real-data opt-in

set -euo pipefail

# --------------------------------------------------------------------- config
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/chat-stasher"
MACHINE="gate-mbp"        # fixed path partition + snapshot host
N_SESSIONS=3              # sessions to archive in either fixture mode
SHARDS=4                  # sealed shards per session
TARGET_BYTES=$((1500 * 1024))   # ~1.5 MiB cap per session -> ~4.5 MiB total
SYNTH_LINES=20000         # deterministic JSONL source; cap still leaves >16 KiB shards

SELFTEST=0
REAL_DATA=0
for arg in "$@"; do
  case "$arg" in
    --selftest) SELFTEST=1 ;;
    --real-data) REAL_DATA=1 ;;
    *) echo "[gate] unknown argument: $arg"; exit 2 ;;
  esac
done

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
gate "step 1/8 · build fixtures (JSONL -> sealed shards)"
mkdir -p "$STAGE"
sources=()
if [ "$REAL_DATA" -eq 1 ]; then
  SRC_ROOT="${CLAUDE_PROJECTS_DIR:-$HOME/.claude/projects}"
  if [ -n "${CLAUDE_PROJECTS_DIR:-}" ]; then
    SRC_LABEL="CLAUDE_PROJECTS_DIR override"
  else
    SRC_LABEL='$HOME/.claude/projects'
  fi
  while IFS= read -r p; do sources+=("$p"); done < <(
    find "$SRC_ROOT" -name '*.jsonl' -type f -exec stat -f '%z %N' {} \; 2>/dev/null \
      | sort -rn | head -n "$N_SESSIONS" | awk '{print $2}'
  )
  if [ "${#sources[@]}" -lt "$N_SESSIONS" ]; then
    echo "[gate] only ${#sources[@]} candidate sessions found under $SRC_LABEL"
    fail_gate
  fi
  gate "!!! REAL DATA OPT-IN: cp ${#sources[@]} files from $SRC_LABEL (cp only; no mv) !!!"
else
  SYNTH_ROOT="$TMP/synthetic-sources"
  mkdir -p "$SYNTH_ROOT"
  for ((n = 1; n <= N_SESSIONS; n++)); do
    synth="$SYNTH_ROOT/synthetic-$n.jsonl"
    awk -v sid="$n" -v count="$SYNTH_LINES" 'BEGIN {
      for (i = 1; i <= count; i++)
        printf "{\"session\":\"synthetic-%d\",\"seq\":%d,\"text\":\"B25 synthetic payload %06d\"}\n", sid, i, i
    }' > "$synth"
    sources+=("$synth")
  done
  gate "synthetic mode: generated ${#sources[@]} deterministic JSONL sessions; no real-data directory access"
fi

# session metadata: (id, shard_count, bytes, sha) — the golden manifest.
declare -a SESS_IDS SESS_SHA
n=0
for src in "${sources[@]}"; do
  n=$((n + 1))
  sid="gate-${n}"
  sdir="$STAGE/sessions/$MACHINE/$sid"
  mkdir -p "$sdir"

  # Copy the selected source into scratch, then cap to TARGET_BYTES, keeping whole lines.
  cap="$TMP/cap.$n"
  cp "$src" "$cap"
  if [ "$(wc -c < "$cap")" -gt "$TARGET_BYTES" ]; then
    head -c "$TARGET_BYTES" "$cap" > "$cap.truncated"
    cp "$cap.truncated" "$cap"
  fi
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
if [ "$REAL_DATA" -eq 0 ]; then
  max_shard_bytes=0
  for sid in "${SESS_IDS[@]}"; do
    sdir="$STAGE/sessions/$MACHINE/$sid"
    shards=$(shard_count "$sdir")
    [ "$shards" -ge 3 ] || { echo "[gate] synthetic $sid has $shards shards (expected >= 3)"; fail_gate; }
    session_max=$(find "$sdir" -name '*.jsonl' -type f -exec wc -c {} \; | awk 'max < $1 { max = $1 } END { print max + 0 }')
    [ "$session_max" -gt "$max_shard_bytes" ] && max_shard_bytes="$session_max"
    gate "  synthetic shape $sid  shards=$shards  max_shard_bytes=$session_max"
  done
  [ "$max_shard_bytes" -gt 16384 ] || { echo "[gate] synthetic max shard is $max_shard_bytes bytes (expected > 16384)"; fail_gate; }
  gate "synthetic shape OK · every session has >=3 shards; max shard >16 KiB"
fi
gate "step 1 OK · ${#sources[@]} sessions, ~$(du -sk "$STAGE" | awk '{print $1}') KiB staging"

# ------------------------------------------------------------------- step 2
gate "step 2/8 · push -> fresh local repo"
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
gate "step 3/8 · repeat push -> assert data_added == 0 (idempotent)"
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
gate "step 4/8 · read --all-machines -> assert per-session sha == source"
if ! out=$("$BIN" read --all-machines --repo "$REPO" --key-file "$KEY" --no-reap 2>&1); then
  echo "$out"; echo "[gate] read --all-machines failed"; fail_gate
fi
echo "$out" | sed 's/^/  /'
seen=0
for i in "${!SESS_IDS[@]}"; do
  sid="${SESS_IDS[$i]}"
  want="${SESS_SHA[$i]}"
  # B54: `read` prints the *short* session id (`id::short_session_id`), which is
  # `<first 8 chars>~<6 hex of sha256(full id)>` — a bare 8-char prefix does not
  # distinguish two sessions of the same platform. Match on the head, not on the
  # whole field, or this assertion only passes for ids of 8 characters or fewer.
  got=$(echo "$out" | awk -v s="$sid" \
    '$1=="session" { split($2, p, "~"); if (p[1]==substr(s,1,8)) print $NF }' | sed 's/^sha256=//')
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
gate "step 5/8 · verify --level l1"
if ! out=$("$BIN" verify --level l1 --stage "$STAGE" --repo "$REPO" --key-file "$KEY" \
              --machine "$MACHINE" --no-reap 2>&1); then
  echo "$out"; echo "[gate] verify l1 FAILED"; fail_gate
fi
echo "$out" | sed 's/^/  /'
echo "$out" | grep -q 'RESULT         : OK' || { echo "[gate] verify l1 not OK"; fail_gate; }
gate "step 5 OK · l1 structure check passed"

# ------------------------------------------------------------------- step 6
gate "step 6/8 · verify --level l3"
if ! out=$("$BIN" verify --level l3 --stage "$STAGE" --repo "$REPO" --key-file "$KEY" \
              --machine "$MACHINE" --no-reap 2>&1); then
  echo "$out"; echo "[gate] verify l3 FAILED"; fail_gate
fi
echo "$out" | sed 's/^/  /'
echo "$out" | grep -q 'L3 verdict       : OK' || { echo "[gate] verify l3 not OK"; fail_gate; }
gate "step 6 OK · l3 reconcile: archived content == sealed originals"

# ------------------------------------------------------------------- step 7
if [ "$REAL_DATA" -eq 0 ]; then
  DOCTOR_HOME="$TMP/doctor-home"
  mkdir -p "$DOCTOR_HOME"
  gate "step 7/8 · doctor (synthetic mode, isolated HOME)"
  if ! out=$(HOME="$DOCTOR_HOME" "$BIN" doctor 2>&1); then
    echo "$out"; echo "[gate] doctor errored"; fail_gate
  fi
else
  gate "step 7/8 · doctor (real-data opt-in)"
  if ! out=$("$BIN" doctor 2>&1); then
    echo "$out"; echo "[gate] doctor errored"; fail_gate
  fi
fi
gate "step 7 OK · doctor ran clean"

# ------------------------------------------------------------------- step 8
gate "step 8/8 · check semantic defaults in production code"
# B93: the checker itself lives in scripts/check-semantic-defaults.py so that
# the meta-test scripts/gate-selftest-semantic.sh can exercise the SAME code
# this gate runs, instead of a second copy that would drift.
# Exit codes: 0 = clean · 1 = violations · anything else = the checker itself
# broke. That last case must FAIL the gate, not pass it: a checker that
# crashed printed no violations, and "printed no violations" is exactly what
# clean code looks like. Never let a dead checker read as OK.
if SEMANTIC_CHECK=$(python3 "$ROOT/scripts/check-semantic-defaults.py" "$ROOT" 2>&1); then
  semantic_rc=0
else
  semantic_rc=$?
fi

if [ "$semantic_rc" -gt 1 ]; then
  echo "$SEMANTIC_CHECK"
  echo "[gate] step 8 FAILED · 语义默认值检查器自身报错（exit $semantic_rc），本步的结论无效"
  fail_gate
fi

if [ "$semantic_rc" -eq 1 ] && [[ "$SEMANTIC_CHECK" != FAILED:* ]]; then
  echo "$SEMANTIC_CHECK"
  echo "[gate] step 8 FAILED · 检查器退出码为 1 但没有输出违规清单，输出与退出码不自洽"
  fail_gate
fi

if [[ "$SEMANTIC_CHECK" == FAILED:* ]]; then
  count=$(echo "$SEMANTIC_CHECK" | head -n 1 | cut -d: -f2)
  echo "[gate] step 8 FAILED · 发现 $count 处未注明理由的语义默认值！"
  echo "[gate] 规则说明：你在这里把未知变成了一个具体值（0 / false / 空列表），如果这是有意的，请在上一行写："
  echo "[gate]   // reason: <为什么这个默认值是诚实的>"
  echo "$SEMANTIC_CHECK" | tail -n +2
  fail_gate
fi
gate "step 8 OK · semantic defaults in production code carry reason annotations"

# ---------------------------------------------------------------- selftest
if [ "$SELFTEST" -eq 1 ]; then
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
