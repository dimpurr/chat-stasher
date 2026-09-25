#!/usr/bin/env bash
#
# The selftest for scripts/crates-version-state.sh: proof that the lookup
# answers "published", answers "absent", and — the point of the whole script —
# refuses to answer anything else, in particular the 403 that cost v0.5.0-rc.2
# its crates.io publish (run 36137101318).
#
# It drives the real script, with `curl` shimmed on PATH, the way
# scripts/selftest-check-static-binary.sh shims `file` and `readelf`, and
# scripts/check-workflows.sh shims `curl` and `uname`. The shim does two things
# the real curl cannot be asked to do on demand: it returns an arbitrary status
# with an arbitrary body, and it records its own argv so the *request shape* can
# be asserted — which is where the actual bug lived.
#
# The three probes that matter most, and what each one would catch:
#
#   · **A 403 is not an absence.** Exit must be 3 and stdout must be empty. If
#     this ever regresses to "absent", the next release publishes over a version
#     that may already exist — the failure p-diogo/totalreclaw PR #632 shipped.
#   · **A descriptive User-Agent is sent.** Every request must carry a UA that
#     names the project and its contact URL, and must not carry a bare client
#     string. This is the regression guard for the exact defect.
#   · **`-f` is never used.** `curl -f` collapses every 4xx into one generic
#     failure, which is the mechanism that made the 403 above unreadable in the
#     other project. Asserted on the recorded argv rather than on behaviour.
#
# Fixture provenance, because it is not uniform. The 403 body is a **verbatim**
# record of what crates.io returned on 2026-09-25 to a request whose
# User-Agent was `curl/8.x` — the real policy text, with the request id it
# quoted replaced by `<request-id>` because that value is per-request and would
# otherwise look like something the test depends on. The 429 and 500 bodies are
# NOT recorded: they are short plausible shapes, and nothing in the script reads
# them, so they exist only to prove the script forwards a body it does not
# understand.
#
# One property of the real 403 is pinned deliberately, because it is the reason
# the original error was so bare: the 403 earned by `curl/<version>` has an
# **empty body**. Both shapes are probed — body and no body — and the no-body
# one must still say so rather than printing a blank line.
#
# Exit codes: 0 = every probe behaved · 1 = at least one did not · 2 = usage.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT/scripts/crates-version-state.sh"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cs-crates-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

SHIM="$TMP/shim"
BODY="$TMP/body"
mkdir -p "$SHIM" "$BODY"

ARGV_LOG="$TMP/curl-argv.log"
: >"$ARGV_LOG"

PROBES=0
FAILED=0

# ---------------------------------------------------------------------------
# The shim. It answers with CS_CURL_STATUS and bodies from CS_CURL_BODY (named
# with the leading `@` — `@403` means `$BODY/403.json`), writes that body to
# whatever path `-o` names, prints the status on stdout the way `-w` would, and
# always records its own argv first so the request shape can be audited.
#
# CS_CURL_FAIL=1 makes it exit non-zero WITHOUT printing a status, which is how
# a transport/TLS/timeout failure looks to the caller: curl never got an answer
# to report.
# ---------------------------------------------------------------------------
cat >"$SHIM/curl" <<'SHIM_EOF'
#!/bin/sh
printf '%s\n' "$*" >> "${CS_ARGV_LOG:-/dev/null}"

if [ "${CS_CURL_FAIL:-0}" = 1 ]; then
  echo "curl: (7) Failed to connect to crates.io port 443: Connection refused" >&2
  exit 7
fi

out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done

if [ -n "$out" ]; then
  case "${CS_CURL_BODY:-}" in
    ""|"-") : >"$out" ;;
    @*)     cat "${CS_BODY_DIR}/${CS_CURL_BODY#@}" >"$out" ;;
    *)      printf '%s' "$CS_CURL_BODY" >"$out" ;;
  esac
fi

printf '%s' "${CS_CURL_STATUS:-200}"
exit 0
SHIM_EOF
chmod +x "$SHIM/curl"

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

# --- the recorded 403. Verbatim from crates.io on 2026-09-25, answering a
# request that identified itself only as `curl/8.x`. This is the text the
# workflow could not print, because `-o /dev/null` had already discarded it.
cat >"$BODY/403.json" <<'JSON_EOF'
{"errors":[{"detail":"We require that all requests include a `User-Agent` header.  To allow us to determine the impact your bot has on our service, we ask that your user agent actually identify your bot, and not just report the HTTP client library you're using.  Including contact information will also reduce the chance that we will need to take action against your bot.\n\nBad:\n  User-Agent: reqwest/0.9.1\n\nBetter:\n  User-Agent: my_crawler\n\nBest:\n  User-Agent: my_crawler (my_crawler.com/info)\n  User-Agent: my_crawler (help@my_crawler.com)\n\nIf you believe you've received this message in error, please email help@crates.io and include the request id <request-id>.\n"}]}
JSON_EOF

