#!/usr/bin/env python3
"""check-commit-messages.py - commit messages in this public repository must be English.

Why this exists: the repository is public and the commit log is part of it. A
reviewer who cannot read a commit message cannot review the change it describes,
and the log outlives every contributor's memory of it. So the log is English.

What is checked is the LANGUAGE of the message, not its wording - this is a gate,
not a copy editor. Lines that git itself would drop as template comments (those
starting with the comment character, "#" unless core.commentChar says otherwise)
are not part of the message and are not checked; that is what lets the hook run
BEFORE git strips them.

Two modes:

    check-commit-messages.py --file <path>
        <path> is a COMMIT_EDITMSG-style file. Used by scripts/hooks/commit-msg,
        which runs before the commit exists.

    check-commit-messages.py --range <base>..<head>
    check-commit-messages.py --range <rev>
        Every commit in the range, each message checked in full (subject and
        body). The single-revision form checks exactly that one commit, for the
        case where a range has nothing to walk: a tag push, whose commit was
        already proven on main, or a push whose range is empty because HEAD is
        already reachable from the base. Used by CI, which can only see what is
        already recorded.

    check-commit-messages.py --selftest
        Proves the gate still catches what it is for, using fixtures in a temp
        repository. It deliberately does NOT check the real history: this repo's
        log contains commits that predate the rule, and a self-test that depended
        on them would go red for the wrong reason.

Both modes accept --repo <path> (default: this script's repository).

Exit codes: 0 = clean (or the self-test passed) . 1 = violation found (or the
self-test failed) . 2 = usage error . 3 = could not read what was asked for, so
nothing was proven - an unreadable message file, or a revision git cannot
resolve. 3 is not 1 and is not 0: "we could not look" must never be reported as
"we looked and it was clean".
"""

from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SIBLING_TERMINOLOGY = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "check-terminology.py"
)

# The CJK character range, as a fallback copy of the one check-terminology.py's
# T5 rule uses. The two must cover the same characters - --selftest compares what
# they MATCH, since they are spelled differently (escapes there, code points here)
# - and the imported one wins whenever the sibling lint can be loaded.
FALLBACK_CJK_PATTERN = "[%s-%s]" % (chr(0x4E00), chr(0x9FFF))


def _load_t5_pattern() -> tuple[str | None, str]:
    """Return (pattern, where it came from). pattern is None if it could not be read."""
    try:
        spec = importlib.util.spec_from_file_location("_check_terminology_t5", SIBLING_TERMINOLOGY)
        if spec is None or spec.loader is None:
            return None, f"cannot load {SIBLING_TERMINOLOGY}"
        module = importlib.util.module_from_spec(spec)
        # Registered before execution because the module uses @dataclass, and the
        # dataclass machinery looks itself up in sys.modules while it runs.
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        for rule in module.RULES:
            if rule["id"] == "T5":
                return rule["patterns"][0]["forbidden"], "check-terminology.py RULES/T5"
    except Exception as exc:  # any import failure means "use the fallback"
        return None, f"cannot import {SIBLING_TERMINOLOGY}: {exc}"
    return None, "check-terminology.py has no T5 rule"


_T5_PATTERN, T5_SOURCE = _load_t5_pattern()
CJK_PATTERN = _T5_PATTERN if _T5_PATTERN else FALLBACK_CJK_PATTERN
CJK_RE = re.compile(CJK_PATTERN)

SUGGESTION = "write the commit message in English; Chinese is allowed only in apps/extension/locales/zh_CN.yml"

UNREADABLE_NOTE = "  nothing was checked, so a clean result would prove nothing."


# ------------------------------------------------------------------ decoding

def decode_candidates(raw: bytes) -> list[tuple[str, str]]:
    """(encoding, text) candidates for a byte string, best first.

    UTF-8 first. If the bytes are not valid UTF-8 they may be a GBK/GB18030
    message, so that decoding is examined too: a Chinese message that is not
    UTF-8 would otherwise pass this gate for the wrong reason - we could not read
    it, which is not the same as there being nothing there to read.
    """
    try:
        return [("utf-8", raw.decode("utf-8"))]
    except UnicodeDecodeError:
        candidates = [("utf-8", raw.decode("utf-8", errors="replace"))]
        try:
            candidates.append(("gb18030", raw.decode("gb18030")))
        except UnicodeDecodeError:
            pass
        return candidates


