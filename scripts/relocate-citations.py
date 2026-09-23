#!/usr/bin/env python3
"""relocate-citations.py — move each `path:line` citation to where its text went.

Every merge of a parallel branch edits code, which moves lines, which leaves the
citations in README.md and docs/ pointing at text that is no longer there. The
relocation is mechanical — take the text a citation named at the commit whose
numbers the document still carries, find that same text in the merged working
tree, rewrite the range — and doing it by hand is what has cost this repository
several extra rounds (W46b, W59d, W65m, W64c). This script does that one step.

🔴 It does **exact shifts only**. A citation is relocated when, and only when,
the block of lines it named at the parent is still in the merged file as the
same run of lines, unchanged, in exactly one place. Anything else — a line
inserted inside the block, a line deleted from it, a reworded line, a block
that now occurs twice, a block that is gone — is refused, reported as needing a
human, and leaves every document byte-identical.

That is a deliberate narrowing. The earlier version also tried to follow a
block that grew: it walked the file for the cited lines in order and took the
earliest alignment each step allowed, with checks meant to prove "earliest" was
the only answer. Three review rounds each built an input where it was not — a
`);` belonging to an inner call, a `}` written inside a `// }` comment, a
two-line block with no interior line to check — and each time the tool wrote a
range nobody had cited and exited 0. On the merge it was built for, the answer
it needed was exact shifts and nothing else (3 of 3; no citation needed lines
inserted inside it). The heuristic and its brace arithmetic are gone, not
disabled.

    python3 scripts/relocate-citations.py --old <parentA> --old <parentB> --dry-run
    python3 scripts/relocate-citations.py --old <parentA> --old <parentB>
    python3 scripts/check-citation-drift.py        # then read every failure
    python3 scripts/check-citation-drift.py --update   # only once they are clean

🔴 The repository it rewrites is the one the **working directory** is in
(`git rev-parse --show-toplevel`), never the one this file happens to sit in. A
copy left in another worktree used to read and write *that* worktree's
documents, and to run `git` there: the invocation below would have edited the
wrong tree and printed an ordinary-looking plan while doing it. Run outside a
git repository at all and it refuses rather than guessing which tree was meant.

Pass every parent of the merge, in one run, after resolving the prose conflicts
by hand. A resolved document keeps citations from both sides, and which side a
given citation came from is not a guess this script makes: **a citation is read
in the numbers of the side whose own document writes that same range.** The
document at the parent commit is the authority on that parent's coordinates, so
`--old B` cannot touch a citation that only A's document ever wrote down. A
citation no declared side's document writes is reported, not relocated — after a
run, that is what the already-relocated citations look like, which is why the
second run is a refusal and not a second relocation.

What counts as "the text a citation named" is the repository's own definition,
the one docs/citations.lock already pins: the range's lines with surrounding
whitespace removed, joined by newlines. Pure reindentation is therefore not a
move; a changed word is.

🔴 A relocation is applied only when the old text is found as one unchanged run
of lines, in exactly one place, in every side that claims it. Zero matches,
several matches, and sides that disagree are all left alone and reported, and
the run exits non-zero: an edit made under any of those is a guess about which
of several candidate claims a sentence was written about.

🔴 A token must also name the same *file* on both sides. Which file `a.ts`
refers to is a fact about a tree, and a merge can change it — delete the root
`a.ts` and leave `pkg/a.ts`, and a sentence about the root file reads as a
citation of the other one. Each side's document is therefore parsed in that
side's own tree (`git ls-tree` at the commit), and a citation whose token
resolved to a different file there than in the merged tree is refused by name,
even when the merged file offers a perfectly clean range for it.

**Which documents this rewrites** is not a list kept here: it is exactly
`drift.doc_files()`, the scan set of scripts/check-citation-drift.py — README.md,
SECURITY.md, CONTRIBUTING.md, docs/install.md, docs/privacy.md,
docs/threat-model.md and every contracts/*.md. SECURITY.md, CONTRIBUTING.md and
contracts/*.md are in that set deliberately: a citation left stale in one of
them is the same stale anchor as one left stale in README.md, and they are the
documents the drift check will fail on next. Source files, and
docs/citations.lock itself, are never written.

Each document is replaced in one step — a sibling temporary file and
`os.replace` — and a document's own line endings and final-newline state are
copied through untouched, so a CRLF file stays CRLF and a file without a final
newline does not gain one. If any document cannot be written, every document
already written in that run is put back; a document that could not be put back
is named as a document left rewritten, not folded into the claim that nothing
changed, and the run exits non-zero either way.

A comma list is rewritten all at once or not at all: `path:12,15` whose span 15
cannot be placed keeps the text it had, because a token half in the old numbers
and half in the new ones reads as a citation of a range nobody wrote.

Usage:
    python3 scripts/relocate-citations.py --old <commit> [--old <commit> ...]
                                          [--dry-run] [--quiet]

Exit codes: 0 = every citation is now right · 1 = at least one needs a human ·
            2 = usage error, or not run from inside a git repository.
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import re
import stat
import subprocess
import sys
import tempfile

# The repository this run rewrites, resolved from the working directory by
# resolve_repo() before anything is read. Deliberately not derived from
# __file__: see the note above.
REPO = ""

# The target repository's citation parser, loaded by load_drift_module() once
# REPO is known. The parser and the tree it parses travel together.
drift = None  # type: ignore[assignment]


def resolve_repo() -> str:
    """The repository the working directory is in, or refuse.

    🔴 `os.path.dirname(__file__)` answers "where does this copy of the script
    live", which is not the question. Running another worktree's copy of this
    file from the worktree being merged read and wrote the *other* worktree's
    documents and ran every `git` command with the other worktree as its cwd,
    so the whole run was about the wrong tree while looking entirely normal.
    """
    proc = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        cwd=os.getcwd(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        encoding="utf-8",
        errors="replace",
    )
    root = proc.stdout.strip()
    if proc.returncode != 0 or not root:
        detail = proc.stderr.strip() or "git said nothing"
        die(f"the working directory is not inside a git repository ({detail}); "
            f"run this from the worktree whose documents it should rewrite", 2)
    return root


def load_drift_module():
    """Import the target repository's scripts/check-citation-drift.py.

    The citation parser lives there and is the authority on what a citation is;
    a second copy of that regex here is a second answer to "what does the doc
    cite", and the two would drift. The selftest of the drift checker imports it
    the same way (probe 5).

    It is loaded from the repository being rewritten, not from beside this file:
    the parser's own doc set, lockfile and file lookups are all relative to that
    repository, and taking them from wherever this copy of the tool happens to
    sit is the same wrong-tree mistake resolve_repo() exists to prevent.
    """
    path = os.path.join(REPO, "scripts", "check-citation-drift.py")
    if not os.path.exists(path):
        die(f"{path} does not exist: the repository being rewritten has no citation "
            f"parser for this tool to use", 2)
    spec = importlib.util.spec_from_file_location("citation_drift", path)
    if spec is None or spec.loader is None:  # pragma: no cover - unreachable in a checkout
        die(f"cannot load {path}", 2)
    module = importlib.util.module_from_spec(spec)
    sys.modules["citation_drift"] = module
    spec.loader.exec_module(module)
    module.REPO = REPO
    return module


def die(msg: str, code: int = 2) -> None:
    print(f"[citation-relocate] {msg}", file=sys.stderr)
    sys.exit(code)


# --------------------------------------------------------------------------
# Outcomes of looking for one citation's old text in the merged file.
# --------------------------------------------------------------------------
RIGHT = "right"            # the cited lines are still the cited lines: nothing to do
SHIFTED = "shifted"        # the whole block moved, unchanged: relocate
AMBIGUOUS = "ambiguous"    # the block's text is in several places
MISSING = "missing"        # the block's text is not there as one run of lines
UNKNOWN = "unknown"        # the file, or the range, is not readable at --old
UNCLAIMED = "unclaimed"    # no declared side's own document writes this range
BLANK = "blank"            # every cited line is whitespace, so it names nothing
ELSEWHERE = "elsewhere"    # the same token names a different file on each side

# Outcomes that rewrite a range. 🔴 Exactly one, and it has to be: the cited
# block's text must still sit in the merged file as the same run of lines, in
# exactly one place. That is the only placement the file forces.
#
# A block that grew, shrank, was reworded, now occurs twice, or is gone has
# either more than one placement a reader could defend or none at all, and this
# tool does not choose between them. It used to. The grown heuristic walked the
# file for the block's lines in order and took the earliest alignment each step
# allowed, guarded by checks that tried to prove "earliest" was the only answer —
# and three review rounds each built an input where the walk was not the only
# answer, landed on a range nobody had written, and exited 0: a `);` that closes
# an inner call rather than the cited one; a `}` written inside a `// }` comment
# cancelling the real closing brace; a two-line block with no interior line for
# the checks to look at. The heuristic and its brace arithmetic are deleted, not
# disabled: there is no config that turns them back on.
RELOCATED = (SHIFTED,)

# The refusing outcomes, kept apart because they mean different things to
# whoever has to fix them: several matches is "this sentence could be about any
# of these", zero matches is "the block is not in the merged file unchanged",
# blank is "there is no text here to look for", unclaimed is "these numbers are
# in nobody's coordinate system", and elsewhere is "this token names a different
# file here than it did at --old". All five need a human, and none of them is
# another.
REFUSALS = (AMBIGUOUS, MISSING, UNKNOWN, UNCLAIMED, BLANK, ELSEWHERE)


class SourceIndex:
    """One file's lines, normalised once, plus a joined copy for fast window search.

    Windows are found with str.find over a joined copy rather than a Python loop
    over lines: the largest cited file is over 4000 lines and the block search
    runs once per citation. The separator is a string that does not occur in any
    line, so a match can only start and end on a line boundary — searching the
    joined text for a bare block would also match a block that starts in the
    middle of a line.
    """

    def __init__(self, lines: list[str]):
        self.norm = [ln.strip() for ln in lines]
        sep = "\x00"
        while any(sep in ln for ln in self.norm):
            sep += "\x00"
        self.sep = sep
        # Padding separators at both ends so that a match on the first or the
        # last line of the file has the separator its needle needs.
        self.joined = sep + sep.join(self.norm) + sep

    def __len__(self) -> int:
        return len(self.norm)

    def block(self, start: int, end: int) -> str:
        """The normalised text of lines start..end, 1-based and inclusive."""
        return self.sep.join(self.norm[start - 1 : end])

    def windows(self, block: str) -> list[int]:
        """0-based line indices where `block` sits as a run of whole lines."""
        needle = self.sep + block + self.sep
        found: list[int] = []
        pos = 0
        while True:
            at = self.joined.find(needle, pos)
            if at < 0:
                return found
            found.append(self.joined.count(self.sep, 0, at))
            pos = at + 1


def git(*args: str) -> tuple[int, str]:
    proc = subprocess.run(
        ["git", *args],
        cwd=REPO,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        encoding="utf-8",
        errors="replace",
    )
    return proc.returncode, proc.stdout


_old_cache: dict[tuple[str, str], SourceIndex | None] = {}


def source_at(commit: str, rel: str) -> SourceIndex | None:
    """A file's lines at `commit`, or None when that commit does not carry it."""
    key = (commit, rel)
    if key not in _old_cache:
        rc, out = git("show", f"{commit}:{rel}")
        _old_cache[key] = SourceIndex(out.splitlines()) if rc == 0 else None
    return _old_cache[key]


