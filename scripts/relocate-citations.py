#!/usr/bin/env python3
"""relocate-citations.py — move each `path:line` citation to where its text went.

Every merge of a parallel branch edits code, which moves lines, which leaves the
citations in README.md and docs/ pointing at text that is no longer there. The
relocation is mechanical — take the text a citation named at the commit whose
numbers the document still carries, find that same text in the merged working
tree, rewrite the range — and doing it by hand is what has cost this repository
several extra rounds (W46b, W59d, W65m, W64c). This script does that one step.

    python3 scripts/relocate-citations.py --old <parentA> --old <parentB> --dry-run
    python3 scripts/relocate-citations.py --old <parentA> --old <parentB>
    python3 scripts/check-citation-drift.py        # then read every failure
    python3 scripts/check-citation-drift.py --update   # only once they are clean

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

🔴 A relocation is applied only when the old text is found in exactly one place,
in every side that claims it. Zero matches, several matches, and sides that
disagree are all left alone and reported, and the run exits non-zero: an edit
made under any of those is a guess about which of several candidate claims a
sentence was written about.

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
already written in that run is put back, and the run reports failure.

Usage:
    python3 scripts/relocate-citations.py --old <commit> [--old <commit> ...]
                                          [--dry-run] [--quiet]

Exit codes: 0 = every citation is now right · 1 = at least one needs a human ·
            2 = usage error.
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

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HERE = os.path.dirname(os.path.abspath(__file__))


def load_drift_module():
    """Import scripts/check-citation-drift.py as a module.

    The citation parser lives there and is the authority on what a citation is;
    a second copy of that regex here is a second answer to "what does the doc
    cite", and the two would drift. The selftest of the drift checker imports it
    the same way (probe 5).
    """
    path = os.path.join(HERE, "check-citation-drift.py")
    spec = importlib.util.spec_from_file_location("citation_drift", path)
    if spec is None or spec.loader is None:  # pragma: no cover - unreachable in a checkout
        die(f"cannot load {path}", 2)
    module = importlib.util.module_from_spec(spec)
    sys.modules["citation_drift"] = module
    spec.loader.exec_module(module)
    return module


drift = load_drift_module()


def die(msg: str, code: int = 2) -> None:
    print(f"[citation-relocate] {msg}", file=sys.stderr)
    sys.exit(code)


# --------------------------------------------------------------------------
# Outcomes of looking for one citation's old text in the merged file.
# --------------------------------------------------------------------------
RIGHT = "right"            # the same text is at the same lines: nothing to do
SHIFTED = "shifted"        # found as one contiguous run: relocate
GROWN = "grown"            # found with lines inserted inside it: relocate, and say so
AMBIGUOUS = "ambiguous"    # found in several places
MISSING = "missing"        # found nowhere
UNKNOWN = "unknown"        # the file, or the range, is not readable at --old
UNCLAIMED = "unclaimed"    # no declared side's own document writes this range
BLANK = "blank"            # every cited line is whitespace, so it names nothing

# Outcomes that rewrite a range. Everything else leaves the document alone.
RELOCATED = (SHIFTED, GROWN)

# The refusing outcomes, kept apart because they mean different things to
# whoever has to fix them: several matches is "this sentence could be about any
# of these", zero matches is "the text it was written about is gone", blank is
# "there is no text here to look for", and unclaimed is "these numbers are in
# nobody's coordinate system".
REFUSALS = (AMBIGUOUS, MISSING, UNKNOWN, UNCLAIMED, BLANK)


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


_RANGE_RE = re.compile(r":(\d+)(?:-(\d+))?(?![\d-])")

# The path token the author wrote immediately before a `:N`. Used to tell one
# file's citation from another file's citation of the same line numbers.
_PATH_BEFORE_COLON_RE = re.compile(r"([A-Za-z0-9_./-]+)$")


def ranges_written(text: str) -> set[tuple[str | None, int, int]]:
    """Every (path token, start, end) a document writes as a citation.

    This reads the range text rather than re-parsing the document, because the
    only question it answers is "is this range in this side's coordinate
    system". A citation written as `a/b.ts:2-4`, as `b.ts:2-4` or as a bare
    `:2-4` continuation all carry the same `:2-4`, and the side that wrote it
    owns that range however it spelled the path **to that file**. The trailing
    guard is what keeps `:2-4` from matching inside `:2-40`.

    The path token is kept because the numbers alone are not a coordinate: two
    files have a line 3, and a document that only ever wrote `alpha.ts:3-4` was
    never a statement about `beta.ts:3-4`. A token of None is a bare `:2-4`,
    which spells no path at all and so is not a statement about one file
    specifically — see `Parent.claims()`.
    """
    spans = set()
    for m in _RANGE_RE.finditer(text):
        named = _PATH_BEFORE_COLON_RE.search(text[: m.start()])
        start = int(m.group(1))
        spans.add((named.group(1) if named else None, start,
                   int(m.group(2)) if m.group(2) else start))
    return spans


class Parent:
    """One side of the merge: a commit, and the documents as that side wrote them."""

    def __init__(self, commit: str):
        self.commit = commit
        self.spans: dict[str, set[tuple[str | None, int, int]]] = {}
        self.lines: dict[str, set[str]] = {}
        for doc in drift.doc_files():
            rc, out = git("show", f"{commit}:{doc}")
            if rc == 0:
                self.spans[doc] = ranges_written(out)
                self.lines[doc] = set(out.splitlines())

    @property
    def short(self) -> str:
        return self.commit[:12]

    def claims(self, doc: str, target: str, start: int, end: int) -> bool:
        """Whether this side's own document writes a citation of `target` here.

        The line numbers alone would answer a different question. `alpha.ts:3-4`
        is not a claim about `beta.ts:3-4` even though it carries the same
        numbers, and treating it as one lets a side vote on a citation it never
        made — including voting to relocate it.

        A token that spelled a full path has to be that path. A token that
        spelled a bare file name (`main.rs:3-4` for crates/.../main.rs) matches
        on the basename, which is how the citation parser resolves one too. A
        token of None is a bare `:3-4` continuation: it names no path, so it is
        a coordinate in this side's system for whichever path its own sentence
        named, and it counts here. Erring towards ownership is the safe
        direction, because an owner that cannot place the range vetoes the
        relocation rather than authorising it.
        """
        for token, s, e in self.spans.get(doc, ()):
            if (s, e) != (start, end):
                continue
            if token is None:
                return True
            if token == target or token == os.path.basename(target):
                return True
        return False

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
    """Where the text cited as lines start..end in this side's numbers lives now."""
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
    return grown(old, cur, start, end) or Decision(
        MISSING, start, end, None, None,
        "that text is not in the merged file at all — it was rewritten or removed",
    )


