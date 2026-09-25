#!/usr/bin/env bash
#
# npm-latest-tag.sh — keep the npm `latest` dist-tag off release candidates.
#
# It lives here rather than inline in the release workflow step for the same
# reason scripts/release-tag-gate.sh does: the rule has one owner, the workflow
# calls the script, and scripts/selftest-npm-latest-tag.sh drives the same
# script with `npm` shimmed on PATH — so the thing that is tested is the thing
# that runs.
#
# ---------------------------------------------------------------------------
# The invariant, and the npm behaviour that breaks it
# ---------------------------------------------------------------------------
#
# `latest` is the dist-tag `npm install <pkg>` resolves when the user names no
# version and no tag. So the invariant this enforces is one sentence:
#
#   **`latest` must never point at a prerelease.**
#
# npm gets that wrong on a package's FIRST publish. `npm publish --tag next`
# sets `next` as asked — and also sets `latest`, which the docs say it does not
# do ("Publishing a package sets the `latest` tag to the published version
# unless the `--tag` option is used", docs.npmjs.com/cli/v11/commands/
# npm-dist-tag). On a package that has no stable release yet there is nothing
# else for `latest` to point at, so the candidate simply becomes what `npm
# install` resolves.
#
# Measured on this project's own release, 2026-09-25: v0.5.0-rc.2 was published
# with `--tag next` for all six packages, and all six came back with
# `latest = 0.5.0-rc.2` and exactly one version each. So the harm was live:
# `npm install chat-stasher` installed a release candidate.
#
# Passing `--tag` is therefore necessary but not sufficient, and the repair has
# to happen after the publish. Two outcomes, and only two:
#
#   · a stable version exists -> re-point `latest` at the newest one
#                                (`npm dist-tag add <pkg>@<version> latest`).
#   · no stable version exists -> there is nowhere to re-point it. Write a
#                                clear notice to the job summary saying so,
#                                because the state is expected and temporary,
#                                not an error to fail the release over.
#
# This is written as the invariant rather than as "run this after an rc" so it
# is idempotent and self-describing: on a stable release it finds `latest`
# already correct and does nothing, and it would also repair a package that had
# been left wrong by any other route.
#
# ---------------------------------------------------------------------------
# Usage
# ---------------------------------------------------------------------------
#
#   bash scripts/npm-latest-tag.sh --package-dir npm-packages/chat-stasher
#   bash scripts/npm-latest-tag.sh --package-dir 'npm-packages/@dimpurr/x' --dry-run
#
#   --package-dir DIR   required, repeatable. A directory holding the
#                       `package.json` that was just published; the package
#                       name is read from it, the same way the publish step
#                       reads it, so the two cannot disagree about which
#                       package is being talked about.
#   --dry-run           optional. Report what would be done; change no tag.
#
# The job summary is $GITHUB_STEP_SUMMARY when it is set, and skipped when it is
# not (a local run has no summary), so the notice is never lost silently — the
# same text also goes to stderr either way.
#
# ---------------------------------------------------------------------------
# Exit codes
# ---------------------------------------------------------------------------
#
#   0  every package was left correct, or is reported as unrepairable-yet with
#      a notice. "No stable version exists" is a measurement of the registry,
#      not a failure, and it must not turn a release red.
#   1  a re-point was attempted and failed. This is "finished reading and
#      failed" — the invariant is violated and stays violated.
#   3  a package's state could not be read, so whether `latest` is correct is
#      UNKNOWN. Nothing was changed for that package, and nothing is inferred
#      from the silence.
#   2  usage error.

set -euo pipefail

DRY_RUN=0
PACKAGE_DIRS=""

usage() {
  echo "usage: bash scripts/npm-latest-tag.sh --package-dir DIR [--package-dir DIR ...] [--dry-run]" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --package-dir)
      [ $# -ge 2 ] || { echo "npm-latest-tag: --package-dir needs a value" >&2; usage; exit 2; }
      # One directory per line. A newline is the separator rather than an array
      # because the loop below reads them back the same way, and because a
      # directory containing spaces is handled by quoting at both ends.
      PACKAGE_DIRS="${PACKAGE_DIRS}$2
"
      shift 2 ;;
    --dry-run)
      DRY_RUN=1; shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      echo "npm-latest-tag: unknown argument: $1" >&2
      usage
      exit 2 ;;
  esac
done

if [ -z "$PACKAGE_DIRS" ]; then
  echo "npm-latest-tag: at least one --package-dir is required" >&2
  usage
  exit 2
fi

# A version is a prerelease when it carries a hyphen suffix. That is the whole
# test: `0.5.0-rc.2` is a candidate, `0.5.0` is not. It is deliberately not a
# list of the suffixes this project happens to use, because the invariant is
# about prereleases in general, not about `rc` in particular.
is_prerelease() {
  case "$1" in
    *-*) return 0 ;;
    *) return 1 ;;
  esac
}

# Newest stable version from newline-separated candidates, or empty.
#
# Compared field by field rather than with `sort -V`: BSD sort has no `-V`, and
# this repository's checks are expected to be runnable on the developer's macOS
# as well as on the runner — a comparison that only works on GNU sort would make
# the selftest unreproducible where it is written. The `+ 0` is load-bearing:
# awk compares "9" and "10" as strings without it, and "10" < "9" as a string,
# so 0.10.0 would lose to 0.9.0.
newest_stable() {
  printf '%s\n' "$1" | awk '
    /^[0-9]+\.[0-9]+\.[0-9]+$/ {
      n = split($0, p, "\\.")
      if (n != 3) next
      a = p[1] + 0; b = p[2] + 0; c = p[3] + 0
      if (best == "" || a > b1 || (a == b1 && (b > b2 || (b == b2 && c > b3)))) {
        best = $0; b1 = a; b2 = b; b3 = c
      }
    }
    END { if (best != "") print best }
  '
}

