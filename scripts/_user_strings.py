#!/usr/bin/env python3
"""_user_strings.py — shared extraction of user-visible strings from chat-stasher Rust sources.

Single source of truth for "what text does the CLI show to users", used by:

  - scripts/output-inventory.py   (dumps every prod hit into docs/output-inventory.txt)
  - scripts/check-terminology.py  (lints the prod hits against terminology rules)

Why one module instead of two copies: this repo's ADRs are explicit that a
second copy of a checker silently drifts (see gate-selftest-semantic.sh). The
extraction is the part both tools need; keeping it in one place is the point.

Coverage (grep-verified against this repo, 2026-08-26 — the task insisted we
grep first, not transcribe a list):

  - println! / print! / eprintln! / eprint! / write! / writeln!   → first string literal
  - anyhow::bail! / anyhow! / .context(...) / .with_context(...)  → first string literal
  - panic! / unreachable! / todo! / unimplemented!                → first string literal
      (today every one lives inside #[cfg(test)]; they are still extracted and
       tagged in_test=True so a future prod panic is tracked, not silently missed)
  - clap help surface:
      * explicit  about / long_about / help / long_help = "..."   inside #[command]/#[arg]/#[clap]
      * /// doc-comment lines inside clap-derived type blocks, plus the doc block
        directly above a clap derive attribute (the type-level about)

Deliberately NOT covered (documented in spike/RU-tooling.md):

  - standalone format!  (mostly path/count/identifier construction, not prose;
    format! IS followed when it is the argument of context/with_context/bail,
    because then it is the error message itself)
  - JSON / --json output values (machine contract, not prose)
  - apps/extension (TypeScript) — a separate surface from the CLI
  - the tests/ directory, and #[cfg(test)] modules inside src (tagged in_test)

Each hit carries a deterministic single-line representation of the source
literal: escapes preserved (\\n stays \\n), bare newlines shown as \\n, and
line continuations (backslash + newline) stripped exactly like the compiler
strips them. help_doc hits carry the doc line text with "/// " removed.
"""

from __future__ import annotations

import bisect
import glob
import os
import re
from dataclasses import dataclass

# --------------------------------------------------------------------------- macros

#: macros whose FIRST string-literal argument is user-visible text
MACRO_NAMES = (
    "println", "print", "eprintln", "eprint",
    "write", "writeln",
    "bail", "anyhow",
    "panic", "unreachable", "todo", "unimplemented",
)
PRINT_FAMILY = {"println", "print", "eprintln", "eprint"}

#: identifier-start letters of the tracked macros / methods, used to skip
#: positions that cannot begin one (this is a hot loop over ~250 KB of source)
_MACRO_START = frozenset("pebawcuti.")


@dataclass(frozen=True)
class Hit:
    """One user-visible string.

    kind      — println / print / eprintln / eprint / write / writeln /
                 bail / anyhow / context / with_context / panic / unreachable /
                 todo / unimplemented / help_doc / help_attr
    path      — repo-relative path (forward-slash)
    line      — 1-indexed line of the literal (or of the doc line)
    text      — single-line representation; quoted for literals, raw for help_doc
    unescaped — the actual Rust-decoded string value (quotes stripped, escapes
                decoded); used by the terminology checker for whole-token rules
    in_test   — True when the hit is inside a #[cfg(test)] module
    """

    kind: str
    path: str
    line: int
    text: str
    unescaped: str
    in_test: bool


def src_files(root: str) -> list[str]:
    """All .rs files under crates/chat-stasher/src (the CLI surface)."""
    pattern = os.path.join(root, "crates", "chat-stasher", "src", "**", "*.rs")
    return sorted(glob.glob(pattern, recursive=True))