_cur_cache: dict[str, SourceIndex] = {}


def working_tree_index(rel: str) -> SourceIndex:
    if rel not in _cur_cache:
        _cur_cache[rel] = SourceIndex(drift.read_lines(rel))
    return _cur_cache[rel]


class Parent:
    """One side of the merge: a commit, and the documents as that side wrote them.

    🔴 The documents are parsed **in this side's own tree**, never against the
    merged one. Which file a token names is a fact about a tree: with a root
    `a.ts` here and only `pkg/a.ts` after the merge, `a.ts:1-2` names one file
    then and a different file now. Reading this side's document with the merged
    tree's file list answers the merged tree's question and files the answer
    under this side's commit, which is how a citation of `pkg/a.ts` came to be
    relocated on the strength of a sentence about `a.ts` — and how the run
    exited 0 while writing it. So the basename index and the "is this a file"
    test both come from `git ls-tree` at this commit.
    """

    def __init__(self, commit: str):
        self.commit = commit
        # Every citation this side's own documents write, already resolved to
        # the file it names. A parsed citation, not a raw range scan: the
        # question "does this side's document claim this range of this file" is
        # the same question the merged tree's citations are read with, and a
        # second, rougher reader here answered it differently — see claims().
        self.spans: dict[str, set[tuple[str, int, int]]] = {}
        self.lines: dict[str, set[str]] = {}
        # (doc, doc line) -> the tokens written there, as (raw, target, start,
        # end). Kept so that a citation whose token resolves to a different file
        # here than in the merged tree can be *told apart* from one this side
        # never wrote: the first is a hazard to name, the second is ordinary
        # unclaimed numbers. See decide().
        self.resolved: dict[tuple[str, int], list[tuple[str, str, int, int]]] = {}

        files = self._tree_files()
        self._files = set(files)
        basenames: dict[str, list[str]] = {}
        for rel in files:
            basenames.setdefault(os.path.basename(rel), []).append(rel)

        for doc in drift.doc_files():
            rc, out = git("show", f"{commit}:{doc}")
            if rc != 0:
                continue
            citations, _ = drift.parse_text(doc, out.splitlines(), basenames, self._exists)
            self.spans[doc] = {(c.target, c.start, c.end) for c in citations}
            self.lines[doc] = set(out.splitlines())
            for c in citations:
                self.resolved.setdefault((doc, c.doc_line), []).append(
                    (c.raw, c.target, c.start, c.end)
                )

    def _tree_files(self) -> list[str]:
        """This commit's tracked files, repo-relative.

        `ls-tree` rather than a walk of some directory: it is the tree, it needs
        no checkout, and it does not pick up the build output and untracked
        scratch a working-tree walk would.
        """
        rc, out = git("ls-tree", "-r", "--name-only", self.commit)
        if rc != 0:
            die(f"cannot list the files of --old {self.commit[:12]}")
        return [line for line in out.splitlines() if line]

    def _exists(self, rel: str) -> bool:
        return rel in self._files

    @property
    def short(self) -> str:
        return self.commit[:12]

    def claims(self, doc: str, target: str, start: int, end: int) -> bool:
        """Whether this side's own document writes a citation of `target` here.

        The line numbers alone would answer a different question. `alpha.ts:3-4`
        is not a claim about `beta.ts:3-4` even though it carries the same
        numbers, and treating it as one lets a side vote on a citation it never
        made — including voting to relocate it.

        The side's document is read by the same parser that reads the merged
        tree, so a claim is the *file the citation resolves to*, never the shape
        of the token that was written:

          * a bare `:3-4` continuation belongs to the file its own sentence
            named. Reading it as a token of "no path" made it a claim about
            every file with those line numbers, and a document that only ever
            wrote `alpha.ts:1-2`, `:3-4` then voted to move a `cross.ts:3-4` it
            had never heard of;
          * a basename belongs to the one file it resolves to. Matching on
            `os.path.basename(target)` made `a.ts:1-2` a claim about every
            `a.ts` in the repository, so a document about `pkg/one/a.ts` moved
            the citation of `pkg/two/a.ts`. A name shared by several files does
            not resolve at all, and a name that does not resolve claims nothing
            — the safe direction, because a citation with no owner is refused
            rather than relocated.
        """
        return (target, start, end) in self.spans.get(doc, ())

    def wrote_this_line(self, doc: str, line: str) -> bool:
        """Whether this side's version of the document carries this exact line.

        Two sides can both write `foo.ts:40`, meaning different code by it. The
        sentence the citation sits in is what breaks that tie: a conflict
        resolution keeps one side's prose, so the side whose document already
        has this line verbatim is the side that wrote the number.
        """
        return line in self.lines.get(doc, ())