def cjk_lines(raw: bytes, strip_comment: str | None) -> list[tuple[str, int, str]]:
    """[(encoding, lineno, line)] for message lines containing CJK.

    strip_comment is the character git uses for template comments, or None to
    check every line (a recorded commit message has already been cleaned up, so
    in --range mode there are no comments left to skip).
    """
    for encoding, text in decode_candidates(raw):
        found: list[tuple[str, int, str]] = []
        for lineno, line in enumerate(text.splitlines(), start=1):
            if strip_comment and line.lstrip().startswith(strip_comment):
                continue
            if CJK_RE.search(line):
                found.append((encoding, lineno, line.strip()))
        if found:
            return found
    return []


def comment_char(repo: str) -> str:
    """The character git strips commit-template comments with (default '#').

    core.commentChar may be set to a single character; 'auto' lets git choose per
    message, which cannot be mirrored here, so anything that is not one character
    falls back to '#' - the character git itself picks unless the message already
    uses it.
    """
    try:
        proc = subprocess.run(
            ["git", "-C", repo, "config", "--get", "core.commentChar"],
            capture_output=True, text=True,
        )
    except OSError:
        return "#"
    value = proc.stdout.strip()
    return value if len(value) == 1 else "#"


# -------------------------------------------------------------------- --file

def check_file(path: str, repo: str) -> int:
    try:
        with open(path, "rb") as fh:
            raw = fh.read()
    except OSError as exc:
        print(f"check-commit-messages: cannot read {path}: {exc}", file=sys.stderr)
        print(UNREADABLE_NOTE, file=sys.stderr)
        return 3

    hits = cjk_lines(raw, strip_comment=comment_char(repo))
    if not hits:
        print(f"OK - {path}: commit message is English")
        return 0

    print(f"FAILED:{len(hits)}")
    for encoding, lineno, line in hits:
        print(f"  ! {path}:{lineno} [commit-message-must-be-english] -> {SUGGESTION}")
        print(f"      {line}")
        if encoding != "utf-8":
            print(f"      (the message is not valid UTF-8; read as {encoding})")
    return 1


# ------------------------------------------------------------------- --range

# %h (short sha), then the separator, then the full message. `git log -z`
# separates commits with a NUL and adds no newline of its own, so a record is
# exactly the bytes between two NULs - without -z git puts a newline in front of
# every record after the first, which lands inside the message. The separator is
# written as an escape rather than a literal byte so that it survives every
# transport; the sha is hex, so the FIRST separator in a record is always git's.
LOG_FORMAT = "--format=format:%h%x1f%B"
RECORD_SEP = b"\x00"
FIELD_SEP = b"\x1f"


