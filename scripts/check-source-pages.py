#!/usr/bin/env python3
"""check-source-pages.py — CI gate: the per-source pages are fresh.

Thin entry point over `scripts/gen-source-pages.py`, so the generator and the
check cannot drift into two answers to the same question. It re-derives the
expected set of pages under `docs/sources/` — every local harness in
`crates/chat-stasher/data/harness-registry-v1.json`, every web platform in
`apps/extension/lib/contract.ts`'s `ALL_PLATFORMS`, and every official-export
platform the import seam in `crates/chat-stasher/src/import.rs` offers — and
fails when any of these move without the committed pages being re-derived:

  * a source with no page, or a page no source owns;
  * a generated header block that no longer matches its inputs (id, surface,
    status, last verified, the per-OS cells);
  * the generated index `docs/sources/README.md`;
  * a hand-written body that lost one of its fixed headings, or reordered
    them. The body's prose is never judged — only its shape.

The `Last verified` fact is the same record the support tables render, read
from the same registry and `lastVerified` fields with the same freshness
rule, so this gate goes red on the same day `check-support-matrix.py` does
when a recorded date crosses the 90-day threshold. That red is the expiry
signal working, not a failure of this gate.

Usage:
    python3 scripts/check-source-pages.py            # check; exit 1 if stale
    python3 scripts/check-source-pages.py --selftest # prove the check can fail

Exit codes: 0 = fresh · 1 = stale, missing or unrecognized page(s) · 2 = the
generator itself could not run (an input cannot be read or is malformed).
"""

from __future__ import annotations

import importlib.util
import os
import sys


def _load_generator():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "gen-source-pages.py")
    spec = importlib.util.spec_from_file_location("source_pages_gen", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main(argv: list[str]) -> int:
    gen = _load_generator()
    root = gen.default_root()
    if "--selftest" in argv:
        return gen.selftest()
    unknown = [a for a in argv if a not in ("--selftest",)]
    if unknown:
        print(f"[source-pages] unknown argument: {unknown[0]}", file=sys.stderr)
        return 2
    return gen.run_check(root)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
