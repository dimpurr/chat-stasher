#!/usr/bin/env bash
#
# crates-version-state.sh — ask crates.io whether one exact crate version is
# already published, and say so in a way a caller cannot misread.
#
# It lives here rather than inline in the release workflow step for the same
# reason scripts/release-tag-gate.sh and scripts/check-static-binary.sh do: the
# rule has one owner. The workflow calls this script, and
# scripts/selftest-crates-version-state.sh drives the same script with `curl`
# shimmed on PATH, so the thing that is tested is the thing that runs.
#
# ---------------------------------------------------------------------------
# Why this exists at all: the 403 that cost a release
# ---------------------------------------------------------------------------
#
# The version this replaced was one line in release.yml:
#
#   status="$(curl -sS -o /dev/null -w '%{http_code}' \
#     "https://crates.io/api/v1/crates/chat-stasher/${VERSION}")"
#
# It failed on v0.5.0-rc.2 (run 36137101318) with `HTTP 403`, and the cause was
# the User-Agent. crates.io enforces a data-access policy (https://crates.io/
# data-access) and answers a request whose User-Agent only names the HTTP client
# library with a 403 — its own error body spells the rule out:
#
#   "We require that all requests include a `User-Agent` header. ... we ask that
#    your user agent actually identify your bot, and not just report the HTTP
#    client library you're using."
#   Bad:  User-Agent: reqwest/0.9.1
#   Best: User-Agent: my_crawler (my_crawler.com/info)
#
# curl sends `curl/<version>` on its own, which is the "Bad" case. So the
# request was never header-less; it identified the wrong thing. USER_AGENT below
# is the "Best" shape: a name, then contact in parentheses.
#
# Two further properties of that one line are worth naming, because they are
# what made the failure expensive to diagnose:
#
#   · `-o /dev/null` threw the response body away, so the 403 arrived with no
#     explanation attached. Worse, the 403 earned by `curl/<version>` has an
#     EMPTY body — measured, 0 bytes — so printing the body would not by itself
#     have helped. Both the status and the body are reported below, and neither
#     is discarded.
#   · **`-f` is deliberately not used here.** `curl -f` collapses every 4xx into
#     one generic failure, which is how the same bug elsewhere turned a 403 into
#     a false "not found" and caused a successfully published release candidate
#     to be rejected (p-diogo/totalreclaw PR #632, merged 2026-08-16). A 403 must
#     never read as "not published yet": that is the branch that publishes.
#
# ---------------------------------------------------------------------------
# Usage
# ---------------------------------------------------------------------------
#
#   bash scripts/crates-version-state.sh --crate chat-stasher --version 0.5.0
#
#   --crate NAME     required. The crate name.
#   --version V      required. The exact version, without a leading `v`.
#   --api-base URL   optional. Defaults to https://crates.io/api/v1. The API
#                    origin is a parameter so the check can be pointed at a
#                    recorded transcript; it is not a knob any release sets.
#
# On stdout, exactly one machine-readable line and nothing else:
#
#   state=published   crates.io has this exact version      -> exit 0
#   state=absent      crates.io does not have it (yet)      -> exit 0
#
# Everything a human reads goes to stderr, including, on an unreadable answer,
# the HTTP status, the URL and the response body verbatim.
#
# ---------------------------------------------------------------------------
# Exit codes
# ---------------------------------------------------------------------------
#
#   0  the question was answered — either way. "absent" is a measurement, not a
#      failure: a version that is not there yet is exactly when a publish is
#      supposed to proceed.
#   3  the question was NOT answered, so `absent` must not be inferred from this
#      run: a transport failure, a timeout, or any status other than 200/404 —
#      a 403, a 429, a 5xx, a captive portal. The workflow stops here rather
#      than guessing, and invariant 2 is why this is 3 and not 1: 1 would say
#      "we looked and it failed", and what actually happened is that we did not
#      finish looking, so an absence proves nothing.
#   2  usage error. Nothing was asked and nothing was proven.
#
# There is deliberately no exit 1. This script either answers or it does not;
# there is no third outcome where it finished and the answer is a refusal.
#
# It reads no file, writes no file, and its only side effect is one HTTP GET.