def _git(repo: str, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", repo, *args], capture_output=True)


def subject_of(text: str, limit: int = 120) -> str:
    for line in text.splitlines():
        line = line.strip()
        if line:
            return line if len(line) <= limit else line[: limit - 1] + "..."
    return "(empty message)"


def parse_range_spec(spec: str) -> str | None:
    """Reject a malformed range. Returns an error message, or None if it is usable."""
    if spec.startswith("-"):
        return "a spec that starts with '-' would be read by git as an option, not a revision"
    if "..." in spec:
        return "'...' has different semantics than '..' and is not supported"
    if ".." not in spec:
        return None if spec.strip() else "empty revision"
    base, _, head = spec.partition("..")
    if not base.strip() or not head.strip():
        return "expected <base>..<head>"
    return None


def check_range(repo: str, spec: str) -> int:
    problem = parse_range_spec(spec)
    if problem:
        print(f"check-commit-messages: bad range {spec!r}: {problem}", file=sys.stderr)
        return 2

    # No "..": check exactly the named commit. `git log -1 <rev>` walks no
    # further, which is what "only this one commit" has to mean here.
    cmd = ["log", "--no-color", "-z", LOG_FORMAT]
    # `--end-of-options` makes git read the spec as a revision (or range) even
    # if it diverged from the shape parse_range_spec already rejected - so a
    # value that walks like a git flag is never forwarded as one. `-1` has to
    # stay before it in the single-revision form: that is a real git flag that
    # limits the walk, not part of the spec.
    cmd += ["-1", "--end-of-options", spec] if ".." not in spec else ["--end-of-options", spec]

    proc = _git(repo, *cmd)
    if proc.returncode != 0:
        print(f"check-commit-messages: git could not resolve {spec!r}:", file=sys.stderr)
        print((proc.stderr.decode("utf-8", errors="replace").strip() or "(no message)"),
              file=sys.stderr)
        print(UNREADABLE_NOTE, file=sys.stderr)
        return 3

    records = [rec for rec in proc.stdout.split(RECORD_SEP) if rec.strip()]
    if not records:
        # A range that resolves to zero commits proves nothing about any commit.
        # "OK - 0 commit(s)" would report a measurement nobody made, turning
        # "we did not look" into "we looked and it was clean".
        print(f"check-commit-messages: the range {spec!r} resolved to 0 commit(s); nothing was proven",
              file=sys.stderr)
        print(UNREADABLE_NOTE, file=sys.stderr)
        return 3
    violations: list[tuple[str, str, list[tuple[str, int, str]]]] = []
    for rec in records:
        short, sep, message = rec.partition(FIELD_SEP)
        if not sep:
            # git cannot produce this; if it ever does, the parse is wrong and
            # saying "clean" would be reporting a measurement nobody made.
            print(f"check-commit-messages: cannot parse a git log record for {spec!r}",
                  file=sys.stderr)
            print(UNREADABLE_NOTE, file=sys.stderr)
            return 3
        hits = cjk_lines(message, strip_comment=None)
        if hits:
            text = decode_candidates(message)[0][1]
            violations.append((short.decode("ascii", errors="replace"), subject_of(text), hits))

    if not violations:
        print(f"OK - {len(records)} commit(s) in {spec}: every message is English")
        return 0

    print(f"FAILED:{len(violations)}")
    for short, subject, hits in violations:
        print(f"  ! {short} [commit-message-must-be-english] -> {SUGGESTION}")
        print(f"      subject: {subject}")
        for encoding, lineno, line in hits:
            print(f"      line {lineno}: {line}")
            if encoding != "utf-8":
                print(f"      (the message is not valid UTF-8; read as {encoding})")
    print(f"  ({len(violations)} of {len(records)} commit(s) in {spec} failed)")
    return 1


# ----------------------------------------------------------------- selftest
# The fixtures are Chinese, but this file - like every other file in the
# repository - must not contain the characters themselves, so _cjk() below builds
# them from code points instead.

def _cjk(*codes: int) -> str:
    """Build fixture text from code points.

    The fixtures below are Chinese, but this file - like every other file in the
    repository - must not contain the characters themselves, so they are spelled
    as numbers. Every code point used here is inside the range T5 forbids.
    """
    return "".join(chr(c) for c in codes)


# "repair the title" - a subject line a person might actually write.
CJK_SUBJECT = _cjk(0x4FEE, 0x590D, 0x6807, 0x9898)
# "the body is Chinese too" - so the gate is exercised past the subject line.
CJK_BODY = _cjk(0x6B63, 0x6587, 0x4E5F, 0x662F, 0x4E2D, 0x6587)
# A template-comment line, which git strips: it must NOT be treated as message.
CJK_COMMENT = "# " + _cjk(0x6CE8, 0x91CA, 0x884C)

FIXTURE_ENGLISH = (
    "fix: keep the archive append-only\n"
    "\n"
    "Deleting the source must not delete what was already archived.\n"
)
FIXTURE_CJK_SUBJECT = f"fix: {CJK_SUBJECT}\n"
FIXTURE_CJK_BODY = f"docs: describe the rule\n\n{CJK_BODY}\n"
FIXTURE_CJK_COMMENT_ONLY = f"fix: keep the archive append-only\n\n{CJK_COMMENT}\n"


def _commit(repo: str, message: str) -> None:
    counter_path = os.path.join(repo, "counter.txt")
    try:
        with open(counter_path, "r", encoding="utf-8") as fh:
            n = int(fh.read().strip() or "0")
    except FileNotFoundError:
        n = 0
    with open(counter_path, "w", encoding="utf-8") as fh:
        fh.write(str(n + 1))
    _git(repo, "add", "counter.txt")
    proc = _git(
        repo,
        "-c", "commit.gpgsign=false",
        "-c", "user.name=commit-msg-selftest",
        "-c", "user.email=selftest@example.invalid",
        "commit", "--no-verify", "-q", "-m", message,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            "selftest could not create a fixture commit: "
            + proc.stderr.decode("utf-8", errors="replace")
        )


def selftest() -> int:
    passed = 0
    failed = 0

    def expect(cond: bool, name: str) -> None:
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"[selftest]   PASS . {name}")
        else:
            failed += 1
            print(f"[selftest]   FAIL . {name}")

    def run(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, os.path.abspath(__file__), *args],
                              capture_output=True, text=True)

    # The range has to be T5's range. The two file spell it differently - the
    # lint writes escapes, this file builds the characters from code points - so
    # comparing the two pattern STRINGS would compare nothing. Compare what they
    # match, on the boundaries where a widened or narrowed range would show up.
    probes = [chr(c) for c in (0x0041, 0x002E, 0x4DFF, 0x4E00, 0x4FFF, 0x9FFE, 0x9FFF, 0xA000)]
    expect(T5_SOURCE == "check-terminology.py RULES/T5",
           f"CJK pattern comes from the T5 rule ({T5_SOURCE})")
    expect(all(bool(re.search(_T5_PATTERN or "", p)) == bool(CJK_RE.search(p)) for p in probes),
           "the T5 pattern and the fallback here match the same characters")
    expect(not CJK_RE.search(chr(0x4DFF)) and not CJK_RE.search(chr(0xA000)),
           "the range stops where T5's stops (U+4DFF and U+A000 are outside it)")
    expect(bool(CJK_RE.search(chr(0x4E00))) and bool(CJK_RE.search(chr(0x9FFF))),
           "the range covers both ends of U+4E00..U+9FFF")

    with tempfile.TemporaryDirectory(prefix="commit-msg-selftest-") as tmp:
        # ---- --file
        cases = [
            ("english.txt", FIXTURE_ENGLISH, 0),
            ("cjk_subject.txt", FIXTURE_CJK_SUBJECT, 1),
            ("cjk_body.txt", FIXTURE_CJK_BODY, 1),
            ("cjk_comment_only.txt", FIXTURE_CJK_COMMENT_ONLY, 0),
        ]
        paths = {}
        for name, content, _ in cases:
            path = os.path.join(tmp, name)
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(content)
            paths[name] = path

        for name, _, want in cases:
            proc = run("--file", paths[name], "--repo", tmp)
            say = "stays out of the report" if want == 0 else "is rejected"
            expect(proc.returncode == want,
                   f"--file {name} exits {want} ({say}) got {proc.returncode}")
            if want == 0:
                expect("FAILED:" not in proc.stdout and "OK -" in proc.stdout,
                       f"--file {name} reports OK and names no violation")
            if name == "cjk_body.txt":
                expect(f"cjk_body.txt:3" in proc.stdout,
                       "--file names the offending line (line 3 of the body)")
            if name == "cjk_subject.txt":
                expect("FAILED:1" in proc.stdout, "--file reports exactly one violation")

        proc = run("--file", os.path.join(tmp, "does_not_exist.txt"), "--repo", tmp)
        expect(proc.returncode == 3,
               f"--file on an unreadable path exits 3, not 0 and not 1 (got {proc.returncode})")

        # ---- --range, on a throwaway repository
        repo = os.path.join(tmp, "fixture-repo")
        os.makedirs(repo)
        init = _git(repo, "init", "-q", "-b", "main")
        if init.returncode != 0:
            raise RuntimeError("selftest could not create the fixture repository: "
                               + init.stderr.decode("utf-8", errors="replace"))

        _commit(repo, FIXTURE_ENGLISH)          # A: English
        _commit(repo, FIXTURE_CJK_SUBJECT)      # B: Chinese subject
        _commit(repo, FIXTURE_CJK_BODY)         # C: English subject, Chinese body
        _commit(repo, FIXTURE_ENGLISH)          # D: English

        def rev(spec: str) -> str:
            proc = _git(repo, "rev-parse", spec)
            if proc.returncode != 0:
                raise RuntimeError(f"selftest could not resolve {spec}")
            return proc.stdout.decode("ascii").strip()

        a, b, c, d = rev("HEAD~3"), rev("HEAD~2"), rev("HEAD~1"), rev("HEAD")

        expect(run("--range", f"{a}..{b}", "--repo", repo).returncode == 1,
               "--range over a Chinese-subject commit exits 1")
        expect(f"! {b[:7]}" in run("--range", f"{a}..{b}", "--repo", repo).stdout,
               "--range names the offending commit's short SHA")
        expect(run("--range", f"{a}..{b}", "--repo", repo).stdout.find(CJK_SUBJECT) >= 0,
               "--range prints the offending subject")

        expect(run("--range", f"{b}..{d}", "--repo", repo).returncode == 1,
               "--range catches CJK in the BODY behind an English subject")
        expect(f"! {c[:7]}" in run("--range", f"{b}..{d}", "--repo", repo).stdout,
               "--range names the body-only offender too")
        expect(f"! {d[:7]}" not in run("--range", f"{b}..{d}", "--repo", repo).stdout,
               "--range does not blame the clean commit next to it")

        empty = run("--range", f"{a}..{a}", "--repo", repo)
        expect(empty.returncode == 3,
               "an empty range proves nothing and exits 3, not 0")
        expect("0 commit(s)" in empty.stderr and "nothing was proven" in empty.stderr,
               "an empty range says so on stderr instead of printing a bare OK")

        expect(run("--range", d, "--repo", repo).returncode == 0,
               "the single-revision form exits 0 for an English commit")
        expect(run("--range", b, "--repo", repo).returncode == 1,
               "the single-revision form exits 1 for a Chinese commit")
        expect(f"! {b[:7]}" in run("--range", b, "--repo", repo).stdout,
               "the single-revision form names the commit")

        expect(run("--range", "no-such-revision", "--repo", repo).returncode == 3,
               "an unresolvable revision exits 3, not 0")
        expect(run("--range", "A...B", "--repo", repo).returncode == 2,
               "'...' is rejected as a usage error rather than silently misread")
        expect(run("--range", "-1", "--repo", repo).returncode == 2,
               "a range spec that starts with '-' is rejected rather than forwarded to git as a flag")
        expect(run("--range", f"{a}..", "--repo", repo).returncode == 2,
               "a half-open range is a usage error")

        expect(run("--selftest", "--file", "x").returncode == 2,
               "combining modes is a usage error")

    print(f"[selftest] assertions: {passed} passed, {failed} failed")
    if failed:
        print("SELFTEST: FAIL - the commit-message gate is blind or over-eager")
        return 1
    print("SELFTEST: PASS - every violation caught, every near-miss left alone")
    return 0