# --- NOT recorded. Short shapes standing in for two other refusals; the script
# does not interpret either, it only has to stop and forward them.
printf '%s' '{"errors":[{"detail":"You have hit the rate limit."}]}' >"$BODY/429.json"
printf '%s' '<html><body><h1>500 Internal Server Error</h1></body></html>' >"$BODY/500.json"

# --- NOT recorded, and it matters that it is not: at the moment this probe
# describes, the crate itself does not exist, so crates.io's 404 body names the
# *crate*. The script must reach the same verdict as a 404 naming a missing
# version — "absent" — because that is the first-publish case, and it must not
# start parsing the body to tell the two apart.
printf '%s' '{"errors":[{"detail":"crate `chat-stasher` does not exist"}]}' >"$BODY/404-crate-missing.json"

# ---------------------------------------------------------------------------
# probe <description> <want-rc> <want-stdout|-exact|-> <want-stderr-substring|->
#       <status> <body|@name|-> <curl-fail 0|1>
#
# want-stdout is either the literal `-` (meaning: stdout must be EMPTY) or the
# exact single line stdout must be. Comparing exactly rather than by substring
# is the point: an "unknown" that leaks a `state=` line would pass a substring
# test and must not pass this one.
#
# want-stderr-substring begins with `!` to mean "this must NOT appear" — needed
# because a silent success and a success that leaked a warning into stderr both
# look like exit 0, and only the second is a defect.
# ---------------------------------------------------------------------------
probe() {
  desc="$1"; want_rc="$2"; want_stdout="$3"; want_err="$4"
  status="$5"; body="$6"; fail="$7"
  PROBES=$((PROBES + 1))

  set +e
  stdout="$(env PATH="$SHIM:/usr/bin:/bin" \
    CS_ARGV_LOG="$ARGV_LOG" \
    CS_BODY_DIR="$BODY" \
    CS_CURL_STATUS="$status" \
    CS_CURL_BODY="$body" \
    CS_CURL_FAIL="$fail" \
    bash "$SCRIPT" --crate chat-stasher --version 0.5.0-rc.2 2>"$TMP/stderr.txt")"
  rc=$?
  set -e
  stderr="$(cat "$TMP/stderr.txt")"

  bad=""
  [ "$rc" = "$want_rc" ] || bad="exit ${rc}, wanted ${want_rc}"
  if [ "$want_stdout" = "-" ]; then
    [ -z "$stdout" ] || bad="${bad:+${bad} and }stdout was not empty: '${stdout}'"
  else
    [ "$stdout" = "$want_stdout" ] || bad="${bad:+${bad} and }stdout was '${stdout}', wanted '${want_stdout}'"
  fi
  if [ "$want_err" != "-" ]; then
    case "$want_err" in
      !*)
        forbidden="${want_err#!}"
        if printf '%s' "$stderr" | grep -qF -e "$forbidden"; then
          bad="${bad:+${bad} and }stderr mentioned ${forbidden}, which it must not"
        fi ;;
      *)
        if ! printf '%s' "$stderr" | grep -qF -e "$want_err"; then
          bad="${bad:+${bad} and }stderr did not mention ${want_err}"
        fi ;;
    esac
  fi

  if [ -n "$bad" ]; then
    FAILED=$((FAILED + 1))
    echo "FAIL: ${desc}: ${bad}" >&2
    printf '%s\n' "$stderr" | sed 's/^/      stderr| /' >&2
  fi
}

# probe_usage <description> <want-rc> <want-substring> [args...]
probe_usage() {
  desc="$1"; want_rc="$2"; want_text="$3"; shift 3
  PROBES=$((PROBES + 1))
  set +e
  out="$(env PATH="$SHIM:/usr/bin:/bin" bash "$SCRIPT" "$@" 2>&1)"
  rc=$?
  set -e
  bad=""
  [ "$rc" = "$want_rc" ] || bad="exit ${rc}, wanted ${want_rc}"
  if [ "$want_text" != "-" ] && ! printf '%s' "$out" | grep -qF -e "$want_text"; then
    bad="${bad:+${bad} and }did not mention ${want_text}"
  fi
  if [ -n "$bad" ]; then
    FAILED=$((FAILED + 1))
    echo "FAIL: ${desc}: ${bad}" >&2
    printf '%s\n' "$out" | sed 's/^/      /' >&2
  fi
}

# ---- answered --------------------------------------------------------------

probe "an existing version answers 'published'" \
  0 "state=published" - 200 - 0
probe "  ... and says nothing else on the happy path" \
  0 "state=published" "!error" 200 - 0
probe "an unpublished version answers 'absent'" \
  0 "state=absent" - 404 - 0
probe "a 404 for a crate that does not exist yet is still 'absent'" \
  0 "state=absent" - 404 "@404-crate-missing.json" 0

# ---- the rc.2 failure ------------------------------------------------------

# The whole reason the script exists: 403 must NOT read as "absent", because
# that branch publishes.
probe "a 403 is 'did not finish reading', never 'absent'" \
  3 - "HTTP 403" 403 "@403.json" 0
