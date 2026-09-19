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
#   GITHUB_REF   github.ref                        refs/tags/* => a tag push
#   PUSH_BASE    base a branch push ranges against (origin/main) when the
#                workflow wants every push proven against the default branch
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
    # A tag push is a new ref: the event carries no "before". The tagged commit
    # was already proven when it landed on main, so ranging against any base is
    # either empty (HEAD already on origin/main) or misleading. The honest check
    # is the tagged commit itself - the thing the release names, checked for
    # English on its own rather than let an empty range fail the gate for nothing.
    if [[ "${GITHUB_REF:-}" == refs/tags/* ]]; then
      echo "tag push ${GITHUB_REF}: no new branch history to prove; checking the tagged commit" >&2
      range="$HEAD_SHA"
    elif [ -n "${EVENT_BEFORE:-}" ] && [ "$EVENT_BEFORE" != "$zero" ] \
       && git cat-file -e "${EVENT_BEFORE}^{commit}" 2>/dev/null; then
      # A normal push: check exactly the commits this push adds. A rewind whose
      # range resolves to nothing still exits 3 - that empty range reaches the
      # checker through this branch, and 3 is the honest answer for it.
      range="$EVENT_BEFORE..$HEAD_SHA"
    else
      # No usable before-sha: a new branch, a force-push whose old tip is gone,
      # or - when the calling workflow sets PUSH_BASE - every branch push. Range
      # against that base (origin/main by default), so a cancelled earlier run of
      # a burst can never remove a commit from the window a later run proves:
      # the base does not move when you push to a branch, while `before` does.
      PUSH_BASE="${PUSH_BASE:-origin/main}"
      if ! git rev-parse --verify -q "$PUSH_BASE" >/dev/null 2>&1; then
        echo "no usable base commit (before=${EVENT_BEFORE:-unset}) and $PUSH_BASE cannot be resolved" >&2
        exit 3
      fi
      if [ "$(git rev-list --count "$PUSH_BASE..$HEAD_SHA" 2>/dev/null || echo 1)" = "0" ]; then
        # HEAD is already reachable from the base - a branch at a main commit, or
        # a release tagged on main. Ranging would prove nothing; nothing new was
        # introduced, and "nothing new" is a measurement of an empty set, not a
        # failure to look. Still check the one commit HEAD names, because it is
        # cheap and honest to confirm the ref's message is English.
        echo "HEAD ($HEAD_SHA) is already reachable from $PUSH_BASE; nothing new to prove, checking HEAD itself" >&2
        range="$HEAD_SHA"
      else
        echo "ranging ${PUSH_BASE}..$HEAD_SHA (no before-sha)" >&2
        range="$PUSH_BASE..$HEAD_SHA"
      fi
    fi
    ;;
  *)
    echo "${EVENT_NAME:-unset} has no base commit; checking $HEAD_SHA alone" >&2
    range="$HEAD_SHA"
    ;;
esac

python3 scripts/check-commit-messages.py --range "$range"
