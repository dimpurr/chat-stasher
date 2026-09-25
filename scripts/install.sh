#!/usr/bin/env bash
# install.sh — download and install the prebuilt chat-stasher binary.
#
# Distribution:  curl -fsSL https://<host>/install.sh | sh
# We deliberately fetch the raw binary with curl rather than asking the user to
# download a zip in a browser. A browser download attaches com.apple.quarantine
# to the file, and macOS SIGKILLs (rc=137) an unsigned quarantined binary on
# first launch. curl does not set the quarantine flag, so this path needs no
# Apple signing/notarization.
set -euo pipefail

# ---------------------------------------------------------------------------
# 1. Pin the version (env-overridable). We never default to "latest": an
#    unbounded tag makes installs non-reproducible and lets a future bad
#    release silently reach everyone. Override with CHAT_STASHER_VERSION.
#
#    This default is the newest *stable* release, and moving it is a release
#    step (RELEASING.md step 4). It is never a `-dev` and never a `-rc.N`:
#    this is the version `curl | sh` installs for everyone who does not name
#    one, so a prerelease here would hand every new user an unreleased build.
#    Asking for a prerelease by name is exactly what the override is for.
# ---------------------------------------------------------------------------
VERSION="${CHAT_STASHER_VERSION:-0.4.0}"

# Where the binary + SHA256SUMS live. The default is the tagged GitHub release
# (https only). Override with CHAT_STASHER_BASE_URL, e.g. to test against a
# local file:// or https:// mirror.
BASE_URL="${CHAT_STASHER_BASE_URL:-https://github.com/dimpurr/chat-stasher/releases/download/v${VERSION}}"

# ---------------------------------------------------------------------------
# 3. Default install dir is ~/.local/bin — user-writable and already on most
#    PATHs, on macOS and Linux alike. Override with CHAT_STASHER_INSTALL_DIR.
# 5. We never sudo and never write to /usr/local or any system directory.
# ---------------------------------------------------------------------------
INSTALL_DIR="${CHAT_STASHER_INSTALL_DIR:-$HOME/.local/bin}"

# ---------------------------------------------------------------------------
# 6. Detect platform. macOS and Linux ship prebuilt binaries; anything else
#    gets a clear "unsupported" message instead of a silently broken install.
#
#    The Linux artifacts are built against musl and statically linked, so one
#    binary per architecture covers every distribution: the artifact does not
#    depend on which libc this machine has, and no glibc version has to be
#    guessed from userspace.
#
#    Windows is not an oversight — this is a POSIX shell script, and Windows
#    has no `sh` to run it with except the one Git Bash ships. Its prebuilt
#    binary is a release asset, so that branch points at the asset instead of
#    pretending this script can install it.
# ---------------------------------------------------------------------------
OS="$(uname -s | tr 'A-Z' 'a-z')"
ARCH="$(uname -m | tr 'A-Z' 'a-z')"
[ "$ARCH" = "aarch64" ] && ARCH="arm64"
TARGET="$OS-$ARCH"

case "$TARGET" in
  darwin-arm64|darwin-x86_64|linux-x86_64|linux-arm64) : ;;
  mingw*|msys*|cygwin*)
    cat >&2 <<EOF
Unsupported platform: ${TARGET}

This installer is a POSIX shell script and does not install the Windows build
of chat-stasher. Windows ships a prebuilt binary as a release asset instead:

  ${BASE_URL}/chat-stasher-windows-x86_64.exe

Download that file, put it anywhere on your PATH, and run
'chat-stasher doctor'. Its checksum is in ${BASE_URL}/SHA256SUMS.

To build from source instead:
  git clone https://github.com/dimpurr/chat-stasher
  cd chat-stasher && cargo build --release
See docs/install.md for details.
EOF
    exit 1
    ;;
  *)
    cat >&2 <<EOF
Unsupported platform: ${TARGET}

chat-stasher currently ships prebuilt binaries for macOS
(darwin-arm64 and darwin-x86_64) and Linux (linux-x86_64 and linux-arm64).
There is no working binary for your platform yet, so
this installer refuses to write a broken one.

To use chat-stasher on your platform, build from source:
  git clone https://github.com/dimpurr/chat-stasher
  cd chat-stasher && cargo build --release
See docs/install.md for details.
EOF
    exit 1
    ;;
esac

ARTIFACT="chat-stasher-${TARGET}"

# Work in a private temp dir so a failed download never leaves a partial binary
# in the destination. Removed on exit (success or failure).
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

