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
  T4 · one-word-one-meaning-reap. "reap" is only permitted in ssh master connection
        reclaim context; all stage shard-body reclamation must say reclaim.
  T5 · no-cjk-characters.       Source code and tests under crates/ and under
        apps/extension/ must not contain Chinese characters in comments or code
        strings. The extension's Chinese lives in apps/extension/locales/zh_CN.yml
        and nowhere else; T5_SCOPES below lists what is scanned and what is not.
  T6 · captured-by-this-browser-≠-saved. Decision D6 (36-EXTENSION-TOPOLOGY §5):
        the extension's surfaces may state what THIS install has confirmed only
        as "captured by this browser", the archive layer (CLI, dashboard) speaks
        "saved"/"archived" for archive contents, and the two never say each other.
        Enforced in two places: a CLI user-visible string may not claim
        "captured by this browser" (that is the extension's claim word, and a CLI
        surface showing it is the mix D6 exists to prevent), and an extension
        locale value may not word a count claim as "Archived {n}" / "saved {n}"
        (so the next "Archived 5 of 40" top line is stopped before it ships).

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
from dataclasses import dataclass

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
    {
        "id": "T4",
        "name": "one-word-one-meaning-reap",
        "patterns": [
            {
                "forbidden": r"\breap(?:ed|ing|s)?\b",
                # "reap" applied to processes is standard Unix terminology: it stays
                # legal for killing leaked ssh ControlMaster processes — but the SAME
                # string must carry an ssh/connection/master-family word to say so.
                # Any other "reap" is stage shard-body reclamation and must say reclaim.
                "not_context": (
                    r"\b(?:ssh|sftp|connection|connections|master|masters|"
                    r"control|socket)\b"
                ),
            },
        ],
        "suggestion": (
            "『reap』只许在 ssh 连接回收语境出现（同一字符串里要有 ssh / master / "
            "connection / socket 等词，否则读的人不知道是哪种 reap）。stage 分片体回收"
            "统一说 reclaim / reclaimed。若这条确实是 ssh 语义，请把 ssh / master 写明白。"
        ),
    },
    {
        "id": "T5",
        "name": "no-cjk-characters",
        "patterns": [
            {"forbidden": r"[\u4e00-\u9fff]"},
        ],
        "suggestion": (
            "crates/ 与 apps/extension/ 下源码与测试中禁止出现中文字符（注释与代码均"
            "包含）；请替换为地道、准确的英文。中文词条只允许出现在 "
            "apps/extension/locales/zh_CN.yml 里。"
        ),
    },
    {
        # 🔴 T6 · Decision D6 (`36-EXTENSION-TOPOLOGY.md` §5), landed as EXT-10.
        # Two surfaces, one boundary: an install's confirmed counts are the
        # extension's claim ("captured by this browser"); the archive's own
        # contents are the CLI's claim ("saved", "archived"). Neither may say
        # the other's word, because a user with two browsers reading the wrong
        # layer's word on the wrong surface is exactly whose misreading D6
        # records as a defect.
        "id": "T6",
        "name": "captured-by-this-browser-≠-saved",
        "patterns": [
            # The CLI half: the extracted CLI user-visible strings may never
            # carry the extension's claim phrase. Near-misses stay silent: the
            # plain word "captured" in other CLI sentences is not the phrase,
            # and tests are already excluded by the extractor's in_test flag.
            {"forbidden": r"captured by this browser"},
        ],
        # The extension half is scanned by check_d6_locales(): every
        # apps/extension/locales/*.yml value, never a comment. `Archived {…}`
        # and `saved {…}` before a placeholder are the count-claim shapes that
        # pass an install's tally off as an archive total.
        "locale_patterns": (
            r"\b(?:archived|saved)\s*\{",
            r"(?:已归档|已保存)\s*\{",
        ),
        "suggestion": (
            "D6 措辞：扩展一侧（弹窗/覆盖页/弹条）报这个实例自己的对账数，只能说 "
            "『captured by this browser』；归档层（CLI/看板）说 saved/archived。"
            "两边不许互串：CLI 字符串里不许出现 captured by this browser；扩展语料里"
            "不许再出现 『Archived {n}』『saved {n}』（中文『已归档 {n}』『已保存 {n}』）"
            "这类把本实例计数说成归档总数的说法。"
        ),
    },
]

