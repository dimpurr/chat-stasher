#!/bin/sh
# install.sh — download and install the prebuilt chat-stasher binary.
#
# Distribution:  curl -fsSL https://<host>/install.sh | sh
#
# It is a POSIX shell script on purpose, and that is a property with a test:
# the documented command pipes it into `sh`, and on the distributions the Linux
# binaries exist for, `sh` is dash — which rejects `set -o pipefail` as an
# illegal option and exits 2 before reading a line of this file. So the language
# here is the portable subset: no `pipefail`, no `[[ ]]`, no arrays, no `local`,
# no `${var^^}`. scripts/self-test-install.sh runs every one of its cases
# through dash as well as bash for that reason, and `shellcheck --shell=sh` is
# the static form of the same check (plain `shellcheck` assumes bash and would
# accept the bashism). `/bin/sh` is not a substitute for dash: on macOS it is
# bash under another name.
#
# We deliberately fetch the raw binary with curl rather than asking the user to
# download a zip in a browser. A browser download attaches com.apple.quarantine
# to the file, and macOS SIGKILLs (rc=137) an unsigned quarantined binary on
# first launch. curl does not set the quarantine flag, so this path needs no
# Apple signing/notarization.
set -eu

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
#
#    It is also the version whose manifest is read back below, and not every
#    release carries every platform — a release is published from whatever the
#    workflow built for its tag, so a version that predates a platform has no
#    artifact for it. The default is named separately here so a refusal can say
#    "this is the version you got without asking for one" rather than leaving
#    the reader to work out why the documented command failed.
# ---------------------------------------------------------------------------
DEFAULT_VERSION="0.4.0"
VERSION="${CHAT_STASHER_VERSION:-$DEFAULT_VERSION}"

# Where the binary + SHA256SUMS live. The default is the tagged GitHub release
# (https only). Override with CHAT_STASHER_BASE_URL, e.g. to test against a
# local file:// or https:// mirror.
BASE_URL="${CHAT_STASHER_BASE_URL:-https://github.com/dimpurr/chat-stasher/releases/download/v${VERSION}}"
RELEASES_URL="https://github.com/dimpurr/chat-stasher/releases"

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
# `tr '[:upper:]' '[:lower:]'` rather than `tr 'A-Z' 'a-z'`: both are POSIX and
# the two agree on every string this script lowercases (`uname -s`, `uname -m`,
# a hex digest), but only the first says what it means. `A-Z` is a range over
# whatever collation the locale defines, which is the case shellcheck SC2018 /
# SC2019 describe — and in `--shell=sh` mode, the mode this file is checked in,
# an info-level finding is a failing exit code.
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m | tr '[:upper:]' '[:lower:]')"
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
See docs-dev/install.md for details.
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
See docs-dev/install.md for details.
EOF
    exit 1
    ;;
esac

ARTIFACT="chat-stasher-${TARGET}"

# Work in a private temp dir so a failed download never leaves a partial binary
# in the destination. Removed on exit (success or failure).
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

if ! command -v curl >/dev/null 2>&1; then
  echo "error: curl is required to download chat-stasher and is not on PATH." >&2
  echo "       Install curl and re-run, or build from source: docs-dev/install.md." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 7. Download the release manifest (SHA256SUMS) first, and the artifact second.
#
#    The order is the point. SHA256SUMS is the release's own list of what it
#    carries, so reading it first is what keeps three situations apart, which
#    is the difference between a message and a bare curl 404:
#
#      * the release could not be reached  -> say so, and say that nothing is
#        therefore known about what it carries;
#      * the release was read and does not carry this artifact -> say which
#        release, which platform, and what it does carry (below);
#      * the manifest lists the artifact and the asset is not downloadable ->
#        that is a broken release, and it is not the platform's fault.
#
#    Only after the manifest names our artifact do we try to fetch it. curl -f
#    makes any HTTP/non-zero exit a failure, and that failure is handled where
#    it happens rather than being left to `set -e`, so a 404 says which of the
#    three it was.
# ---------------------------------------------------------------------------
if ! curl -fsSL --fail --retry 3 -o "$TMP_DIR/SHA256SUMS" "$BASE_URL/SHA256SUMS"; then
  cat >&2 <<EOF
error: could not download the release manifest:
  ${BASE_URL}/SHA256SUMS

Nothing is known about what this release carries: the manifest did not arrive,
which is not the same as the release not having it. Check that v${VERSION} is a
release (${RELEASES_URL}) and that this machine can reach it, then re-run.
Set CHAT_STASHER_BASE_URL to fetch from a mirror instead.
EOF
  exit 1
fi