class Decision:
    __slots__ = ("status", "old_start", "old_end", "new_start", "new_end", "detail", "via")

    def __init__(self, status, old_start, old_end, new_start, new_end, detail="", via=""):
        self.status = status
        self.old_start = old_start
        self.old_end = old_end
        self.new_start = new_start
        self.new_end = new_end
        self.detail = detail
        self.via = via


def is_right(old: SourceIndex, cur: SourceIndex, start: int, end: int) -> bool:
    return (
        end <= len(old)
        and end <= len(cur)
        and start >= 1
        and cur.block(start, end) == old.block(start, end)
    )


def locate(old: SourceIndex, cur: SourceIndex, start: int, end: int) -> Decision:
    """Where the text cited as lines start..end in this side's numbers lives now.

    A relocation requires the merged file to carry the cited block as the same
    run of lines, in exactly one place. "The same run of lines" is the
    repository's own definition of what a citation pins, the one
    docs/citations.lock already uses: the range's lines with surrounding
    whitespace removed, joined by newlines. Pure reindentation is therefore not
    a move; a changed word is, and so is an inserted or deleted line, because
    both change the run.

    Everything that is not that one case is a refusal, and the run exits
    non-zero: a block with a line inserted inside it, a block that lost a line,
    a reworded block, a block that now occurs twice, and a block that is gone
    all mean the merged file does not force a placement. They are reported
    apart, because the repairs differ.
    """
    if end > len(old):
        return Decision(
            UNKNOWN, start, end, None, None,
            f"lines {start}-{end} do not exist at --old ({len(old)} lines there)",
        )
    block = old.block(start, end)
    if end <= len(cur) and cur.block(start, end) == block:
        return Decision(RIGHT, start, end, start, end)
    if not "".join(old.norm[start - 1 : end]).strip():
        # Every line this range names is blank. Lines are compared with their
        # surrounding whitespace removed, so a blank block is the empty string,
        # and the empty string is "found" at every blank line in the file. The
        # citation identifies nothing to search for, so any blank line would be
        # taken as its answer — a real move in the document to a line that was
        # never cited. There is nothing here to relocate.
        return Decision(
            BLANK, start, end, None, None,
            f"every line in {start}-{end} is blank at --old, so the range names no "
            f"text, and any blank line in the merged file would match it",
        )
    hits = cur.windows(block)
    if len(hits) == 1:
        new_start = hits[0] + 1
        return Decision(SHIFTED, start, end, new_start, new_start + (end - start))
    if len(hits) > 1:
        return Decision(
            AMBIGUOUS, start, end, None, None,
            f"that text sits in {len(hits)} places in the merged file "
            f"(lines {', '.join(str(h + 1) for h in hits[:6])}"
            + (", …" if len(hits) > 6 else "") + ")",
        )
    return Decision(
        MISSING, start, end, None, None,
        "that text is not in the merged file at all as one run of lines — it was "
        "rewritten, removed, or had lines inserted inside it, and which of the "
        "three it was decides the repair. Needs a human",
    )