probe "  ... and it quotes the policy crates.io sent" \
  3 - "We require that all requests include a \`User-Agent\` header" 403 "@403.json" 0
probe "  ... and it names the User-Agent policy as the likely cause" \
  3 - "https://crates.io/data-access" 403 "@403.json" 0
probe "  ... and it says the emptiness of the answer, not the crate" \
  3 - "is UNKNOWN" 403 "@403.json" 0

# The real 403 that `curl/<version>` earns has an EMPTY body. Printing the body
# must not become printing nothing where an explanation is expected.
probe "a 403 with an empty body still explains itself" \
  3 - "(the response body was empty)" 403 - 0

# ---- other refusals are refusals, not answers ------------------------------

probe "a 429 is not an answer" \
  3 - "HTTP 429" 429 "@429.json" 0
probe "  ... and its body is forwarded verbatim" \
  3 - "You have hit the rate limit." 429 "@429.json" 0
probe "a 500 is not an answer" \
  3 - "HTTP 500" 500 "@500.json" 0
probe "an unexpected 200-shaped status like 302 is not an answer" \
  3 - "HTTP 302" 302 - 0
probe "a transport failure is not an answer" \
  3 - "could not complete the crates.io version lookup" 000 - 1
probe "  ... and it does not blame the crate" \
  3 - "UNKNOWN" 000 - 1
probe "  ... and it does not print a state line" \
  3 - "-" 000 - 1

# ---- usage -----------------------------------------------------------------
probe_usage "no arguments is a usage error" 2 "usage:"
probe_usage "--crate with no value is a usage error" 2 "--crate needs a value" --crate
probe_usage "--version with no value is a usage error" 2 "--version needs a value" --crate chat-stasher --version
probe_usage "an unknown argument is a usage error" 2 "unknown argument" --crate chat-stasher --version 0.5.0 --bogus
probe_usage "--crate alone is a usage error, not a default" 2 "--version is required" --crate chat-stasher

# ---------------------------------------------------------------------------
# The regression guard proper: the shape of every request the script made.
#
# This is asserted over the whole recorded log rather than per probe, because
# the property is about every request, and a future code path that added a
# second curl call must not be able to escape it.
# ---------------------------------------------------------------------------
REQUEST_PROBES=0

# The recorded log is non-empty — otherwise the two checks below would pass
# vacuously over zero requests.
REQUEST_PROBES=$((REQUEST_PROBES + 1))
if [ ! -s "$ARGV_LOG" ]; then
  FAILED=$((FAILED + 1))
  echo "FAIL: the shimmed curl was never invoked, so no request shape was proved" >&2
fi

# 1. Every request carries the descriptive User-Agent.
REQUEST_PROBES=$((REQUEST_PROBES + 1))
if ! grep -qF -e '-H User-Agent: chat-stasher-release (https://github.com/dimpurr/chat-stasher)' "$ARGV_LOG"; then
  FAILED=$((FAILED + 1))
  echo "FAIL: no request carried the descriptive User-Agent; the 403 defect is back" >&2
fi

# 2. And it is the ONLY User-Agent any request carries — a bare client string
#    (curl/8.x, reqwest/0.9.1) is the "Bad" case crates.io names in its policy.
REQUEST_PROBES=$((REQUEST_PROBES + 1))
if grep -qE 'User-Agent: *(curl|Wget|HTTPie|python-requests|reqwest|Go-http-client)/' "$ARGV_LOG"; then
  FAILED=$((FAILED + 1))
  echo "FAIL: a request identified itself only as an HTTP client library" >&2
  grep -nE 'User-Agent:' "$ARGV_LOG" | sed 's/^/      /' >&2
fi

# 3. No request uses -f/--fail. It would collapse a 403 into a generic failure,
#    which is the mechanism that turned a 403 into a false "not found" in the
#    other project (totalreclaw PR #632).
REQUEST_PROBES=$((REQUEST_PROBES + 1))
if grep -qE '(^| )(-f|--fail)( |$)' "$ARGV_LOG"; then
  FAILED=$((FAILED + 1))
  echo "FAIL: a request used -f/--fail, which hides the status this script reads" >&2
  grep -nE '(^| )(-f|--fail)( |$)' "$ARGV_LOG" | sed 's/^/      /' >&2
fi

# 4. The URL under test is the crate+version address, not the crate alone —
#    asking about the crate would answer "published" for a version that is not.
REQUEST_PROBES=$((REQUEST_PROBES + 1))
if ! grep -qF -e 'https://crates.io/api/v1/crates/chat-stasher/0.5.0-rc.2' "$ARGV_LOG"; then
  FAILED=$((FAILED + 1))
  echo "FAIL: no request addressed the exact crate version" >&2
fi

PROBES=$((PROBES + REQUEST_PROBES))

if [ "$FAILED" -ne 0 ]; then
  echo "selftest-crates-version-state: FAIL (${FAILED} of ${PROBES} probes failed)" >&2
  exit 1
fi
echo "selftest-crates-version-state: PASS (${PROBES} probes)"
