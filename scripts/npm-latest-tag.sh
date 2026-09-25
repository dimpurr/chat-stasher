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
#   **`latest` must name the newest published stable version.**
#
# The narrower form of that sentence — "must never point at a prerelease" — is
# what this script said until it was reviewed, and the difference is not
# pedantic. A registry holding 0.4.0 and 0.5.0 with `latest` still on 0.4.0 has
# no prerelease anywhere, so the narrow test passes it, and a plain
# `npm install <pkg>` then installs the older build. That is the same user-visible
# harm as installing a candidate, arrived at by a route the narrow test cannot
# see. The two agree on every case where `latest` is a prerelease or absent;
# they differ exactly where an older stable is left behind.
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
# to happen after the publish. The question the script asks is the invariant
# itself — does `latest` name the newest published stable version? — and the
# three answers to it:
#
#   · yes -> nothing to do. On an ordinary stable release this is the answer for
#            every package, which is why the repair cannot undo the publish it
#            follows.
#   · no, and a stable version exists -> re-point `latest` at the newest one
#            (`npm dist-tag add <pkg>@<version> latest`). Whether `latest` held
#            a prerelease, an older stable, or no version at all, the repair is
#            the same one, and it is what makes this idempotent: run it twice
#            and the second run has nothing to do.
#   · no, and no stable version exists -> there is nowhere to re-point it. Write
#            a clear notice to the job summary saying so, because the state is
#            expected and temporary, not an error to fail the release over.
#
# This is written as the invariant rather than as "run this after an rc" so it
# is self-describing, and so it also repairs a package left wrong by any other
# route — including one whose `latest` was never touched by the publish at all.
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
      echo "       Whether 'latest' names the newest stable version is UNKNOWN, so"
      echo "       nothing was changed for it."
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

  stable="$(newest_stable "$versions")"

  # Correct means `latest` IS the newest stable version — compared as the exact
  # string the registry reports, not by re-parsing it, so this cannot drift from
  # the value the read above produced.
  #
  # Everything else falls through to the repair path, and each excluded case is
  # excluded for its own reason:
  #   · a prerelease is not a stable version, so it cannot be the newest one;
  #   · an absent `latest` is not "correct" either — a package with no latest
  #     tag is exactly one a fresh install cannot resolve;
  #   · a stable that is not the newest stable (0.4.0 while 0.5.0 is published)
  #     is the case the prerelease-only test used to pass over, and it leaves a
  #     plain `npm install` on the older build;
  #   · a stable naming a version the registry no longer lists is not the newest
  #     published stable by definition — whatever it points at is not published,
  #     so a fresh install cannot resolve it either.
  # `stable` being empty is the one thing that keeps a package out of the repair
  # path regardless: with no stable version to point at, there is nothing to
  # move `latest` to, and that is the notice path below.
  if [ -n "$stable" ] && [ "$latest" = "$stable" ]; then
    echo "${name}: latest=${latest} is the newest stable version; nothing to do."
    ALREADY=$((ALREADY + 1))
    continue
  fi

  if [ -n "$stable" ]; then
    # How the move reads in the log. A prerelease or an absent tag is what this
    # exists for and needs no explanation; a stable that is merely older is the
    # one a reader would not expect the repair to touch, so it says so.
    if [ -n "$latest" ] && ! is_prerelease "$latest"; then
      moved_from="${latest} (an older stable)"
    else
      moved_from="${latest:-<none>}"
    fi
    if [ "$DRY_RUN" = 1 ]; then
      echo "${name}: would re-point latest ${moved_from} -> ${stable} (dry run)."
      REPAIRED=$((REPAIRED + 1))
      continue
    fi
    if npm dist-tag add "${name}@${stable}" latest; then
      echo "${name}: latest re-pointed ${moved_from} -> ${stable}."
      REPAIRED=$((REPAIRED + 1))
    else
      echo "error: failed to re-point latest of ${name} to ${stable}." >&2
      exit 1
    fi
    continue
  fi

  # No stable release exists yet. There is nowhere to point `latest`, so the
  # only honest thing is to say so where a human will see it. The two reasons
  # `latest` can be wrong here are different states and are named differently —
  # an absent tag is not a prerelease, and reporting it as one would be the
  # "record an unknown as a value" mistake in miniature.
  if [ -n "$latest" ]; then
    echo "${name}: latest=${latest} is a prerelease and no stable version exists yet." >&2
  else
    echo "${name}: no latest tag and no stable version exists yet." >&2
  fi
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