def decide(
    parents: list[Parent],
    cur: SourceIndex,
    doc: str,
    line: str,
    cit,
) -> Decision:
    """Decide one citation, using only the sides whose own document writes it."""
    target, start, end = cit.target, cit.start, cit.end

    # 🔴 Before anything else: the token has to name the same file on both
    # sides. `a.ts:1-2` is a claim about whatever file the tree it was written
    # in called `a.ts`, and the two trees need not agree — delete the root
    # `a.ts` in the merge and leave `pkg/a.ts`, and the sentence that was about
    # the root file now reads as a citation of a file it never named. Every
    # number in it means something different, so no range is forced, however
    # cleanly the merged file offers one. Checked here rather than left to the
    # ownership test below, because an unclaimed range can still come back
    # "already right" against the wrong file and be reported as a clean run.
    for p in parents:
        for p_raw, p_target, p_start, p_end in p.resolved.get((doc, cit.doc_line), ()):
            if (
                p_target != target
                and p_raw == cit.raw
                and (p_start, p_end) == (start, end)
            ):
                return Decision(
                    ELSEWHERE, start, end, None, None,
                    f"the token `{cit.raw}` on this line names `{target}` in the merged "
                    f"tree but named `{p_target}` at --old {p.short}: the same citation "
                    f"resolves to a different file on each side, so its numbers mean "
                    f"different things and no range is forced. Needs a human",
                )

    owners = [p for p in parents if p.claims(doc, target, start, end)]
    if len(owners) > 1:
        # Both sides wrote this range, which means different code by it. The
        # surviving prose is the tie-break; if neither side's document has this
        # line (the resolution reworded it), both stay and the disagreement
        # below decides.
        by_line = [p for p in owners if p.wrote_this_line(doc, line)]
        if len(by_line) == 1:
            owners = by_line
    if not owners:
        # Not in any declared side's coordinates. It may still be right already —
        # a hand-written citation whose code did not move — but it may equally be
        # a citation this script relocated on an earlier run, and relocating it
        # again from a side that never wrote it would move a correct anchor onto
        # an unrelated range.
        for p in parents:
            old = source_at(p.commit, target)
            if old is not None and is_right(old, cur, start, end):
                return Decision(RIGHT, start, end, start, end)
        return Decision(
            UNCLAIMED, start, end, None, None,
            "no --old document writes this range, and it is not already right: these "
            "numbers are in no declared side's coordinate system",
        )

    candidates: list[tuple[str, Decision]] = []
    rights: list[str] = []
    reasons: list[str] = []
    for p in owners:
        old = source_at(p.commit, target)
        if old is None:
            reasons.append(f"{p.short}: {target} does not exist at --old")
            continue
        d = locate(old, cur, start, end)
        if d.status == RIGHT:
            rights.append(p.short)
        elif d.status in RELOCATED:
            candidates.append((p.commit, d))
        else:
            reasons.append(f"{p.short}: {d.detail}")

    # 🔴 A side that cannot place the range is a veto, not a footnote. It used to
    # be collected and then dropped whenever some other side produced a move, so
    # one side's "the text this sentence names is gone" was overruled by another
    # side's "I found my text over there" — two different sentences' worth of
    # evidence, and the run reported a relocation and exited 0. Sides that do not
    # all agree leave the citation alone and say so.
    if reasons and (candidates or rights):
        return Decision(
            AMBIGUOUS, start, end, None, None,
            "the sides that write this range do not agree about it: "
            + "; ".join(
                reasons
                + [f"{c[:12]}: its text is now at {d.new_start}-{d.new_end}"
                   for c, d in candidates]
                + [f"{s}: it is already right" for s in rights]
            ),
        )
    if rights and candidates:
        return Decision(
            AMBIGUOUS, start, end, None, None,
            "the sides that write this range disagree about where it went: "
            + ", ".join(f"{s} says it is already right" for s in rights)
            + ", "
            + ", ".join(f"{c[:12]} says {d.new_start}-{d.new_end}" for c, d in candidates),
        )
    if rights:
        return Decision(RIGHT, start, end, start, end)
    if not candidates:
        return Decision(MISSING, start, end, None, None, "; ".join(reasons))
    moves = {(d.new_start, d.new_end) for _, d in candidates}
    if len(moves) > 1:
        return Decision(
            AMBIGUOUS, start, end, None, None,
            "the sides that write this range disagree about where it went: "
            + ", ".join(f"{c[:12]} -> {d.new_start}-{d.new_end}" for c, d in candidates),
        )
    commit, d = candidates[0]
    return Decision(d.status, start, end, d.new_start, d.new_end, d.detail, via=commit)


