#!/usr/bin/env bash
#
# The selftest for scripts/release-tag-gate.sh: proof that the gate accepts the
# two tag shapes a release is cut from, and refuses the three things that must
# not reach a build — a ref that is not a tag, a tag that is not one of the two
# release shapes, and a tag that disagrees with Cargo.toml.
#
# It drives the real script against throwaway refs and throwaway Cargo.toml
# files built under a temp directory, the way scripts/selftest-relocate-citations.sh
# builds throwaway repositories. It never reads this checkout's own Cargo.toml
# except in the one probe that asks whether the answer is the `[package]` one.
#
# Every probe below is a refusal the gate has to make or a shape it has to
# accept, and the refusals are the point: a gate whose only cases are the
# happy path cannot prove it catches anything. The ref-shaped probes matter
# most, because `workflow_dispatch` accepts a branch — a branch named `v0.3.0`
# reaches the tag-shape rule with exactly the string it is looking for.
#
# The bare-tag-name probe (`--ref v1.2.3`) is the one that pins the actual
# regression: the workflow used to hand the gate `github.ref_name`, which for a
# branch named `v0.3.0` is `v0.3.0`. A gate that accepts a bare name cannot tell
# that apart from a real tag, so refusing it is what keeps the workflow from
# going back to `ref_name` unnoticed. Removing the ref check from the gate makes
# that probe, and the two that assert the refusal's reason, go red — which is how
# this file was shown to be able to fail.
#
# Probes (expected: the tag it names, and the two output lines on stdout):
#
#   accepted  v1.2.3                  -> prerelease=0, npm_tag=latest
#             v1.2.3-rc.1             -> prerelease=1, npm_tag=next
#             [package] wins over a `version =` under [dependencies]
#   refused   refs/heads/v1.2.3       -> a branch that is named like the tag
#             refs/heads/main         -> an ordinary branch
#             v1.2.3                  -> the bare tag name, not a full ref
#             refs/tags/main          -> a tag that is not a version
#             refs/tags/v1.2.3rc1     -> a typo shape that `v*` would trigger
#             refs/tags/v1.2.3-rc     -> a candidate with no number
#             refs/tags/v1.2.3-beta.1 -> a channel that is not a release channel
#             v1.2.3 vs 1.2.4         -> tag and Cargo.toml disagree
#             v1.2.3-rc.1 vs 1.2.3    -> an rc whose Cargo.toml is the stable's
#             v1.2.3 vs 0.5.0-dev     -> a tree between releases
#             no Cargo.toml at all
#
# Exit codes: 0 = every probe behaved · 1 = at least one did not.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="$ROOT/scripts/release-tag-gate.sh"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cs-tag-gate-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

PROBES=0
FAILED=0

# A Cargo.toml carrying `pkg` as the [package] version, plus a decoy `version =`
# under [dependencies] — which appears *before* [package] so that a scanner that
# simply took the first `version =` it saw would answer with the decoy.
write_cargo() {
  cat >"$TMP/Cargo.toml" <<EOF
[dependencies]
serde = { version = "1.0.0" }

[package]
name = "chat-stasher"
version = "$1"
edition = "2021"
EOF
}

# Run the gate and capture everything, without letting `set -e` end the run on
# the refusals this file exists to observe.
RC=0
OUT=""
ERR=""
run_gate() {
  set +e
  bash "$GATE" "$@" >"$TMP/out" 2>"$TMP/err"
  RC=$?
  set -e
  OUT="$(cat "$TMP/out")"
  ERR="$(cat "$TMP/err")"
}

check() {   # check <description> <condition>; the condition is last so it reads as prose
  PROBES=$((PROBES + 1))
  if ! eval "$2"; then
    FAILED=$((FAILED + 1))
    echo "FAIL: $1" >&2
    echo "      rc=${RC} stdout=[${OUT}]" >&2
    echo "      stderr=[${ERR}]" >&2
  fi
}

expect_output() {   # expect_output <prerelease> <npm_tag>
  check "prints exactly prerelease=$1 and npm_tag=$2" \
    "[ \"\$OUT\" = \"prerelease=$1
npm_tag=$2\" ]"
}

# --------------------------------------------------------------- accepted ----
write_cargo 1.2.3
run_gate --ref refs/tags/v1.2.3 --cargo "$TMP/Cargo.toml"
check "v1.2.3 is accepted" '[ "$RC" = 0 ]'
expect_output 0 latest

write_cargo 1.2.3-rc.1
run_gate --ref refs/tags/v1.2.3-rc.1 --cargo "$TMP/Cargo.toml"
check "v1.2.3-rc.1 is accepted" '[ "$RC" = 0 ]'
expect_output 1 next

