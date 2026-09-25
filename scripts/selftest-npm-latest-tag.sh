#!/usr/bin/env bash
#
# The selftest for scripts/npm-latest-tag.sh: proof that a `latest` pointing at
# a release candidate is re-pointed at the newest stable when one exists, is
# reported as a notice when one does not — and is never silently left wrong.
#
# It drives the real script with `npm` shimmed on PATH, the way
# scripts/selftest-check-static-binary.sh shims `file` and `readelf`, and
# scripts/selftest-crates-version-state.sh shims `curl`. The shim answers `npm
# view` from per-package fixtures — so a probe can say "this package's latest is
# X and these are its versions" without a registry — and it records every `npm
# dist-tag add`, which is how the probes assert what the script did or did not
# change. Nothing here touches the network, and no real dist-tag is modified.
#
# The probes that matter most, and what each would catch:
#
#   · **A prerelease is never left as `latest` when a stable exists.** The
#     recorded `dist-tag add` must name the stable version. This is the defect
#     that shipped: all six packages came back from v0.5.0-rc.2 with
#     `latest = 0.5.0-rc.2`.
#   · **The newest stable, not the last one listed.** Version lists are not
#     ordered by contract, and the two-digit probe (`0.9.0` vs `0.10.0`) is the
#     one that catches a comparator using string order — under which "10" sorts
#     below "9" and the script would re-point `latest` backwards.
#   · **No stable at all is a notice, not an error and not a repair.** The
#     release must not go red, and the script must not invent a version to
#     point at.
#   · **An unreadable registry is exit 3.** "We could not look" must stay
#     distinct from "we looked and it is fine".
#
# Exit codes: 0 = every probe behaved · 1 = at least one did not · 2 = usage.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT/scripts/npm-latest-tag.sh"

if ! command -v node >/dev/null 2>&1; then
  echo "selftest-npm-latest-tag: node is required (the script reads package.json with it)" >&2
  exit 2
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cs-npm-latest-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

SHIM="$TMP/shim"
FIX="$TMP/fixtures"
PKG="$TMP/pkgs"
ARGV_LOG="$TMP/dist-tag.log"
SUM="$TMP/summary.md"
mkdir -p "$SHIM" "$FIX" "$PKG"

# The probes run the script under a PATH that puts the `npm` shim first and
# keeps only /usr/bin and /bin after it — so that no `npm` of the host's own can
# be reached instead of the shim. `node` is NOT shimmed and must stay reachable,
# and on this repository's development machines it is usually neither in /usr/bin
# nor in /bin (Homebrew and nvm both install it elsewhere), so its directory is
# carried through explicitly rather than assumed.
NODE_DIR="$(cd "$(dirname "$(command -v node)")" && pwd)"
SHIM_PATH="$SHIM:$NODE_DIR:/usr/bin:/bin"

PROBES=0
FAILED=0

# ---------------------------------------------------------------------------
# The shim. `npm view <name> dist-tags --json` prints the fixture
# `dist-tags.json` and `npm view <name> versions --json` prints the fixture
# `versions.json`; a missing fixture is how a probe says "the registry could not
# be read", so the shim then fails the way npm does, with E404 on stderr and a
# non-zero exit. `npm dist-tag add <spec> latest` is recorded and answered per
# NS_DISTTAG_FAIL.
#
# Fixture directories are keyed by the package name with `/` and `@` replaced by
# `_`, because a scoped name is not a usable path segment.
# ---------------------------------------------------------------------------
cat >"$SHIM/npm" <<'SHIM_EOF'
#!/bin/sh
kind="$1"; shift

fixture_dir() {
  key="$(printf '%s' "$1" | tr '/@' '__')"
  printf '%s' "${NS_FIXTURES}/${key}"
}

case "$kind" in
  view)
    name="$1"; field="$2"
    dir="$(fixture_dir "$name")"
    case "$field" in
      dist-tags)
        [ -f "$dir/dist-tags.json" ] || { echo "npm error code E404" >&2; exit 1; }
        cat "$dir/dist-tags.json" ;;
      versions)
        [ -f "$dir/versions.json" ] || { echo "npm error code E404" >&2; exit 1; }
        cat "$dir/versions.json" ;;
      *) echo "npm shim: unexpected view field: $field" >&2; exit 1 ;;
    esac
    ;;
  dist-tag)
    # $1=add $2=<name>@<version> $3=latest
    printf '%s %s %s\n' "$1" "$2" "$3" >>"${NS_ARGV_LOG}"
    [ "${NS_DISTTAG_FAIL:-0}" = 1 ] && { echo "npm error code EOTP" >&2; exit 1; }
    echo "+latest: $2"
    ;;
  *)
    echo "npm shim: unexpected command: $kind" >&2
    exit 1 ;;
