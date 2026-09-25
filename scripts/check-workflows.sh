#!/usr/bin/env bash
#
# check-workflows.sh — lint every workflow under .github/workflows/ with
# actionlint.
#
# This is the gate for the four files that decide what the project publishes and
# with whose credentials, and nothing else in the repository reads them. A
# workflow is the one program here that cannot be run locally before it ships:
# its syntax errors, unknown keys, unreachable `needs:` and misspelled step
# outputs are found by GitHub *when it runs*, which for release.yml means when a
# release is being published. actionlint reads them statically instead.
#
# Two decisions worth stating, because both look like omissions otherwise:
#
#   1. **The actionlint version is pinned, and the binary is fetched from the
#      upstream release.** An `actionlint` already on PATH is used when there is
#      one — that is what a developer wants — but the version it reports is
#      printed, so a run that used something other than the pin says so. The
#      download is checked against a sha256 pinned below, which is the only
#      thing standing between this script and executing whatever the network
#      handed it. Those four digests were verified by downloading all four
#      tarballs and matching them against the release's own
#      `actionlint_<version>_checksums.txt`, not copied out of it.
#
#   2. **shellcheck integration is off** (`-shellcheck=`). actionlint runs
#      shellcheck over every `run:` block when shellcheck is on PATH, and which
#      shellcheck that is depends on the machine — ubuntu-24.04 ships 0.9.0
#      while the current upstream release is 0.11.0, and the two do not report
#      the same set. A gate whose verdict depends on which optional linter the
#      runner happens to have installed is a gate that says different things in
#      CI and on a laptop, which is the one thing this repository's check list
#      must not do. actionlint's own rules are self-contained, so with this flag
#      the result depends only on the pinned version. shellcheck still runs over
#      install.sh, deliberately and with a pinned `--shell=sh`, in
#      scripts/self-test-install.sh.
#
#      (Exercised with shellcheck 0.11.0 in place, actionlint reports exactly
#      two findings in release.yml — SC2012, `ls` where `find` would be safer,
#      at the two places that read the staged asset directory. Both predate this
#      script and neither is a defect in the current code: the names compared
#      there are this project's own asset names. They are recorded here rather
#      than fixed here, because editing the asset-set equality check to satisfy
#      a linter is how a release integrity check changes meaning.)
#
# Usage:
#   bash scripts/check-workflows.sh              # skip, in as many words, when the tool cannot be obtained
#   bash scripts/check-workflows.sh --require    # a tool that cannot be obtained is a failure (CI)
#   bash scripts/check-workflows.sh --selftest   # prove the four branches below still do what they say
#
# `--selftest` exists for the same reason the other gates have one: the pinned
# digest is a security property, and a digest comparison that cannot be shown to
# fail is decoration. It shims `uname` and `curl` on PATH — the technique
# scripts/self-test-install.sh uses to reach branches the host cannot — and
# asserts that the skip, the require-failure and the digest-mismatch paths each
# do what they claim.
#
# Exit codes: 0 = every workflow is clean (or the tool is missing and --require
# was not given) · 1 = actionlint reported something, or --require could not get
# the tool · 2 = usage error.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
ACTIONLINT_VERSION="1.7.7"

REQUIRE=0
SELFTEST=0
for arg in "$@"; do
  case "$arg" in
    --require) REQUIRE=1 ;;
    --selftest) SELFTEST=1 ;;
    *)
      echo "check-workflows: unknown argument: $arg" >&2
      echo "usage: bash scripts/check-workflows.sh [--require] [--selftest]" >&2
      exit 2
      ;;
  esac
done

# ---------------------------------------------------------------------------
# --selftest: drive this file as a child process, with a PATH that has no
# actionlint on it and shims for the two commands whose environment decides
# which branch runs.
# ---------------------------------------------------------------------------
if [ "$SELFTEST" = 1 ]; then
  SHIM="$(mktemp -d "${TMPDIR:-/tmp}/cs-check-workflows-selftest.XXXXXX")"
  trap 'rm -rf "$SHIM"' EXIT

  cat >"$SHIM/uname" <<'SHIM_EOF'
#!/bin/sh
# Reports a platform with no pinned digest, so the download branch is entered
# on a machine that in fact has one.
case "$1" in
  -s) echo "${CS_SHIM_UNAME_S:-Linux}" ;;
  -m) echo "${CS_SHIM_UNAME_M:-sparc64}" ;;
  *) echo unknown ;;
