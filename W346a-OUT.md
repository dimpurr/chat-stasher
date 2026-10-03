# W346a scoped fix — round 1

Implementation commit: `6353e7aa4cb442d26341a9a31a9c8a4d4b47c0bc`

The semantic citation guard now keeps the live threat-model binding active when
claim prose is wrapped or uses the described “fetches” wording. A missing or
drifted live claim fails loudly; claimless synthetic relocation fixtures retain
ordinary citation behavior. Semantic failures have their own diagnostic, the
message is derived from each binding, and cited implementation ranges are
bounded to 40 lines. Focused regression tests now run from both CONTRIBUTING.md
and the CI gates job.

The parallel claim in `docs-dev/privacy.md` remains outside this round's scope.

Verification completed:

- `python3 -m unittest scripts/tests/test_check_citation_drift.py` — 10 tests pass.
- `python3 scripts/check-citation-drift.py` — pass.
- `python3 scripts/check-doc-links.py` — pass.
- The CONTRIBUTING local checks, including the Rust suite under both isolation
guards, passed.
- New regression tests were run against the original checker and failed on the
  disappearance, duplicate-claim, wrong-range, and broad-range cases.

No citation lock data was regenerated.