esac
exit 0
SHIM_EOF
chmod +x "$SHIM/npm"

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

# pkg <directory-name> <package-name> — a real directory with a real
# package.json, because the script reads the name out of it rather than being
# told the name separately. That is the property the publish step relies on.
pkg() {
  mkdir -p "$PKG/$1"
  printf '{\n  "name": "%s",\n  "version": "0.0.0"\n}\n' "$2" >"$PKG/$1/package.json"
}

# fixture <package-name> <latest|-> <versions...>
#
# `-` for latest writes an empty dist-tags object: the package answers, and has
# no `latest` key. That is different from leaving the fixture out entirely,
# which is how a probe says the package could not be read at all.
fixture() {
  name="$1"; latest="$2"; shift 2
  key="$(printf '%s' "$name" | tr '/@' '__')"
  mkdir -p "$FIX/$key"
  if [ "$latest" = "-" ]; then
    printf '%s\n' '{}' >"$FIX/$key/dist-tags.json"
  else
    printf '{"latest":"%s"}\n' "$latest" >"$FIX/$key/dist-tags.json"
  fi
  # Written as a JSON array, pretty-printed the way `npm view --json` prints it.
  {
    echo "["
    n=$#
    i=0
    for v in "$@"; do
      i=$((i + 1))
      if [ "$i" -lt "$n" ]; then printf '  "%s",\n' "$v"; else printf '  "%s"\n' "$v"; fi
    done
    echo "]"
  } >"$FIX/$key/versions.json"
}

for spec in \
  "launcher chat-stasher" \
  "scoped @dimpurr/chat-stasher-darwin-arm64" \
  "two-digit chat-stasher-two-digit" \
  "correct chat-stasher-correct" \
  "no-latest chat-stasher-no-latest" \
  "betas chat-stasher-betas" \
  "tie chat-stasher-tie" \
  "unreadable chat-stasher-unreadable"
do
  # shellcheck disable=SC2086 # two fixed words, split on purpose
  pkg $spec
done

# --- the shipped defect: a first publish, one version, and it is a candidate.
fixture chat-stasher 0.5.0-rc.2 0.5.0-rc.2
# --- a stable exists, so there is somewhere to re-point. The stable is NOT the
# last entry, so a script taking the tail would get this wrong.
fixture '@dimpurr/chat-stasher-darwin-arm64' 0.5.0-rc.2 0.4.0 0.5.0-rc.2 0.3.0
# --- ... and its dist-tags carry `next` BEFORE `latest`, the way npm prints
# them for a real candidate publish. Recorded shape from the v0.5.0-rc.2 registry
# state. This is what proves the script reads the `latest` key rather than the
# first version-looking string in the object.
printf '%s\n' '{"next":"0.5.0-rc.2","latest":"0.5.0-rc.2"}' \
  >"$FIX/$(printf '%s' '@dimpurr/chat-stasher-darwin-arm64' | tr '/@' '__')/dist-tags.json"
# --- two-digit components: 0.10.0 must beat 0.9.0. As strings "0.9.0" > "0.10.0",
# so a lexicographic comparator re-points `latest` BACKWARDS to 0.9.0 — a wrong
# answer that looks like a repair. 0.10.0 is also not the last entry, so taking
# the tail gets the candidate instead.
fixture chat-stasher-two-digit 0.10.0-rc.1 0.9.0 0.10.0 0.10.0-rc.1
# --- already correct: nothing to do, and nothing must be recorded.
fixture chat-stasher-correct 0.4.0 0.4.0 0.5.0-rc.2
# --- the package answers and has no `latest` key at all, with a stable
# available: must be repaired, not reported as unknown.
fixture chat-stasher-no-latest - 0.4.0 0.5.0-rc.2
# --- prerelease-only list in a shape that is not `-rc`.
fixture chat-stasher-betas 1.0.0-beta.3 1.0.0-beta.1 1.0.0-beta.3
# --- latest is a stable and is the only version: must not be moved.
fixture chat-stasher-tie 0.9.0 0.9.0
# --- nothing written for chat-stasher-unreadable: the registry refuses.

