#!/usr/bin/env python3
"""check-private-paths.py — no tracked file names a private working directory.

The public repository is read by people who were never given the private
checkout that sits beside it, the working notes kept next to the code, or the
scratch tree the owner runs from. A path that names one of those is not a
remark about the code: it tells the reader to look somewhere they cannot go,
and an absolute one also publishes the shape of a machine's own directories.

That is not hypothetical here. Files written *after* the public/private split
was adopted still pointed at private material, and one of them carried an
absolute scratch path. Nothing caught it, because no check read a file for the
paths it names: `check-doc-links.py` resolves links between our own Markdown,
and the citation lock only asks whether the lines a citation already points at
still hold.

What is checked: every file git tracks. A path is reported once per line that
carries it, as `file:line`, so the fix is a place to go rather than a pattern
to hunt.

What is deliberately NOT checked:

  * Untracked files. The private checkout and the scratch tree live inside this
    working directory and are supposed to name themselves; only what would be
    published is a public-surface question. Reading the tree instead of the
    index would therefore report the private material itself, every run.
  * Prose that merely resembles a path. This is not a general link checker; the
    three families below are the ones this project actually uses, and a fourth
    would be a new rule rather than a new pattern.
  * Anything that only looks like an outside project's own directory layout. A
    citation of another implementation's file (`its-project/src/api.ts:436`) is
    the evidence being quoted, and stays legible without the directory this
    project happened to clone it into.

This file is exempt from its own scan, and has to be: it names the three
families in order to forbid them, so it is the one tracked file that cannot
pass. Renaming it therefore turns the gate red on its own source, which is the
loud direction — the alternative, a pattern built out of fragments so the
script does not contain it, would hide the rule from the person editing it.

Usage:
    python3 scripts/check-private-paths.py             # check this checkout
    python3 scripts/check-private-paths.py --root DIR  # check another tree
    python3 scripts/check-private-paths.py --selftest  # prove the check can fail

Exit codes: 0 = clean · 1 = at least one private path is named ·
2 = usage error (this is not a git work tree, so the tracked set — and with it
the answer — is unknown, and unknown is not clean).
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# The one tracked file that names these families on purpose. See the docstring.
SELF = "scripts/check-private-paths.py"

# Each family with the name this repository uses for it. Both patterns carry a
# boundary, because both are spelled the same way as ordinary code: `private`
# is a field name (`delete mainManifest.private;`) and `nm` ends words
# (`album/`, `column/`). The boundary is what separates the directory from the
# word — a name preceded by a character that can be part of an identifier is
# not a directory being named.
FAMILIES: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("the private checkout", re.compile(r"(?<![A-Za-z0-9_$])\.private\b")),
    ("the private scratch tree", re.compile(r"scratch/DimLifeS")),
    ("the private working notes", re.compile(r"(?:^|[^A-Za-z0-9_-])nm/")),
)

# Files are read as text. A tracked binary is not a document and cannot cite a
# path; decoding it with `replace` keeps one from aborting the whole run.
DECODE = "utf-8"


class Finding:
    __slots__ = ("path", "line", "family", "text")

    def __init__(self, path: str, line: int, family: str, text: str) -> None:
        self.path, self.line, self.family, self.text = path, line, family, text

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: names {self.family}: {self.text.strip()}"


def tracked_files(root: str) -> list[str] | None:
    """The tracked set, or None when it cannot be read.

    `git ls-files` is the definition of "tracked" and nothing else computes it
    the same way — a hand-maintained skip list would drift from .gitignore and
    from what a checkout actually contains. It is read from the index, so an
    untracked file in the working tree neither passes nor fails anything.
    """
    proc = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"],
        capture_output=True,
    )
    if proc.returncode != 0:
        return None
    return [name for name in proc.stdout.decode(DECODE, "replace").split("\0") if name]


def check(root: str) -> tuple[list[Finding], int] | None:
    """Every name of a private directory, and how many files were read.

    None means the tracked set could not be read, which is not the same answer
    as "nothing found" and is never reported as one.
    """
    names = tracked_files(root)
    if names is None:
        return None
    findings: list[Finding] = []
    read = 0
    for rel in names:
        if rel == SELF:
            continue
        full = os.path.join(root, rel)
        if not os.path.isfile(full):
            continue  # a tracked symlink whose target is gone: nothing to read
        try:
            with open(full, "rb") as handle:
                body = handle.read()
        except OSError:
            # Unreadable is not clean either, but it is a fact about this
            # machine rather than about the file's contents: the count below
            # says which files were read, and the reader can see the gap.
            continue
        read += 1
        for number, text in enumerate(body.decode(DECODE, "replace").splitlines(), 1):
            for family, pattern in FAMILIES:
                for _ in pattern.finditer(text):
                    findings.append(Finding(rel, number, family, text))
                    break
    return findings, read


def selftest() -> int:
    """Prove the check can fail, on each way it is supposed to fail.

    Every case is a whole fixture: files, and whether the check must complain.
    A case meant to pass carries the text it is supposed to tolerate, so
    passing means the pattern stayed narrow rather than that nothing was read.
    The fixture files are real files in a real (throwaway) git repository,
    because the tracked set is the thing being read.
    """
    cases: list[tuple[str, dict[str, str], bool]] = [
        ("the private checkout by path", {"src/a.rs": "// see .private/docs/14-ADR.md\n"}, True),
        ("the private checkout bare", {"src/a.rs": "// the .private tree\n"}, True),
        ("the private scratch tree", {"src/a.rs": "// ~/scratch/DimLifeS/chat-stasher/x.mjs\n"}, True),
        ("an absolute scratch path", {"a/b.ts": "// /Users/x/scratch/DimLifeS/y/z.mjs\n"}, True),
        ("the private notes after a slash", {"a/b.ts": "// path: proj/nm/W1-OUT.md\n"}, True),
        ("the private notes in a quote", {"a/b.ts": 'x = "nm/w5-competitors/repos/p"\n'}, True),
        ("the private notes at the start of a line", {"a/b.ts": "nm/W2-OUT.md is the record\n"}, True),
        ("a word ending in those letters", {"a/b.ts": "// column/ and album/ are not paths\n"}, False),
        ("a field named private", {"scripts/x.mjs": "delete mainManifest.private;\n"}, False),
        ("another project's own file", {"a/b.ts": "// its-project/src/api.ts:436\n"}, False),
        ("a directory that merely starts with them", {"a/b.ts": "// nmodel/x\n"}, False),
        ("an ordinary relative path", {"a/b.ts": "// lib/backfill/enumerate.ts:77\n"}, False),
        ("a gemini ledger fixture name", {"a/b.ts": "// (w92-ledger-read.json)\n"}, False),
    ]
    failures = 0
    for label, files, want_fail in cases:
        with tempfile.TemporaryDirectory(prefix="private-paths-selftest-") as tmp:
            init = subprocess.run(
                ["git", "-C", tmp, "init", "-q", "-b", "main"], capture_output=True
            )
            if init.returncode != 0:
                print(f"[private-paths] selftest cannot run: git init failed")
                return 1
            for name, body in files.items():
                full = os.path.join(tmp, name)
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, "w", encoding=DECODE) as handle:
                    handle.write(body)
            add = subprocess.run(["git", "-C", tmp, "add", "-A"], capture_output=True)
            if add.returncode != 0:
                print("[private-paths] selftest cannot run: git add failed")
                return 1
            result = check(tmp)
            if result is None:
                print("[private-paths] selftest cannot run: no tracked set")
                return 1
            problems, read = result
            got_fail = bool(problems)
            # A case that passes must have passed by reading the file, not by
            # finding nothing to read.
            wrong = got_fail != want_fail or read == 0
            failures += 1 if wrong else 0
            verdict = "ok" if not wrong else "WRONG"
            print(
                f"[private-paths] selftest {verdict}: {label} "
                f"(want_fail={want_fail}, got={got_fail}, files_read={read})"
            )
    print(f"[private-paths] SELFTEST: {'PASS' if failures == 0 else 'FAIL'} ({failures} wrong)")
    return 0 if failures == 0 else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Check that no tracked file names a private working directory."
    )
    parser.add_argument("--selftest", action="store_true", help="prove the check can fail")
    parser.add_argument("--root", default=REPO, help="repository root (default: this checkout)")
    args = parser.parse_args(argv)

    if args.selftest:
        return selftest()

    root = os.path.abspath(args.root)
    if not os.path.isdir(root):
        print(f"[private-paths] root is not a directory: {root}", file=sys.stderr)
        return 2

    result = check(root)
    if result is None:
        print(
            f"[private-paths] {root} is not a git work tree, so which files are tracked "
            f"— and therefore whether any of them names a private directory — is unknown. "
            f"Refusing to report a tree it could not read.",
            file=sys.stderr,
        )
        return 2

    problems, read = result
    for problem in problems:
        print(str(problem), file=sys.stderr)
    print(
        f"[private-paths] {read} tracked file(s) read, {len(problems)} private reference(s)"
    )
    if problems:
        print(
            "[private-paths] FAIL: a tracked file names a private directory. Name the "
            "document, not the path to it."
        )
        return 1
    print("[private-paths] OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
