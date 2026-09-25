#!/usr/bin/env python3
"""gen-support-matrix.py — render the support matrix from the two data sources.

The support matrix is a DERIVED artifact. Its two inputs are:

  * `crates/chat-stasher/data/harness-registry-v1.json` — the 12 local AI
    coding harnesses and, per harness, one cell per OS (macOS / Linux /
    Windows) with the session-path template, format, evidence confidence and
    source. This file ships inside the binary (`scanner.rs` embeds it), so what
    this script renders is what an installed `chat-stasher` will scan.
  * `apps/extension/lib/contract.ts` — the browser extension's `ALL_PLATFORMS`
    table: each web chat platform's exact origins, its release `channel`
    (`stable` / `experimental`) and its capture `credibility`. That table is the
    single source of truth for which origins the extension is built to capture
    (`apps/extension/wxt.config.ts:87` derives the content-script matches from
    it), so the matrix reads it rather than keeping a second copy.

Two renderings are emitted:

  * a short table for the README, between `<!-- support-matrix:short:start -->`
    and `<!-- support-matrix:short:end -->`;
  * a full table for the docs, between `<!-- support-matrix:full:start -->` and
    `<!-- support-matrix:full:end -->`.

Status vocabulary. The whole point of this script is that "we have a source for
the path" and "a real session was archived end to end" are TWO different facts
and must not collapse into one glyph:

    verified end-to-end (DATE)   an explicit `verified: {date, version, scope}`
                                 record on the harness — a human archived a real
                                 session on a real machine and wrote down when.
    supported                    the registry will scan this harness (at least
                                 one OS cell is source-confirmed / official-docs
                                 / measured-locally), or the extension ships the
                                 platform in its stable channel with source-backed
                                 capture. Path/source known; end-to-end not
                                 claimed.
    experimental                 registered, but enabled only in the extension's
                                 dev build (`channel: experimental`).
    uncertain (unverified)       registered on a community claim only, or the
                                 extension's capture credibility is `unverified`.
                                 Scanned/attempted, but the evidence is weaker.
    not supported                no scannable cell at all (every OS cell is
                                 `unascertained`), or the platform has no cell.

A registry cell whose confidence is `unascertained` is never rendered as
"supported": the scanner skips it, and this script says so.

Usage:
    python3 scripts/gen-support-matrix.py                    # print both tables
    python3 scripts/gen-support-matrix.py --emit short       # one of them
    python3 scripts/gen-support-matrix.py --update-fixtures  # rewrite the committed tables
    python3 scripts/gen-support-matrix.py --write-readme README.md
    python3 scripts/gen-support-matrix.py --write-docs docs-dev/support.md
    python3 scripts/gen-support-matrix.py --check            # verify committed tables
    python3 scripts/gen-support-matrix.py --selftest         # prove the check can fail

Exit codes: 0 = ok · 1 = stale / mismatch · 2 = usage or input error.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import tempfile
from typing import Any

# --------------------------------------------------------------------------
# Paths, relative to the repository root.
# --------------------------------------------------------------------------

REGISTRY_REL = os.path.join("crates", "chat-stasher", "data", "harness-registry-v1.json")
CONTRACT_REL = os.path.join("apps", "extension", "lib", "contract.ts")
SHORT_FIXTURE_REL = os.path.join("scripts", "support-matrix", "short-table.md")
FULL_FIXTURE_REL = os.path.join("scripts", "support-matrix", "full-table.md")

SHORT_START = "<!-- support-matrix:short:start -->"
SHORT_END = "<!-- support-matrix:short:end -->"
FULL_START = "<!-- support-matrix:full:start -->"
FULL_END = "<!-- support-matrix:full:end -->"

# The CLI flag that rewrites each artifact. The failure texts in `check` name
# these, and the self-test reads its own failure texts back and asserts every
# flag they mention is one argparse accepts — a stale short block once told the
# reader to run `--write-short`, which the CLI does not define, so the advice
# was a dead end. Derived artifacts and the advice about them come from one
# place or the advice goes wrong on its own.
FIXTURE_FLAG = "--update-fixtures"
WRITE_FLAG = {"short": "--write-readme", "full": "--write-docs"}

OS_ORDER = ("macos", "linux", "windows")
OS_LABEL = {"macos": "macOS", "linux": "Linux", "windows": "Windows"}

SCANNABLE_CONFIDENCE = {"source-confirmed", "official-docs", "measured-locally"}
COMMUNITY_CONFIDENCE = {"community-claim-unverified"}
VALID_CHANNELS = {"stable", "experimental"}
VALID_CREDIBILITY = {"from-source", "unverified"}

STATUS_VERIFIED = "verified end-to-end"
STATUS_SUPPORTED = "supported"
STATUS_EXPERIMENTAL = "experimental"
STATUS_UNCERTAIN = "uncertain (unverified)"
STATUS_UNSUPPORTED = "not supported"


class SupportMatrixError(Exception):
    """A problem that makes the matrix unrenderable — never guessed past."""


def repo_root_containing(path: str) -> str:
    return os.path.dirname(os.path.dirname(os.path.abspath(path)))


def default_root() -> str:
    return repo_root_containing(__file__)


# --------------------------------------------------------------------------
# Reading the two sources
# --------------------------------------------------------------------------


def load_harnesses(root: str) -> list[dict[str, Any]]:
    path = os.path.join(root, REGISTRY_REL)
    if not os.path.isfile(path):
        raise SupportMatrixError(f"harness registry not found: {REGISTRY_REL}")
    with open(path, "r", encoding="utf-8") as fh:
        try:
            data = json.load(fh)
        except json.JSONDecodeError as exc:
            raise SupportMatrixError(f"{REGISTRY_REL} is not valid JSON: {exc}") from exc
    harnesses = data.get("harnesses")
    if not isinstance(harnesses, list) or not harnesses:
        raise SupportMatrixError(f"{REGISTRY_REL} has no `harnesses` list")
    out: list[dict[str, Any]] = []
    for h in harnesses:
        hid = h.get("id")
        if not isinstance(hid, str) or not hid:
            raise SupportMatrixError(f"{REGISTRY_REL}: a harness has no string `id`")
        out.append(h)
    return out


_CONTRACT_ID_RE = re.compile(r"^\s+id:\s*'([^']+)',\s*$")
_CONTRACT_ORIGINS_RE = re.compile(r"^\s+origins:\s*\[([^\]]*)\],?\s*$")
_CONTRACT_CHANNEL_RE = re.compile(r"^\s+channel:\s*'([^']+)',\s*$")
_CONTRACT_CRED_RE = re.compile(r"^\s+credibility:\s*'([^']+)',\s*$")
_QUOTED_RE = re.compile(r"'([^']*)'")


def parse_contract_platforms(root: str) -> list[dict[str, Any]]:
    """Parse `ALL_PLATFORMS` out of the extension's contract.ts.

    The contract is TypeScript, and a full parser is out of scope. The table's
    shape is stable and load-bearing though: each platform object opens with
    `id: '<x>',` at four-space indent and carries exactly one `channel:` and one
    `credibility:` before the next object. This reads those lines and then
    verifies its own work — a table that no longer matches the shape is an error
    here, never a silent partial matrix.
    """
    path = os.path.join(root, CONTRACT_REL)
    if not os.path.isfile(path):
        raise SupportMatrixError(f"extension contract not found: {CONTRACT_REL}")
    with open(path, "r", encoding="utf-8") as fh:
        lines = fh.read().splitlines()

    marker = "export const ALL_PLATFORMS"
    start = next((i for i, ln in enumerate(lines) if marker in ln), None)
    if start is None:
        raise SupportMatrixError(f"{CONTRACT_REL}: no `{marker}` declaration found")
    end = next((i for i in range(start + 1, len(lines)) if lines[i].strip() == "];"), None)
    if end is None:
        raise SupportMatrixError(f"{CONTRACT_REL}: `ALL_PLATFORMS` array is not closed")

    platforms: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    for raw in lines[start + 1 : end]:
        stripped = raw.lstrip()
        if stripped.startswith("//"):
            continue
        m_id = _CONTRACT_ID_RE.match(raw)
        if m_id:
            current = {"id": m_id.group(1), "origins": [], "channel": None, "credibility": None}
            platforms.append(current)
            continue
        if current is None:
            continue
        m_origins = _CONTRACT_ORIGINS_RE.match(raw)
        if m_origins:
            current["origins"] = _QUOTED_RE.findall(m_origins.group(1))
            continue
        m_channel = _CONTRACT_CHANNEL_RE.match(raw)
        if m_channel:
            current["channel"] = m_channel.group(1)
            continue
        m_cred = _CONTRACT_CRED_RE.match(raw)
        if m_cred:
            current["credibility"] = m_cred.group(1)
            continue

    if not platforms:
        raise SupportMatrixError(f"{CONTRACT_REL}: parsed no platforms from `ALL_PLATFORMS`")
    for p in platforms:
        if not p["origins"]:
            raise SupportMatrixError(f"{CONTRACT_REL}: platform `{p['id']}` has no `origins`")
        if p["channel"] is None:
            raise SupportMatrixError(f"{CONTRACT_REL}: platform `{p['id']}` has no `channel`")
        if p["credibility"] is None:
            raise SupportMatrixError(f"{CONTRACT_REL}: platform `{p['id']}` has no `credibility`")
        if p["channel"] not in VALID_CHANNELS:
            raise SupportMatrixError(
                f"{CONTRACT_REL}: platform `{p['id']}` has unknown channel `{p['channel']}`"
            )
        if p["credibility"] not in VALID_CREDIBILITY:
            raise SupportMatrixError(
                f"{CONTRACT_REL}: platform `{p['id']}` has unknown credibility `{p['credibility']}`"
            )
    return platforms


def first_url(*candidates: str) -> str:
    for text in candidates:
        if not text:
            continue
        m = re.search(r"https?://[^\s()\"'<>]+", text)
        if m:
            return m.group(0).rstrip(".,;:")
    return ""


# --------------------------------------------------------------------------
# Status derivation
# --------------------------------------------------------------------------


def harness_verified(h: dict[str, Any]) -> dict[str, Any] | None:
    v = h.get("verified")
    if isinstance(v, dict) and isinstance(v.get("date"), str) and v["date"]:
        return v
    return None


def cell_status(cell: dict[str, Any] | None, verified: dict[str, Any] | None) -> str:
    if verified is not None:
        return f"{STATUS_VERIFIED} ({verified['date']})"
    if cell is None:
        return STATUS_UNSUPPORTED
    confidence = cell.get("confidence", "")
    if confidence in SCANNABLE_CONFIDENCE:
        return STATUS_SUPPORTED
    if confidence in COMMUNITY_CONFIDENCE:
        return STATUS_UNCERTAIN
    return STATUS_UNSUPPORTED


_STATUS_RANK = {
    "not supported": 0,
    "uncertain": 1,
    "supported": 2,
    "experimental": 3,
    "verified end-to-end": 4,
}


def status_rank(status: str) -> int:
    base = status.split(" (", 1)[0]
    return _STATUS_RANK.get(base, 0)


def harness_status(h: dict[str, Any]) -> str:
    verified = harness_verified(h)
    if verified is not None:
        return f"{STATUS_VERIFIED} ({verified['date']})"
    paths = h.get("paths") or {}
    best = STATUS_UNSUPPORTED
    for os_name in OS_ORDER:
        cell = paths.get(os_name)
        st = cell_status(cell, None)
        if status_rank(st) > status_rank(best):
            best = st
    return best


def web_status(p: dict[str, Any]) -> str:
    if p["channel"] == "experimental":
        return STATUS_EXPERIMENTAL
    if p["credibility"] == "unverified":
        return STATUS_UNCERTAIN
    return STATUS_SUPPORTED


def verified_date(status: str) -> str:
    m = re.search(r"\(([^)]+)\)$", status)
    return m.group(1) if m and status.startswith(STATUS_VERIFIED) else "—"


# --------------------------------------------------------------------------
# Rendering
# --------------------------------------------------------------------------


def esc(text: Any) -> str:
    return str(text).replace("|", "\\|").replace("\n", " ")


def code(text: Any) -> str:
    return "`" + str(text).replace("`", "'") + "`"


def render_short(harnesses: list[dict[str, Any]], platforms: list[dict[str, Any]]) -> str:
    out: list[str] = []
    out.append("**5+ platforms.** Local AI coding tools and web chats, archived the same way.")
    out.append("")
    out.append("| Surface | Platform | Status | Last verified |")
    out.append("|---|---|---|---|")
    for h in harnesses:
        st = harness_status(h)
        out.append(
            f"| Local | {esc(h.get('display_name') or h['id'])} | {esc(st)} | {esc(verified_date(st))} |"
        )
    for p in platforms:
        st = web_status(p)
        out.append(f"| Web | {esc(p['id'])} | {esc(st)} | {esc(verified_date(st))} |")
    out.append("")
    out.append(
        "Verified means a maintainer archived a real session end to end on their own machine. "
        "Formats change, so a date is recorded instead of a permanent check."
    )
    return "\n".join(out) + "\n"


def render_full(harnesses: list[dict[str, Any]], platforms: list[dict[str, Any]]) -> str:
    out: list[str] = []
    out.append("### Local AI coding tools")
    out.append("")
    out.append("| Harness | OS | Session path template | Format | Confidence | Status | Source |")
    out.append("|---|---|---|---|---|---|---|")
    for h in harnesses:
        name = h.get("display_name") or h["id"]
        verified = harness_verified(h)
        paths = h.get("paths") or {}
        ref = (h.get("reference") or {}).get("source_url", "")
        for os_name in OS_ORDER:
            cell = paths.get(os_name)
            status = cell_status(cell, verified)
            if cell is None:
                out.append(
                    f"| {esc(name)} | {OS_LABEL[os_name]} | — | — | — | {esc(status)} | — |"
                )
                continue
            source = first_url(cell.get("source", "")) or first_url(ref)
            out.append(
                "| {name} | {os} | {template} | {fmt} | {conf} | {status} | {src} |".format(
                    name=esc(name),
                    os=OS_LABEL[os_name],
                    template=code(cell.get("template", "")),
                    fmt=esc(cell.get("format", "—") or "—"),
                    conf=esc(cell.get("confidence", "—") or "—"),
                    status=esc(status),
                    src=esc(source or "—"),
                )
            )
    out.append("")
    out.append("### Web AI chats (browser extension)")
    out.append("")
    out.append("| Platform | Origins | Channel | Capture credibility | Status |")
    out.append("|---|---|---|---|---|")
    for p in platforms:
        out.append(
            "| {id} | {origins} | {channel} | {cred} | {status} |".format(
                id=esc(p["id"]),
                origins=esc(", ".join(p["origins"])),
                channel=esc(p["channel"]),
                cred=esc(p["credibility"]),
                status=esc(web_status(p)),
            )
        )
    out.append("")
    out.append(
        "`supported` means the path or route has a source and the scanner/extension "
        "will act on it; `verified end-to-end` additionally means a real session was "
        "archived on a real machine and the date is recorded. `unascertained` cells are "
        "not scanned and are rendered as `not supported`."
    )
    return "\n".join(out) + "\n"


def render(kind: str, root: str) -> str:
    harnesses = load_harnesses(root)
    platforms = parse_contract_platforms(root)
    if kind == "short":
        return render_short(harnesses, platforms)
    if kind == "full":
        return render_full(harnesses, platforms)
    raise SupportMatrixError(f"unknown rendering kind: {kind}")


# --------------------------------------------------------------------------
# Marker handling
# --------------------------------------------------------------------------


def extract_between(text: str, start_marker: str, end_marker: str) -> tuple[str | None, str | None]:
    """Return (content, problem). Exactly one is non-None.

    A start marker without an end marker is a problem, not a no-op: half a
    marker pair is a table that is written but not checked.
    """
    has_start = start_marker in text
    has_end = end_marker in text
    if not has_start and not has_end:
        return None, None
    if has_start != has_end:
        missing = end_marker if has_start else start_marker
        return None, f"marker pair is incomplete (missing `{missing}`)"
    if text.count(start_marker) != 1 or text.count(end_marker) != 1:
        return None, "marker appears more than once; refusing to guess which block is the table"
    start = text.index(start_marker) + len(start_marker)
    end = text.index(end_marker)
    if end < start:
        return None, "end marker precedes start marker"
    return text[start:end], None


def replace_between(text: str, start_marker: str, end_marker: str, content: str) -> str:
    start = text.index(start_marker) + len(start_marker)
    end = text.index(end_marker)
    if end < start:
        raise SupportMatrixError("end marker precedes start marker")
    return text[:start] + "\n" + content.strip("\n") + "\n" + text[end:]


def normalize(text: str) -> str:
    return "\n".join(line.rstrip() for line in text.strip("\n").split("\n"))


def markdown_docs(root: str) -> list[str]:
    docs = ["README.md"]
    docs_dir = os.path.join(root, "docs-dev")
    if os.path.isdir(docs_dir):
        for name in sorted(os.listdir(docs_dir)):
            if name.endswith(".md"):
                docs.append(os.path.join("docs-dev", name))
    return docs


def check(root: str) -> tuple[list[str], list[str]]:
    """Return (failures, notes). A failure is always an exit-1 condition."""
    failures: list[str] = []
    notes: list[str] = []
    expected = {"short": render("short", root), "full": render("full", root)}

    fixtures = {"short": SHORT_FIXTURE_REL, "full": FULL_FIXTURE_REL}
    for kind, rel in fixtures.items():
        path = os.path.join(root, rel)
        if not os.path.isfile(path):
            failures.append(f"{rel}: committed table is missing (run {FIXTURE_FLAG})")
            continue
        with open(path, "r", encoding="utf-8") as fh:
            actual = fh.read()
        if normalize(actual) != normalize(expected[kind]):
            failures.append(
                f"{rel}: committed {kind} table is stale — regenerate with {FIXTURE_FLAG}"
            )

    markers = {
        "short": (SHORT_START, SHORT_END),
        "full": (FULL_START, FULL_END),
    }
    found_any = False
    for rel in markdown_docs(root):
        path = os.path.join(root, rel)
        if not os.path.isfile(path):
            continue
        with open(path, "r", encoding="utf-8") as fh:
            text = fh.read()
        for kind, (start_marker, end_marker) in markers.items():
            content, problem = extract_between(text, start_marker, end_marker)
            if problem is not None:
                failures.append(f"{rel}: {kind} {problem}")
                continue
            if content is None:
                continue
            found_any = True
            if normalize(content) != normalize(expected[kind]):
                failures.append(
                    f"{rel}: {kind} table between markers is stale — regenerate "
                    f"with {WRITE_FLAG[kind]}"
                )
    if not found_any:
        notes.append(
            "no support-matrix markers in README.md or docs-dev/*.md yet; "
            "the committed fixtures above are the gate"
        )
    return failures, notes


def run_check(root: str) -> int:
    try:
        failures, notes = check(root)
    except SupportMatrixError as exc:
        print(f"[support-matrix] cannot render the matrix: {exc}", file=sys.stderr)
        return 2
    for note in notes:
        print(f"[support-matrix] note: {note}")
    if failures:
        print(f"[support-matrix] FAILED: {len(failures)} stale table(s)", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        print(
            "[support-matrix] the registry or the extension contract changed; "
            "re-derive the tables deliberately (--update-fixtures / --write-*).",
            file=sys.stderr,
        )
        return 1
    print("[support-matrix] OK: committed tables match the registry and the extension contract")
    return 0


# --------------------------------------------------------------------------
# Writing
# --------------------------------------------------------------------------


def update_fixtures(root: str) -> int:
    try:
        expected = {"short": render("short", root), "full": render("full", root)}
    except SupportMatrixError as exc:
        print(f"[support-matrix] cannot render the matrix: {exc}", file=sys.stderr)
        return 2
    mapping = {"short": SHORT_FIXTURE_REL, "full": FULL_FIXTURE_REL}
    for kind, rel in mapping.items():
        path = os.path.join(root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(expected[kind])
        print(f"[support-matrix] wrote {rel}")
    return 0


def write_into_file(root: str, rel_path: str, kind: str) -> int:
    start_marker, end_marker, fixture_kind = {
        "short": (SHORT_START, SHORT_END, "short"),
        "full": (FULL_START, FULL_END, "full"),
    }[kind]
    path = os.path.join(root, rel_path)
    if not os.path.isfile(path):
        print(f"[support-matrix] refusing to create {rel_path}; the file must exist", file=sys.stderr)
        return 2
    with open(path, "r", encoding="utf-8") as fh:
        text = fh.read()
    if start_marker not in text or end_marker not in text:
        print(
            f"[support-matrix] {rel_path} has no `{start_marker}` / `{end_marker}` pair; "
            "add the markers first, this tool will not create documents",
            file=sys.stderr,
        )
        return 2
    try:
        content = render(fixture_kind, root)
    except SupportMatrixError as exc:
        print(f"[support-matrix] cannot render the matrix: {exc}", file=sys.stderr)
        return 2
    new_text = replace_between(text, start_marker, end_marker, content)
    if new_text == text:
        print(f"[support-matrix] {rel_path}: already up to date")
        return 0
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(new_text)
    print(f"[support-matrix] updated {kind} table in {rel_path}")
    return 0


# --------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------

_SELFTEST_REGISTRY = {
    "schema_version": 1,
    "generated": "2026-01-01",
    "harnesses": [
        {
            "id": "alpha",
            "display_name": "Alpha",
            "paths": {
                "macos": {
                    "template": "~/.alpha/<uuid>.jsonl",
                    "format": "jsonl",
                    "confidence": "source-confirmed",
                    "source": "https://example.com/alpha",
                }
            },
        },
        {
            "id": "beta",
            "display_name": "Beta",
            "paths": {
                "macos": {
                    "template": "~/Library/Beta/sessions",
                    "format": "sqlite",
                    "confidence": "community-claim-unverified",
                    "source": "community report",
                }
            },
        },
        {
            "id": "gamma",
            "display_name": "Gamma",
            "paths": {
                "macos": {
                    "template": "~/.gamma",
                    "format": "jsonl",
                    "confidence": "unascertained",
                    "source": "not read",
                }
            },
        },
        {
            "id": "delta",
            "display_name": "Delta",
            "verified": {"date": "2026-09", "version": "1.2.3", "scope": "one real session"},
            "paths": {
                "macos": {
                    "template": "~/.delta/<id>.jsonl",
                    "format": "jsonl",
                    "confidence": "source-confirmed",
                    "source": "https://example.com/delta",
                }
            },
        },
    ],
}

_SELFTEST_CONTRACT = """\
export const ALL_PLATFORMS: readonly ChatPlatform[] = [
  {
    id: 'p-stable',
    origins: ['https://stable.example'],
    credibility: 'from-source',
    channel: 'stable',
  },
  {
    id: 'p-exp',
    origins: ['https://exp.example'],
    credibility: 'from-source',
    channel: 'experimental',
  },
  {
    id: 'p-unver',
    origins: ['https://unver.example'],
    credibility: 'unverified',
    channel: 'stable',
  },
];
"""


def _scaffold(root: str, registry: dict[str, Any] | None = None, contract: str | None = None) -> None:
    reg_path = os.path.join(root, REGISTRY_REL)
    os.makedirs(os.path.dirname(reg_path), exist_ok=True)
    with open(reg_path, "w", encoding="utf-8") as fh:
        json.dump(registry if registry is not None else _SELFTEST_REGISTRY, fh, indent=2)
    con_path = os.path.join(root, CONTRACT_REL)
    os.makedirs(os.path.dirname(con_path), exist_ok=True)
    with open(con_path, "w", encoding="utf-8") as fh:
        fh.write(contract if contract is not None else _SELFTEST_CONTRACT)


def selftest() -> int:
    failures = 0
    total = 0

    def probe(name: str, ok: bool) -> None:
        nonlocal failures, total
        total += 1
        if ok:
            print(f"[selftest]   PASS · {name}")
        else:
            print(f"[selftest]   FAIL · {name}")
            failures += 1

    with tempfile.TemporaryDirectory(prefix="support-matrix-selftest-") as tmp:
        _scaffold(tmp)

        harnesses = load_harnesses(tmp)
        platforms = parse_contract_platforms(tmp)
        probe("registry parsed", [h["id"] for h in harnesses] == ["alpha", "beta", "gamma", "delta"])
        probe("contract parsed", [p["id"] for p in platforms] == ["p-stable", "p-exp", "p-unver"])
        probe(
            "a URL followed by a parenthetical is extracted without the parenthetical",
            first_url("https://example.com/epsilon(original text: x)") == "https://example.com/epsilon",
        )

        by_id = {h["id"]: h for h in harnesses}
        probe("source-confirmed cell is supported", harness_status(by_id["alpha"]) == STATUS_SUPPORTED)
        probe("community-only cell is uncertain", harness_status(by_id["beta"]) == STATUS_UNCERTAIN)
        probe("all-unascertained cell is not supported", harness_status(by_id["gamma"]) == STATUS_UNSUPPORTED)
        probe(
            "verified record wins and carries its date",
            harness_status(by_id["delta"]) == f"{STATUS_VERIFIED} (2026-09)",
        )
        web = {p["id"]: p for p in platforms}
        probe("stable + from-source is supported", web_status(web["p-stable"]) == STATUS_SUPPORTED)
        probe("experimental channel is experimental", web_status(web["p-exp"]) == STATUS_EXPERIMENTAL)
        probe("unverified credibility is uncertain", web_status(web["p-unver"]) == STATUS_UNCERTAIN)

        short = render_short(harnesses, platforms)
        full = render_full(harnesses, platforms)
        probe("short table carries the fixed 5+ platforms headline", "5+ platforms" in short)
        probe("short table has no generated count in prose", not re.search(r"\b\d+ (?:platform|harness|tool)", short))
        probe("short table renders every status word", all(
            s in short for s in (STATUS_SUPPORTED, STATUS_UNCERTAIN, STATUS_UNSUPPORTED, STATUS_EXPERIMENTAL)
        ))
        probe("full table carries a path template", "~/.alpha/<uuid>.jsonl" in full)
        probe("full table carries the verified date", "(2026-09)" in full)

        # --update-fixtures makes a fresh checkout pass the check.
        probe("fixtures update cleanly", update_fixtures(tmp) == 0)
        probe("fresh fixtures pass the check", run_check(tmp) == 0)

        # A stale committed table must fail.
        short_path = os.path.join(tmp, SHORT_FIXTURE_REL)
        with open(short_path, "r", encoding="utf-8") as fh:
            original_short = fh.read()
        with open(short_path, "w", encoding="utf-8") as fh:
            fh.write(original_short.replace("supported", "SUPPORTED", 1))
        probe("a corrupted fixture fails the check", run_check(tmp) == 1)
        with open(short_path, "w", encoding="utf-8") as fh:
            fh.write(original_short)

        # Changing the input without regenerating must fail.
        changed = json.loads(json.dumps(_SELFTEST_REGISTRY))
        changed["harnesses"][0]["paths"]["macos"]["template"] = "~/.alpha/changed/<uuid>.jsonl"
        _scaffold(tmp, registry=changed)
        probe("a changed registry fails the check", run_check(tmp) == 1)
        _scaffold(tmp)

        # Markers in a document are checked, and a half pair is loud.
        readme = os.path.join(tmp, "README.md")
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\n{render_short(harnesses, platforms).strip()}\n{SHORT_END}\n")
        probe("a correct in-document block passes", run_check(tmp) == 0)
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\nwrong\n{SHORT_END}\n")
        probe("a stale in-document block fails", run_check(tmp) == 1)
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\nno end marker\n")
        probe("a half marker pair fails loudly", run_check(tmp) == 1)

        # --write-* refuses to create or guess.
        probe("write refuses when markers are absent", write_into_file(tmp, "README.md", "short") == 2)
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\nold\n{SHORT_END}\n")
        probe("write replaces between markers", write_into_file(tmp, "README.md", "short") == 0)
        probe("write result passes the check", run_check(tmp) == 0)
        probe("write refuses a missing file", write_into_file(tmp, "NOPE.md", "short") == 2)

        # Malformed inputs must fail loudly, never render a partial matrix.
        _scaffold(tmp, contract="export const ALL_PLATFORMS: readonly ChatPlatform[] = [\n"
                                 "  {\n    id: 'p',\n    origins: ['https://x.example'],\n  },\n];\n")
        probe("a platform with no channel is an error", run_check(tmp) == 2)
        os.remove(os.path.join(tmp, REGISTRY_REL))
        probe("a missing registry is an error", run_check(tmp) == 2)

        # Advice a failure text prints is only advice if the CLI accepts it.
        # Stale the fixtures, a short block and a full block at once, so the
        # probe sees every remediation message `check` can emit, then read the
        # flags back out of those messages and hold them against argparse. This
        # is the guard for the day one of them named a flag that does not exist.
        _scaffold(tmp)
        update_fixtures(tmp)
        with open(short_path, "w", encoding="utf-8") as fh:
            fh.write("stale\n")
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\nwrong\n{SHORT_END}\n")
        os.makedirs(os.path.join(tmp, "docs-dev"), exist_ok=True)
        with open(os.path.join(tmp, "docs-dev", "support.md"), "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{FULL_START}\nwrong\n{FULL_END}\n")
        printed_flags = {
            flag for text in check(tmp)[0] for flag in re.findall(r"--[a-z][a-z-]*", text)
        }
        probe(
            "the stale-text probes reached every remediation flag",
            printed_flags == {FIXTURE_FLAG, WRITE_FLAG["short"], WRITE_FLAG["full"]},
        )
        probe(
            "every remediation flag the check prints is a flag the CLI accepts",
            printed_flags <= cli_option_strings(),
        )

    if failures:
        print(f"[selftest] FAILED: {failures}/{total} probe(s) did not hold")
        return 1
    print(f"[selftest] PASS: all {total} probes held")
    return 0


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    """The CLI, in one place: `main` runs it, and the self-test asks it which
    flags exist rather than trusting a second list of them to stay true."""
    ap = argparse.ArgumentParser(description="Render and check the chat-stasher support matrix")
    ap.add_argument("--root", default=default_root(), help="repository root (default: this checkout)")
    ap.add_argument("--emit", choices=["short", "full", "both"], default="both")
    ap.add_argument("--update-fixtures", action="store_true", help="rewrite the committed tables")
    ap.add_argument("--write-readme", metavar="PATH", help="replace the short block in this file")
    ap.add_argument("--write-docs", metavar="PATH", help="replace the full block in this file")
    ap.add_argument("--check", action="store_true", help="verify committed tables; exit 1 if stale")
    ap.add_argument("--selftest", action="store_true", help="prove the check can fail")
    return ap


def cli_option_strings() -> set[str]:
    """Every flag the CLI accepts, asked of the parser itself."""
    return {s for action in build_parser()._actions for s in action.option_strings}


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)

    root = os.path.abspath(args.root)
    try:
        if args.selftest:
            return selftest()
        if args.update_fixtures:
            return update_fixtures(root)
        if args.write_readme:
            return write_into_file(root, args.write_readme, "short")
        if args.write_docs:
            return write_into_file(root, args.write_docs, "full")
        if args.check:
            return run_check(root)
        kinds = ["short", "full"] if args.emit == "both" else [args.emit]
        for kind in kinds:
            sys.stdout.write(render(kind, root))
            if len(kinds) > 1:
                sys.stdout.write("\n")
        return 0
    except SupportMatrixError as exc:
        print(f"[support-matrix] {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
