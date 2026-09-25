# Working on chat-stasher

## What belongs in this file

This file is loaded into every agent's context automatically, which means
anything written here is followed without being questioned. So it holds only
invariants — properties that do not change when a version ships or a dependency
moves.

Anything of the form "which approach is currently best", "what we are working on
now", or "which backend to prefer" does not belong here. It goes stale while
still being injected, and a stale instruction that is always present beats a
correct one that requires opening a file. Before adding a line, ask whether it
will still be true in six months. If unsure, write a pointer, not an answer.

## Checks

`CONTRIBUTING.md` lists the exact commands, all of which must exit 0. Run them
before proposing a change; that list and CI are meant to be the same set. This
file deliberately does not repeat them — two copies of a gate drift apart, and
the copy nobody reads is the one that rots.

## Invariants

These three are the reason the tool exists. A change that weakens any of them is
not a tradeoff to be argued; it is out of scope.

1. **An unknown must never be recorded as empty.** "Not found", "unreadable",
   and "not there" are three different states and must stay three different
   states, all the way to what the user sees. A count of zero is a measurement,
   not a fallback.
2. **Exit codes carry that distinction.** `3` = did not finish reading (so any
   absence proves nothing) · `1` = finished reading and failed · `2` = usage
   error. Collapsing 3 into 1 turns "we do not know" into "we checked".
3. **The archive is append-only.** Deleting or rotating a source must not delete
   what was already archived, and the archive must not quietly become a mirror
   of the current machine state.

## Language

The public surface is English: commit messages, pull request titles and bodies,
issues, and the source, comments and docs that ship with the tool. The one
exception is `apps/extension/locales/zh_CN.yml` — the same exception
`check-terminology.py`'s T5 rule encodes, and no new Chinese belongs anywhere
else either. `scripts/hooks/commit-msg` stops a violation before the commit
exists and `scripts/check-commit-messages.py` in CI catches it after. The hook
only runs if `core.hooksPath` points at `scripts/hooks`; git does not read hooks
out of a checkout by itself, so set that once per clone (`git config
core.hooksPath scripts/hooks`).

## Do not

Each of these has happened here, and each looked harmless in review.

- **Do not weaken an assertion to make a test pass.** If a test fails, either
  the code is wrong or the test's premise is wrong. Say which.
- **Do not change what a test exercises in order to make it stable.** A flaky
  corruption test was once "fixed" by disabling the on-disk cache in the test
  setup. It passed 30/30, no assertion was touched, and the diff read as
  harmless — but real runs have the cache enabled, so the test no longer
  covered the configuration users have. This is harder to catch than a weakened
  assertion and should be suspected first when a fix makes a flake disappear.
- **Do not regenerate a derived lockfile to clear a failure.** Citations,
  inventories, and manifests are checked precisely so that a change to what they
  describe is noticed. Re-derive only after confirming the underlying claim
  still holds.
- **Do not silence a platform with a one-sided `cfg` guard.** Compiling a test
  out on one platform removes coverage without removing the red, which is worse
  than the red. Assert the correct behavior on both sides, or state in the file
  why the property does not exist on that platform.
- **Do not treat a passing test as evidence that it can fail.** A new regression
  test must be shown failing against the unfixed code before it means anything.

## Before changing recorded behavior

Much of the current design exists because an alternative was tried and rejected.
Before changing an established behavior, look for the decision record that
explains it, and if the change invalidates an assumption another part of the
system depends on, say so explicitly in the change description.

For the rule governing `docs-dev/`, see [`docs-dev/README.md`](docs-dev/README.md).
