#!/usr/bin/env python3
"""check-doc-links.py — every link between our own Markdown files resolves.

A reader page that points at a file that moved, or at a heading that was
renamed under it, fails silently: the page still renders, and the reader
either lands on the wrong section or on nothing. Nothing else in this
repository reads the Markdown for its links, so a rename could break one and
every gate stayed green — which is exactly what happened when `docs/` became
`docs-dev/` and `README.md`'s own support section was rewritten twice.

Which documents are read, and why that has to be a decision at all:

  * Tracked Markdown, and only tracked Markdown, read from the index the way
    `check-private-paths.py` reads it. A working checkout sits beside material
    that is not part of the public document set — draft notes, scratch trees —
    and that is exactly what a clean CI checkout does not have. Walking the
    whole filesystem instead made a local run report 458 broken links a CI
    checkout could not even see: red here, green there, which is the inverse
    of what a local gate is for. Reading the index means a local run asks the
    same question a clean checkout will ask, so the two answers agree.
  * A root that is not a git work tree — the self-test's fixtures, an exported
    tree — has no tracked set, so the fallback walks the filesystem directly.
    `SKIP_DIRS` below is that fallback's skip list; the tracked set needs no
    such list, because nothing Git ignores is in the index.

What is checked, and what is deliberately not:

  * Relative links between repository files: the target must exist. Both
    `foo.md` and `foo.md#a-heading` are resolved relative to the file that
    writes them.
  * Fragments: a `#heading` must match a heading in the target file (or in the
    same file, for a bare `#heading`), by GitHub's own slug rule.
  * External `http(s)://` links are NOT fetched. A gate that needs the network
    fails when the network does, and a 404 from someone else's site is not a
    fact about this repository. They are counted and reported, never failed on.
  * Code is not prose: fenced blocks are skipped, so a `#` inside a shell
    example is not read as a link.

Usage:
    python3 scripts/check-doc-links.py            # check; exit 1 on any dead link
    python3 scripts/check-doc-links.py --list     # print every link it resolves
    python3 scripts/check-doc-links.py --selftest # prove the check can fail

Exit codes: 0 = every local link resolves · 1 = at least one does not ·
2 = usage error.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Directories the filesystem fallback does not descend into: build output,
# dependencies, and anything fetched rather than written. A tracked set has no
# use for this list — nothing Git ignores is in the index — so it applies only
# when the root is not a git work tree and there is no index to read.
SKIP_DIRS = {".git", "target", "node_modules", ".wxt", ".output", "__pycache__", "dist"}

# `[text](target)` and `![alt](target)`, with an optional title after the
# target. Bare targets keep the historical whitespace/paren boundary; angle-
# bracket targets may contain spaces and parentheses until their closing `>`.
INLINE_LINK = re.compile(
    r"!?\[[^\]]*\]\(\s*(?:<([^>]+)>|([^)\s>]+))"
    r"(?:\s+[\"'][^\"']*[\"'])?\s*\)"
)
# `[label]: target` link definitions.
LINK_DEFINITION = re.compile(r"^\s*\[[^\]]+\]:\s*<?([^\s>]+)>?")
# A heading, ATX only: our documents do not use setext headings, and guessing
# at them would be a rule nobody can read out of the file.
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
FENCE = re.compile(r"^\s*(```+|~~~+)")


def slugify(heading: str) -> str:
    """GitHub's heading anchor rule: lowercase, drop punctuation, spaces to `-`.

    Letters and digits are kept whatever their script (`str.isalnum`), which is
    what keeps a heading with a non-ASCII word addressable; `-` and `_` survive.
    """
    out = []
    for ch in heading.strip().lower():
        if ch.isalnum() or ch in "-_":
            out.append(ch)
        elif ch.isspace():
            out.append("-")
    return "".join(out)


def anchors(text: str) -> set[str]:
    """Every fragment a reader could land on in this document.

    Headings are the usual source, but a document may also carry an anchor of
    its own (`<a id="...">`), which `docs-dev/threat-model.md` does for a list
    item that has no heading to point at. Both are legitimate targets, and
    reporting the second kind as missing would be a false alarm about a link
    that works.
    """
    found: set[str] = set()
    lines = text.splitlines()
    fenced = in_fence_flags(lines)
    for index, line in enumerate(lines):
        if fenced[index]:
            continue
        match = HEADING.match(line)
        if match:
            found.add(slugify(match.group(2)))
        for name in re.findall(r"""\bid=["']([^"']+)["']|\bname=["']([^"']+)["']""", line):
            found.add(name[0] or name[1])
    return found


def in_fence_flags(lines: list[str]) -> list[bool]:
    """One flag per line: was it inside a fenced code block."""
    flags: list[bool] = []
    fence: str | None = None
    for line in lines:
        match = FENCE.match(line)
        if fence is None:
            flags.append(False)
            if match:
                fence = match.group(1)[0] * 3
                flags[-1] = True
        else:
            flags.append(True)
            if match and match.group(1)[0] * 3 == fence:
                fence = None
    return flags


def targets_in(text: str) -> list[tuple[int, str]]:
    """(line number, raw target) for every link in one document."""
    lines = text.splitlines()
    fenced = in_fence_flags(lines)
    found: list[tuple[int, str]] = []
    for index, line in enumerate(lines):
        if fenced[index]:
            continue
        for match in INLINE_LINK.finditer(line):
            found.append((index + 1, match.group(1) or match.group(2)))
        definition = LINK_DEFINITION.match(line)
        if definition:
            found.append((index + 1, definition.group(1)))
    return found


def out_of_scope(target: str) -> bool:
    return (
        target.startswith(("http://", "https://", "mailto:", "data:", "//"))
        or target.startswith("#")
        or target == ""
    )


class Problem:
    def __init__(self, path: str, line: int, target: str, why: str) -> None:
        self.path = path
        self.line = line
        self.target = target
        self.why = why

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: {self.why}: {self.target}"


def tracked_markdown(root: str) -> list[str] | None:
    """Every tracked Markdown file, or None when Git cannot answer.

    `git ls-files` reads the index, which is what makes the answer the public
    document set rather than whatever happens to sit on disk. A tracked path
    whose working-tree copy is gone contributes nothing: a deletion is a fact
    `git status` reports, and a link checker that crashed on one would be a
    crash, not a finding.
    """
    proc = subprocess.run(["git", "-C", root, "ls-files", "-z"], capture_output=True)
    if proc.returncode != 0:
        return None
    names = proc.stdout.decode("utf-8", "replace").split("\0")
    return [
        name
        for name in names
        if name.endswith((".md", ".markdown")) and os.path.isfile(os.path.join(root, name))
    ]


def markdown_files(root: str) -> list[str]:
    """Every document the check reads, as root-relative paths.

    In a git work tree that is the tracked Markdown set, because "what a
    clean checkout will have" is the question the check exists to answer the
    same way locally that CI answers it. A root that is not a git work tree —
    the self-test's fixtures, an exported tree — has no index to read, and
    there the filesystem is walked and SKIP_DIRS is what keeps build output
    out of the answer: walking what is there is a weaker but real answer,
    never a silent green.
    """
    tracked = tracked_markdown(root)
    if tracked is not None:
        return sorted(tracked)
    found: list[str] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            if name.endswith((".md", ".markdown")):
                found.append(os.path.relpath(os.path.join(dirpath, name), root))
    return sorted(found)


def check(root: str) -> tuple[list[Problem], int, int]:
    """Returns (problems, local links checked, external links counted)."""
    problems: list[Problem] = []
    local = external = 0
    anchor_cache: dict[str, set[str] | None] = {}

    for rel in markdown_files(root):
        path = os.path.join(root, rel)
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        for line, raw in targets_in(text):
            if raw.startswith(("http://", "https://", "mailto:", "data:", "//")):
                external += 1
                continue
            if raw == "":
                problems.append(Problem(rel, line, raw, "empty link target"))
                continue
            target, _, fragment = raw.partition("#")
            if target == "":
                target_path = path
                target_rel = rel
            else:
                target_rel = os.path.normpath(os.path.join(os.path.dirname(rel), target))
                target_path = os.path.join(root, target_rel)
                if os.path.isabs(target_rel) or target_rel.startswith(".."):
                    # A link out of the repository cannot be resolved from here,
                    # and saying "missing" about it would be a claim we cannot
                    # check. Reported, so it is visible rather than silent.
                    problems.append(
                        Problem(rel, line, raw, "link leaves the repository; cannot be checked")
                    )
                    continue
            local += 1
            if not os.path.exists(target_path):
                problems.append(Problem(rel, line, raw, "target does not exist"))
                continue
            if fragment:
                if target_rel not in anchor_cache:
                    if target_path.endswith((".md", ".markdown")):
                        with open(target_path, encoding="utf-8") as handle:
                            anchor_cache[target_rel] = anchors(handle.read())
                    else:
                        anchor_cache[target_rel] = None
                known = anchor_cache[target_rel]
                if known is None:
                    problems.append(
                        Problem(rel, line, raw, "fragment on a file that has no headings")
                    )
                elif fragment not in known:
                    problems.append(
                        Problem(rel, line, raw, f"no heading in {target_rel} gives #{fragment}")
                    )
    return problems, local, external


def selftest() -> int:
    """Prove the check can fail, on each way it is supposed to fail.

    Each case is the whole fixture: a file set and whether the check must
    complain about it. A case that is meant to pass carries the file it links
    to, so passing means resolving something real rather than resolving nothing.

    The fixtures come in two shapes, because there are two ways to enumerate
    the documents. Plain directories exercise the filesystem fallback; the
    git battery exercises the tracked set, with real (throwaway) git
    repositories, because the index is the thing that decides what is read.
    """
    b = "# B\n\n## A heading here\n"
    cases: list[tuple[str, dict[str, str], bool]] = [
        ("a link to a file that exists", {"docs/a.md": "# A\n\nSee [b](b.md).\n", "docs/b.md": b}, False),
        ("an angle-bracket link to a spaced path", {"docs/a.md": "# A\n\nSee [b](<guide (advanced).md>).\n", "docs/guide (advanced).md": b}, False),
        ("an angle-bracket link to a missing spaced path", {"docs/a.md": "# A\n\nSee [b](<missing (advanced).md>).\n"}, True),
        ("a link to a file that does not", {"docs/a.md": "# A\n\nSee [b](missing.md).\n"}, True),
        ("a fragment that exists", {"docs/a.md": "# A\n\nSee [b](b.md#a-heading-here).\n", "docs/b.md": b}, False),
        ("a fragment that does not", {"docs/a.md": "# A\n\nSee [b](b.md#nope).\n", "docs/b.md": b}, True),
        ("a fragment in the same file", {"docs/a.md": "# A\n\n## Ünïcode heading\n\nSee [u](#ünïcode-heading).\n"}, False),
        (
            "a fragment carried by an explicit anchor",
            {"docs/a.md": "# A\n\nSee [s](b.md#supply-chain).\n",
             "docs/b.md": '# B\n\n- <a id="supply-chain"></a>**Supply chain.**\n'},
            False,
        ),
        ("an image that does not exist", {"docs/a.md": "# A\n\n![img](../missing.png)\n"}, True),
        ("a reference-style definition", {"docs/a.md": "# A\n\n[def]: missing.md\n"}, True),
        ("a link out of the repository", {"docs/a.md": "# A\n\nSee [out](../outside.md).\n"}, True),
        ("an external link", {"docs/a.md": "# A\n\nSee [us](https://example.com/x).\n"}, False),
        ("a link inside a fenced block", {"docs/a.md": "# A\n\n```sh\nsee [b](missing.md)\n```\n"}, False),
    ]
    failures = 0
    for label, files, want_fail in cases:
        with tempfile.TemporaryDirectory() as tmp:
            for name, body in files.items():
                full = os.path.join(tmp, name)
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, "w", encoding="utf-8") as handle:
                    handle.write(body)
            problems, local, _ = check(tmp)
            got_fail = bool(problems)
            case_ok = got_fail == want_fail
            if label == "an angle-bracket link to a spaced path":
                source = os.path.join(tmp, "docs/a.md")
                with open(source, encoding="utf-8") as handle:
                    parsed = targets_in(handle.read())
                case_ok = case_ok and parsed == [(3, "guide (advanced).md")]
                case_ok = case_ok and local == 1
                case_ok = case_ok and not problems
            elif label == "an angle-bracket link to a missing spaced path":
                source = os.path.join(tmp, "docs/a.md")
                with open(source, encoding="utf-8") as handle:
                    parsed = targets_in(handle.read())
                case_ok = case_ok and parsed == [(3, "missing (advanced).md")]
                case_ok = case_ok and local == 1
                case_ok = case_ok and len(problems) == 1
                case_ok = case_ok and problems[0].target == "missing (advanced).md"
                case_ok = case_ok and problems[0].why == "target does not exist"
            if not case_ok:
                failures += 1
            verdict = "ok" if case_ok else "WRONG"
            print(f"[doc-links] selftest {verdict}: {label} (want_fail={want_fail}, got={got_fail})")
    # The tracked-set half. The draft that sits beside the index carries two
    # dead links, so a walker that read what Git does not track would flip the
    # passing case red — that discrimination is the property under test, not a
    # detail of the fixture. A passing case must also have read and resolved
    # the one tracked link, or its green would prove nothing.
    git_cases: list[tuple[str, dict[str, str], list[str], bool]] = [
        (
            "a tracked link resolves and an untracked draft beside it is not read",
            {
                "docs/a.md": "# A\n\nSee [b](b.md).\n",
                "docs/b.md": b,
                "drafts/notes.md": "# Drafts\n\nSee [x](missing.md) and [y](b.md#nope).\n",
            },
            ["docs/a.md", "docs/b.md"],
            False,
        ),
        (
            "a tracked broken link still fails",
            {"docs/a.md": "# A\n\nSee [b](missing.md).\n"},
            ["docs/a.md"],
            True,
        ),
    ]
    for label, files, tracked, want_fail in git_cases:
        with tempfile.TemporaryDirectory() as tmp:
            for name, body in files.items():
                full = os.path.join(tmp, name)
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, "w", encoding="utf-8") as handle:
                    handle.write(body)
            init = subprocess.run(
                ["git", "-C", tmp, "init", "-q", "-b", "main"], capture_output=True
            )
            if init.returncode != 0:
                print("[doc-links] selftest cannot run: git init failed")
                return 1
            add = subprocess.run(
                ["git", "-C", tmp, "add", "--"] + tracked, capture_output=True
            )
            if add.returncode != 0:
                print("[doc-links] selftest cannot run: git add failed")
                return 1
            problems, local, _ = check(tmp)
            got_fail = bool(problems)
            case_ok = got_fail == want_fail and (want_fail or (local == 1 and not problems))
            if not case_ok:
                failures += 1
            verdict = "ok" if case_ok else "WRONG"
            print(f"[doc-links] selftest {verdict}: {label} (want_fail={want_fail}, got={got_fail}, local={local})")
    print(f"[doc-links] SELFTEST: {'PASS' if failures == 0 else 'FAIL'} ({failures} wrong)")
    return 0 if failures == 0 else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Check that every link between repository Markdown resolves."
    )
    parser.add_argument("--list", action="store_true", help="print every local link found")
    parser.add_argument("--selftest", action="store_true", help="prove the check can fail")
    parser.add_argument("--root", default=REPO, help="repository root (default: this checkout)")
    args = parser.parse_args(argv)

    if args.selftest:
        return selftest()

    root = os.path.abspath(args.root)
    if not os.path.isdir(root):
        print(f"[doc-links] root is not a directory: {root}", file=sys.stderr)
        return 2

    if args.list:
        for rel in markdown_files(root):
            with open(os.path.join(root, rel), encoding="utf-8") as handle:
                for line, target in targets_in(handle.read()):
                    print(f"{rel}:{line}: {target}")
        return 0

    problems, local, external = check(root)
    for problem in problems:
        print(str(problem), file=sys.stderr)
    print(
        f"[doc-links] {local} local link(s) checked, {external} external link(s) counted, "
        f"{len(problems)} problem(s)"
    )
    if problems:
        print("[doc-links] FAIL: a link points at something that is not there")
        return 1
    print("[doc-links] OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