# --------------------------------------------------------------------- main

def usage_error(message: str) -> int:
    print(f"check-commit-messages: {message}", file=sys.stderr)
    print("usage: check-commit-messages.py --file <path> [--repo <path>]", file=sys.stderr)
    print("       check-commit-messages.py --range <base>..<head> [--repo <path>]", file=sys.stderr)
    print("       check-commit-messages.py --selftest", file=sys.stderr)
    return 2


def main(argv: list[str]) -> int:
    file_path = None
    range_spec = None
    repo = REPO
    selftest_wanted = False

    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--selftest":
            selftest_wanted = True
        elif arg in ("--file", "--range", "--repo"):
            if i + 1 >= len(argv):
                return usage_error(f"{arg} needs a value")
            value = argv[i + 1]
            if arg == "--file":
                file_path = value
            elif arg == "--range":
                range_spec = value
            else:
                repo = value
            i += 1
        else:
            return usage_error(f"unknown argument: {arg}")
        i += 1

    modes = [name for name, on in
             (("--file", file_path is not None),
              ("--range", range_spec is not None),
              ("--selftest", selftest_wanted)) if on]
    if len(modes) != 1:
        return usage_error(
            "expected exactly one of --file, --range, --selftest" if modes
            else "expected one of --file, --range, --selftest"
        )

    if selftest_wanted:
        return selftest()
    if file_path is not None:
        return check_file(file_path, repo)
    return check_range(repo, range_spec)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