# ---------------------------------------------------------------------------
# probe <description> <want-rc> <want-substring|-> <pkg-dir> [pkg-dir...]
#
# One invocation of the real script with every directory given to it, so
# multi-package handling in a single run is exercised rather than looped around.
# PROBE_FLAGS adds flags to that invocation and PROBE_NPM_FAIL makes the shim
# refuse a dist-tag add; both are set by the caller and cleared by probe.
#
# Afterwards NREC holds how many dist-tag adds were recorded and NSUM the job
# summary, so a probe can assert on what the script did and not only on what it
# said.
# ---------------------------------------------------------------------------
PROBE_FLAGS=""
PROBE_NPM_FAIL=0
NREC=0
NSUM=""

probe() {
  desc="$1"; want_rc="$2"; want_text="$3"; shift 3

  PROBES=$((PROBES + 1))
  : >"$ARGV_LOG"
  : >"$SUM"

  dir_args=""
  for d in "$@"; do dir_args="${dir_args} --package-dir ${d}"; done

  set +e
  # Deliberately unquoted: dir_args and PROBE_FLAGS are assembled word lists.
  # A temp directory from mktemp contains no space, and every other word here is
  # a literal, so this cannot surprise the argument list.

  # shellcheck disable=SC2086
  out="$(env PATH="$SHIM_PATH" \
    NS_FIXTURES="$FIX" \
    NS_ARGV_LOG="$ARGV_LOG" \
    NS_DISTTAG_FAIL="$PROBE_NPM_FAIL" \
    GITHUB_STEP_SUMMARY="$SUM" \
    bash "$SCRIPT" $dir_args $PROBE_FLAGS 2>&1)"
  rc=$?
  set -e

  NREC="$(wc -l <"$ARGV_LOG" | tr -d ' ')"
  NSUM="$(cat "$SUM")"

  bad=""
  [ "$rc" = "$want_rc" ] || bad="exit ${rc}, wanted ${want_rc}"
  if [ "$want_text" != "-" ] && ! printf '%s' "$out" | grep -qF -e "$want_text"; then
    bad="${bad:+${bad} and }output did not mention ${want_text}"
  fi

  PROBE_FLAGS=""
  PROBE_NPM_FAIL=0

  if [ -n "$bad" ]; then
    FAILED=$((FAILED + 1))
    echo "FAIL: ${desc}: ${bad}" >&2
    printf '%s\n' "$out" | sed 's/^/      /' >&2
    printf '%s\n' "$NSUM" | sed 's/^/      summary| /' >&2
    sed 's/^/      dist-tag| /' "$ARGV_LOG" >&2
  fi
}

