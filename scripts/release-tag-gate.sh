#!/usr/bin/env bash
#
# release-tag-gate.sh — decide whether a `Release` run may proceed, and what
# kind of release its tag names.
#
# It lives here rather than inline in the workflow step for the same reason
# scripts/commit-message-range.sh does: the rule has one owner. The workflow
# calls this script, and scripts/selftest-release-tag-gate.sh drives the same
# script against synthetic refs — so the thing that is tested is the thing that
# runs, and there is no second copy to drift.
#
# Three refusals, in this order:
#
#   1. **The ref is not a tag.** `workflow_dispatch` accepts a branch, and a
#      branch may be *named* `v0.3.0`; `github.ref_name` for that branch is then
#      exactly the string rule 2 is looking for, which is how a manual run on a
#      branch reached the publication path. Two things fix that and both are
#      here: the gate takes the **full ref** (`github.ref`, not `ref_name`) and
#      derives the tag from it, and it refuses anything that is not under
#      `refs/tags/`. Deriving the tag is what makes the input unambiguous; the
#      prefix check is what makes a bare tag name — the shape `ref_name` hands
#      over — a refusal rather than a silently accepted one, so the workflow
#      cannot go back to reading `ref_name` without this going red.
#   2. **The tag's shape.** The trigger is `v*`, which is wider than a release
#      tag: `v0.3.0rc1`, `v0.3.0-rc` and `v0.3.0-beta.1` all match it too. Each
#      is a plausible typo for a tag someone meant, and under the earlier
#      "does the version contain a hyphen?" rule the first published as a
#      *normal* release and took the "latest" slot — which is what a reader of
#      the Releases page means by stable — while the other two published as
#      prereleases.
#   3. **The tag and Cargo.toml disagree.** The binary embeds CARGO_PKG_VERSION,
#      so a mismatch ships a Release titled after the tag whose binary reports
#      the other string. For an rc it is worse than cosmetic: the rc and the
#      stable it is a candidate for would print the same version, which is why
#      the rc carries its `-rc.N` suffix in Cargo.toml too.
#
# Usage:
#   bash scripts/release-tag-gate.sh --ref "$GITHUB_REF"
#   bash scripts/release-tag-gate.sh --ref refs/tags/v0.3.0-rc.1 --cargo crates/chat-stasher/Cargo.toml
#
#   --ref REF      required. The *full* ref, e.g. `refs/tags/v1.2.3`. A bare tag
#                  name is refused: the ref is the only thing that says whether
#                  this run is a tag at all.
#   --cargo PATH   optional. Defaults to crates/chat-stasher/Cargo.toml.
#
# On success it prints two `key=value` lines on stdout and nothing else, so a
# caller can append them to `$GITHUB_OUTPUT` unmodified:
#
#   prerelease=0|1        the GitHub Release's prerelease flag
#   npm_tag=latest|next   the npm dist-tag every package is published under
#
# Outputting `npm_tag` is the point of the second line. npm's default dist-tag
# is `latest` for *every* version, prerelease included, so an rc published
# without an explicit `--tag` becomes the version `npm install chat-stasher`
# picks — the opposite of what a candidate is for. Only the tag shape knows
# which of the two a run is, so the choice is made here, where it is tested.
#
# Every human-readable line goes to stderr. It writes no file, reads no
# environment, and touches no network.
#
# Exit codes: 0 = a release tag whose version Cargo.toml agrees with · 1 = refused.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO="$ROOT/crates/chat-stasher/Cargo.toml"
REF=""

while [ $# -gt 0 ]; do
  case "$1" in
    --ref)
      [ $# -ge 2 ] || { echo "release-tag-gate: --ref needs a value" >&2; exit 1; }
      REF="$2"; shift 2 ;;
    --cargo)
      [ $# -ge 2 ] || { echo "release-tag-gate: --cargo needs a value" >&2; exit 1; }
      CARGO="$2"; shift 2 ;;
    *)
      echo "release-tag-gate: unknown argument: $1" >&2
      echo "usage: bash scripts/release-tag-gate.sh --ref REF [--cargo PATH]" >&2
      exit 1 ;;
  esac
done

if [ -z "$REF" ]; then
  echo "release-tag-gate: --ref is required" >&2
  echo "usage: bash scripts/release-tag-gate.sh --ref REF [--cargo PATH]" >&2
  exit 1
fi

# ---- 1. the ref is a tag ----------------------------------------------------
case "$REF" in
  refs/tags/*) ;;
  *)
    {
      echo "error: '${REF}' is not a tag ref."
      echo "       A release is cut by pushing a tag. This workflow also accepts a"
      echo "       manual run (workflow_dispatch), which GitHub offers against a"
      echo "       branch as well — and a branch can be named exactly like a release"
      echo "       tag, so a branch ref is refused here rather than read as one."
      echo "       Nothing was built and nothing was published by this run."
    } >&2
    exit 1
    ;;
esac

TAG="${REF#refs/tags/}"

# ---- 2. the tag's shape -----------------------------------------------------
# Exactly two shapes are release tags (RELEASING.md, "Channels"):
#
#   v0.3.0        a stable release  -> published as the repository's "latest"
#   v0.3.0-rc.1   a candidate       -> published as a GitHub *prerelease*, and
#                                      under npm's `next` dist-tag
if printf '%s' "$TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
  KIND="stable"
  PRERELEASE=0
  NPM_TAG="latest"
elif printf '%s' "$TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+-rc\.[0-9]+$'; then
  KIND="release candidate"
  PRERELEASE=1
  NPM_TAG="next"
else
  {
    echo "error: '${TAG}' is not a release tag."
    echo "       a release tag is exactly one of:"
    echo "         vMAJOR.MINOR.PATCH        a stable release"
    echo "         vMAJOR.MINOR.PATCH-rc.N   a release candidate"
    echo "       Nothing was built and nothing was published by this run."
  } >&2
  exit 1
fi

VERSION="${TAG#v}"

# ---- 3. the tag agrees with Cargo.toml --------------------------------------
if [ ! -f "$CARGO" ]; then
  echo "error: no such Cargo.toml: ${CARGO}" >&2
  echo "       (nothing was built and nothing was published by this run)" >&2
  exit 1
fi

# The `[package]` version, which is the string CARGO_PKG_VERSION gets. Scoped to
# the `[package]` table so a `version =` under `[dependencies]` can never be the
# one that answers.
CARGO_VERSION="$(awk '
  /^\[/ { section = $0 }
  section == "[package]" && /^version[ ]*=/ {
    line = $0
    sub(/^[^"]*"/, "", line)
    sub(/".*$/, "", line)
    print line
    exit
  }
' "$CARGO")"

if [ -z "$CARGO_VERSION" ]; then
  echo "error: no version found under [package] in ${CARGO}" >&2
  exit 1
fi

if [ "$CARGO_VERSION" != "$VERSION" ]; then
  {
    echo "error: the tag and Cargo.toml disagree."
    echo "       tag:      ${TAG} -> ${VERSION} (${KIND})"
    echo "       ${CARGO}:  ${CARGO_VERSION}"
    echo "       The binary embeds the Cargo.toml version, so this Release"
    echo "       would ship a binary reporting '${CARGO_VERSION}'."
    echo "       Set Cargo.toml (and Cargo.lock) on main first — RELEASING.md."
  } >&2
  exit 1
fi

echo "tag OK: ${TAG} — ${KIND}; Cargo.toml agrees at ${CARGO_VERSION}" >&2

# stdout: machine-readable, and nothing else.
printf 'prerelease=%s\n' "$PRERELEASE"
printf 'npm_tag=%s\n' "$NPM_TAG"
