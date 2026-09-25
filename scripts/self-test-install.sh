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
#   6. linux-x86_64 / linux-arm64 -> installs that architecture's artifact
#   7. windows (mingw/msys/cygwin uname) -> refuses and names the .exe asset
#   8. darwin-x86_64 -> the same install as the host's own darwin target
#   9. payload that does not run -> refused, and a working install is kept
#  10. release publishes no binary for this platform -> refused, naming the
#      release, the platform and what the release does carry
#  11. manifest lists our artifact but the asset is not downloadable -> refused
#
# Cases 1-4 are driven by the host's own platform, so on a macOS machine they
# cover the darwin path and only the darwin path. Cases 6-8 exist because that
# is a hole once more than one OS is supported: the refusal branch, the
# architecture normalisation (`aarch64` -> `arm64`) and the artifact name that
# follows from both are only reachable by pretending to be another machine, so
# `uname` is shadowed on PATH and install.sh's own detection code is what runs.
#
# EVERY CASE RUNS TWICE: under `bash`, and under `dash` when dash is installed.
# The second run is the one the documented install path needs. `install.sh` is
# shipped into `sh`, and on the distributions the Linux binaries are for, `sh`
# is dash — where `set -o pipefail` is an illegal option. While this suite only
# ever invoked `bash`, that bashism was invisible to it: the suite was green and
# the user's install died on line 10. `/bin/sh` is not a substitute for dash
# here, because on macOS it is bash under another name and accepts the same
# bashism; the run is skipped, in as many words, when dash is genuinely absent.
#
# `shellcheck --shell=sh` runs too, when the linter is installed. The default
# `shellcheck` mode is bash, which accepts `pipefail` — the same blind spot in
# static form — so `--shell=sh` is the flag that carries the property. The lint
# is skipped loudly rather than silently when shellcheck is absent; CI asserts
# both tools are present before this runs, so the skip cannot become the normal
# case in the one place that is meant to catch it.
#
# Usage: bash scripts/self-test-install.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALL_SH="$ROOT/scripts/install.sh"
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

HOST_OS="$(uname -s | tr 'A-Z' 'a-z')"
HOST_ARCH="$(uname -m | tr 'A-Z' 'a-z')"
[ "$HOST_ARCH" = "aarch64" ] && HOST_ARCH="arm64"
HOST_TARGET="$HOST_OS-$HOST_ARCH"
PASSED=0
FAILED=0

say()  { printf '[selftest] %s\n' "$1"; }
pass() { printf '[selftest]   PASS · %s\n' "$1"; PASSED=$((PASSED + 1)); }
fail() { printf '[selftest]   FAIL · %s\n' "$1"; FAILED=$((FAILED + 1)); }

# A mock dist holding one artifact, checksummed, named whatever the platform
# under test would ask for. Echoes the directory to serve.
mock_dist() {
  local artifact="$1" dir="$TMP/dist-$1" hash
  mkdir -p "$dir"
  printf '#!/bin/sh\necho fake-chat-stasher\n' > "$dir/$artifact"
  chmod +x "$dir/$artifact"
  hash="$(shasum -a 256 "$dir/$artifact" | awk '{print $1}')"
  ( cd "$dir" && printf '%s  %s\n' "$hash" "$artifact" > SHA256SUMS )
  printf '%s' "$dir"
}

# A fake `uname` reporting a chosen platform, so install.sh's platform
# detection runs rather than the host's. Echoes the directory to prepend to
# PATH. The `-s` / `-m` shapes are all install.sh asks for.
make_uname() {
  local os="$1" arch="$2" dir="$TMP/uname-$1-$2"
  mkdir -p "$dir"
  cat > "$dir/uname" <<SH
#!/bin/sh
case "\$1" in
  -s) echo "$os" ;;
  -m) echo "$arch" ;;
  *)  echo "$os" ;;
esac
SH
  chmod +x "$dir/uname"
  printf '%s' "$dir"
}