set -euo pipefail

# The "Best" shape from crates.io's own policy: a name that identifies the bot,
# then contact information. Sent on every request this script makes.
USER_AGENT="chat-stasher-release (https://github.com/dimpurr/chat-stasher)"

# Bounded so a hung connection cannot hold a release job open indefinitely. A
# timeout is not an answer, so it lands on exit 3 with everything else that is
# not a 200 or a 404.
CONNECT_TIMEOUT=10
MAX_TIME=30

API_BASE="https://crates.io/api/v1"
CRATE=""
VERSION=""

usage() {
  echo "usage: bash scripts/crates-version-state.sh --crate NAME --version V [--api-base URL]" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --crate)
      [ $# -ge 2 ] || { echo "crates-version-state: --crate needs a value" >&2; usage; exit 2; }
      CRATE="$2"; shift 2 ;;
    --version)
      [ $# -ge 2 ] || { echo "crates-version-state: --version needs a value" >&2; usage; exit 2; }
      VERSION="$2"; shift 2 ;;
    --api-base)
      [ $# -ge 2 ] || { echo "crates-version-state: --api-base needs a value" >&2; usage; exit 2; }
      API_BASE="$2"; shift 2 ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      echo "crates-version-state: unknown argument: $1" >&2
      usage
      exit 2 ;;
  esac
done

if [ -z "$CRATE" ]; then
  echo "crates-version-state: --crate is required" >&2
  usage
  exit 2
fi
if [ -z "$VERSION" ]; then
  echo "crates-version-state: --version is required" >&2
  usage
  exit 2
fi

URL="${API_BASE%/}/crates/${CRATE}/${VERSION}"

BODY="$(mktemp "${TMPDIR:-/tmp}/cs-crates-body.XXXXXX")"
trap 'rm -f "$BODY"' EXIT

# `-sS`: quiet, but still print errors. The status comes back through `-w` and
# the body goes to a file, so nothing is discarded.
#
# A transport failure is separated from an HTTP status on purpose. curl exits
# non-zero when it could not complete the exchange (DNS, TLS, connect, timeout),
# and that is "we did not finish asking" — exit 3 — not an HTTP answer to
# interpret.
if ! STATUS="$(curl -sS \
      --connect-timeout "$CONNECT_TIMEOUT" \
      --max-time "$MAX_TIME" \
      -H "User-Agent: ${USER_AGENT}" \
      -o "$BODY" \
      -w '%{http_code}' \
      "$URL")"; then
  {
    echo "error: could not complete the crates.io version lookup."
    echo "       url:    ${URL}"
    echo "       curl could not finish the request (transport, TLS or timeout),"
    echo "       so whether ${CRATE} ${VERSION} is published is UNKNOWN. Nothing"
    echo "       was published and nothing was skipped."
  } >&2
  exit 3
fi

case "$STATUS" in
  200)
    printf 'state=published\n'
    ;;
  404)
    printf 'state=absent\n'
    ;;
  *)
    {
      echo "error: crates.io version lookup did not answer (HTTP ${STATUS})."
      echo "       url:    ${URL}"
      echo "       status: ${STATUS}"
      echo "       Whether ${CRATE} ${VERSION} is published is UNKNOWN, so this"
      echo "       run stops rather than guessing. A 403 here is usually the"
      echo "       User-Agent policy (https://crates.io/data-access) and a 429 is"
      echo "       rate limiting; both are the registry refusing to answer, not"
      echo "       the crate being absent."
      echo "       body follows, verbatim:"
      cat "$BODY" >&2
      # The body can legitimately be empty — the 403 that `curl/<version>`
      # earns is 0 bytes — so say so rather than leaving a blank where an
      # explanation looks like it should be.
      if [ ! -s "$BODY" ]; then
        echo "       (the response body was empty)"
      fi
    } >&2
    exit 3
    ;;
esac

exit 0