# probe_usage <description> <want-rc> <want-substring> [args...]
probe_usage() {
  desc="$1"; want_rc="$2"; want_text="$3"; shift 3
  PROBES=$((PROBES + 1))
  set +e
  out="$(env PATH="$SHIM_PATH" bash "$SCRIPT" "$@" 2>&1)"
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

# assert_eq <description> <actual> <expected> — records the previous probe's
# recorded dist-tags/summary, which is what "and then what did it do" means.
assert_eq() {
  desc="$1"; actual="$2"; expected="$3"
  PROBES=$((PROBES + 1))
  if [ "$actual" != "$expected" ]; then
    FAILED=$((FAILED + 1))
    echo "FAIL: ${desc}: got '${actual}', wanted '${expected}'" >&2
  fi
}

# assert_absent <description> <haystack> <needle>
assert_absent() {
  desc="$1"; hay="$2"; needle="$3"
  PROBES=$((PROBES + 1))
  if printf '%s' "$hay" | grep -qF -e "$needle"; then
    FAILED=$((FAILED + 1))
    echo "FAIL: ${desc}: found '${needle}', which must not be there" >&2
  fi
}

# ---- the shipped defect ----------------------------------------------------

probe "a first-publish candidate with no stable is a notice, not a failure" \
  0 "no stable version exists yet" "$PKG/launcher"
assert_eq "  ... and it changed no dist-tag" "$NREC" "0"
assert_eq "  ... and the job summary carries the notice heading" \
  "$(printf '%s' "$NSUM" | grep -c 'npm .latest. currently points at a release candidate')" "1"
assert_eq "  ... and the summary names the package" \
  "$(printf '%s' "$NSUM" | grep -c 'chat-stasher')" "1"
assert_eq "  ... and the summary says it is a notice, not a failure" \
  "$(printf '%s' "$NSUM" | grep -c 'This is a notice, not a failure')" "1"

# ---- the repair ------------------------------------------------------------

probe "a candidate with a stable available is re-pointed" \
  0 "re-pointed" "$PKG/scoped"
assert_eq "  ... and it ran exactly one dist-tag add" "$NREC" "1"
assert_eq "  ... and it named the stable, not the candidate" \
  "$(cat "$ARGV_LOG")" "add @dimpurr/chat-stasher-darwin-arm64@0.4.0 latest"
assert_absent "  ... and the summary stayed empty" "$NSUM" "release candidate"

probe "the newest stable wins even when it is not the last one listed" \
  0 "-> 0.4.0" "$PKG/scoped"
assert_eq "  ... and the recorded spec is the newest stable" \
  "$(cat "$ARGV_LOG")" "add @dimpurr/chat-stasher-darwin-arm64@0.4.0 latest"

# The comparator probe: 0.10.0 must beat 0.9.0. String order says otherwise.
probe "two-digit version components compare numerically" \
  0 "-> 0.10.0" "$PKG/two-digit"
assert_eq "  ... and 0.10.0 is what was recorded, not 0.9.0" \
  "$(cat "$ARGV_LOG")" "add chat-stasher-two-digit@0.10.0 latest"

probe "an absent latest tag with a stable available is repaired" \
  0 "re-pointed" "$PKG/no-latest"
assert_eq "  ... and it pointed at the stable" \
  "$(cat "$ARGV_LOG")" "add chat-stasher-no-latest@0.4.0 latest"

# ---- nothing to do ---------------------------------------------------------

probe "a correct stable latest is left alone" \
  0 "is a stable version; nothing to do" "$PKG/correct"
assert_eq "  ... and no dist-tag add was recorded" "$NREC" "0"
assert_absent "  ... and no notice was written" "$NSUM" "release candidate"

probe "a stable latest is not moved to a lower stable" \
  0 "nothing to do" "$PKG/tie"
assert_eq "  ... and no dist-tag add was recorded" "$NREC" "0"

# ---- prerelease-only, other shapes ----------------------------------------

probe "prereleases that are not -rc are still prereleases" \
  0 "no stable version exists yet" "$PKG/betas"
assert_eq "  ... and nothing was re-pointed at a beta" "$NREC" "0"
assert_eq "  ... and the notice lists the candidate it saw" \
  "$(printf '%s' "$NSUM" | grep -c '1.0.0-beta.3')" "1"

# ---- failure paths ---------------------------------------------------------

probe "an unreadable registry is exit 3, not 'fine'" \
  3 "is UNKNOWN" "$PKG/unreadable"
assert_eq "  ... and nothing was changed" "$NREC" "0"

PROBE_FLAGS="--dry-run"
probe "a dry run reports what it would do" \
  0 "would re-point" "$PKG/scoped"
assert_eq "  ... and records no dist-tag add" "$NREC" "0"
assert_absent "  ... and writes no notice" "$NSUM" "release candidate"

PROBE_NPM_FAIL=1
probe "a failed dist-tag add is exit 1, not silence" \
  1 "failed to re-point" "$PKG/scoped"

# ---- multiple packages in one run -----------------------------------------

probe "two packages are both handled in one invocation" \
  0 "re-pointed" "$PKG/scoped" "$PKG/correct"
assert_eq "  ... and only the repairable one was recorded" "$NREC" "1"
assert_absent "  ... and the already-correct one was not touched" \
  "$(cat "$ARGV_LOG")" "chat-stasher-correct"

# ---- usage -----------------------------------------------------------------

probe_usage "no arguments is a usage error" 2 "at least one --package-dir is required"
probe_usage "an unknown argument is a usage error" 2 "unknown argument" --package-dir "$PKG/scoped" --bogus
probe_usage "--package-dir with no value is a usage error" 2 "--package-dir needs a value" --package-dir
probe_usage "a directory with no package.json is a usage error" 2 "no package.json under" --package-dir "$TMP/does-not-exist"

if [ "$FAILED" -ne 0 ]; then
  echo "selftest-npm-latest-tag: FAIL (${FAILED} of ${PROBES} probes failed)" >&2
  exit 1
fi
echo "selftest-npm-latest-tag: PASS (${PROBES} probes)"