def group_chunks(citations: list, index: int) -> tuple[list, int]:
    """The citations that make up ONE written token, starting at `index`.

    A comma list (`contract.ts:319,387,403`) is one token in the document but N
    citations out of the parser, all sharing the same `raw`. They are rewritten
    together or the token comes back mangled, so they are regrouped here — the
    number of chunks is taken from the commas in the token itself, never from
    "how many identical raws happen to follow", which would swallow a second,
    separate token that reads the same.
    """
    raw = citations[index].raw
    span_count = len(raw.rpartition(":")[2].split(","))
    return citations[index : index + span_count], index + span_count


# The characters a citation token is made of. A token matches only where these
# are absent on both sides, so `pkg/src/a.ts:12` is never read as the
# `src/a.ts:12` written inside it — different files whose citations share a
# suffix — and a suffix is not silently rewritten in the longer token's place.
_PATH_CHARS_RE = re.compile(r"[A-Za-z0-9_./-]")


def find_token(line: str, token: str, pos: int) -> int:
    """Where `token` occurs in `line` at or after `pos`, as a whole token.

    The citation parser and this rewriter have to agree about which characters
    are the citation. `str.find` alone does not: a citation of `src/a.ts:12` is
    a substring of `pkg/src/a.ts:12`, so a line that cites both had the shorter
    one rewritten inside the longer one, and the document then cited a line of a
    file the sentence never mentioned. Returns -1 when no whole-token occurrence
    is left.

    The test is the character on each side of the match, and it is deliberately
    the same on both sides even though it makes one shape unreachable: a bare
    `:N` written directly after path characters that do not themselves name a
    file (`HH:23`, inherited from the sentence's real citation) is refused as
    "the parser reports a citation here and I cannot find it", because relaxing
    it for `:` would also let a bare `:N` match the tail of `a.ts:N`. Refusing
    costs a human one look; matching the wrong one costs the document a wrong
    anchor that still validates. Every citation in this repository's own
    documents is found by this rule.
    """
    at = line.find(token, pos)
    while at >= 0:
        before = at > 0 and _PATH_CHARS_RE.match(line[at - 1]) is not None
        end = at + len(token)
        after = end < len(line) and _PATH_CHARS_RE.match(line[end]) is not None
        if not before and not after:
            return at
        at = line.find(token, at + 1)
    return -1


def rewrite_line(line: str, replacements: list[tuple[str, str]]) -> str | None:
    """Apply (old_token, new_token) pairs left to right within one line.

    Each search resumes after the previous match, so two identical tokens on one
    line are rewritten as the two separate occurrences they are. A pair that
    cannot be found means this script and the parser disagree about what the
    document says; that returns None and no edit is made anywhere.
    """
    out: list[str] = []
    pos = 0
    for old_token, new_token in replacements:
        at = find_token(line, old_token, pos)
        if at < 0:
            return None
        out.append(line[pos:at])
        out.append(new_token)
        pos = at + len(old_token)
    out.append(line[pos:])
    return "".join(out)