class _Scanner:
    """Forward character scanner over one Rust source file.

    Stateful enough to keep strings, comments and attributes from confusing
    each other, and to know when we are inside a #[cfg(test)] module or inside
    a clap-derived type block.
    """

    def __init__(self, text: str):
        self.text = text
        self.n = len(text)
        #: 1-indexed line of any character index
        self.line_starts = [0]
        for idx, ch in enumerate(text):
            if ch == "\n":
                self.line_starts.append(idx + 1)
        self.lines = text.split("\n")

    # ------------------------------------------------------------- line / eol
    def line_of(self, pos: int) -> int:
        return bisect.bisect_right(self.line_starts, pos)

    def _eol(self, pos: int) -> int:
        nl = self.text.find("\n", pos)
        return self.n if nl == -1 else nl + 1

    # ---------------------------------------------------------- string literals
    def parse_string(self, i: int) -> tuple[int | None, str | None, str | None]:
        """Parse a Rust string literal starting at i.

        Returns (end_pos, display, value): display is the single-line, still-quoted
        source form; value is the decoded string value. Any is None on failure.
        """
        t, n = self.text, self.n

        # raw string r"..." / r#"..."# / r##"..."## ...
        if t[i] == "r":
            j = i + 1
            hashes = 0
            while j < n and t[j] == "#":
                hashes += 1
                j += 1
            if j >= n or t[j] != '"':
                return i, None, None
            j += 1
            close = '"' + "#" * hashes
            end = t.find(close, j)
            if end == -1:
                return i, None, None
            raw = t[j:end]
            display = 'r' + "#" * hashes + '"' + raw.replace("\n", "\\n") + '"' + "#" * hashes
            return end + len(close), display, raw

        # regular string
        j = i + 1
        disp: list[str] = ['"']
        val: list[str] = []
        while j < n:
            c = t[j]
            if c == "\\":
                if j + 1 >= n:
                    break
                nx = t[j + 1]
                if nx == "\n":            # line continuation: drop \ + newline + indent
                    j += 2
                    while j < n and t[j] in " \t":
                        j += 1
                    continue
                if nx == "\r" and j + 2 < n and t[j + 2] == "\n":
                    j += 3
                    while j < n and t[j] in " \t":
                        j += 1
                    continue
                disp.append("\\")
                disp.append(nx)
                j += 2
                if nx == "u":             # \u{HEX}
                    hx = []
                    while j < n and t[j] != "}":
                        hx.append(t[j])
                        disp.append(t[j])
                        j += 1
                    if j < n:
                        disp.append("}")
                        j += 1
                    try:
                        val.append(chr(int("".join(hx), 16)))
                    except ValueError:
                        val.append("?")
                elif nx == "x":           # \xHH
                    hx = []
                    for _ in range(2):
                        if j < n:
                            hx.append(t[j])
                            disp.append(t[j])
                            j += 1
                    try:
                        val.append(chr(int("".join(hx), 16)))
                    except ValueError:
                        val.append("?")
                else:
                    val.append({"n": "\n", "t": "\t", "r": "\r", "0": "\0",
                                "\\": "\\", '"': '"', "'": "'"}.get(nx, nx))
                continue
            if c == '"':
                j += 1
                disp.append('"')
                return j, "".join(disp), "".join(val)
            if c == "\n":
                disp.append("\\n")
                val.append("\n")
                j += 1
                continue
            disp.append(c)
            val.append(c)
            j += 1
        return i, None, None

    # ------------------------------------------------------------- region scan
    def _find_matching_paren(self, open_pos: int) -> int:
        """Index of the ')' matching the '(' at open_pos (string/comment aware)."""
        t, n = self.text, self.n
        depth = 0
        j = open_pos
        while j < n:
            c = t[j]
            if c == '"' or (c == "r" and j + 1 < n and t[j + 1] in '"#'):
                end, _d, _v = self.parse_string(j)
                if end is not None:
                    j = end
                    continue
            if c == "/" and j + 1 < n:
                if t[j + 1] == "/":
                    j = self._eol(j)
                    continue
                if t[j + 1] == "*":
                    end = t.find("*/", j + 2)
                    j = n if end == -1 else end + 2
                    continue
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
                if depth == 0:
                    return j
            j += 1
        return -1

    def _first_string_literal(self, open_pos: int, close_pos: int) -> tuple[int | None, str | None, str | None]:
        """First string literal inside a macro-call region (any depth)."""
        t = self.text
        j = open_pos + 1
        while j < close_pos:
            c = t[j]
            if c == '"' or (c == "r" and j + 1 < close_pos and t[j + 1] in '"#'):
                end, disp, val = self.parse_string(j)
                if disp is not None:
                    return j, disp, val
                j = end if end is not None else j + 1
                continue
            if c == "/" and j + 1 < close_pos:
                if t[j + 1] == "/":
                    nl = t.find("\n", j, close_pos)
                    j = close_pos if nl == -1 else nl + 1
                    continue
                if t[j + 1] == "*":
                    end = t.find("*/", j + 2, close_pos)
                    j = close_pos if end == -1 else end + 2
                    continue
            j += 1
        return None, None, None

    # ------------------------------------------------------------------- scan
    def scan(self, path: str) -> list[Hit]:
        t, n = self.text, self.n
        hits: list[Hit] = []

        in_test = False
        test_pending = False
        clap_pending = False
        expect_clap_brace = False
        clap_type_start = 0
        clap_derive_ranges: list[tuple[int, int]] = []
        clap_type_ranges: list[tuple[int, int]] = []
        brace_stack: list[str | None] = []
        doc_lines: list[tuple[int, str]] = []

        def emit(kind: str, line: int, disp: str | None, val: str | None) -> None:
            if disp is not None:
                hits.append(Hit(kind=kind, path=path, line=line,
                                text=disp, unescaped=val or "", in_test=in_test))

        i = 0
        while i < n:
            c = t[i]

            # -- string literal (skip; literals inside macros are handled below)
            if c == '"' or (c == "r" and i + 1 < n and t[i + 1] in '"#'):
                end, _d, _v = self.parse_string(i)
                if end is not None:
                    i = end
                    continue

            # -- comments
            if c == "/" and i + 1 < n:
                if t[i + 1] == "/":
                    if t[i + 2 : i + 3] == "/":  # doc comment ///
                        ln = self.line_of(i)
                        stripped = self.lines[ln - 1].lstrip()
                        if stripped.startswith("///"):
                            content = stripped[3:]
                            if content.startswith(" "):
                                content = content[1:]
                            if not in_test:
                                doc_lines.append((ln, content))
                            i = self._eol(i)
                            continue
                    i = self._eol(i)
                    continue
                if t[i + 1] == "*":
                    end = t.find("*/", i + 2)
                    i = n if end == -1 else end + 2
                    continue

            # -- attributes #[ ... ]
            if c == "#" and i + 1 < n and t[i + 1] == "[":
                attr_start = i
                j = i + 2
                while j < n:
                    cj = t[j]
                    if cj == '"' or (cj == "r" and j + 1 < n and t[j + 1] in '"#'):
                        end, _d, _v = self.parse_string(j)
                        if end is not None:
                            j = end
                            continue
                        j += 1
                        continue
                    if cj == "]":
                        break
                    if cj == "/" and j + 1 < n and t[j + 1] == "/":
                        j = self._eol(j)
                        continue
                    j += 1
                attr_end = j
                attr_start_line = self.line_of(attr_start)
                attr_text = t[attr_start + 2 : attr_end]

                if not in_test:
                    if re.search(r"\bcfg\s*\(\s*test\s*\)", attr_text):
                        test_pending = True
                    if re.search(r"\bderive\s*\([^)]*\b(Parser|Subcommand|Args)\b", attr_text):
                        clap_pending = True
                        clap_derive_ranges.append((attr_start_line, self.line_of(attr_end)))
                    # explicit about/help = "..." inside #[command(...)] / #[arg(...)] / #[clap(...)]
                    for m in re.finditer(r"\b(about|long_about|help|long_help)\s*=", attr_text):
                        kpos = attr_start + 2 + m.end()
                        while kpos < n and t[kpos] in " \t":
                            kpos += 1
                        if kpos < n and (t[kpos] == '"' or (t[kpos] == "r" and kpos + 1 < n and t[kpos + 1] in '"#')):
                            end, disp, val = self.parse_string(kpos)
                            if disp is not None:
                                hits.append(Hit(kind="help_attr", path=path, line=self.line_of(kpos),
                                                text=disp, unescaped=val or "", in_test=in_test))
                i = attr_end + 1
                continue

            # -- braces
            if c == "{":
                label = None
                if test_pending:
                    label = "test"
                    test_pending = False
                elif expect_clap_brace:
                    label = "clap"
                    expect_clap_brace = False
                    clap_type_start = self.line_of(i)
                brace_stack.append(label)
                if label == "test":
                    in_test = True
                i += 1
                continue
            if c == "}":
                if brace_stack:
                    label = brace_stack.pop()
                    if label == "clap":
                        clap_type_ranges.append((clap_type_start, self.line_of(i)))
                    elif label == "test":
                        in_test = any(x == "test" for x in brace_stack)
                i += 1
                continue

            # -- struct / enum keyword (opens a clap type block when pending)
            if c.isalpha():
                m = re.match(r"\b(struct|enum)\b", t[i:])
                if m:
                    if clap_pending and not in_test:
                        expect_clap_brace = True
                        clap_pending = False
                    i += m.end()
                    continue

            # -- macro calls / method-context calls
            if c.isalpha() or c == ".":
                if c == ".":
                    mm = re.match(r"\.(with_context|context)\s*\(", t[i:])
                    if mm:
                        open_pos = i + mm.end() - 1
                        close = self._find_matching_paren(open_pos)
                        if close != -1:
                            s, disp, val = self._first_string_literal(open_pos, close)
                            if disp is not None:
                                hits.append(Hit(kind=mm.group(1), path=path, line=self.line_of(s),
                                                text=disp, unescaped=val or "", in_test=in_test))
                            i = close + 1
                            continue
                else:
                    mm = re.match(
                        r"(?:anyhow::)?(println|print|eprintln|eprint|write|writeln|"
                        r"bail|anyhow|panic|unreachable|todo|unimplemented)!\s*\(",
                        t[i:],
                    )
                    if mm:
                        open_pos = i + mm.end() - 1
                        close = self._find_matching_paren(open_pos)
                        if close != -1:
                            s, disp, val = self._first_string_literal(open_pos, close)
                            kind = mm.group(1)
                            if disp is not None:
                                hits.append(Hit(kind=kind, path=path, line=self.line_of(s),
                                                text=disp, unescaped=val or "", in_test=in_test))
                            elif kind in PRINT_FAMILY:
                                # blank print, e.g. println!()
                                hits.append(Hit(kind=kind, path=path, line=self.line_of(open_pos),
                                                text='""', unescaped="", in_test=in_test))
                            i = close + 1
                            continue

            i += 1

        # -- doc comments used as clap help: inside a clap type block, or the
        #    doc block directly above a clap derive attribute (type-level about)
        doc_line_set = {ln for ln, _ in doc_lines}
        doc_above: list[tuple[int, int]] = []
        for s, _e in clap_derive_ranges:
            run_end = s - 1
            cur = run_end
            while cur >= 1 and cur in doc_line_set:
                cur -= 1
            if cur < run_end:
                doc_above.append((cur + 1, run_end))

        help_ranges = clap_type_ranges + doc_above
        for ln, content in doc_lines:
            if any(a <= ln <= b for a, b in help_ranges):
                hits.append(Hit(kind="help_doc", path=path, line=ln,
                                text=content, unescaped=content, in_test=False))

        # deterministic, no duplicates
        seen: set[tuple[str, int, str]] = set()
        out: list[Hit] = []
        for h in sorted(hits, key=lambda h: (h.path, h.line, h.kind)):
            key = (h.kind, h.line, h.text)
            if key not in seen:
                seen.add(key)
                out.append(h)
        return out


def extract_file(path: str, root: str) -> list[Hit]:
    """All user-visible string hits in one source file."""
    rel = os.path.relpath(path, root)
    with open(path, "r", encoding="utf-8") as fh:
        text = fh.read()
    return _Scanner(text).scan(rel)


def extract_all(root: str) -> list[Hit]:
    """All user-visible string hits across the CLI crate."""
    hits: list[Hit] = []
    for path in src_files(root):
        hits.extend(extract_file(path, root))
    return hits