# ------------------------------------------------------------------ T5 scope
# T5 covers two source trees, and they are different surfaces that happen to
# share one rule:
#
#   crates/          the CLI. .rs and .json, at any depth.
#   apps/extension/  the browser extension. .ts, .tsx, .json and .html.
#
# What is deliberately NOT covered, and why:
#   · .yml under apps/extension/locales — that is where the Chinese lives on
#     purpose. en.yml is a .yml too, so it is out of scope by extension and not
#     by a special case; zh_CN.yml is ALSO named explicitly below, so that if
#     anyone ever adds a locale file in a scanned format the exclusion is
#     already written down rather than discovered by a red build.
#   · node_modules, .output, .wxt and dist — installed dependencies and build
#     artifacts. They are generated from the sources that ARE scanned, so
#     scanning them would report the same fact twice and make the gate's output
#     depend on whether someone had built recently.
#   · pnpm-lock.yaml and other non-source files — not scanned formats.
T5_SCOPES = (
    {
        "dir": "crates",
        "extensions": (".rs", ".json"),
        "skip_dirs": frozenset(),
        "skip_files": frozenset(),
    },
    {
        "dir": os.path.join("apps", "extension"),
        "extensions": (".ts", ".tsx", ".json", ".html"),
        "skip_dirs": frozenset({"node_modules", ".output", ".wxt", "dist", "build", "coverage"}),
        "skip_files": frozenset({os.path.join("apps", "extension", "locales", "zh_CN.yml")}),
    },
)


@dataclass(frozen=True)
class FileLineHit:
    path: str
    line: int
    text: str


def check_cjk(root: str, rule: dict) -> list[tuple[FileLineHit, dict]]:
    """Scan the T5_SCOPES source trees for CJK characters, line by line.

    `.json` is included under crates/ because the harness registry there ships
    with the binary and some of its fields reach users; it is as much part of the
    public English surface as the source. The extension side adds `.ts`, `.tsx`
    and `.html` for the same reason — those are the popup's markup and the code
    that fills it.

    A file that cannot be read or decoded is reported, not skipped: a gate that
    treats "could not look" as "clean" is the exact failure this repository
    exists to refuse.
    """
    out: list[tuple[FileLineHit, dict]] = []
    pat = re.compile(rule["patterns"][0]["forbidden"])
    for scope in T5_SCOPES:
        base_dir = os.path.join(root, scope["dir"])
        if not os.path.isdir(base_dir):
            continue
        for dirpath, dirnames, filenames in os.walk(base_dir):
            # Pruned in place so os.walk does not descend into them at all.
            dirnames[:] = sorted(d for d in dirnames if d not in scope["skip_dirs"])
            for fname in sorted(filenames):
                if not fname.endswith(scope["extensions"]):
                    continue
                full_path = os.path.join(dirpath, fname)
                rel_path = os.path.relpath(full_path, root)
                if rel_path in scope["skip_files"]:
                    continue
                try:
                    with open(full_path, "r", encoding="utf-8") as fh:
                        for lineno, line in enumerate(fh, start=1):
                            line_content = line.rstrip("\r\n")
                            if pat.search(line_content):
                                out.append((FileLineHit(path=rel_path, line=lineno, text=line_content.strip()), rule))
                except (OSError, UnicodeDecodeError) as exc:
                    out.append((FileLineHit(path=rel_path, line=0, text=f"cannot read file: {exc}"), rule))
    return out