# A line boundary, as str.splitlines() defines one. Every boundary is copied
# through untouched when a document is rewritten, so the file keeps the newline
# style it had; a document rewritten with "\n".join() came back LF-only with a
# final newline added, whatever it was before.
_LINE_BREAK_RE = re.compile(r"(\r\n|[\n\r\v\f\x1c-\x1e\x85\u2028\u2029])")


def read_doc(doc: str) -> str:
    """A document's text, exactly as it is on disk.

    `newline=""` is the point: the default translates CRLF to LF on the way in,
    and the translation cannot be undone on the way out. Line *content* is the
    same either way — `.splitlines()` strips the terminator — which is why the
    line numbers here still match the ones the citation parser counts with
    (check-citation-drift.py reads the document the same way).
    """
    with open(os.path.join(REPO, doc), "r", encoding="utf-8", newline="") as fh:
        return fh.read()


def replace_lines(text: str, edits: list[tuple[int, str]]) -> str:
    """`text` with each (1-based line number, new content) applied to its line.

    The line boundaries are `str.splitlines()`'s, the ones the citation parser
    counts lines with. Splitting into content and terminator and putting them
    back unchanged means only the named lines' contents can differ: a CRLF
    document stays CRLF, a document with no final newline does not gain one, and
    a document with a mixture keeps the mixture.
    """
    parts = _LINE_BREAK_RE.split(text)
    for lineno, new_line in edits:
        at = 2 * (lineno - 1)
        if at >= len(parts):
            raise ValueError(f"line {lineno} is not in the document")
        parts[at] = new_line
    return "".join(parts)