# The decoy under [dependencies] differs from the [package] version on purpose:
# if the gate answered with the decoy it would accept a tag that names no
# version the crate has.
write_cargo 2.0.0-rc.3
run_gate --ref refs/tags/v2.0.0-rc.3 --cargo "$TMP/Cargo.toml"
check "the [package] version answers, not a version under [dependencies]" '[ "$RC" = 0 ]'
expect_output 1 next

check "the success path says nothing on stdout but the two output lines" \
  '! printf "%s" "$OUT" | grep -qv "^\(prerelease\|npm_tag\)="'

# --------------------------------------------------------------- refused -----
# A branch named exactly like a release tag. This is the shape the trigger alone
# cannot tell from a tag, and the reason the ref is checked before the tag.
write_cargo 1.2.3
run_gate --ref refs/heads/v1.2.3 --cargo "$TMP/Cargo.toml"
check "a branch named like the tag is refused" '[ "$RC" != 0 ]'
check "  ... and says the ref is not a tag" 'printf "%s" "$ERR" | grep -q "is not a tag ref"'
check "  ... and prints nothing on stdout" '[ -z "$OUT" ]'

run_gate --ref refs/heads/main --cargo "$TMP/Cargo.toml"
check "an ordinary branch is refused" '[ "$RC" != 0 ]'
check "  ... and says the ref is not a tag" 'printf "%s" "$ERR" | grep -q "is not a tag ref"'

# The bare tag name: `git push origin v1.2.3` is what a releaser types, and
# `github.ref` is not that. Accepting it would mean guessing the ref was a tag.
run_gate --ref v1.2.3 --cargo "$TMP/Cargo.toml"
check "a bare tag name is refused" '[ "$RC" != 0 ]'
check "  ... and says the ref is not a tag" 'printf "%s" "$ERR" | grep -q "is not a tag ref"'

# The three shapes `v*` matches that are not release tags.
for bad in main v1.2.3rc1 v1.2.3-rc v1.2.3-beta.1; do
  write_cargo 1.2.3
  run_gate --ref "refs/tags/$bad" --cargo "$TMP/Cargo.toml"
  check "the tag '${bad}' is refused" '[ "$RC" != 0 ]'
  check "  ... and says it is not a release tag" 'printf "%s" "$ERR" | grep -q "is not a release tag"'
  check "  ... and prints nothing on stdout" '[ -z "$OUT" ]'
done

# Tag and Cargo.toml disagree.
write_cargo 1.2.4
run_gate --ref refs/tags/v1.2.3 --cargo "$TMP/Cargo.toml"
check "a tag that disagrees with Cargo.toml is refused" '[ "$RC" != 0 ]'
check "  ... and says so" 'printf "%s" "$ERR" | grep -q "disagree"'

# The rc/stable collision: an rc whose Cargo.toml carries the stable's version
# would ship a binary that prints the same string as the release it is only a
# candidate for, so the two are not interchangeable.
write_cargo 1.2.3
run_gate --ref refs/tags/v1.2.3-rc.1 --cargo "$TMP/Cargo.toml"
check "an rc whose Cargo.toml is the stable's is refused" '[ "$RC" != 0 ]'
check "  ... and says so" 'printf "%s" "$ERR" | grep -q "disagree"'

# A tree between releases: Cargo.toml on main is 0.5.0-dev, which is not `v*`
# and must never reach a registry.
write_cargo 0.5.0-dev
run_gate --ref refs/tags/v0.5.0 --cargo "$TMP/Cargo.toml"
check "a tag against the dev version on main is refused" '[ "$RC" != 0 ]'
check "  ... and says so" 'printf "%s" "$ERR" | grep -q "disagree"'

# A missing file is refused in as many words rather than reading an absent
# version as an empty one.
run_gate --ref refs/tags/v1.2.3 --cargo "$TMP/does-not-exist.toml"
check "a missing Cargo.toml is refused" '[ "$RC" != 0 ]'
check "  ... and names the file" 'printf "%s" "$ERR" | grep -q "no such Cargo.toml"'

# A missing --ref is a usage failure, not a silent pass.
run_gate
check "an invocation with no --ref is refused" '[ "$RC" != 0 ]'

# --------------------------------------------------------------- summary -----
if [ "$FAILED" -ne 0 ]; then
  echo "SELFTEST: FAIL (${FAILED} of ${PROBES} probes failed)" >&2
  exit 1
fi
echo "SELFTEST: PASS (${PROBES} probes)"