def check_d6_locales(root: str, rule: dict) -> list[tuple[FileLineHit, dict]]:
    """Scan every apps/extension/locales/*.yml value for the archive layer's
    count words (D6). The extension's own well-formed claim ("Captured by this
    browser: …") never matches: the pattern fires only when archived/saved sits
    immediately before a `{placeholder}`, which is the shape of a count claim
    and of nothing else in this catalog ("the speed setting could not be saved
    ({detail})" has the placeholder after a parenthesis, and keys like
    `archived: captured in full` have a colon, not a brace).

    Comments are skipped: the catalog documents its own wording decisions in
    prose above the values, including quotes of old wording, and a comment is
    not a user-visible string. A file that cannot be read or decoded is
    reported, never skipped, for the same reason as T5.
    """
    out: list[tuple[FileLineHit, dict]] = []
    locale_dir = os.path.join(root, "apps", "extension", "locales")
    if not os.path.isdir(locale_dir):
        return out
    # IGNORECASE because the en pattern must catch a sentence-leading capital
    # ("Archived {n}") as well as a mid-sentence "saved {n}"; the Chinese
    # patterns have no case and are unaffected by the flag.
    patterns = [re.compile(p, re.IGNORECASE) for p in rule.get("locale_patterns", ())]
    if not patterns:
        return out
    for fname in sorted(os.listdir(locale_dir)):
        if not fname.endswith(".yml"):
            continue
        full_path = os.path.join(locale_dir, fname)
        rel_path = os.path.relpath(full_path, root)
        try:
            with open(full_path, "r", encoding="utf-8") as fh:
                for lineno, line in enumerate(fh, start=1):
                    content = line.rstrip("\r\n")
                    if content.lstrip().startswith("#") or not content.strip():
                        continue
                    if any(pat.search(content) for pat in patterns):
                        out.append(
                            (
                                FileLineHit(
                                    path=rel_path,
                                    line=lineno,
                                    text=content.strip(),
                                ),
                                rule,
                            )
                        )
        except (OSError, UnicodeDecodeError) as exc:
            out.append(
                (FileLineHit(path=rel_path, line=0, text=f"cannot read file: {exc}"), rule)
            )
    return out


def run_rules(hits: list[us.Hit]) -> list[tuple[us.Hit, dict]]:
    """(hit, rule) pairs where a rule fired. One report per rule per hit."""
    out: list[tuple[us.Hit, dict]] = []
    for hit in hits:
        if hit.in_test:
            continue
        for rule in RULES:
            if rule["id"] == "T5":
                continue
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
    for special in ("T5", "T6"):
        rule = next((r for r in RULES if r["id"] == special), None)
        if rule is None:
            continue
        if special == "T5":
            violations.extend(check_cjk(root, rule))
        else:
            violations.extend(check_d6_locales(root, rule))
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
    "t4_violation.rs": (
        'pub fn bad_reap() {\n'
        '    println!("reap-stage: dry run reports what would be reclaimed");\n'
        '    eprintln!("the reap is blocked: nothing was deleted");\n'
        '}\n'
    ),
    "t6_violation.rs": (
        # The CLI half of D6: the archive layer never says the extension's
        # claim phrase. This is the shape of a future "--extension-counts"
        # string sneaking the install tally into a CLI surface.
        'pub fn bad_d6_cli() {\n'
        '    eprintln!("overview: captured by this browser: 3 of 30, still owed 27");\n'
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
    "t5_violation.rs": (
        '// 这是中文注释\n'
        'pub fn bad_cjk() {}\n'
    ),
    "t5_violation.json": (
        '{"display_name": "中文显示名"}\n'
    ),
}

# The extension half of T5. These live under apps/extension/, so they are written
# with their directory prefix rather than into the crates/ source directory.
FIXTURE_VIOLATING_EXTENSION = {
    os.path.join("apps", "extension", "lib", "t5_violation.ts"): (
        '// 这是扩展里的中文注释\n'
        'export const bad = 1;\n'
    ),
    os.path.join("apps", "extension", "entrypoints", "popup", "t5_violation.tsx"): (
        '// 这是 TSX 里的中文注释\n'
        'export const Bad = () => <div />;\n'
    ),
    os.path.join("apps", "extension", "entrypoints", "popup", "t5_violation.html"): (
        '<div id="status">中文</div>\n'
    ),
    # Would be caught if T5_SCOPES' extensions ever widened to .yml, so the
    # skip_files entry is exercised rather than merely documented. Today the
    # extension filter is what keeps it out; both mechanisms are asserted below.
    os.path.join("apps", "extension", "locales", "zh_CN.yml"): (
        'extDescription: 这是唯一允许出现中文的文件。\n'
    ),
}