def write_atomic(doc: str, text: str) -> None:
    """Replace one document with `text`, or leave it exactly as it was.

    The write goes to a temporary file in the same directory and is then renamed
    over the document, so the document is never the half-written one: either the
    old bytes or the new ones are there. Writing in place with `open(..., "w")`
    truncates first and holds the only copy of the original in memory, which is
    how a kill or a full disk in that window leaves a truncated document behind.

    The temporary file is a sibling, not a file in /tmp, because the rename has
    to be within one filesystem to be atomic, and it is given the document's own
    mode before the rename: mkstemp creates the file 0600, and a rewrite that
    also changed a document's permissions would be a change nobody asked for.
    """
    abs_doc = os.path.join(REPO, doc)
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(abs_doc),
                               prefix=".relocate-citations.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="") as fh:
            fh.write(text)
        os.chmod(tmp, stat.S_IMODE(os.stat(abs_doc).st_mode))
        os.replace(tmp, abs_doc)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def plan_doc(
    doc: str, citations: list, parents: list[Parent]
) -> tuple[list[Decision], list[tuple[int, str]], list[str]]:
    """Decisions plus rewritten text for one document.

    Returns (decisions, edits, problems) where `edits` is a list of
    (doc_line, rewritten_line) and `problems` are reasons nothing could be
    written. Nothing is applied if `problems` is non-empty — a rewrite built on
    a document this script cannot fully account for is not a rewrite to trust.
    """
    decisions: list[Decision] = []
    per_line: dict[int, list[tuple[str, str]]] = {}
    problems: list[str] = []

    lines = read_doc(doc).splitlines()

    by_line: dict[int, list] = {}
    for cit in citations:
        by_line.setdefault(cit.doc_line, []).append(cit)

    for lineno, cits in by_line.items():
        replacements: list[tuple[str, str]] = []
        i = 0
        while i < len(cits):
            group, i = group_chunks(cits, i)
            cur = working_tree_index(group[0].target)
            # The chunk's own range as written, by position: the token's comma
            # list is the authority on how many chunks there are, and a chunk
            # that did not move must keep exactly the text it already had.
            written = group[0].raw.rpartition(":")[2].split(",")
            new_spans: list[str] = []
            group_decisions: list[Decision] = []
            moved = False
            stuck: Decision | None = None
            for j, cit in enumerate(group):
                d = decide(parents, cur, doc, lines[lineno - 1], cit)
                group_decisions.append(d)
                if d.status in RELOCATED:
                    moved = True
                    new_spans.append(
                        str(d.new_start) if d.new_start == d.new_end
                        else f"{d.new_start}-{d.new_end}"
                    )
                else:
                    new_spans.append(written[j])
                    if d.status != RIGHT and stuck is None:
                        stuck = d
            if stuck is not None and moved:
                # 🔴 One token, N citations out of the parser, and one of them
                # cannot be placed. Rewriting the token with that span left in
                # the old numbers produced `src/a.ts:17,15` — half the token in
                # the merged file's coordinates and half in the parent's, with
                # nothing in the document saying which half is which, and the
                # summary counting the placed half as a relocation. The token
                # keeps the text it had, and *every* span of it is reported as
                # needing a human: a reader cannot act on half a token either.
                for d in group_decisions:
                    if d.status in RELOCATED:
                        d.status = AMBIGUOUS
                        d.new_start = d.new_end = None
                        d.detail = (
                            f"this token is left whole: its span "
                            f"{stuck.old_start}-{stuck.old_end} could not be placed — "
                            f"{stuck.detail}"
                        )
                decisions.extend(group_decisions)
                continue
            decisions.extend(group_decisions)
            if not moved:
                continue
            prefix = group[0].raw.rpartition(":")[0]
            replacements.append((group[0].raw, f"{prefix}:{','.join(new_spans)}"))
        if replacements:
            per_line[lineno] = replacements

    edits: list[tuple[int, str]] = []
    for lineno, replacements in per_line.items():
        new_line = rewrite_line(lines[lineno - 1], replacements)
        if new_line is None:
            problems.append(
                f"{doc}:{lineno}: cannot find a citation the parser reports on this line"
            )
            continue
        if new_line != lines[lineno - 1]:
            edits.append((lineno, new_line))
    return decisions, edits, problems


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Relocate documentation citations to where their cited text went. "
                    "Exact shifts only: a citation moves when the block it named is "
                    "still one unchanged run of lines in exactly one place. A block "
                    "that grew, shrank, was reworded, is duplicated or is gone is "
                    "reported as needing a human, and the run exits non-zero with "
                    "every document left as it was.",
        epilog="The documents this rewrites are the ones "
               "scripts/check-citation-drift.py scans: README.md, SECURITY.md, "
               "CONTRIBUTING.md, docs/install.md, docs/privacy.md, "
               "docs/threat-model.md and contracts/*.md. SECURITY.md, "
               "CONTRIBUTING.md and contracts/*.md are included deliberately — a "
               "citation left stale in one of them fails the drift check exactly "
               "as one left stale in README.md does. Source files and "
               "docs/citations.lock are never written. Each document is replaced "
               "in one step and keeps its own line endings and final-newline "
               "state; if any document cannot be written, the documents already "
               "written in that run are put back, and a document that could not "
               "be put back is named as one left rewritten. A comma list is "
               "rewritten whole or not at all.",
    )
    ap.add_argument("--old", required=True, action="append", metavar="COMMIT",
                    help="a side of the merge whose line numbers the documents carry; "
                         "repeat it once per parent")
    ap.add_argument("--dry-run", action="store_true",
                    help="print the plan and change nothing")
    ap.add_argument("--quiet", action="store_true",
                    help="print only the relocations, the refusals and the summary")
    args = ap.parse_args()

    # After the arguments, so `--help` still works from anywhere. Everything
    # below is relative to the working directory's repository: the documents,
    # the citation parser, and every `git` call.
    global REPO, drift
    REPO = resolve_repo()
    drift = load_drift_module()

    if len(set(args.old)) != len(args.old):
        die("the same --old was given twice; each side is one coordinate system")

    basenames = drift.build_basename_index()

    parents: list[Parent] = []
    for rev in args.old:
        rc, out = git("rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}")
        if rc != 0:
            die(f"--old {rev} is not a commit in this repository")
        parents.append(Parent(out.strip()))

    citations, parse_problems = drift.parse_docs(basenames)
    if parse_problems:
        die(
            "the citation parser cannot resolve every citation in the tree, so a "
            "relocation would be applied to a document it does not fully understand:\n"
            + "\n".join(f"  - {p}" for p in parse_problems),
            1,
        )

    by_doc: dict[str, list] = {}
    for cit in citations:
        by_doc.setdefault(cit.doc, []).append(cit)

    plans: dict[str, list[tuple[int, str]]] = {}
    all_decisions: list[tuple[str, Decision, str]] = []  # (doc, decision, target)
    problems: list[str] = []
    for doc in drift.doc_files():
        if doc not in by_doc:
            continue
        decisions, edits, doc_problems = plan_doc(doc, by_doc[doc], parents)
        problems.extend(doc_problems)
        plans[doc] = edits
        for cit, d in zip(by_doc[doc], decisions):
            all_decisions.append((doc, d, cit.target))

    if problems:
        print("[citation-relocate] refusing to write anything:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1

    counts = {s: 0 for s in (RIGHT, SHIFTED, AMBIGUOUS, MISSING, UNKNOWN,
                             UNCLAIMED, BLANK, ELSEWHERE)}
    for doc, d, target in all_decisions:
        counts[d.status] += 1
        if d.status == RIGHT:
            if not args.quiet:
                print(f"  right    {doc}:  {target}:{d.old_start}-{d.old_end}")
            continue
        if d.status in RELOCATED:
            via = f"  [via {d.via[:12]}]" if len(parents) > 1 else ""
            print(f"  shifted  {doc}:  {target}:{d.old_start}-{d.old_end} -> "
                  f"{target}:{d.new_start}-{d.new_end}  "
                  f"({d.old_end - d.old_start + 1} lines, unchanged)"
                  + via)
            continue
        print(f"  REFUSE   {doc}:  {target}:{d.old_start}-{d.old_end} — {d.detail}")

    refusals = sum(counts[s] for s in REFUSALS)
    rewritten = sorted(doc for doc, edits in plans.items() if edits)
    total_lines = sum(len(edits) for edits in plans.values())
    print(
        f"[citation-relocate] --old {' '.join(p.short for p in parents)}: "
        f"{len(all_decisions)} citations, {counts[SHIFTED]} relocated "
        f"(exact shifts only, every block byte-identical), "
        f"{counts[RIGHT]} already right, {refusals} need a human"
    )

    if args.dry_run:
        print(f"[citation-relocate] dry run: {total_lines} line(s) in "
              f"{len(rewritten)} document(s) would change; nothing was written")
        return 1 if refusals else 0

    # Every document's new text is built before any of them is touched, so a
    # document that cannot be read, or an edit that cannot be placed, stops the
    # run with nothing written at all. The writes then go one document at a time
    # through write_atomic(), and if one of them fails the ones already done are
    # put back from `originals` — a run either lands whole or changes nothing.
    originals: dict[str, str] = {}
    new_text: dict[str, str] = {}
    for doc, edits in plans.items():
        if not edits:
            continue
        original = read_doc(doc)
        originals[doc] = original
        try:
            new_text[doc] = replace_lines(original, edits)
        except ValueError as exc:
            print(f"[citation-relocate] refusing to write anything: {doc}: {exc}",
                  file=sys.stderr)
            return 1

    written: list[str] = []
    try:
        for doc, text in new_text.items():
            write_atomic(doc, text)
            written.append(doc)
    except OSError as exc:
        stranded: list[str] = []
        for doc in written:
            try:
                write_atomic(doc, originals[doc])
            except OSError as undo:
                # 🔴 A document that could not be put back is *still rewritten*
                # in the tree. Saying "the documents already rewritten were put
                # back, so nothing was changed" over an unrestored one reports a
                # document that is in the wrong state as untouched, which is the
                # one sentence the reader must not be given here. It is named,
                # and it is named in the summary rather than only in the error
                # line above it.
                stranded.append(f"{doc} ({undo})")
                print(f"[citation-relocate] {doc} could not be restored: {undo}",
                      file=sys.stderr)
        drift._file_cache.clear()
        if stranded:
            print(f"[citation-relocate] writing a document failed: {exc}; "
                  f"{len(written) - len(stranded)} of the {len(written)} document(s) "
                  f"already rewritten were put back. COULD NOT RESTORE: "
                  + "; ".join(stranded)
                  + " — these documents are left carrying the rewrite and the tree "
                    "is NOT as the run found it. Fix them before anything else.",
                  file=sys.stderr)
        else:
            print(f"[citation-relocate] writing a document failed: {exc}; the "
                  f"{len(written)} document(s) already rewritten were put back, so "
                  f"nothing was changed", file=sys.stderr)
        return 1

    if rewritten:
        print(f"[citation-relocate] rewrote {len(rewritten)} document(s): {', '.join(rewritten)}")

    # Read back what was just written, through the same parser the drift check
    # uses, and confirm every relocated citation is now exactly where the plan
    # said — the same *file*, at the same lines. The file is part of the check
    # because the same numbers in another file are a different citation: a
    # rewrite that landed on `pkg/src/a.ts:17` when the plan said `src/a.ts:17`
    # satisfied a check that compared (document, start, end) alone, and the
    # damaged document was reported as a clean run. A parse that disagrees means
    # the edit landed on the wrong token, and the documents go back to their
    # previous bytes before anything is reported.
    drift._file_cache.clear()
    after, after_problems = drift.parse_docs(basenames)
    expected = {(doc, target, d.new_start, d.new_end)
                for doc, d, target in all_decisions if d.status in RELOCATED}
    got = {(c.doc, c.target, c.start, c.end) for c in after}
    if after_problems or not expected <= got:
        # 🔴 Putting the documents back is itself a write, and it can fail. An
        # OSError escaping here used to be a traceback out of main(): a document
        # earlier in the loop already restored and a later one still carrying
        # the rewrite, with nothing on stdout saying which — the read-back check
        # exists precisely so that a rewritten tree is never reported as a clean
        # run, and an unguarded restore put that report behind a stack trace.
        # Each document is restored on its own, and one that cannot be put back
        # is named to stderr *and* counted in the summary below, because "the
        # edits were reverted" over an unrestored document is the one sentence
        # this branch must not print.
        stranded: list[str] = []
        for doc, original in originals.items():
            try:
                write_atomic(doc, original)
            except OSError as undo:
                stranded.append(f"{doc} ({undo})")
                print(f"[citation-relocate] {doc} could not be restored: {undo}",
                      file=sys.stderr)
        drift._file_cache.clear()
        detail = "\n".join(f"  - {p}" for p in after_problems) if after_problems else ""
        missing = "; ".join(f"{d} {t}:{s}-{e}" for d, t, s, e in sorted(expected - got))
        head = (
            "[citation-relocate] the rewritten documents do not parse back to the plan:\n"
            f"  expected {len(expected)} relocated anchor(s), {len(expected - got)} of "
            f"them not read back where the plan put them"
            + (f": {missing}" if missing else "")
            + "\n"
            + (f"  the read-back reports:\n{detail}\n" if detail else "")
        )
        if stranded:
            print(
                head
                + f"  {len(originals) - len(stranded)} of the {len(originals)} "
                f"rewritten document(s) were put back. COULD NOT RESTORE: "
                + "; ".join(stranded)
                + " — these documents are left carrying the rewrite and the tree is "
                  "NOT as the run found it. Fix them before anything else.",
                file=sys.stderr,
            )
        else:
            print(head
                  + f"  all {len(originals)} rewritten document(s) were put back, so "
                    f"nothing was changed",
                  file=sys.stderr)
        return 1

    if rewritten:
        print("[citation-relocate] these documents now carry the working tree's numbers; "
              "run: python3 scripts/check-citation-drift.py")
    return 1 if refusals else 0


if __name__ == "__main__":
    sys.exit(main())
