#!/usr/bin/env python3
"""
check-semantic-defaults.py — release-gate step 8, as a standalone checker.

Rule: in production code, turning an unknown into a concrete value
(`.unwrap_or(0)` / `.unwrap_or(false)` / `.unwrap_or_default()`) must carry a
`// reason:` annotation explaining why that default is honest.

Extracted out of release-gate.sh (B93) so that the meta-test
`scripts/gate-selftest-semantic.sh` can exercise *this exact code* instead of
a second copy that would silently drift from the one the gate runs.

Usage:  check-semantic-defaults.py [REPO_ROOT]
Exit:   0 = no violations · 1 = violations found · 2 = usage/IO error
Stdout: "OK", or "FAILED:<n>" followed by one "  ! path:line [rule] -> code"
        per violation (format kept byte-compatible with the old inline block).
"""

import glob
import os
import re
import sys


def scan(root):
    src_pattern = os.path.join(root, "crates", "chat-stasher", "src", "**", "*.rs")
    files = sorted(glob.glob(src_pattern, recursive=True))

    all_violations = []
    for f in files:
        with open(f, "r", encoding="utf-8") as fh:
            lines = fh.readlines()

        # B93 fix: brace counting was the second blind spot. `doctor.rs` is full
        # of format!("{...}") literals, so a naive {/} tally never returns to zero
        # and everything after its test module stayed unchecked — the same class
        # of hole as the first version, found the same way: by injecting at a spot
        # the author of the check did not pick.
        # Rust convention puts the test module at top level, so use column-0
        # structure instead of brace arithmetic: the module opens at an
        # unindented `mod ... {` and closes at an unindented `}`.
        in_test = False
        pending_test = False
        test_indent = 0
        for idx, line in enumerate(lines):
            stripped = line.strip()
            if not in_test and stripped.startswith("#[cfg(test)]"):
                pending_test = True
                continue
            if pending_test:
                if stripped.startswith("mod ") or stripped.startswith("pub mod "):
                    # A one-liner module (`mod m { .. }`) opens and closes on the
                    # same line — entering test mode there would swallow the rest
                    # of the file, which is the very bug this check keeps hitting.
                    if stripped.count("{") > stripped.count("}"):
                        in_test = True
                        test_indent = len(line) - len(line.lstrip())
                pending_test = False
                continue
            if in_test:
                # Close on the first `}` whose indentation is at or left of the
                # `mod ... {` that opened it. Indentation is structural here and
                # brace arithmetic is not: format!("{}") literals are everywhere.
                if stripped == "}" and (len(line) - len(line.lstrip())) <= test_indent:
                    in_test = False
                continue
                continue
            if stripped.startswith("//") or stripped.startswith("/*") or stripped.startswith("*"):
                continue

            m1 = re.search(r"\.unwrap_or\s*\(\s*0[a-zA-Z0-9_.]*\s*\)", line)
            m2 = re.search(r"\.unwrap_or\s*\(\s*false\s*\)", line)
            m3 = re.search(r"\.unwrap_or_default\s*\(\s*\)", line)

            if m1 or m2 or m3:
                rule_name = "unwrap_or(0)" if m1 else ("unwrap_or(false)" if m2 else "unwrap_or_default()")
                # B92 fix 2: scan the whole contiguous comment block above, not just
                # the single line directly above. Accepting only a one-liner pushes
                # authors toward one-line reasons, and a one-line reason is exactly
                # the shape a rationalisation takes. A real explanation needs room.
                has_reason = False
                reason_text = ""
                if "// reason:" in line:
                    reason_text = line.split("// reason:", 1)[1].strip()
                else:
                    j = idx - 1
                    while j >= 0:
                        prev = lines[j].strip()
                        if not prev.startswith("//"):
                            break
                        if "// reason:" in prev:
                            reason_text = prev.split("// reason:", 1)[1].strip()
                            break
                        j -= 1

                if reason_text and len(reason_text) > 5 and reason_text.lower() != "ok":
                    has_reason = True

                if not has_reason:
                    rel_path = os.path.relpath(f, root)
                    all_violations.append((rel_path, idx + 1, rule_name, stripped))
    return all_violations


def main(argv):
    if len(argv) > 2:
        print("usage: check-semantic-defaults.py [REPO_ROOT]", file=sys.stderr)
        return 2
    root = argv[1] if len(argv) == 2 else os.getcwd()
    if not os.path.isdir(root):
        print(f"not a directory: {root}", file=sys.stderr)
        return 2

    violations = scan(root)
    if violations:
        print(f"FAILED:{len(violations)}")
        for rel_path, line_no, rule, code in violations:
            print(f"  ! {rel_path}:{line_no} [{rule}] -> {code}")
        return 1
    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
