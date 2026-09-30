#!/usr/bin/env python3
"""regenerate-merge-drivers.py — the merge driver for the two derived files.

These two files — ``docs-dev/citations.lock`` and ``docs-dev/output-inventory.txt``
— are RECORDINGS of derived state. Two branches that each regenerated them
against their own base disagree on every line, so no textual 3-way mix of the
three blobs (``%O``/``%A``/``%B``) is ever the right merge result; the only
correct value is a fresh regeneration from the *merged* source tree.

A per-file merge driver cannot produce that value, and it is worth saying why
the obvious attempt fails rather than silently building a half-truth. Git runs a
merge driver before it has laid the merged source into the working tree — the
tree the driver reads is still the pre-merge checkout, so a rebuild would pin
stale line numbers and a missing file would be read as deleted. (Verified in the
W275 scratch replay: a file added by the merged-in branch was not present when
the driver ran.) No per-blob merge driver can see the merged tree.

So the correct behaviour for these files at merge time is: never *stop* the
merge over a recording, take the current side (disposable — it is exactly the
thing regeneration replaces), and let the integration step regenerate. That is
what this driver does: it exits 0, which tells git to keep ``%A`` (the current
branch's version) as the resolution, and the integration step regenerates both
files from the merged tree before committing. There are two of those, and the
value the driver leaves behind is only ever corrected by one of them:
``scripts/dev/rebase-onto-main.sh`` when the branch is rebased, and the manual
flow in CONTRIBUTING.md ("Resolving a merge") when a merge is landed directly.

The alternative — marking these files so a merge always stops and a human
regenerates by hand — is precisely the extra rebase worker this removes, and it
adds nothing: regeneration is deterministic, so there is nothing for the human
to judge.

Exit codes: 0 = keep ``%A`` (regeneration deferred to integration) · 2 = misuse.
"""

from __future__ import annotations

import sys


def main(argv: list[str]) -> int:
    if len(argv) != 5:
        print("usage: regenerate-merge-drivers.py <kind> %O %A %B", file=sys.stderr)
        return 2
    kind = argv[1]
    print(
        f"[merge-regenerate] {kind}: resolved from the current side; regenerate "
        'from the merged tree before committing (rebase-onto-main.sh for a rebase, '
        'CONTRIBUTING.md "Resolving a merge" for a direct merge)',
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