# Every case, parameterised by the shell install.sh is invoked with. It is a
# function so the suite can be run once per shell without a second copy of any
# assertion — the coverage has to be the same in both runs or it is not
# coverage. `$WORK` is the run's private directory, so the two runs cannot
# collide on a destination or see each other's leftovers.
run_suite() {
  local SHELL_UNDER_TEST="$1"
  local WORK="$TMP/run-$SHELL_UNDER_TEST"
  mkdir -p "$WORK"

  say "--- $SHELL_UNDER_TEST: $(command -v "$SHELL_UNDER_TEST") ---"

  local DIST="$WORK/dist"
  local MOCK_ARTIFACT="chat-stasher-$HOST_TARGET"
  local MOCK_BIN="$DIST/$MOCK_ARTIFACT"
  local FAKE_HASH
  local INSTALL_DIR="$WORK/install"
  local BASE="file://$DIST"

  # --- build a mock dist with a known-content fake binary ------------------
  mkdir -p "$DIST"
  printf '#!/bin/sh\necho fake-chat-stasher\n' > "$MOCK_BIN"
  chmod +x "$MOCK_BIN"
  # sha256 of the fake binary; install.sh should find this exact value.
  FAKE_HASH="$(shasum -a 256 "$MOCK_BIN" | awk '{print $1}')"
  ( cd "$DIST" && printf '%s  %s\n' "$FAKE_HASH" "$MOCK_ARTIFACT" > SHA256SUMS )

  # 1) happy path ------------------------------------------------------------
  if CHAT_STASHER_BASE_URL="$BASE" \
     CHAT_STASHER_INSTALL_DIR="$INSTALL_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" >/dev/null 2>&1; then
    if [ -x "$INSTALL_DIR/chat-stasher" ]; then pass "installs binary + executable"
    else fail "installed but not executable"; fi
    if [ "$(shasum -a 256 "$INSTALL_DIR/chat-stasher" | awk '{print $1}')" = "$FAKE_HASH" ]; then
      pass "installed bytes match source"
    else fail "installed bytes differ"; fi
  else
    fail "happy path returned non-zero"
  fi

  # 2) idempotent ------------------------------------------------------------
  if CHAT_STASHER_BASE_URL="$BASE" \
     CHAT_STASHER_INSTALL_DIR="$INSTALL_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" >/dev/null 2>&1; then
    pass "re-run is idempotent (exit 0)"
  else
    fail "re-run failed"
  fi

  # 3) bad hash: corrupt the served binary's checksum -------------------------
  local BAD_HASH_DIST="$WORK/dist-bad"
  mkdir -p "$BAD_HASH_DIST"
  cp "$MOCK_BIN" "$BAD_HASH_DIST/$MOCK_ARTIFACT"
  printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" \
    "$MOCK_ARTIFACT" > "$BAD_HASH_DIST/SHA256SUMS"

  local BAD_DIR="$WORK/bad-install"
  if CHAT_STASHER_BASE_URL="file://$BAD_HASH_DIST" \
     CHAT_STASHER_INSTALL_DIR="$BAD_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" >/dev/null 2>&1; then
    fail "bad sha256 did NOT hard-fail"
  else
    pass "bad sha256 hard-fails"
    if [ -e "$BAD_DIR/chat-stasher" ]; then fail "bad-hash case wrote a binary"
    else pass "bad-hash case wrote nothing"; fi
  fi

  # 4) SHA256SUMS missing our artifact ----------------------------------------
  # The manifest names a file that is not ours, so the installer must refuse
  # before it downloads anything: "this release has no binary for you" and
  # "the download failed" are different situations and must not share a message.
  local MISSING_DIST="$WORK/dist-missing"
  local MISSING_DIR="$WORK/missing-install"
  local MISSING_OUT="" MISSING_RC=0
  mkdir -p "$MISSING_DIST"
  cp "$MOCK_BIN" "$MISSING_DIST/$MOCK_ARTIFACT"
  printf '%s  %s\n' "$FAKE_HASH" "some-other-file" > "$MISSING_DIST/SHA256SUMS"

  MISSING_OUT="$(CHAT_STASHER_BASE_URL="file://$MISSING_DIST" \
     CHAT_STASHER_INSTALL_DIR="$MISSING_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" 2>&1)" || MISSING_RC=$?

  if [ "$MISSING_RC" = 0 ]; then
    fail "missing artifact in SHA256SUMS did NOT fail"
  else
    pass "missing artifact in SHA256SUMS hard-fails"
    if printf '%s' "$MISSING_OUT" | grep -q "$MOCK_ARTIFACT"; then
      pass "refusal names the artifact it could not find"
    else fail "refusal does not name the artifact"; fi
    if printf '%s' "$MISSING_OUT" | grep -q "some-other-file"; then
      pass "refusal lists what the release does carry"
    else fail "refusal does not list what the release carries"; fi
    if [ -e "$MISSING_DIR/chat-stasher" ]; then fail "missing-artifact case wrote a binary"
    else pass "missing-artifact case wrote nothing"; fi
  fi

  # 5) unsupported platform ---------------------------------------------------
  # A platform with no prebuilt binary, forced by shadowing `uname` on PATH so
  # install.sh's platform-detection branch genuinely runs. This case used to be
  # `linux x86_64`; Linux is supported now, so the example moved to an OS that is
  # still unshipped rather than the assertion being dropped.
  local UNSUPPORTED_OUT="" UNSUPPORTED_RC=0
  UNSUPPORTED_OUT="$(PATH="$(make_uname freebsd x86_64):$PATH" \
     CHAT_STASHER_BASE_URL="$BASE" \
     CHAT_STASHER_INSTALL_DIR="$WORK/unsupported-install" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" 2>&1)" || UNSUPPORTED_RC=$?

  if [ "$UNSUPPORTED_RC" = 0 ]; then
    fail "unsupported platform did NOT refuse"
  else
    pass "unsupported platform refuses with non-zero exit"
    if printf '%s' "$UNSUPPORTED_OUT" | grep -q 'freebsd-x86_64'; then
      pass "unsupported platform names the target it refused"
    else fail "unsupported message does not name the target"; fi
    if [ -e "$WORK/unsupported-install/chat-stasher" ]; then
      fail "unsupported platform wrote a binary"
    else pass "unsupported platform wrote nothing"; fi
  fi

  # 6) Linux: the artifact name follows the architecture, and musl is not a
  #    separate name to detect — one artifact per architecture.
  local platform LINUX_OS LINUX_UNAME_ARCH LINUX_ARTIFACT LINUX_DIST LINUX_DIR
  for platform in "linux x86_64 chat-stasher-linux-x86_64" "linux aarch64 chat-stasher-linux-arm64"; do
    read -r LINUX_OS LINUX_UNAME_ARCH LINUX_ARTIFACT <<< "$platform"
    LINUX_DIST="$(mock_dist "$LINUX_ARTIFACT")"
    LINUX_DIR="$WORK/install-$LINUX_ARTIFACT"
    if PATH="$(make_uname "$LINUX_OS" "$LINUX_UNAME_ARCH"):$PATH" \
       CHAT_STASHER_BASE_URL="file://$LINUX_DIST" \
       CHAT_STASHER_INSTALL_DIR="$LINUX_DIR" \
       "$SHELL_UNDER_TEST" "$INSTALL_SH" >/dev/null 2>&1; then
      if [ -x "$LINUX_DIR/chat-stasher" ]; then
        pass "installs ${LINUX_ARTIFACT} on ${LINUX_OS} ${LINUX_UNAME_ARCH}"
      else fail "${LINUX_ARTIFACT} installed but not executable"; fi
      if [ "$(shasum -a 256 "$LINUX_DIR/chat-stasher" | awk '{print $1}')" = \
           "$(shasum -a 256 "$LINUX_DIST/$LINUX_ARTIFACT" | awk '{print $1}')" ]; then
        pass "${LINUX_ARTIFACT} bytes match the served artifact"
      else fail "${LINUX_ARTIFACT} bytes differ"; fi
    else
      fail "installing ${LINUX_ARTIFACT} returned non-zero"
    fi
  done

  # 7) Windows: this installer cannot serve it, so it must say so and point at
  #    the release asset rather than downloading the wrong thing.
  local WINDOWS_OUT="" WINDOWS_RC=0
  WINDOWS_OUT="$(PATH="$(make_uname mingw64_nt-10.0 x86_64):$PATH" \
     CHAT_STASHER_BASE_URL="$BASE" \
     CHAT_STASHER_INSTALL_DIR="$WORK/windows-install" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" 2>&1)" || WINDOWS_RC=$?

  if [ "$WINDOWS_RC" = 0 ]; then
    fail "windows did NOT refuse"
  else
    pass "windows refuses with non-zero exit"
    if printf '%s' "$WINDOWS_OUT" | grep -q 'chat-stasher-windows-x86_64\.exe'; then
      pass "windows refusal names the .exe release asset"
    else fail "windows refusal does not name the .exe asset"; fi
    if printf '%s' "$WINDOWS_OUT" | grep -q "$BASE/chat-stasher-windows-x86_64.exe"; then
      pass "windows refusal gives the URL to fetch it from"
    else fail "windows refusal gives no URL"; fi
    if [ -e "$WORK/windows-install/chat-stasher" ]; then
      fail "windows case wrote a binary"
    else pass "windows case wrote nothing"; fi
  fi

  # 8) darwin-x86_64 installs the Intel artifact ---------------------------------
  # The one macOS branch a macOS arm64 host never takes on its own.
  local DARWIN_X86_DIST DARWIN_X86_DIR
  DARWIN_X86_DIST="$(mock_dist chat-stasher-darwin-x86_64)"
  DARWIN_X86_DIR="$WORK/install-darwin-x86_64"
  if PATH="$(make_uname darwin x86_64):$PATH" \
     CHAT_STASHER_BASE_URL="file://$DARWIN_X86_DIST" \
     CHAT_STASHER_INSTALL_DIR="$DARWIN_X86_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" >/dev/null 2>&1; then
    if [ -x "$DARWIN_X86_DIR/chat-stasher" ]; then pass "installs chat-stasher-darwin-x86_64"
    else fail "darwin-x86_64 installed but not executable"; fi
  else
    fail "installing chat-stasher-darwin-x86_64 returned non-zero"
  fi

  # 9) a payload that matched its checksum and still does not run ---------------
  # "The bytes are the ones the release published" and "they start on this
  # machine" are two claims, and the checksum only makes the first. On Linux the
  # second one has no other guard: a musl binary is built for the architecture,
  # but nothing had started it on the machine it was installed to.
  local BROKEN_DIST="$WORK/dist-wont-run"
  local BROKEN_ARTIFACT="chat-stasher-$HOST_TARGET"
  local BROKEN_DIR="$WORK/wont-run-install"
  local BROKEN_OUT="" BROKEN_RC=0
  mkdir -p "$BROKEN_DIST"
  # A payload that is genuinely executable and genuinely fails: a shebang script
  # that exits 7. Its checksum matches, so the only thing that can refuse it is
  # having run it.
  printf '#!/bin/sh\nexit 7\n' > "$BROKEN_DIST/$BROKEN_ARTIFACT"
  chmod +x "$BROKEN_DIST/$BROKEN_ARTIFACT"
  printf '%s  %s\n' "$(shasum -a 256 "$BROKEN_DIST/$BROKEN_ARTIFACT" | awk '{print $1}')" \
    "$BROKEN_ARTIFACT" > "$BROKEN_DIST/SHA256SUMS"

  BROKEN_OUT="$(CHAT_STASHER_BASE_URL="file://$BROKEN_DIST" \
     CHAT_STASHER_INSTALL_DIR="$BROKEN_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" 2>&1)" || BROKEN_RC=$?

  if [ "$BROKEN_RC" = 0 ]; then
    fail "a payload that does not run was installed anyway"
  else
    pass "a payload that does not run is refused"
    if [ -e "$BROKEN_DIR/chat-stasher" ]; then fail "failed smoke test still wrote a binary"
    else pass "failed smoke test wrote nothing"; fi
  fi

  # The same failure over an install that already works: the check runs before
  # the move, so what is on disk must be byte-for-byte what was there before.
  local KEEP_DIR="$WORK/keep-install"
  local KEEP_BEFORE
  mkdir -p "$KEEP_DIR"
  printf '#!/bin/sh\necho already-installed\n' > "$KEEP_DIR/chat-stasher"
  chmod +x "$KEEP_DIR/chat-stasher"
  KEEP_BEFORE="$(shasum -a 256 "$KEEP_DIR/chat-stasher" | awk '{print $1}')"
  CHAT_STASHER_BASE_URL="file://$BROKEN_DIST" \
    CHAT_STASHER_INSTALL_DIR="$KEEP_DIR" \
    "$SHELL_UNDER_TEST" "$INSTALL_SH" >/dev/null 2>&1 || true
  if [ "$(shasum -a 256 "$KEEP_DIR/chat-stasher" | awk '{print $1}')" = "$KEEP_BEFORE" ]; then
    pass "a refused install left the existing binary untouched"
  else fail "a refused install replaced a working binary"; fi

  # 10) the release exists and carries no binary for this platform --------------
  # This is the shape the version this installer defaults to actually has:
  # v0.4.0's assets are two macOS binaries, the extension zip and SHA256SUMS
  # (gh release view v0.4.0, 2026-09-25). A Linux user who runs the documented
  # command with no CHAT_STASHER_VERSION lands here, so it is not enough to
  # fail: the message has to say which release, which platform, what that
  # release does carry, and that the version is the default one.
  #
  # CHAT_STASHER_VERSION is deliberately NOT set here, so the run takes the
  # installer's own default whatever it is — the property under test is "the
  # default version is named and called the default", which stays true when the
  # default moves; pinning today's value would make this case a tripwire for a
  # release step instead.
  local NOASSET_DIST="$WORK/dist-macos-only"
  local NOASSET_DIR="$WORK/noasset-install"
  local NOASSET_OUT="" NOASSET_RC=0
  local DEFAULT_VERSION
  DEFAULT_VERSION="$(sed -n 's/^DEFAULT_VERSION="\(.*\)"$/\1/p' "$INSTALL_SH")"
  if [ -z "$DEFAULT_VERSION" ]; then
    fail "cannot read DEFAULT_VERSION from $INSTALL_SH (did its declaration change shape?)"
  fi
  mkdir -p "$NOASSET_DIST"
  local f
  for f in chat-stasher-darwin-arm64 chat-stasher-darwin-x86_64; do
    printf '#!/bin/sh\necho fake-chat-stasher\n' > "$NOASSET_DIST/$f"
    chmod +x "$NOASSET_DIST/$f"
  done
  ( cd "$NOASSET_DIST" && shasum -a 256 chat-stasher-darwin-arm64 chat-stasher-darwin-x86_64 \
      > SHA256SUMS )

  NOASSET_OUT="$(PATH="$(make_uname linux x86_64):$PATH" \
     CHAT_STASHER_BASE_URL="file://$NOASSET_DIST" \
     CHAT_STASHER_INSTALL_DIR="$NOASSET_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" 2>&1)" || NOASSET_RC=$?

  if [ "$NOASSET_RC" = 0 ]; then
    fail "a release with no artifact for the platform was installed anyway"
  else
    pass "a release with no artifact for the platform refuses"
    if printf '%s' "$NOASSET_OUT" | grep -q 'chat-stasher-linux-x86_64'; then
      pass "refusal names the artifact the platform needs"
    else fail "refusal does not name the artifact the platform needs"; fi
    # `-n` first: an unreadable DEFAULT_VERSION would make grep -F "" match
    # everything, which is a PASS for the wrong reason.
    if [ -n "$DEFAULT_VERSION" ] && printf '%s' "$NOASSET_OUT" | grep -qF "$DEFAULT_VERSION"; then
      pass "refusal names the default release version (${DEFAULT_VERSION})"
    else fail "refusal does not name the default release version"; fi
    if printf '%s' "$NOASSET_OUT" | grep -q 'chat-stasher-darwin-arm64'; then
      pass "refusal lists what the release does carry"
    else fail "refusal does not list what the release carries"; fi
    if printf '%s' "$NOASSET_OUT" | grep -q 'no Linux binary at all'; then
      pass "refusal says the release holds no Linux binary of any architecture"
    else fail "refusal does not say the release holds no Linux binary"; fi
    if printf '%s' "$NOASSET_OUT" | grep -q 'defaults to'; then
      pass "refusal says this is the version the installer defaults to"
    else fail "refusal does not mention the default version"; fi
    if [ -e "$NOASSET_DIR/chat-stasher" ]; then
      fail "no-artifact case wrote a binary"
    else pass "no-artifact case wrote nothing"; fi
  fi

  # 11) the manifest lists our artifact and the asset is not downloadable -------
  # The opposite disagreement: the manifest promises a file the release does not
  # serve. That is a broken release, not a platform this installer cannot serve,
  # and the message must not blame the platform.
  local GONE_DIST="$WORK/dist-gone"
  local GONE_DIR="$WORK/gone-install"
  local GONE_OUT="" GONE_RC=0
  mkdir -p "$GONE_DIST"
  printf '%s  %s\n' "$FAKE_HASH" "$MOCK_ARTIFACT" > "$GONE_DIST/SHA256SUMS"

  GONE_OUT="$(CHAT_STASHER_BASE_URL="file://$GONE_DIST" \
     CHAT_STASHER_INSTALL_DIR="$GONE_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" 2>&1)" || GONE_RC=$?

  if [ "$GONE_RC" = 0 ]; then
    fail "a listed-but-absent asset did NOT fail"
  else
    pass "a listed-but-absent asset fails"
    if printf '%s' "$GONE_OUT" | grep -q 'could not be downloaded'; then
      pass "refusal distinguishes a broken release from an unserved platform"
    else fail "refusal does not distinguish the two failures"; fi
    if [ -e "$GONE_DIR/chat-stasher" ]; then fail "listed-but-absent case wrote a binary"
    else pass "listed-but-absent case wrote nothing"; fi
  fi

  # 12) an install dir that is already on PATH gets no PATH hint ----------------
  # 12 and 13 are a pair on purpose: 12 alone could pass by never printing the
  # hint at all, so 13 asserts the hint is still there to be suppressed.
  #
  # PATH is padded by a few thousand entries deliberately. The check this
  # replaced was `printf | tr | grep -Fxq`, and grep -q exits at the first
  # match — so with the match in the first component the writer was still
  # writing when grep exited, died of SIGPIPE, and under `pipefail` that
  # non-zero pipeline read as "not on PATH" and printed the hint to a user who
  # did not need it. Measured before the rewrite: 5 runs out of 5 printed the
  # hint wrongly at 4000 entries (40 KB of PATH), so 8000 is a tripwire with
  # margin rather than a race that has to be won.
  local ONPATH_DIR="$WORK/on-path-bin"
  local ONPATH_OUT="" ONPATH_RC=0
  local ONPATH_PATH
  mkdir -p "$ONPATH_DIR"
  ONPATH_PATH="$ONPATH_DIR:$PATH$(awk 'BEGIN { for (i = 0; i < 8000; i++) printf ":/pad-%d", i }')"

  ONPATH_OUT="$(PATH="$ONPATH_PATH" \
     CHAT_STASHER_BASE_URL="$BASE" \
     CHAT_STASHER_INSTALL_DIR="$ONPATH_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" 2>&1)" || ONPATH_RC=$?

  if [ "$ONPATH_RC" != 0 ]; then
    fail "install with a padded PATH returned non-zero"
  else
    pass "install with a padded PATH succeeds"
  fi
  if printf '%s' "$ONPATH_OUT" | grep -q 'is not on your PATH yet'; then
    fail "an install dir already on PATH still got the PATH hint"
  else
    pass "an install dir already on PATH gets no PATH hint"
  fi

  # 13) ... and a directory that is not on PATH still gets it -------------------
  local NOTON_DIR="$WORK/not-on-path-bin"
  local NOTON_OUT="" NOTON_RC=0
  mkdir -p "$NOTON_DIR"
  NOTON_OUT="$(CHAT_STASHER_BASE_URL="$BASE" \
     CHAT_STASHER_INSTALL_DIR="$NOTON_DIR" \
     "$SHELL_UNDER_TEST" "$INSTALL_SH" 2>&1)" || NOTON_RC=$?

  if [ "$NOTON_RC" != 0 ]; then
    fail "install to a directory off PATH returned non-zero"
  elif printf '%s' "$NOTON_OUT" | grep -q 'is not on your PATH yet'; then
    pass "a directory that is not on PATH still gets the hint"
  else
    fail "the PATH hint is gone for a directory that is not on PATH"
  fi
}