# Near-misses on the extension side: everything here must stay out of the report.
FIXTURE_CLEAN_EXTENSION = {
    os.path.join("apps", "extension", "locales", "en.yml"): (
        'extName: Chat Stasher\n'
    ),
    os.path.join("apps", "extension", "lib", "t5_clean.ts"): (
        '// English comments only\n'
        'export const good = 1;\n'
    ),
    os.path.join("apps", "extension", "entrypoints", "popup", "t5_clean.tsx"): (
        '// English comments only\n'
        'export const Good = () => <div>Plain English</div>;\n'
    ),
    # Installed dependencies and build artifacts: excluded directories, so the
    # Chinese inside them is nobody\'s business — they are generated from the
    # sources that ARE scanned.
    os.path.join("apps", "extension", "node_modules", "dep", "t5_violation.ts"): (
        '// 依赖里的中文\n'
    ),
    os.path.join("apps", "extension", ".output", "chrome-mv3", "t5_violation.ts"): (
        '// 构建产物里的中文\n'
    ),
    os.path.join("apps", "extension", ".wxt", "t5_violation.ts"): (
        '// 生成目录里的中文\n'
    ),
}

# The D6 half of the extension fixtures: locale value violations only, in
# locale files the other fixtures do not touch (so the "near-miss stays out of
# the report" probes above keep meaning what they say). Written to the
# violating tree only; the clean tree never sees them.
FIXTURE_VIOLATING_D6_LOCALES = {
    os.path.join("apps", "extension", "locales", "de.yml"): (
        '# a comment quoting old wording is not a value: Archived {n} stays silent here\n'
        'd6_bad: "Archived {n} of {total}, still owed {pending}"\n'
    ),
    os.path.join("apps", "extension", "locales", "ja.yml"): (
        'd6_bad: 已归档 {n} 条，欠账 {pending} 条\n'
    ),
}