def embedding(cur: SourceIndex, block: list[str]) -> "list[int] | None":
    """The earliest line indices at which `block` embeds in `cur`, lines in order.

    `block` is the cited lines in order. A merge edits the middle of a construct
    a sentence cites, so the block's lines stay in the file in order but not
    adjacent any more; an embedding picks one line of the file for each line of
    the block, and the earliest one is the tightest window there is.

    🔴 "Earliest" is a choice, and the choice is only trustworthy when each step
    had no alternative — see `grown()`, which checks that before using this.
    """
    hits: list[int] = []
    at = 0
    for want in block:
        while at < len(cur) and cur.norm[at] != want:
            at += 1
        if at >= len(cur):
            return None
        hits.append(at)
        at += 1
    return hits


def grown(old: SourceIndex, cur: SourceIndex, start: int, end: int) -> "Decision | None":
    """The block with lines inserted inside it, so it is no longer one run.

    This is the common shape after a merge: the other side edited the middle of
    the function a sentence cites, and every line the citation named is still
    there, just not adjacent any more. It is not a guess, and it is not "nearest
    match": the block's own first line must occur in the merged file **exactly
    once** — that is the anchor — and then every remaining line of the block must
    be found after it, in order. The window is the tightest one that satisfies
    both, so it cannot stretch further than the content forces.

    A block whose lines genuinely changed fails the in-order test and returns
    None, which is what sends it to a human rather than to a neighbouring range.
    A block whose first line is not unique returns None too: four occurrences of
    `*/` at the top of four different comments is not an anchor.

    "In order" is not enough on its own, because a file can offer the block more
    than one alignment and only one of them is the construct the sentence was
    written about. Two more things are required of the walk, and both are checks
    on the alignment the greedy walk produced:

      * every cited line except the first and the last must have had no
        alternative at its step — the line must not occur again later in the
        file. A cited block whose middle line also occurs further down has two
        alignments, and the greedy walk silently takes the earlier one, which is
        the alignment that stops short of the real text;
      * no line skipped *between* two matched lines may be one of the block's
        own lines. That means the "inserted" line is really a second copy of a
        cited line: the walk has stepped over the block's own tail to reach a
        later copy of it, and the window has run off the end of the cited
        construct onto whatever repeats it further down the file.

    Two identical cited lines with one of them deleted used to come back as a
    *shrunk* one-line range, and a deleted line whose text still existed later
    used to come back as a window covering two other constructs; both are
    refusals now.

    🔴 The result can be longer than the old range, and how much longer is a
    judgement about whether the inserted lines belong to the cited claim. The
    caller prints both numbers and the count of inserted lines; read them before
    `--update`.
    """
    if end - start < 1:
        return None
    anchors = cur.windows(old.block(start, start))
    if len(anchors) != 1:
        return None
    block = old.norm[start - 1 : end]
    matched = embedding(cur, block)
    if matched is None:
        # Not a refusal to report from here: the caller does not know either
        # whether the block embeds, and "these lines are not in the file in this
        # order at all" is the `missing` it reports.
        return None
    for i in range(1, len(block) - 1):
        if cur.norm[matched[i - 1] + 1 :].count(block[i]) > 1:
            return Decision(
                AMBIGUOUS, start, end, None, None,
                f"the cited lines are in the merged file, but in more than one "
                f"alignment: `{block[i]}` is one of them and occurs again later, so "
                f"which copy the citation was about cannot be told",
            )
    first, last = matched[0], matched[-1]
    matched_at = set(matched)
    cited = set(block)
    for between in range(first, last + 1):
        if between not in matched_at and cur.norm[between] in cited:
            return Decision(
                AMBIGUOUS, start, end, None, None,
                f"the cited lines are in the merged file, but in more than one "
                f"alignment: line {between + 1} is `{cur.norm[between]}`, one of them, "
                f"and it sits between two of the others",
            )
    if (first, last) == (start - 1, end - 1):
        # Every old line is still on its own line; only lines *between* them were
        # removed. The citation's range has not moved, which is this script's
        # whole question — the content change is check-citation-drift.py's, and
        # it reports it on the next run.
        return Decision(RIGHT, start, end, start, end)
    return Decision(
        GROWN, start, end, first + 1, last + 1,
        f"{last - first + 1 - (end - start + 1)} line(s) inserted inside the block",
    )