# --- what runs ---------------------------------------------------------------
# bash always. dash as well when it is installed: see the header for why the
# `sh` a Linux user has is the shell that matters, and why `/bin/sh` on this
# machine is not that shell.
run_suite bash

if command -v dash >/dev/null 2>&1; then
  run_suite dash
else
  say "SKIP · dash is not installed: every case ran under bash only."
  say "       The documented 'curl | sh' path puts this script under the system"
  say "       sh, which is dash on Debian and Ubuntu; bash does not cover it,"
  say "       and /bin/sh is not a substitute where it is bash under another"
  say "       name. CI asserts dash is present, so the skip cannot be silent"
  say "       in the place that has to catch it."
fi

# The syntax check, in the mode that carries the property. `shellcheck` without
# `--shell=sh` assumes bash and accepts exactly the construct that killed the
# documented install on dash, so the flag is the whole test.
if command -v shellcheck >/dev/null 2>&1; then
  if shellcheck --shell=sh "$INSTALL_SH"; then
    pass "shellcheck --shell=sh (POSIX) is clean"
  else
    fail "shellcheck --shell=sh reported problems"
  fi
else
  say "SKIP · shellcheck is not installed: --shell=sh was not checked here."
  say "       CI asserts the linter is present, so the skip cannot be silent"
  say "       in the place that has to catch it."
fi

echo
printf '[selftest] RESULT: %s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" = 0 ] || exit 1
