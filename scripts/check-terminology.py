#!/usr/bin/env python3
"""check-terminology.py — terminology-consistency lint over user-visible strings.

The CLI is being Anglicized; the final terms are not yet frozen. So the rule
table below is deliberately a *skeleton + a few hard entries* — each rule is one
data dict, adding a rule means adding a dict, and the hard entries are the three
semantic constraints this repo will not bend on (they are spec, not style):

  T1 · absence ≠ read-failure.  "the content is not stored on this machine" must
       NOT be phrased as "failed to read / unreadable / cannot read" — those are
       reserved for genuine I/O failures (permission / corruption). The rule only
       fires when a read-failure word co-occurs with an absence marker in the SAME
       string, so "cannot read masterkey file: {e}" (real failure) stays silent.
  T2 · tri-state wording.       Unknown/known states must be spelled unknown/known.
       n/a / N/A / none / null must not mean "unknown". ("none"/"null" are only
       caught when the ENTIRE string is exactly that token, so prose "none of the
       files" never triggers.)
  T3 · unarchived wording.      "not archived" is the phrase; "not backed up" and
       "missing" (in an archive/backup context) are not. "missing" is only caught
       with a word-boundary archive-context word, so "bundle sessionId is missing"
       (a missing JSON field) is NOT an archive claim and stays silent.

Output format mirrors scripts/check-semantic-defaults.py:
    FAILED:<n>
      ! path:line [T1 absence-≠-read-failure] -> <what to use instead>
          "the offending string"

Usage:
    python3 scripts/check-terminology.py [REPO_ROOT]   # lint; exit 1 on violations
    python3 scripts/check-terminology.py --selftest    # prove the lint catches (and
                                                       # does not over-catch); exit 0/1

Exit codes: 0 = clean / selftest passed · 1 = violations found / selftest failed ·
            2 = usage error.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _user_strings as us  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


# ------------------------------------------------------------------------ rules
# Each pattern dict has either:
#   forbidden    — regex matched (case-insensitive) against the hit's single-line text
#   exact        — set of whole-string tokens (matched against the decoded string value)
#   context      — optional regex that must ALSO match for the pattern to fire
#   not_context  — optional regex that, if it matches, SUPPRESSES the pattern. This is
#                  how T3 distinguishes the forbidden "missing = 未归档" sense from the
#                  legitimate verification sense ("MISSING IN ARCHIVE" = expected by the
#                  manifest but absent — a defect report, not an archival-status claim).
RULES = [
    {
        "id": "T1",
        "name": "absence-≠-read-failure",
        "patterns": [
            {
                "forbidden": r"failed to read|unreadable|cannot read|could not read|not readable",
                "context": (
                    r"not stored|on this machine|on this host|not local|not here|"
                    r"not present|not (?:in|on) (?:the )?(?:archive|repo)"
                ),
            },
        ],
        "suggestion": (
            "『正文不在本机可读位置』≠ 读取失败。failed to read / unreadable / "
            "cannot read 只留给真实读取失败（权限/损坏）；absence 请说 not stored "
            "on this machine / not available locally。"
        ),
    },
    {
        "id": "T2",
        "name": "tri-state-unknown",
        "patterns": [
            {"forbidden": r"\bn\s*/\s*a\b"},
            {"exact": {"none", "null"}},
        ],
        "suggestion": (
            "三态措辞统一用 unknown / known。n/a、N/A、none、null 不能当『未知』讲；"
            "若 n/a 指『不适用』就拼写 not applicable，别用缩写。"
        ),
    },
    {
        "id": "T3",
        "name": "unarchived-terminology",
        "patterns": [
            {"forbidden": r"not backed up"},
            {
                "forbidden": r"\bmissing\b",
                "context": (
                    r"\b(?:archive|archives|archived|archival|backup|backups|backed|"
                    r"shard|shards)\b"
                ),
                # the verification/audit "expected but absent" sense is NOT the
                # "未归档" misuse: a session the manifest expected but the archive
                # lacks is a defect, and saying MISSING IN ARCHIVE is correct.
                "not_context": r"MISSING IN ARCHIVE|\bmissing=",
            },
        ],
        "suggestion": (
            "『未归档』统一说 not archived；不许 not backed up / missing（missing 会"
            "被误读成『文件找不到了』）。MISSING IN ARCHIVE（验证结论：清单预期但归档"
            "缺失）是另一种语义，不受本规则约束。"
        ),
    },
]


def run_rules(hits: list[us.Hit]) -> list[tuple[us.Hit, dict]]:
    """(hit, rule) pairs where a rule fired. One report per rule per hit."""
    out: list[tuple[us.Hit, dict]] = []
    for hit in hits:
        if hit.in_test:
            continue
        for rule in RULES:
            for pat in rule["patterns"]:
                if "exact" in pat:
                    fired = hit.unescaped.strip().lower() in {t.lower() for t in pat["exact"]}
                else:
                    m = re.search(pat["forbidden"], hit.text, re.IGNORECASE)
                    if m:
                        ctx = pat.get("context")
                        fired = ctx is None or re.search(ctx, hit.text, re.IGNORECASE)
                        if fired and pat.get("not_context"):
                            if re.search(pat["not_context"], hit.text, re.IGNORECASE):
                                fired = False
                    else:
                        fired = False
                if fired:
                    out.append((hit, rule))
                    break
    return out


def check(root: str) -> int:
    hits = us.extract_all(root)
    violations = run_rules(hits)
    if not violations:
        print("OK — no terminology violations")
        return 0
    print(f"FAILED:{len(violations)}")
    for hit, rule in violations:
        print(f"  ! {hit.path}:{hit.line} [{rule['id']} {rule['name']}] -> {rule['suggestion']}")
        print(f"      {hit.text}")
    return 1


# -------------------------------------------------------------------- selftest
# Follows the gate-selftest-semantic.sh pattern: inject violations, demand the
# checker catches every one; ALSO inject the near-misses that must stay silent,
# or the selftest proves nothing ("a hollow always-zero lint"). The fixtures are
# written to a temp tree and never touch real source.
FIXTURE_VIOLATING = {
    "t1_violation.rs": (
        'pub fn bad_absence() {\n'
        '    eprintln!("payload text is not readable on this machine");\n'
        '    eprintln!("session body unreadable, not stored on this host");\n'
        '}\n'
    ),
    "t2_violation.rs": (
        'pub fn bad_tri_state() {\n'
        '    println!("coverage: n/a");\n'
        '    bail!("state: N/A");\n'
        '    bail!("none");\n'
        '}\n'
    ),
    "t3_violation.rs": (
        'pub fn bad_unarchived() {\n'
        '    bail!("not backed up");\n'
        '    eprintln!("shard missing from archive");\n'
        '}\n'
    ),
    "clap_help_violation.rs": (
        '#[derive(Parser)]\n'
        'struct Args {\n'
        '    /// Retention state is n/a.\n'
        '    #[arg(long)]\n'
        '    retention: String,\n'
        '}\n'
    ),
}

# Near-misses: same words, but the OTHER meaning. The lint must stay silent on these.
FIXTURE_CLEAN = {
    "t1_clean.rs": (
        'pub fn good_read_failure() {\n'
        '    eprintln!("failed to read config file: {e}");\n'
        '    eprintln!("cannot read shard file: {e}");\n'
        '}\n'
    ),
    "t2_clean.rs": (
        'pub fn good_tri_state() {\n'
        '    println!("coverage: unknown");\n'
        '    bail!("coverage: known");\n'
        '}\n'
    ),
    "t3_clean.rs": (
        'pub fn good_unarchived() {\n'
        '    println!("missing file: {}", path);\n'
        '    bail!("bundle sessionId is missing");\n'
        '}\n'
    ),
    "clap_help_clean.rs": (
        '#[derive(Parser)]\n'
        'struct Args {\n'
        '    /// Retention state is unknown.\n'
        '    #[arg(long)]\n'
        '    retention: String,\n'
        '}\n'
    ),
}


def _write_fixtures(src_dir: str, fixtures: dict) -> None:
    for name, content in fixtures.items():
        with open(os.path.join(src_dir, name), "w", encoding="utf-8") as fh:
            fh.write(content)


def selftest() -> int:
    say = lambda msg: print(f"[selftest] {msg}")
    passed = 0
    failed = 0

    def expect(cond: bool, name: str) -> None:
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"[selftest]   PASS · {name}")
        else:
            failed += 1
            print(f"[selftest]   FAIL · {name}")

    script = os.path.abspath(__file__)

    with tempfile.TemporaryDirectory(prefix="terminology-selftest-") as tmp:
        src = os.path.join(tmp, "crates", "chat-stasher", "src")
        os.makedirs(src)

        # -- half: violating fixtures only
        _write_fixtures(src, FIXTURE_VIOLATING)
        proc = subprocess.run([sys.executable, script, tmp], capture_output=True, text=True)
        report = proc.stdout + proc.stderr
        say(f"violating tree -> exit {proc.returncode}")
        for line in report.splitlines()[:12]:
            say(f"  {line}")

        expect(proc.returncode == 1, "violating tree exits 1")
        expect("FAILED:8" in report, "violating tree reports exactly 8 violations")
        expect("t1_violation.rs" in report, "T1 fixture is named in the report")
        expect("t2_violation.rs" in report, "T2 fixture is named in the report")
        expect("t3_violation.rs" in report, "T3 fixture is named in the report")
        expect("clap_help_violation.rs" in report, "clap-help surface fixture is named in the report")
        expect("[T1 " in report and "[T2 " in report and "[T3 " in report,
               "each of T1/T2/T3 is named with its suggestion")

        # -- clean fixtures must NOT appear
        for name in FIXTURE_CLEAN:
            expect(name not in report, f"clean fixture {name} stays out of the report")

        # -- half: clean fixtures only -> must pass
        src2 = os.path.join(tmp, "clean", "crates", "chat-stasher", "src")
        os.makedirs(src2)
        _write_fixtures(src2, FIXTURE_CLEAN)
        proc2 = subprocess.run([sys.executable, script, os.path.join(tmp, "clean")],
                               capture_output=True, text=True)
        say(f"clean tree -> exit {proc2.returncode} ({proc2.stdout.strip()})")
        expect(proc2.returncode == 0, "clean tree exits 0")
        expect("OK" in proc2.stdout, "clean tree reports OK")

    # -- the real repo: the lint must not be over-eager on today's source
    proc3 = subprocess.run([sys.executable, script, REPO], capture_output=True, text=True)
    say(f"real repo -> exit {proc3.returncode}")
    for line in (proc3.stdout + proc3.stderr).splitlines()[:12]:
        say(f"  {line}")
    expect(proc3.returncode == 0, "real repo is clean under the current rules")

    print(f"[selftest] assertions: {passed} passed, {failed} failed")
    if failed:
        print("SELFTEST: FAIL — the terminology lint is blind or over-eager")
        return 1
    print("SELFTEST: PASS — every violation caught, every near-miss left alone")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) > 1:
        print("check-terminology: expected at most one argument (REPO_ROOT or --selftest)",
              file=sys.stderr)
        return 2
    if argv and argv[0] == "--selftest":
        return selftest()
    root = os.path.abspath(argv[0]) if argv else REPO
    if not os.path.isdir(os.path.join(root, "crates", "chat-stasher", "src")):
        print(f"check-terminology: {root} does not look like the repo root "
              f"(no crates/chat-stasher/src)", file=sys.stderr)
        return 2
    return check(root)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
