#!/usr/bin/env bash
# self-test-install.sh — offline self-test for scripts/install.sh.
#
# install.sh normally downloads from the network (the release is not published
# yet, so we must not hit the real network). Instead we build a mock dist with
# a fake binary + SHA256SUMS, serve it via file://, and exercise install.sh's
# happy path and its failure modes:
#
#   1. happy path  -> installs, binary present + executable, correct sha256
#   2. idempotent  -> re-running succeeds (exit 0) and still works
#   3. bad hash    -> install.sh must hard-fail (non-zero) and not install
#   4. bad target  -> a SHA256SUMS missing our artifact must hard-fail
#   5. unsupported platform -> must refuse with a clear message
#
# Usage: bash scripts/self-test-install.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALL_SH="$ROOT/scripts/install.sh"
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

DIST="$TMP/dist"
MOCK_BIN="$DIST/chat-stasher-darwin-arm64"
PASSED=0
FAILED=0

say()  { printf '[selftest] %s\n' "$1"; }
pass() { printf '[selftest]   PASS · %s\n' "$1"; PASSED=$((PASSED + 1)); }
fail() { printf '[selftest]   FAIL · %s\n' "$1"; FAILED=$((FAILED + 1)); }

# --- build a mock dist with a known-content fake binary --------------------
mkdir -p "$DIST"
printf '#!/bin/sh\necho fake-chat-stasher\n' > "$MOCK_BIN"
chmod +x "$MOCK_BIN"
# sha256 of the fake binary; install.sh should find this exact value.
FAKE_HASH="$(shasum -a 256 "$MOCK_BIN" | awk '{print $1}')"
( cd "$DIST" && printf '%s  %s\n' "$FAKE_HASH" "chat-stasher-darwin-arm64" > SHA256SUMS )

INSTALL_DIR="$TMP/install"
BASE="file://$DIST"

# 1) happy path --------------------------------------------------------------
if CHAT_STASHER_BASE_URL="$BASE" \
   CHAT_STASHER_INSTALL_DIR="$INSTALL_DIR" \
   bash "$INSTALL_SH" >/dev/null 2>&1; then
  if [ -x "$INSTALL_DIR/chat-stasher" ]; then pass "installs binary + executable"
  else fail "installed but not executable"; fi
  if [ "$(shasum -a 256 "$INSTALL_DIR/chat-stasher" | awk '{print $1}')" = "$FAKE_HASH" ]; then
    pass "installed bytes match source"
  else fail "installed bytes differ"; fi
else
  fail "happy path returned non-zero"
fi

# 2) idempotent --------------------------------------------------------------
if CHAT_STASHER_BASE_URL="$BASE" \
   CHAT_STASHER_INSTALL_DIR="$INSTALL_DIR" \
   bash "$INSTALL_SH" >/dev/null 2>&1; then
  pass "re-run is idempotent (exit 0)"
else
  fail "re-run failed"
fi

# 3) bad hash: corrupt the served binary's checksum ---------------------------
BAD_HASH_DIST="$TMP/dist-bad"
mkdir -p "$BAD_HASH_DIST"
cp "$MOCK_BIN" "$BAD_HASH_DIST/chat-stasher-darwin-arm64"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" \
  "chat-stasher-darwin-arm64" > "$BAD_HASH_DIST/SHA256SUMS"

BAD_DIR="$TMP/bad-install"
if CHAT_STASHER_BASE_URL="file://$BAD_HASH_DIST" \
   CHAT_STASHER_INSTALL_DIR="$BAD_DIR" \
   bash "$INSTALL_SH" >/dev/null 2>&1; then
  fail "bad sha256 did NOT hard-fail"
else
  pass "bad sha256 hard-fails"
  if [ -e "$BAD_DIR/chat-stasher" ]; then fail "bad-hash case wrote a binary"
  else pass "bad-hash case wrote nothing"; fi
fi

# 4) SHA256SUMS missing our artifact -----------------------------------------
MISSING_DIST="$TMP/dist-missing"
mkdir -p "$MISSING_DIST"
cp "$MOCK_BIN" "$MISSING_DIST/chat-stasher-darwin-arm64"
printf '%s  %s\n' "$FAKE_HASH" "some-other-file" > "$MISSING_DIST/SHA256SUMS"

MISSING_DIR="$TMP/missing-install"
if CHAT_STASHER_BASE_URL="file://$MISSING_DIST" \
   CHAT_STASHER_INSTALL_DIR="$MISSING_DIR" \
   bash "$INSTALL_SH" >/dev/null 2>&1; then
  fail "missing artifact in SHA256SUMS did NOT fail"
else
  pass "missing artifact in SHA256SUMS hard-fails"
fi

# 5) unsupported platform -----------------------------------------------------
# Force a non-darwin, non-arm64 target by shadowing `uname` on PATH so
# install.sh's platform-detection branch genuinely runs.
FAKE_UNAME="$TMP/fake-uname"
mkdir -p "$FAKE_UNAME"
cat > "$FAKE_UNAME/uname" <<'SH'
#!/bin/sh
case "$1" in
  -s) echo linux ;;
  -m) echo x86_64 ;;
  *)  echo linux ;;
esac
SH
chmod +x "$FAKE_UNAME/uname"

if PATH="$FAKE_UNAME:$PATH" \
   CHAT_STASHER_BASE_URL="$BASE" \
   CHAT_STASHER_INSTALL_DIR="$TMP/unsupported-install" \
   bash "$INSTALL_SH" >/dev/null 2>&1; then
  fail "unsupported platform did NOT refuse"
else
  pass "unsupported platform refuses with non-zero exit"
fi

echo
printf '[selftest] RESULT: %s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" = 0 ] || exit 1