# ---------------------------------------------------------------------------
# 7. Download binary + checksums. curl -f makes any HTTP/non-zero exit abort
#    under set -e, so a failed fetch, a flaky connection after retries, or a
#    full disk mid-write all hard-fail here rather than continuing.
# ---------------------------------------------------------------------------
echo "Downloading ${ARTIFACT} v${VERSION} ..."
curl -fsSL --fail --retry 3 -o "$TMP_DIR/$ARTIFACT" "$BASE_URL/$ARTIFACT"
curl -fsSL --fail --retry 3 -o "$TMP_DIR/SHA256SUMS" "$BASE_URL/SHA256SUMS"

# ---------------------------------------------------------------------------
# 2. Verify SHA256. On mismatch we MUST hard-fail — never "warn and continue".
#    We parse the expected digest for our exact artifact out of SHA256SUMS.
# ---------------------------------------------------------------------------
EXPECTED="$(awk -v a="$ARTIFACT" '$2 == a { print $1 }' "$TMP_DIR/SHA256SUMS")"
if [ -z "$EXPECTED" ]; then
  echo "error: SHA256SUMS has no entry for '${ARTIFACT}'" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "$TMP_DIR/$ARTIFACT" | awk '{print $1}')"
else
  # macOS ships shasum instead of sha256sum.
  ACTUAL="$(shasum -a 256 "$TMP_DIR/$ARTIFACT" | awk '{print $1}')"
fi

if [ "$(printf '%s' "$ACTUAL" | tr 'A-Z' 'a-z')" != "$(printf '%s' "$EXPECTED" | tr 'A-Z' 'a-z')" ]; then
  echo "error: sha256 mismatch for ${ARTIFACT}" >&2
  echo "  expected  ${EXPECTED}" >&2
  echo "  actual    ${ACTUAL}" >&2
  echo "Refusing to install a binary that does not match its checksum." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Install. chmod first, then move into place.
# 4. Idempotent: mkdir -p is a no-op if the dir exists; mv -f overwrites a
#    previous install cleanly, so re-running never errors out.
# 7. If the target dir is unwritable or the disk is full, mv fails; we catch
#    it here and print a clear message instead of letting set -e abort with a
#    bare code.
# ---------------------------------------------------------------------------
chmod +x "$TMP_DIR/$ARTIFACT"

# ---------------------------------------------------------------------------
# 9. Start it, before it is installed. The checksum proved these are the bytes
#    the release published; it did not prove they run here, and those are two
#    different claims. The release workflow starts each binary on the runner
#    that built it, which is a native machine of the same architecture — so
#    what is left unproven is this machine's kernel and libc, and a musl build
#    is chosen precisely so that nothing about the distribution matters.
#
#    A binary that already exists at the destination is only overwritten once
#    the replacement is known to work, so a failed check leaves a working
#    install untouched rather than replacing it with one that cannot start.
#
#    This runs from $TMP_DIR, so a machine that mounts its temp directory
#    noexec would fail here rather than in the binary; the message says so and
#    names the way out.
# ---------------------------------------------------------------------------
SMOKE_OUTPUT=""
if ! SMOKE_OUTPUT="$("$TMP_DIR/$ARTIFACT" --version 2>&1)"; then
  echo "error: ${ARTIFACT} matched its checksum but did not run on this machine:" >&2
  printf '  %s\n' "${SMOKE_OUTPUT:-<no output>}" >&2
  echo "Nothing was installed." >&2
  echo "If you are sure this platform is the one the artifact is for, check" >&2
  echo "that your temp directory allows executables (TMPDIR) and re-run." >&2
  exit 1
fi

if ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
  echo "error: cannot create install directory: ${INSTALL_DIR}" >&2
  echo "       (set CHAT_STASHER_INSTALL_DIR to a writable path)" >&2
  exit 1
fi

DEST="$INSTALL_DIR/chat-stasher"
if ! mv -f "$TMP_DIR/$ARTIFACT" "$DEST" 2>/dev/null; then
  echo "error: cannot write ${DEST}" >&2
  echo "       (is ${INSTALL_DIR} writable? is the disk full?)" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 8. Tell the user whether the binary will be on their PATH. We NEVER modify
#    the user's shell config files — we only print a hint.
# ---------------------------------------------------------------------------
if ! printf '%s' "$PATH" | tr ':' '\n' | grep -Fxq "$INSTALL_DIR"; then
  cat >&2 <<EOF

Note: ${INSTALL_DIR} is not on your PATH yet.
Add it yourself (for example) with one of:

  echo 'export PATH="\$HOME/.local/bin:\$PATH"' >> ~/.zshrc
  echo 'export PATH="\$HOME/.local/bin:\$PATH"' >> ~/.bashrc

Your shell config was not modified.
EOF
fi

echo "Installed chat-stasher v${VERSION} to ${DEST}"
echo "Run 'chat-stasher doctor' to verify the install."
