#!/usr/bin/env python3
"""check-support-matrix.py — CI gate: the committed support tables are fresh.

Thin entry point over `scripts/gen-support-matrix.py`, so the generator and the
check cannot drift into two answers to the same question. It regenerates the
short and full tables from the two sources of truth

  * `crates/chat-stasher/data/harness-registry-v1.json`, and
  * `apps/extension/lib/contract.ts`'s `ALL_PLATFORMS`,

and fails when either differs from what is committed:

  * the committed tables under `scripts/support-matrix/`, and
  * any `<!-- support-matrix:short|full:start/end -->` block in README.md or
    `docs/*.md` (none have been inserted yet — the README rewrite owns that).

Usage:
    python3 scripts/check-support-matrix.py            # check; exit 1 if stale
    python3 scripts/check-support-matrix.py --selftest # prove the check can fail

Exit codes: 0 = fresh · 1 = stale · 2 = the generator itself could not run.
"""

from __future__ import annotations

import importlib.util
import os
import sys


def _load_generator():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "gen-support-matrix.py")
    spec = importlib.util.spec_from_file_location("support_matrix_gen", path)
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
        print(f"[support-matrix] unknown argument: {unknown[0]}", file=sys.stderr)
        return 2
    return gen.run_check(root)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