summary() {
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    printf '%s\n' "$1" >>"$GITHUB_STEP_SUMMARY"
  fi
}

REPAIRED=0
ALREADY=0
NOTICE_ROWS=""
NOTICE_NEEDED=0

# npm's own diagnostics are worth keeping on a failure and are noise on the
# happy path, so they are collected here and printed only by a branch that
# decides the run cannot continue.
TMPERR="$(mktemp "${TMPDIR:-/tmp}/cs-npm-latest.XXXXXX")"
trap 'rm -f "$TMPERR"' EXIT

while IFS= read -r package_dir; do
  [ -n "$package_dir" ] || continue

  if [ ! -f "$package_dir/package.json" ]; then
    echo "npm-latest-tag: no package.json under: ${package_dir}" >&2
    exit 2
  fi

  # `path.resolve` rather than `'./' + dir`: the publish step passes a relative
  # directory, but this script accepts an absolute one too, and `'./' + '/abs'`
  # is `.//abs`, which node resolves against the cwd and cannot find. Resolving
  # makes both shapes work and is what the selftest exercises.
  name="$(node -p "require(require('path').resolve(process.argv[1], 'package.json')).name" "$package_dir")"

  # Read the current tags and the version list. Both are asked of the registry,
  # not of the working tree: the question is what npm would resolve right now.
  #
  # `dist-tags` as a whole rather than `dist-tags.latest`, because npm's exit
  # status cannot tell "this package has no `latest` tag" apart from "this
  # package could not be read at all" — both fail. Asking for the object makes
  # the distinction observable: a package that answered but has no `latest`
  # key is a package to repair, and a package that did not answer is a package
  # whose state is unknown. Collapsing those two is the same mistake as reading
  # a 403 as "not published".
  if ! dist_tags_json="$(npm view "$name" dist-tags --json 2>"$TMPERR")"; then
    {
      echo "error: could not read the dist-tags of ${name}."
      echo "       Whether 'latest' points at a prerelease is UNKNOWN, so nothing"
      echo "       was changed for it."
      cat "$TMPERR" >&2
    } >&2
    exit 3
  fi
  # `|| true` is load-bearing: a package with no `latest` key makes grep find
  # nothing and exit 1, and under `set -o pipefail` that would end the script
  # here — turning "this package has no latest tag", which is a package to
  # repair, into a silent abort with no output at all.
  latest="$(printf '%s\n' "$dist_tags_json" \
    | grep -oE '"latest"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | head -1 \
    | sed 's/.*"\([^"]*\)"$/\1/' || true)"
  if ! versions_json="$(npm view "$name" versions --json 2>"$TMPERR")"; then
    {
      echo "error: could not read the published versions of ${name}."
      echo "       Whether a stable version exists is UNKNOWN, so nothing was"
      echo "       changed for it."
      cat "$TMPERR" >&2
    } >&2
    exit 3
  fi

  # The array is read with a version-shaped grep rather than a JSON parser: the
  # only strings it can contain are versions, and a version-shaped filter is
  # exactly what the next step wants anyway. `npm view --json` prints one
  # version per line, but the shape is not relied on.
  versions="$(printf '%s\n' "$versions_json" | grep -oE '"[0-9]+\.[0-9]+\.[0-9]+(-[^"]*)?"' | tr -d '"' || true)"

  # An absent `latest` is not "correct": a package with no latest tag is exactly
  # one a fresh install cannot resolve, so it falls through to the repair path
  # rather than being reported as fine.
  if [ -n "$latest" ] && ! is_prerelease "$latest"; then
    echo "${name}: latest=${latest} is a stable version; nothing to do."
    ALREADY=$((ALREADY + 1))
    continue
  fi

  stable="$(newest_stable "$versions")"

  if [ -n "$stable" ]; then
    if [ "$DRY_RUN" = 1 ]; then
      echo "${name}: would re-point latest ${latest:-<none>} -> ${stable} (dry run)."
      REPAIRED=$((REPAIRED + 1))
      continue
    fi
    if npm dist-tag add "${name}@${stable}" latest; then
      echo "${name}: latest re-pointed ${latest:-<none>} -> ${stable}."
      REPAIRED=$((REPAIRED + 1))
    else
      echo "error: failed to re-point latest of ${name} to ${stable}." >&2
      exit 1
    fi
    continue
  fi

  # No stable release exists yet. There is nowhere to point `latest`, so the
  # only honest thing is to say so where a human will see it.
  echo "${name}: latest=${latest:-<none>} is a prerelease and no stable version exists yet." >&2
  NOTICE_ROWS="${NOTICE_ROWS}| \`${name}\` | \`${latest:-<none>}\` | $(printf '%s' "$versions" | tr '\n' ' ') |
"
  NOTICE_NEEDED=1
done <<EOF
$PACKAGE_DIRS
EOF

if [ "$NOTICE_NEEDED" = 1 ]; then
  summary "## npm \`latest\` currently points at a release candidate

No stable release exists yet for the package(s) below, so there is nowhere to
re-point \`latest\` at. npm sets \`latest\` on a package's **first** publish even
when \`--tag next\` is passed, so \`npm install <package>\` resolves the
candidate until the first stable release is published — the publish that
creates it sets \`latest\` to it, and this stops being true on its own.

This is a notice, not a failure: the release itself is unaffected.

| package | \`latest\` now | published versions |
| --- | --- | --- |
${NOTICE_ROWS}"
fi

echo "npm-latest-tag: ${REPAIRED} re-pointed, ${ALREADY} already correct, notice_needed=${NOTICE_NEEDED}"
exit 0
