#!/usr/bin/env bash
# commit-message-range.sh - resolve which commits the commit-message gate must
# check for a given CI event, then run the checker on that range.
#
# This is the single source of the event -> range mapping that both
# .github/workflows/ci.yml and .github/workflows/commit-messages.yml invoke.
# It lives in scripts/ so the two workflows cannot drift: a change to when a PR
# vs a push is checked, or how a new branch is ranged, is made here once and
# picked up by both.
#
# Environment (all provided by the workflow step that calls this script):
#   EVENT_NAME   github.event_name                 'pull_request' | 'push' | ...
#   EVENT_BEFORE github.event.before               previous tip sha on a push
#   HEAD_SHA     github.sha                        commit the event is about
#   PR_BASE_SHA  github.event.pull_request.base.sha
#   PR_HEAD_SHA  github.event.pull_request.head.sha
#
# Exit codes follow the checker's contract: 0 = checked and clean, 1 = a
# violation found, 2 = usage / event-misconfiguration, 3 = no usable base, so
# nothing was proven. 3 is the honest answer when a push has no before-sha and
# no default branch to range against, and when the resolved range is empty.
set -euo pipefail

zero="0000000000000000000000000000000000000000"

case "${EVENT_NAME:-}" in
  pull_request)
    if [ -z "${PR_BASE_SHA:-}" ] || [ -z "${PR_HEAD_SHA:-}" ]; then
      echo "the pull_request event carried no base/head sha" >&2
      exit 2
    fi
    range="$PR_BASE_SHA..$PR_HEAD_SHA"
    ;;
  push)
    if [ -n "${EVENT_BEFORE:-}" ] && [ "$EVENT_BEFORE" != "$zero" ] \
       && git cat-file -e "${EVENT_BEFORE}^{commit}" 2>/dev/null; then
      # A normal push: check exactly the commits this push adds.
      range="$EVENT_BEFORE..$HEAD_SHA"
    elif git rev-parse --verify -q origin/main >/dev/null 2>&1; then
      # A new branch, or a force-push whose old tip is not in the checkout.
      # There is no plausible "before". Ranging against the default branch
      # checks every commit this branch introduces, not just its tip, so a
      # non-English commit hiding under an English tip cannot pass.
      echo "no usable before-sha (before=${EVENT_BEFORE:-unset}); ranging against origin/main" >&2
      range="origin/main..$HEAD_SHA"
    else
      # No base at all: failing loudly is the only honest outcome. Checking the
      # tip alone and passing would report "we looked" for a length of history
      # we did not look at.
      echo "no usable base commit (before=${EVENT_BEFORE:-unset}) and no origin/main to range against" >&2
      exit 3
    fi
    ;;
  *)
    echo "${EVENT_NAME:-unset} has no base commit; checking $HEAD_SHA alone" >&2
    range="$HEAD_SHA"
    ;;
esac

python3 scripts/check-commit-messages.py --range "$range"