def decide(
    parents: list[Parent],
    cur: SourceIndex,
    doc: str,
    line: str,
    target: str,
    start: int,
    end: int,
) -> Decision:
    """Decide one citation, using only the sides whose own document writes it."""
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
            moved = False
            for j, cit in enumerate(group):
                d = decide(parents, cur, doc, lines[lineno - 1], cit.target, cit.start, cit.end)
                decisions.append(d)
                if d.status in RELOCATED:
                    moved = True
                    new_spans.append(
                        str(d.new_start) if d.new_start == d.new_end
                        else f"{d.new_start}-{d.new_end}"
                    )
                else:
                    new_spans.append(written[j])
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
        description="Relocate documentation citations to where their cited text went.",
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
               "written in that run are put back.",
    )
    ap.add_argument("--old", required=True, action="append", metavar="COMMIT",
                    help="a side of the merge whose line numbers the documents carry; "
                         "repeat it once per parent")
    ap.add_argument("--dry-run", action="store_true",
                    help="print the plan and change nothing")
    ap.add_argument("--quiet", action="store_true",
                    help="print only the relocations, the refusals and the summary")
    args = ap.parse_args()

    if len(set(args.old)) != len(args.old):
        die("the same --old was given twice; each side is one coordinate system")

    parents: list[Parent] = []
    for rev in args.old:
        rc, out = git("rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}")
        if rc != 0:
            die(f"--old {rev} is not a commit in this repository")
        parents.append(Parent(out.strip()))

    basenames = drift.build_basename_index()
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

    counts = {s: 0 for s in (RIGHT, SHIFTED, GROWN, AMBIGUOUS, MISSING, UNKNOWN,
                             UNCLAIMED, BLANK)}
    for doc, d, target in all_decisions:
        counts[d.status] += 1
        if d.status == RIGHT:
            if not args.quiet:
                print(f"  right    {doc}:  {target}:{d.old_start}-{d.old_end}")
            continue
        if d.status in RELOCATED:
            label = "shift" if d.status == SHIFTED else "GROWN"
            via = f"  [via {d.via[:12]}]" if len(parents) > 1 else ""
            print(f"  {label:8s} {doc}:  {target}:{d.old_start}-{d.old_end} -> "
                  f"{target}:{d.new_start}-{d.new_end}  "
                  f"({d.old_end - d.old_start + 1} -> {d.new_end - d.new_start + 1} lines"
                  + (f", {d.detail})" if d.detail else ")") + via)
            continue
        print(f"  REFUSE   {doc}:  {target}:{d.old_start}-{d.old_end} — {d.detail}")

    refusals = sum(counts[s] for s in REFUSALS)
    rewritten = sorted(doc for doc, edits in plans.items() if edits)
    total_lines = sum(len(edits) for edits in plans.values())
    print(
        f"[citation-relocate] --old {' '.join(p.short for p in parents)}: "
        f"{len(all_decisions)} citations, {counts[SHIFTED] + counts[GROWN]} relocated "
        f"({counts[SHIFTED]} by shift, {counts[GROWN]} with lines inserted inside), "
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
        for doc in written:
            try:
                write_atomic(doc, originals[doc])
            except OSError as undo:
                print(f"[citation-relocate] {doc} could not be restored: {undo}",
                      file=sys.stderr)
        drift._file_cache.clear()
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
        for doc, original in originals.items():
            write_atomic(doc, original)
        drift._file_cache.clear()
        detail = "\n".join(f"  - {p}" for p in after_problems) if after_problems else ""
        missing = "; ".join(f"{d} {t}:{s}-{e}" for d, t, s, e in sorted(expected - got))
        print("[citation-relocate] the rewritten documents do not parse back to the plan; "
              "all edits were reverted:\n"
              f"  expected {len(expected)} relocated anchor(s), {len(expected - got)} of "
              f"them not read back where the plan put them"
              + (f": {missing}" if missing else "") + detail,
              file=sys.stderr)
        return 1

    if rewritten:
        print("[citation-relocate] these documents now carry the working tree's numbers; "
              "run: python3 scripts/check-citation-drift.py")
    return 1 if refusals else 0


if __name__ == "__main__":
    sys.exit(main())