esac
SHIM_EOF

  cat >"$SHIM/curl" <<'SHIM_EOF'
#!/bin/sh
# Stands in for the release download: writes whatever the caller asked for, or
# fails, depending on how the probe set the environment.
[ "${CS_SHIM_CURL_FAIL:-0}" = 1 ] && exit 22
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$out" ] && printf 'this is not an actionlint tarball\n' >"$out"
exit 0
SHIM_EOF
  chmod +x "$SHIM/uname" "$SHIM/curl"

  # /usr/bin and /bin only: neither carries actionlint, so `command -v` misses
  # even on a machine where the developer has one installed.
  PROBES=0
  FAILED=0
  SHIM_UNAME_M=""
  SHIM_CURL_FAIL=""

  # probe <description> <expected-exit> <expected-substring|-> <script-arg|->
  # The args are spelled out rather than collected in an array: macOS ships
  # bash 3.2, where expanding an empty array under `set -u` is an unbound
  # variable, and this file has to run there.
  probe() {
    desc="$1"; want_rc="$2"; want_text="$3"; extra="$4"
    PROBES=$((PROBES + 1))
    set +e
    if [ "$extra" = "-" ]; then
      out="$(env PATH="$SHIM:/usr/bin:/bin" \
        CS_SHIM_UNAME_M="$SHIM_UNAME_M" \
        CS_SHIM_CURL_FAIL="$SHIM_CURL_FAIL" \
        bash "$SELF" 2>&1)"
    else
      out="$(env PATH="$SHIM:/usr/bin:/bin" \
        CS_SHIM_UNAME_M="$SHIM_UNAME_M" \
        CS_SHIM_CURL_FAIL="$SHIM_CURL_FAIL" \
        bash "$SELF" "$extra" 2>&1)"
    fi
    rc=$?
    set -e
    bad=""
    [ "$rc" = "$want_rc" ] || bad="exit ${rc}, wanted ${want_rc}"
    if [ "$want_text" != "-" ] && ! printf '%s' "$out" | grep -q "$want_text"; then
      bad="${bad:+${bad} and }did not mention '${want_text}'"
    fi
    if [ -n "$bad" ]; then
      FAILED=$((FAILED + 1))
      echo "FAIL: ${desc}: ${bad}" >&2
      printf '%s\n' "$out" | sed 's/^/      /' >&2
    fi
  }

  # A platform with no pin is a skip by default and a failure under --require.
  SHIM_UNAME_M="sparc64"
  probe "an unpinned platform skips" 0 "SKIPPED" -
  probe "--require turns that skip into a failure" 1 "no pinned build" --require

  # A download that fails is the same shape. From here the shim reports a
  # platform that IS pinned, so the run gets past the table and reaches curl.
  SHIM_UNAME_M="x86_64"
  SHIM_CURL_FAIL=1
  probe "a failed download skips" 0 "could not download" -
  probe "--require turns a failed download into a failure" 1 "could not download" --require

  # The digest check: the shim now serves bytes that cannot match the pin. This
  # is the shape the pin exists for, and the one probe that proves the
  # comparison is wired to the result rather than merely printed.
  SHIM_CURL_FAIL=""
  probe "bytes that do not match the pinned digest are refused" 1 "does not match its pinned digest" -
  probe "  ... and the refusal is not a skip" 1 "Nothing was run" -

  # The other pinned architecture resolves too, so the table is not a single
  # answer that happens to fit one probe.
  SHIM_UNAME_M="arm64"
  probe "a pinned arm64 platform reaches the download" 1 "does not match its pinned digest" -

  if [ "$FAILED" -ne 0 ]; then
    echo "check-workflows: SELFTEST: FAIL (${FAILED} of ${PROBES} probes failed)" >&2
    exit 1
  fi
  echo "check-workflows: SELFTEST: PASS (${PROBES} probes)"
  exit 0
fi