# ---------------------------------------------------------------------------
# 2. Verify SHA256. On mismatch we MUST hard-fail — never "warn and continue".
#    We parse the expected digest for our exact artifact out of SHA256SUMS.
#
#    A missing entry is not a mismatch, it is a release that does not carry this
#    platform at all, so it gets its own message and its own evidence: the
#    artifact it lacks and the ones the release does have.
# ---------------------------------------------------------------------------
EXPECTED="$(awk -v a="$ARTIFACT" '$2 == a { print $1 }' "$TMP_DIR/SHA256SUMS")"
if [ -z "$EXPECTED" ]; then
  CARRIED="$(awk 'NF >= 2 { print "  " $2 }' "$TMP_DIR/SHA256SUMS")"
  [ -n "$CARRIED" ] || CARRIED="  (the manifest lists no artifacts at all)"

  # Why this release has no artifact for us, as far as the manifest can show.
  # This is derived from the manifest in hand, not from a remembered list of
  # releases, so it stays true as releases are published.
  NOTE=""
  case "$TARGET" in
    linux-*)
      if awk '$2 ~ /^chat-stasher-linux-/ { found = 1 } END { exit !found }' \
           "$TMP_DIR/SHA256SUMS"; then
        NOTE="v${VERSION} does carry a Linux binary, but not one for ${ARCH}."
      else
        NOTE="v${VERSION} holds no Linux binary at all, for either architecture."
        if [ "$VERSION" = "$DEFAULT_VERSION" ]; then
          NOTE="${NOTE}
It is also the version this installer defaults to, so on Linux the documented
'curl | sh' with no CHAT_STASHER_VERSION lands here."
        fi
      fi
      ;;
  esac

  cat >&2 <<EOF
error: release v${VERSION} does not carry an artifact for ${TARGET}.

There is no ${ARTIFACT} in v${VERSION}. The manifest was downloaded and
read, so this is not a network failure: the release is reachable and has no
binary for ${TARGET}. What it carries:

${CARRIED}
EOF
  # Printed only when there is something to say, so a macOS-only release asked
  # for on macOS does not get a paragraph about Linux or a stray blank line.
  if [ -n "$NOTE" ]; then
    printf '\n%s\n' "$NOTE" >&2
  fi
  cat >&2 <<EOF

Not every release carries every platform, so a newer one may have ${TARGET}.
Which assets each release holds is on the Releases page:

  ${RELEASES_URL}

Name a version that carries this platform and re-run the same command with it
set:

  CHAT_STASHER_VERSION=<version>

Or build from source, which needs no release artifact:

  git clone https://github.com/dimpurr/chat-stasher
  cd chat-stasher && cargo build --release
See docs-dev/install.md for details.

Nothing was installed.
EOF
  exit 1
fi

echo "Downloading ${ARTIFACT} v${VERSION} ..."
if ! curl -fsSL --fail --retry 3 -o "$TMP_DIR/$ARTIFACT" "$BASE_URL/$ARTIFACT"; then
  cat >&2 <<EOF
error: ${ARTIFACT} is listed in the release manifest but could not be downloaded:
  ${BASE_URL}/${ARTIFACT}

The manifest and the release disagree about a file the manifest promises. That
is a broken release rather than a platform this installer cannot serve, so
${TARGET} is not the thing to look at. Nothing was installed; please report it
at https://github.com/dimpurr/chat-stasher/issues, or build from source.
EOF
  exit 1
fi

# The digest is computed without a pipeline, so a tool that fails cannot look
# like a mismatch: a failing `sha256sum | awk` yields an empty digest under a
# shell without `pipefail`, and an empty digest compared against a real one is
# the right answer (refuse) for the wrong reason.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1"
  else
    # macOS ships shasum instead of sha256sum.
    shasum -a 256 "$1"
  fi
}

DIGEST_OUT=""
if ! DIGEST_OUT="$(sha256_of "$TMP_DIR/$ARTIFACT")"; then
  echo "error: could not compute the sha256 of ${ARTIFACT}." >&2
  echo "       Nothing was installed." >&2
  exit 1
fi
# "  <hex>  <filename>": the digest is everything before the first space.
ACTUAL="${DIGEST_OUT%% *}"

if [ "$(printf '%s' "$ACTUAL" | tr '[:upper:]' '[:lower:]')" != "$(printf '%s' "$EXPECTED" | tr '[:upper:]' '[:lower:]')" ]; then
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
#
#    A case pattern rather than `... | grep -Fxq`. Two reasons, and the first is
#    a real bug in the pipeline form: grep -q exits at the first match, which
#    can kill the writer with SIGPIPE, and under `pipefail` that non-zero
#    pipeline reads as "not on PATH" — it prints the hint to someone who has it.
#    The second is globbing: splitting $PATH on its own with `for d in $PATH`
#    subjects each entry to pathname expansion, so an entry containing `*` or
#    `?` would be replaced by whatever it matches. A quoted expansion inside a
#    case pattern is matched literally, and the surrounding `:`s make it a
#    whole-component match — `grep -Fxq`'s property, without either hazard.
# ---------------------------------------------------------------------------
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ON_PATH=1 ;;
  *)                  ON_PATH=0 ;;
esac

if [ "$ON_PATH" -eq 0 ]; then
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
