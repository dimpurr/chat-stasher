#!/usr/bin/env bash
# commit-message-range.sh - resolve which commits the commit-message gate must
# check for a given CI event, then run the checker on that range.
#
# There is exactly ONE caller: .github/workflows/commit-messages.yml. ci.yml must
# not grow a second one. The check lives in a workflow with no concurrency
# cancellation because a commit escapes a range gate only when the run that would
# check it is cancelled; invoking this script from ci.yml, whose jobs run under
# cancel-in-progress, would reopen that hole by exactly the mechanism this design
# removes. It lives in scripts/ rather than inline so the event -> range mapping
# is written once and can be driven directly in a test.
#
# Environment (all provided by the workflow step that calls this script):
#   EVENT_NAME   github.event_name                 'pull_request' | 'push' | ...
#   EVENT_BEFORE github.event.before               previous tip sha on a push
#   HEAD_SHA     github.sha                        commit the event is about
#   PR_BASE_SHA  github.event.pull_request.base.sha
#   PR_HEAD_SHA  github.event.pull_request.head.sha
#   GITHUB_REF   github.ref    refs/heads/main => a push to main, whose unusable
#                before-sha proves nothing because main's base moves; any other
#                ref ranges against the default branch when before is unusable
#   PUSH_BASE    base a push with no usable before-sha ranges against
#                (origin/main by default). Never set it for a main push.
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
      # A normal push: check exactly the commits this push adds. Reliable only
      # because this gate is never in a cancelled concurrency group - a burst of
      # pushes each range their own small delta, and no run's commits are lost to
      # a cancellation. A rewind whose range resolves to nothing still exits 3:
      # that empty range reaches the checker through this branch, and 3 is the
      # honest answer for it.
      range="$EVENT_BEFORE..$HEAD_SHA"
    else
      # No usable before-sha: a new branch or tag, or a force-push whose old tip
      # is unreachable. A tag push is NOT a case of its own - it is a ref whose
      # before is zero, so it falls through here like any new ref. Range against
      # the default branch, which subsumes the commits a cancelled or unknown
      # ref move could hide, so an off-main Chinese commit cannot slip under an
      # English tip.
      if [[ "${GITHUB_REF:-}" == refs/heads/main ]]; then
        # On main the default branch IS the ref that moved, so origin/main already
        # equals the (possibly rewritten) tip and an empty range cannot be read as
        # "nothing new": a force-push that buried a Chinese commit under an English
        # tip looks exactly like that. Exit 3 - nothing was proven - instead of
        # blessing the tip.
        echo "push to main (${GITHUB_REF}) with no usable before-sha (${EVENT_BEFORE:-unset}) proves nothing about the commits now on main" >&2
        exit 3
      fi
      PUSH_BASE="${PUSH_BASE:-origin/main}"
      if ! git rev-parse --verify -q "$PUSH_BASE" >/dev/null 2>&1; then
        echo "no usable base commit (before=${EVENT_BEFORE:-unset}) and $PUSH_BASE cannot be resolved" >&2
        exit 3
      fi
      if [ "$(git rev-list --count "$PUSH_BASE..$HEAD_SHA" 2>/dev/null || echo 1)" = "0" ]; then
        # HEAD is already reachable from the base - a branch at a main commit, or
        # a tag whose commit is on main. Ranging would prove nothing; nothing new
        # was introduced, and "nothing new" is a measurement of an empty set, not
        # a failure to look. Still check the one commit HEAD names, because it is
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
    # Only 'push' and 'pull_request' have a defined base here. Any other event -
    # merge_group, workflow_dispatch, a trigger added later - has no base this
    # script can name, and checking HEAD alone is the original tip-only hole: a
    # Chinese parent under an English tip would pass. Exit 3, the same honest
    # "nothing was proven" used everywhere else, so a new trigger has to come
    # back here and say what its range is.
    echo "${EVENT_NAME:-unset} has no base commit this script can resolve; nothing was proven" >&2
    exit 3
    ;;
esac

python3 scripts/check-commit-messages.py --range "$range"