# The digest of each release tarball this project can run: the two Linux
# architectures CI uses and the two macOS ones a developer's laptop has.
sha256_for() {
  case "$1" in
    linux_amd64)  echo "023070a287cd8cccd71515fedc843f1985bf96c436b7effaecce67290e7e0757" ;;
    linux_arm64)  echo "401942f9c24ed71e4fe71b76c7d638f66d8633575c4016efd2977ce7c28317d0" ;;
    darwin_amd64) echo "28e5de5a05fc558474f638323d736d822fff183d2d492f0aecb2b73cc44584f5" ;;
    darwin_arm64) echo "2693315b9093aeacb4ebd91a993fea54fc215057bf0da2659056b4bc033873db" ;;
    *) return 1 ;;
  esac
}

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# Both extensions, because GitHub accepts both and a `.yaml` workflow that was
# silently not linted would be the same green-but-unchecked result this script
# exists to remove. The globs are filtered by existence so an unmatched one
# contributes nothing, and the count is read before the array is expanded:
# under bash 3.2 — what macOS ships — expanding an empty array with `set -u` is
# an unbound-variable error, while reading its length is not.
workflow_files=()
for extension in yml yaml; do
  for candidate in "$ROOT"/.github/workflows/*."$extension"; do
    if [ -e "$candidate" ]; then
      workflow_files+=("$candidate")
    fi
  done
done
if [ "${#workflow_files[@]}" = 0 ]; then
  echo "check-workflows: no workflows found under ${ROOT}/.github/workflows" >&2
  exit 1
fi

TMP=""
# `if`, not `[ ] &&`: this runs from an EXIT trap, and bash takes the exit status
# of that trap as the script's own when the trap does not call `exit`. A
# `[ -n "$TMP" ] && rm -rf "$TMP"` with an empty TMP returns 1 and turns every
# skip below into a failure — which is what the selftest's first probe caught.
cleanup() {
  if [ -n "$TMP" ]; then
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

# ---- pick a binary ----------------------------------------------------------
if command -v actionlint >/dev/null 2>&1; then
  ACTIONLINT="actionlint"
  echo "check-workflows: using the actionlint on PATH (pinned version is ${ACTIONLINT_VERSION})"
else
  # `uname -s`/`-m` are kept whole for the messages below and normalized only
  # for the table lookup, so a platform with no pin is named as the machine
  # reports it rather than as "unknown".
  UNAME_S="$(uname -s)"
  UNAME_M="$(uname -m)"
  case "$UNAME_S" in
    Linux) os="linux" ;;
    Darwin) os="darwin" ;;
    *) os="" ;;
  esac
  case "$UNAME_M" in
    x86_64 | amd64) arch="amd64" ;;
    arm64 | aarch64) arch="arm64" ;;
    *) arch="" ;;
  esac
  platform="${os}_${arch}"

  wanted=""
  if [ -n "$os" ] && [ -n "$arch" ]; then
    wanted="$(sha256_for "$platform" || true)"
  fi

  if [ -z "$wanted" ]; then
    if [ "$REQUIRE" = 1 ]; then
      echo "error: actionlint is not on PATH and this platform (${UNAME_S}/${UNAME_M}) has no pinned build." >&2
      echo "       Install it, or add its digest to sha256_for() in $0." >&2
      exit 1
    fi
    echo "check-workflows: SKIPPED — actionlint is not on PATH and this platform" >&2
    echo "                 (${UNAME_S}/${UNAME_M}) has no pinned build here." >&2
    exit 0
  fi

  asset="actionlint_${ACTIONLINT_VERSION}_${platform}.tar.gz"
  url="https://github.com/rhysd/actionlint/releases/download/v${ACTIONLINT_VERSION}/${asset}"
  TMP="$(mktemp -d "${TMPDIR:-/tmp}/cs-actionlint.XXXXXX")"

  if ! curl -fsSL -o "$TMP/$asset" "$url"; then
    if [ "$REQUIRE" = 1 ]; then
      echo "error: could not download ${url}" >&2
      exit 1
    fi
    echo "check-workflows: SKIPPED — could not download ${url}" >&2
    exit 0
  fi

  got="$(file_sha256 "$TMP/$asset")"
  if [ "$got" != "$wanted" ]; then
    {
      echo "error: ${asset} does not match its pinned digest."
      echo "       expected: ${wanted}"
      echo "       got:      ${got}"
      echo "       Nothing was run. The release page or the pin has changed."
    } >&2
    exit 1
  fi

  if ! tar -xzf "$TMP/$asset" -C "$TMP" actionlint; then
    echo "error: ${asset} did not contain an actionlint binary" >&2
    exit 1
  fi
  ACTIONLINT="$TMP/actionlint"
  echo "check-workflows: using actionlint ${ACTIONLINT_VERSION} (downloaded, digest verified)"
fi

# ---- lint -------------------------------------------------------------------
"$ACTIONLINT" --version | head -1
# -shellcheck= : see decision 2 at the top of this file.
"$ACTIONLINT" -shellcheck= "${workflow_files[@]}"
echo "check-workflows: PASS (${#workflow_files[@]} workflows)"
