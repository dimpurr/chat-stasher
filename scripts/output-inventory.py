#!/usr/bin/env python3
"""output-inventory.py — dump every user-visible string into docs/output-inventory.txt.

The inventory is a DERIVED artifact, not an authority: it is regenerated from
source by this script and any edit to it will be overwritten. Its job is to make
"what user-visible text changed in this PR" visible in a plain diff — one stable
line per string, sorted by file+line, so a one-word change moves exactly one
line.

    python3 scripts/output-inventory.py             # regenerate  docs/output-inventory.txt
    python3 scripts/output-inventory.py --check     # regenerate to memory, diff against the
                                                    # committed file, exit 1 if they differ
                                                    # (this is the CI gate form)

Exit codes: 0 = ok · 1 = --check found drift (or the file is missing) · 2 = usage error.
"""

from __future__ import annotations

import difflib
import os
import sys

# allow running from any cwd: make the sibling shared module importable
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _user_strings as us  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_PATH = os.path.join(REPO, "docs", "output-inventory.txt")

HEADER = """\
# chat-stasher output inventory — DERIVED FILE. Do not edit by hand.
# Regenerate:   python3 scripts/output-inventory.py
# Gate in CI:   python3 scripts/output-inventory.py --check   (exit 1 on drift)
#
# Coverage (grep-verified 2026-08-26, see spike/RU-tooling.md):
#   println!/print!/eprintln!/eprint!/write!/writeln!   first string literal
#   anyhow::bail! / anyhow! / .context() / .with_context()   first string literal
#   clap help: /// doc inside clap types + explicit about/help = "..."  in #[command]/#[arg]
#   panic!/unreachable!/todo!/unimplemented!   prod code only (today: none)
# NOT covered: standalone format! (mostly construction), --json values, apps/extension.
#
# Format: <kind>  <file>:<line>  <text>   · sorted by file+line · deterministic
"""


def render(hits: list[us.Hit]) -> str:
    prod = [h for h in hits if not h.in_test]
    prod.sort(key=lambda h: (h.path, h.line, h.kind))
    lines = [HEADER.rstrip("\n")]
    for h in prod:
        lines.append(f"{h.kind:<12} {h.path}:{h.line}  {h.text}")
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    check = False
    for arg in argv:
        if arg == "--check":
            check = True
        else:
            print(f"output-inventory: unknown argument: {arg}", file=sys.stderr)
            return 2

    hits = us.extract_all(REPO)
    new_text = render(hits)

    if not check:
        with open(OUT_PATH, "w", encoding="utf-8") as fh:
            fh.write(new_text)
        print(f"output-inventory: wrote {os.path.relpath(OUT_PATH, REPO)} "
              f"({sum(1 for h in hits if not h.in_test)} strings)")
        return 0

    # --check: compare against the committed file
    if not os.path.exists(OUT_PATH):
        print(f"output-inventory --check: MISSING {os.path.relpath(OUT_PATH, REPO)} — "
              f"run the script without --check once to create it", file=sys.stderr)
        return 1

    with open(OUT_PATH, "r", encoding="utf-8") as fh:
        old_text = fh.read()
    if old_text == new_text:
        print("output-inventory --check: OK — docs/output-inventory.txt is up to date")
        return 0

    old_lines = old_text.splitlines()
    new_lines = new_text.splitlines()
    diff = list(difflib.unified_diff(old_lines, new_lines,
                                     fromfile="docs/output-inventory.txt (committed)",
                                     tofile="docs/output-inventory.txt (fresh)",
                                     lineterm=""))
    print(f"output-inventory --check: DRIFT — {len(old_lines)} committed lines, "
          f"{len(new_lines)} fresh, {len(diff) // 2} changed lines", file=sys.stderr)
    for line in diff[:60]:
        print(line, file=sys.stderr)
    if len(diff) > 60:
        print(f"  … {len(diff) - 60} more diff lines", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