# D6 near-misses: the same words in the OTHER meanings must stay out of the
# report. Joined into the clean tree as locale files.
FIXTURE_CLEAN_D6_LOCALES = {
    os.path.join("apps", "extension", "locales", "de.yml"): (
        'settledLabel: Credit is due for {n} of {total} (saved progress record)\n'
        'speedNote: "The speed setting could not be saved ({detail}), so nothing changed."\n'
        'archived: captured in full\n'
        'capturedLine: "Captured by this browser: {archived} of {total} ({percent}%)"\n'
    ),
    os.path.join("apps", "extension", "locales", "ja.yml"): (
        'savedLabel: 已保存进度记录\n'
        'd6Good: "{archived} 已捕获、{pending} 待存"\n'
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
    "t4_clean.rs": (
        'pub fn good_reap() {\n'
        '    println!("[reap] ssh masters shut down: {n}");\n'
        '    eprintln!("reap: `ssh -O exit` failed");\n'
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
    "t5_clean.rs": (
        '// English comments only\n'
        'pub fn good_english() {\n'
        '    println!("pure english text");\n'
        '}\n'
    ),
    "t5_clean.json": (
        '{"display_name": "Plain English display name"}\n'
    ),
}


def _write_fixtures(src_dir: str, fixtures: dict) -> None:
    for name, content in fixtures.items():
        path = os.path.join(src_dir, name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
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
        # A file check_cjk cannot decode must be reported, not skipped as clean.
        # It is a .json on purpose: only check_cjk reads .json, so this exercises
        # that branch directly. (An undecodable .rs also stops the gate, because
        # the user-string extractor raises on it and the run exits non-zero —
        # which blocks too, but would mask what this fixture is here to prove.)
        with open(os.path.join(src, "t5_undecodable.json"), "wb") as fh:
            fh.write(b"{\"note\": \"\xff\xfe not valid utf-8\"}\n")
        _write_fixtures(tmp, FIXTURE_VIOLATING_EXTENSION)
        _write_fixtures(tmp, FIXTURE_CLEAN_EXTENSION)
        _write_fixtures(tmp, FIXTURE_VIOLATING_D6_LOCALES)
        proc = subprocess.run([sys.executable, script, tmp], capture_output=True, text=True)
        report = proc.stdout + proc.stderr
        say(f"violating tree -> exit {proc.returncode}")
        for line in report.splitlines()[:12]:
            say(f"  {line}")

        expect(proc.returncode == 1, "violating tree exits 1")
        # 14 from crates/ (the 13 plus T6's CLI fixture) + 3 from apps/extension/
        # (T5) + 2 from the locale files (T6 extension half).
        expect("FAILED:19" in report, "violating tree reports exactly 19 violations")
        expect("t1_violation.rs" in report, "T1 fixture is named in the report")
        expect("t2_violation.rs" in report, "T2 fixture is named in the report")
        expect("t3_violation.rs" in report, "T3 fixture is named in the report")
        expect("t4_violation.rs" in report, "T4 fixture is named in the report")
        expect("t5_violation.rs" in report, "T5 fixture is named in the report")
        expect("t6_violation.rs" in report, "T6 CLI fixture is named in the report")
        expect("t5_violation.json" in report, "T5 also covers .json under crates/")
        expect("d6_bad" in report, "T6 catches the count claim in a locale value")
        expect(
            "a comment quoting old wording is not a value" not in report,
            "T6 skips comments even when they quote a violating shape",
        )
        expect("t5_undecodable.json" in report and "cannot read file" in report,
               "an undecodable file is reported, not silently skipped")
        expect("clap_help_violation.rs" in report, "clap-help surface fixture is named in the report")
        expect(
            "[T1 " in report
            and "[T2 " in report
            and "[T3 " in report
            and "[T4 " in report
            and "[T5 " in report
            and "[T6 " in report,
            "each of T1/T2/T3/T4/T5/T6 is named with its suggestion",
        )

        # -- the extension half of T5
        expect(
            os.path.join("apps", "extension", "lib", "t5_violation.ts") in report,
            "T5 also covers .ts under apps/extension/",
        )
        expect(
            os.path.join("apps", "extension", "entrypoints", "popup", "t5_violation.tsx") in report,
            "T5 also covers .tsx under apps/extension/",
        )
        expect(
            os.path.join("apps", "extension", "entrypoints", "popup", "t5_violation.html") in report,
            "T5 also covers .html under apps/extension/",
        )
        # The path also appears inside T5's own suggestion text, so this looks
        # for a violation REPORT line for it rather than for the string.
        expect(
            f"  ! {os.path.join('apps', 'extension', 'locales', 'zh_CN.yml')}:" not in report,
            "the Chinese locale file is excluded from T5",
        )

        # -- clean fixtures must NOT appear
        for name in FIXTURE_CLEAN:
            expect(name not in report, f"clean fixture {name} stays out of the report")
        for name in FIXTURE_CLEAN_EXTENSION:
            expect(
                name not in report,
                f"extension near-miss {name} stays out of the report",
            )

        # -- half: clean fixtures only -> must pass
        src2 = os.path.join(tmp, "clean", "crates", "chat-stasher", "src")
        os.makedirs(src2)
        _write_fixtures(src2, FIXTURE_CLEAN)
        # The extension half of the clean tree. It deliberately includes the
        # Chinese locale file and the Chinese inside node_modules/.output/.wxt:
        # if any of those exclusions were wrong, this half would not exit 0.
        _write_fixtures(os.path.join(tmp, "clean"), FIXTURE_CLEAN_EXTENSION)
        _write_fixtures(os.path.join(tmp, "clean"), {
            path: content
            for path, content in FIXTURE_VIOLATING_EXTENSION.items()
            if path in T5_SCOPES[1]["skip_files"]
        })
        # The D6 half: the same words in their OTHER meanings, in locale files,
        # must stay out of the report the way the real catalog does.
        _write_fixtures(os.path.join(tmp, "clean"), FIXTURE_CLEAN_D6_LOCALES)
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
