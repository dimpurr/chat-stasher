#!/usr/bin/env python3
"""gen-support-matrix.py — render the support matrix from the two data sources.

The support matrix is a DERIVED artifact. Its two inputs are:

  * `crates/chat-stasher/data/harness-registry-v1.json` — the 12 local AI
    coding harnesses and, per harness, one cell per OS (macOS / Linux /
    Windows) with the session-path template, format, evidence confidence and
    source; plus the `browsers` section: one row per browser the native host
    can register, with one tier cell per OS. The harness cells ship inside the
    binary (`scanner.rs` embeds the file), and the browser rows are what the
    binary's `Browser` enum enacts (`nativehost.rs`; the agreement is pinned
    by a test in the Rust suite, so a row here cannot claim a tier the build
    does not report). This file therefore renders what an installed
    `chat-stasher` will scan and register.
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

Editorial overlay (SB-1). Both sources also carry three hand-written fields
per row — one vocabulary under each spelling — and all three are validated by
this script before any table is rendered from them:

    `verified` (harnesses) /    `{date, version?, scope}`. Recorded only from
    `lastVerified` (platforms)  dated public evidence that a real conversation
                               was archived end to end on a real machine: a
                               commit message, a release note, or a document in
                               this repository. NEVER inferred from `confidence`
                               or `credibility`, which say where the routes were
                               read from, not that an archive ever ran. Absent
                               means no such run is recorded, and renders as
                               ABSENT — not as "never worked".
    `dev_priority` /           one of `high` / `normal` / `low`: an editorial
    `devPriority`              statement of where maintainer attention is,
                               written by hand, never derived from capture
                               behaviour.
    `known_issue` /            one short public-safe caveat plus a pointer to
    `knownIssue`               where it is tracked (an issue number, a commit
                               hash, or a repository document). Never a private
                               path, never a private number.

Freshness rule (ADR-041, decision 7). A recorded verification date that is
more than 90 days old renders as `needs re-check (DATE)` in the Last verified
column, and nothing else changes: the Status column keeps the historical fact
that a verification happened. Because every table is a derived artifact, the
day a recorded date crosses the threshold this check itself goes red ("stale
table") until the tables are re-derived — that red is the expiry signal
working: re-deriving bakes the `needs re-check` marker into the public tables,
and only a new dated verification (or 90 days of it, now freshly recorded)
turns the row back into a plain date. Silencing it by deleting the date is the
one forbidden move.

The browser × OS section carries its own tier vocabulary, because a browser
tier answers a different question than a platform status ("can the native
host register this browser on this OS?"), and collapsing the two vocabularies
would let one borrow the other's promise:

    supported                    the promised tier. The discovery path, and on
                                 Windows the registry key, is documented, and
                                 `install-native-host` registers the pair.
    unverified                   the best-effort tier. Registration is
                                 attempted and reported, never promised: this
                                 marks a browser outside the supported list, a
                                 discovery path with no primary vendor source,
                                 or a Windows build with no registry key known.
    no native build              the browser itself ships no build for this OS,
                                 so there is nothing to register. A fact about
                                 the browser's distribution, never a judgement
                                 by this tool.

A registry cell whose confidence is `unascertained` is never rendered as
"supported": the scanner skips it, and this script says so. The same honesty
applies to the browser sections: a `browsers` key that is missing, or a tier
slug outside the vocabulary above, is an error and never a guess.

Absent values. A cell field with nothing recorded — no verification date, no
format, no source URL — renders as a single plain ASCII hyphen, `ABSENT` below.
It is a placeholder and nothing more: it is NOT a status, and it never stands in
for a status word. Two reasons it is not an em dash. The table is data copied
into other documents, so the glyph has to survive whatever font and pipeline it
lands in, and U+2014 does not exist in every one of them. And an em dash is
punctuation; used as a field value it reads as prose to a reader and is not
something anyone can grep for. The README's own prose calls this mark "a dash",
which is the word for the glyph this emits.

Usage:
    python3 scripts/gen-support-matrix.py                    # print both tables
    python3 scripts/gen-support-matrix.py --emit short       # one of them
    python3 scripts/gen-support-matrix.py --emit full        # ... or the other
    python3 scripts/gen-support-matrix.py --update-fixtures  # rewrite the committed tables
    python3 scripts/gen-support-matrix.py --write-readme README.md
    python3 scripts/gen-support-matrix.py --write-docs docs/support.md
    python3 scripts/gen-support-matrix.py --check            # verify committed tables
    python3 scripts/gen-support-matrix.py --selftest         # prove the check can fail

Exit codes: 0 = ok · 1 = stale / mismatch · 2 = usage or input error.
"""

from __future__ import annotations

import argparse
from datetime import date
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

# The freshness threshold of a recorded verification (ADR-041, decision 7). A
# date older than this many days renders prefixed with RECHECK_LABEL in the
# Last verified column, and the gate goes red until the tables are re-derived.
# See the module docstring for why that red is the mechanism, not a failure
# of it. "Older than" is strict: a date exactly RECHECK_DAYS old still renders
# as a plain date, so the threshold is testable on both sides of one day.
RECHECK_DAYS = 90
RECHECK_LABEL = "needs re-check"

# The editorial priority vocabulary. A closed set, so a typo in the data is an
# error, never a value that renders as if it meant something.
VALID_DEV_PRIORITY = frozenset({"high", "normal", "low"})

# A recorded verification date is a full ISO day. A month ("2026-09") cannot
# answer "is this older than 90 days", so it is not accepted; if a weaker
# record is all the evidence there is, the row simply carries no verified
# record until a dated one exists.
VERIFIED_DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")

# Browser tiers, from the registry's `browsers` section. These answer "can the
# native host register this browser on this OS?" and are deliberately their own
# vocabulary, not a reuse of the platform statuses above: a browser's cell is a
# promise about registration, while a platform's row is a claim about capture,
# and a shared word would let one borrow the other's promise. The slugs are the
# ones `doctor` and the Rust pin test speak (`Support::id` plus the explicit
# third state for "this browser ships no build for that OS").
BROWSER_TIER_SUPPORTED = "supported"
BROWSER_TIER_UNVERIFIED = "unverified"
BROWSER_TIER_NO_NATIVE_BUILD = "no-native-build"
BROWSER_TIERS = {
    BROWSER_TIER_SUPPORTED,
    BROWSER_TIER_UNVERIFIED,
    BROWSER_TIER_NO_NATIVE_BUILD,
}
# The reassuring tier words stay unchanged; the third one becomes prose.
BROWSER_TIER_LABEL = {
    BROWSER_TIER_SUPPORTED: "supported",
    BROWSER_TIER_UNVERIFIED: "unverified",
    BROWSER_TIER_NO_NATIVE_BUILD: "no native build",
}

# The one placeholder for "nothing is recorded here". Defined once so the
# renderings and the probes that guard them cannot disagree about the glyph; see
# the module docstring for why it is a plain ASCII hyphen and not an em dash.
ABSENT = "-"


class SupportMatrixError(Exception):
    """A problem that makes the matrix unrenderable — never guessed past."""


def repo_root_containing(path: str) -> str:
    return os.path.dirname(os.path.dirname(os.path.abspath(path)))


def default_root() -> str:
    return repo_root_containing(__file__)


# --------------------------------------------------------------------------
# Editorial-field validation (SB-1)
#
# The three hand-written fields a row may carry are validated where the row is
# read, so a malformed record is an input error (exit 2), never a rendered
# guess. `owner` names the row for the error message ("registry: harness
# `x`" / "contract: platform `y`").
# --------------------------------------------------------------------------

REJECTED_STATIC_DATE = date(1970, 1, 1)


def validated_verified(owner: str, value: Any) -> dict[str, Any] | None:
    """Normalize a `verified` / `lastVerified` record, or None when absent.

    All three fields are checked here so both loaders stay interchangeable:
    the registry's `verified` and the contract's `lastVerified` are the same
    shape, and a divergence between them must be caught at load time.
    """
    if value is None:
        return None
    if not isinstance(value, dict):
        raise SupportMatrixError(f"{owner}: `verified` must be an object with `date` and `scope`")
    d = value.get("date")
    if not isinstance(d, str) or not VERIFIED_DATE_RE.match(d):
        raise SupportMatrixError(
            f"{owner}: `verified.date` must be a full ISO day (YYYY-MM-DD), not {d!r}"
        )
    try:
        when = date.fromisoformat(d)
    except ValueError:
        raise SupportMatrixError(f"{owner}: `verified.date` {d!r} is not a real calendar date") from None
    if when < REJECTED_STATIC_DATE:
        raise SupportMatrixError(f"{owner}: `verified.date` {d!r} is before 1970")
    # A calendar date far in the past is accepted (it is a fact about a fact):
    # the freshness rule below is what makes it visibly stale. A date in the
    # future cannot be verified yet, and is refused rather than rendered.
    if when > date.today():
        raise SupportMatrixError(f"{owner}: `verified.date` {d!r} is in the future")
    scope = value.get("scope")
    if not isinstance(scope, str) or not scope.strip():
        raise SupportMatrixError(f"{owner}: `verified.scope` must be a non-empty string")
    version = value.get("version")
    if version is not None and not isinstance(version, str):
        raise SupportMatrixError(f"{owner}: `verified.version` must be a string or absent")
    return {"date": d, "version": version, "scope": scope}


def validated_dev_priority(owner: str, value: Any) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str) or value not in VALID_DEV_PRIORITY:
        raise SupportMatrixError(
            f"{owner}: `dev_priority` must be one of {sorted(VALID_DEV_PRIORITY)}, not {value!r}"
        )
    return value


def validated_known_issue(owner: str, value: Any) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise SupportMatrixError(f"{owner}: `known_issue` must be a non-empty string when present")
    if "\n" in value:
        raise SupportMatrixError(f"{owner}: `known_issue` must be one line")
    return value


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
        owner = f"{REGISTRY_REL}: harness `{hid}`"
        # The editorial overlay is normalized in place: validated_* return
        # None for "absent", which the renderers render as ABSENT rather than
        # as a guess, and raise on anything malformed.
        h["verified"] = validated_verified(owner, h.get("verified"))
        h["dev_priority"] = validated_dev_priority(owner, h.get("dev_priority"))
        h["known_issue"] = validated_known_issue(owner, h.get("known_issue"))
        out.append(h)
    return out


def load_browsers(root: str) -> list[dict[str, Any]]:
    """The registry's `browsers` section: one row per browser, one tier cell per OS.

    The section is required, not optional: a registry without browser rows must
    fail loudly rather than render a browser-less matrix nobody asked for. The
    shapes checked here are the same ones the Rust pin test
    (`tests/nativehost_browser_matrix_test.rs`) holds against the `Browser`
    enum, so a row that drifts from what the binary registers is caught twice,
    once by the build and once by this gate's own error path.
    """
    path = os.path.join(root, REGISTRY_REL)
    if not os.path.isfile(path):
        raise SupportMatrixError(f"harness registry not found: {REGISTRY_REL}")
    with open(path, "r", encoding="utf-8") as fh:
        try:
            data = json.load(fh)
        except json.JSONDecodeError as exc:
            raise SupportMatrixError(f"{REGISTRY_REL} is not valid JSON: {exc}") from exc
    browsers = data.get("browsers")
    if not isinstance(browsers, list) or not browsers:
        raise SupportMatrixError(f"{REGISTRY_REL} has no `browsers` list")
    seen_ids: set[str] = set()
    out: list[dict[str, Any]] = []
    for b in browsers:
        bid = b.get("id")
        if not isinstance(bid, str) or not bid:
            raise SupportMatrixError(f"{REGISTRY_REL}: a browser row has no string `id`")
        if bid in seen_ids:
            raise SupportMatrixError(f"{REGISTRY_REL}: browser `{bid}` appears twice")
        seen_ids.add(bid)
        name = b.get("display_name")
        if not isinstance(name, str) or not name:
            raise SupportMatrixError(f"{REGISTRY_REL}: browser `{bid}` has no `display_name`")
        tiers = b.get("tiers")
        if not isinstance(tiers, dict):
            raise SupportMatrixError(f"{REGISTRY_REL}: browser `{bid}` has no `tiers` object")
        # Every OS is required: a missing cell is ambiguous between "nobody
        # recorded this pair" and "there is nothing to record", and a matrix
        # that guesses between the two is exactly the collapse this tool
        # refuses (a `no-native-build` cell is how the second one is said).
        for os_name in OS_ORDER:
            cell = tiers.get(os_name)
            if not isinstance(cell, dict):
                raise SupportMatrixError(
                    f"{REGISTRY_REL}: browser `{bid}` has no `{os_name}` tier cell"
                )
            tier = cell.get("tier")
            if tier not in BROWSER_TIERS:
                raise SupportMatrixError(
                    f"{REGISTRY_REL}: browser `{bid}` x `{os_name}` has unknown tier "
                    f"`{tier}` (known: {sorted(BROWSER_TIERS)})"
                )
        out.append(b)
    return out


_CONTRACT_ID_RE = re.compile(r"^\s+id:\s*'([^']+)',\s*$")
_CONTRACT_ORIGINS_RE = re.compile(r"^\s+origins:\s*\[([^\]]*)\],?\s*$")
_CONTRACT_CHANNEL_RE = re.compile(r"^\s+channel:\s*'([^']+)',\s*$")
_CONTRACT_CRED_RE = re.compile(r"^\s+credibility:\s*'([^']+)',\s*$")
_QUOTED_RE = re.compile(r"'([^']*)'")
# The editorial overlay (SB-1). `devPriority` and `knownIssue` are one line
# each; `lastVerified` opens an object that is read until its closing line,
# which is the same `},` every other multi-line field in this table closes
# with, so the only lines consumed here are the ones inside the object.
_CONTRACT_PRIORITY_RE = re.compile(r"^\s+devPriority:\s*'([^']*)',\s*$")
_CONTRACT_ISSUE_RE = re.compile(r"^\s+knownIssue:\s*'([^']*)',\s*$")
_CONTRACT_VERIFIED_OPEN_RE = re.compile(r"^\s+lastVerified:\s*\{\s*$")
_CONTRACT_VERIFIED_CLOSE_RE = re.compile(r"^\s*\},?\s*$")
_CONTRACT_VERIFIED_DATE_RE = re.compile(r"^\s+date:\s*'([^']*)',\s*$")
_CONTRACT_VERIFIED_VERSION_RE = re.compile(r"^\s+version:\s*'([^']*)',\s*$")
_CONTRACT_VERIFIED_SCOPE_RE = re.compile(r"^\s+scope:\s*'([^']*)',\s*$")


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
    # Inside a platform's `lastVerified: {` object: its `date:` / `version:` /
    # `scope:` lines are collected, and its closing line ends the object.
    verified_open = False
    verified: dict[str, Any] | None = None
    for raw in lines[start + 1 : end]:
        stripped = raw.lstrip()
        if verified_open:
            if _CONTRACT_VERIFIED_CLOSE_RE.match(raw):
                verified_open = False
                continue
            m = _CONTRACT_VERIFIED_DATE_RE.match(raw)
            if m:
                verified["date"] = m.group(1)
                continue
            m = _CONTRACT_VERIFIED_VERSION_RE.match(raw)
            if m:
                verified["version"] = m.group(1)
                continue
            m = _CONTRACT_VERIFIED_SCOPE_RE.match(raw)
            if m:
                verified["scope"] = m.group(1)
                continue
            # A line inside the object that names none of its fields is a
            # malformed record, not a satisfiable one: an unrecognized line
            # silently skipped here would let a broken `lastVerified` render
            # as if it held what it does not.
            raise SupportMatrixError(
                f"{CONTRACT_REL}: unrecognized line inside a `lastVerified` object: {stripped!r}"
            )
        if stripped.startswith("//"):
            continue
        m_id = _CONTRACT_ID_RE.match(raw)
        if m_id:
            current = {
                "id": m_id.group(1),
                "origins": [],
                "channel": None,
                "credibility": None,
                "devPriority": None,
                "knownIssue": None,
                "lastVerified": None,
            }
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
        m_priority = _CONTRACT_PRIORITY_RE.match(raw)
        if m_priority:
            current["devPriority"] = m_priority.group(1)
            continue
        m_issue = _CONTRACT_ISSUE_RE.match(raw)
        if m_issue:
            current["knownIssue"] = m_issue.group(1)
            continue
        if _CONTRACT_VERIFIED_OPEN_RE.match(raw):
            verified = {}
            current["lastVerified"] = verified
            verified_open = True
            continue

    if verified_open:
        raise SupportMatrixError(
            f"{CONTRACT_REL}: a `lastVerified` object is still open at the end of `ALL_PLATFORMS`"
        )
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
        # The editorial overlay is validated to the same rules as the
        # registry's, so one shape answers on both families.
        owner = f"{CONTRACT_REL}: platform `{p['id']}`"
        p["lastVerified"] = validated_verified(owner, p["lastVerified"])
        p["devPriority"] = validated_dev_priority(owner, p["devPriority"] or None)
        p["knownIssue"] = validated_known_issue(owner, p["knownIssue"] or None)
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
    # A recorded verification outranks the channel and credibility readings,
    # exactly as a `verified` record outranks them for a harness: it is the
    # strongest fact the row carries. The date travels inside the status as the
    # historical fact; the freshness judgement on it is rendered separately in
    # the Last verified column.
    if p.get("lastVerified") is not None:
        return f"{STATUS_VERIFIED} ({p['lastVerified']['date']})"
    if p["channel"] == "experimental":
        return STATUS_EXPERIMENTAL
    if p["credibility"] == "unverified":
        return STATUS_UNCERTAIN
    return STATUS_SUPPORTED


def resolve_as_of(as_of: date | None) -> date:
    """The day the freshness rule is judged against.

    Real runs leave this as None (today). The self-test pins an explicit date
    so its probes never depend on the calendar the day they happen to run;
    determinism in the fixtures is what keeps the gate's own tests honest.
    """
    return as_of if as_of is not None else date.today()


def last_verified_cell(record: dict[str, Any] | None, as_of: date) -> str:
    """Render the Last verified cell for one `verified` record.

    Four states, never collapsed into each other:

      record is None            ABSENT — no run is recorded. That is "nothing
                                was written down", not "never worked" and not
                                "failed"; the status column stays in charge of
                                what the row claims.
      age <= RECHECK_DAYS       the date, plain.
      age >  RECHECK_DAYS       `needs re-check (DATE)`: the recorded run is
                                older than the freshness threshold, so the date
                                is displayed as a question rather than as a
                                fact.
      date in the future        impossible from the loaders (they refuse it) and
                                rendered as the date itself here; the function
                                never invents a judgement.
    """
    if record is None:
        return ABSENT
    when = date.fromisoformat(record["date"])
    age = (as_of - when).days
    if age > RECHECK_DAYS:
        return f"{RECHECK_LABEL} ({record['date']})"
    return record["date"]


# ---------------------------------------------------------------------------
# Rendering
# --------------------------------------------------------------------------


def esc(text: Any) -> str:
    return str(text).replace("|", "\\|").replace("\n", " ")


def code(text: Any) -> str:
    return "`" + str(text).replace("`", "'") + "`"


def browser_legend_short() -> str:
    return (
        "`supported` is the promised tier: the discovery path, and on Windows the registry "
        "key, is documented and `install-native-host` registers it. `unverified` is the "
        "best-effort tier: registration is attempted and reported, never promised. "
        "`no native build` means the browser itself ships no build for that OS, so there "
        "is nothing to register. Firefox is carried as supported outside these "
        "Chromium-family tiers, on paths from Mozilla's own documentation. One "
        "registration serves every profile of a browser on a machine; the extension itself "
        "still needs loading once per profile."
    )


def render_short(
    harnesses: list[dict[str, Any]],
    platforms: list[dict[str, Any]],
    browsers: list[dict[str, Any]],
    as_of: date | None = None,
) -> str:
    as_of = resolve_as_of(as_of)
    out: list[str] = []
    out.append("**5+ platforms.** Local AI coding tools and web chats, archived the same way.")
    out.append("")
    out.append("| Surface | Platform | Status | Last verified |")
    out.append("|---|---|---|---|")
    for h in harnesses:
        st = harness_status(h)
        cell = last_verified_cell(harness_verified(h), as_of)
        out.append(
            f"| Local | {esc(h.get('display_name') or h['id'])} | {esc(st)} | {esc(cell)} |"
        )
    for p in platforms:
        st = web_status(p)
        cell = last_verified_cell(p.get("lastVerified"), as_of)
        out.append(f"| Web | {esc(p['id'])} | {esc(st)} | {esc(cell)} |")
    out.append("")
    out.append(
        "Verified means a maintainer archived a real session end to end on their own machine. "
        "Formats change, so a date is recorded instead of a permanent check; a date older "
        f"than {RECHECK_DAYS} days is shown as {RECHECK_LABEL} (DATE)."
    )
    out.append("")
    out.append("**Browsers.** Native host registration, per browser and per OS:")
    out.append("")
    out.append("| Browser | macOS | Linux | Windows |")
    out.append("|---|---|---|---|")
    for b in browsers:
        cells = [BROWSER_TIER_LABEL[b["tiers"][os_name]["tier"]] for os_name in OS_ORDER]
        out.append(f"| {esc(b['display_name'])} | " + " | ".join(esc(c) for c in cells) + " |")
    out.append("")
    out.append(browser_legend_short())
    return "\n".join(out) + "\n"


def render_full(
    harnesses: list[dict[str, Any]],
    platforms: list[dict[str, Any]],
    browsers: list[dict[str, Any]],
    as_of: date | None = None,
) -> str:
    out: list[str] = []
    as_of = resolve_as_of(as_of)
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
                    f"| {esc(name)} | {OS_LABEL[os_name]} | {ABSENT} | {ABSENT} | {ABSENT} "
                    f"| {esc(status)} | {ABSENT} |"
                )
                continue
            source = first_url(cell.get("source", "")) or first_url(ref)
            out.append(
                "| {name} | {os} | {template} | {fmt} | {conf} | {status} | {src} |".format(
                    name=esc(name),
                    os=OS_LABEL[os_name],
                    template=code(cell.get("template", "")),
                    fmt=esc(cell.get("format", ABSENT) or ABSENT),
                    conf=esc(cell.get("confidence", ABSENT) or ABSENT),
                    status=esc(status),
                    src=esc(source or ABSENT),
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
    # The editorial overlay (SB-1). One section for both families, because a
    # reader asking "how fresh is this, who is working on it, what is known to
    # bite" asks that about the Local row and the Web row in one breath, and
    # the same field answers on both.
    out.append("### Last verified, dev priority and known issues")
    out.append("")
    out.append("| Surface | Tool or platform | Last verified | Dev priority | Known issue |")
    out.append("|---|---|---|---|---|")
    for h in harnesses:
        out.append(
            "| Local | {name} | {when} | {prio} | {issue} |".format(
                name=esc(h.get("display_name") or h["id"]),
                when=esc(last_verified_cell(harness_verified(h), as_of)),
                prio=esc(h.get("dev_priority") or ABSENT),
                issue=esc(h.get("known_issue") or ABSENT),
            )
        )
    for p in platforms:
        out.append(
            "| Web | {name} | {when} | {prio} | {issue} |".format(
                name=esc(p["id"]),
                when=esc(last_verified_cell(p.get("lastVerified"), as_of)),
                prio=esc(p.get("devPriority") or ABSENT),
                issue=esc(p.get("knownIssue") or ABSENT),
            )
        )
    out.append("")
    out.append("### Browsers (native messaging host registration)")
    out.append("")
    out.append("| Browser | OS | Status | Source |")
    out.append("|---|---|---|---|")
    for b in browsers:
        for os_name in OS_ORDER:
            cell = b["tiers"][os_name]
            tier = BROWSER_TIER_LABEL[cell["tier"]]
            source = first_url(cell.get("source", "")) or first_url(b.get("source", ""))
            out.append(
                "| {name} | {os} | {status} | {src} |".format(
                    name=esc(b["display_name"]),
                    os=OS_LABEL[os_name],
                    status=esc(tier),
                    src=esc(source or ABSENT),
                )
            )
    out.append("")
    out.append(
        "`supported` means the path or route has a source and the scanner/extension "
        "will act on it; `verified end-to-end` additionally means a real session was "
        "archived on a real machine and the date is recorded. `unascertained` cells are "
        "not scanned and are rendered as `not supported`."
    )
    out.append("")
    out.append(
        "`Last verified` is the date a recorded run archived a real conversation end to "
        f"end on a real machine; {ABSENT} means no run is recorded, and a date older than "
        f"{RECHECK_DAYS} days is shown as `{RECHECK_LABEL} (DATE)`. Both `Dev priority` "
        "(`high` / `normal` / `low`, an editorial statement of where maintainer attention "
        "is) and `Known issue` (one short caveat, with a pointer to where it is tracked) "
        "are written by hand in the registry and the extension's platform table, which is "
        "why a change to them re-derives these tables too."
    )
    out.append("")
    out.append(browser_legend_short())
    return "\n".join(out) + "\n"


def render(kind: str, root: str, as_of: date | None = None) -> str:
    harnesses = load_harnesses(root)
    platforms = parse_contract_platforms(root)
    browsers = load_browsers(root)
    if kind == "short":
        return render_short(harnesses, platforms, browsers, as_of)
    if kind == "full":
        return render_full(harnesses, platforms, browsers, as_of)
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
    """Every document whose marker block this check owns.

    Both documentation trees, because a block is placed by whoever needs it and
    a committed block the check cannot see is a block that can drift: `docs/` is
    the reader-facing set and `docs-dev/` the development one, and neither is a
    more likely home for the full table than the other.
    """
    docs = ["README.md"]
    for name in ("docs", "docs-dev"):
        tree = os.path.join(root, name)
        if os.path.isdir(tree):
            for entry in sorted(os.listdir(tree)):
                if entry.endswith(".md"):
                    docs.append(os.path.join(name, entry))
    return docs


def check(root: str, as_of: date | None = None) -> tuple[list[str], list[str]]:
    """Return (failures, notes). A failure is always an exit-1 condition.

    `as_of` is the day the freshness rule judges against; real runs leave it as
    today. When a recorded verification crosses the 90-day threshold, the
    expected renderings change with no change to the sources, so this check's
    going red is the expiry signal itself: the fix is to re-derive the tables
    (baking the `needs re-check` marker into them), or to record a new dated
    verification — never to delete the date.
    """
    failures: list[str] = []
    notes: list[str] = []
    expected = {"short": render("short", root, as_of), "full": render("full", root, as_of)}

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
            "no support-matrix markers in README.md, docs/*.md or docs-dev/*.md yet; "
            "the committed fixtures above are the gate"
        )
    return failures, notes


def run_check(root: str, as_of: date | None = None) -> int:
    try:
        failures, notes = check(root, as_of)
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


def update_fixtures(root: str, as_of: date | None = None) -> int:
    try:
        expected = {"short": render("short", root, as_of), "full": render("full", root, as_of)}
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


def write_into_file(root: str, rel_path: str, kind: str, as_of: date | None = None) -> int:
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
        content = render(fixture_kind, root, as_of)
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

# Two browsers exercise every part of the browser section's shape: one whose
# three cells share the row-level source, and one whose every cell differs
# (including the third vocabulary word, which must never be spellable as a
# promise). Lives inside _SELFTEST_REGISTRY so a deep-copied variant that
# mutates a harness cell keeps a well-formed browser section with it.
_SELFTEST_REGISTRY = {
    "schema_version": 1,
    "generated": "2026-01-01",
    "browsers": [
        {
            "id": "b-chrome",
            "display_name": "B-Chrome",
            "source": "https://example.com/b-chrome",
            "tiers": {
                "macos": {"tier": "supported"},
                "linux": {"tier": "supported"},
                "windows": {"tier": "supported"},
            },
        },
        {
            "id": "b-arc",
            "display_name": "B-Arc",
            "source": "https://example.com/b-arc",
            "tiers": {
                "macos": {"tier": "supported"},
                "linux": {
                    "tier": "no-native-build",
                    "source": "https://example.com/b-arc-linux",
                },
                "windows": {
                    "tier": "unverified",
                    "source": "https://example.com/b-arc-windows",
                },
            },
        },
    ],
    "harnesses": [
        {
            "id": "alpha",
            "display_name": "Alpha",
            "dev_priority": "normal",
            "known_issue": "a caveat with a | pipe, to prove escaping",
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
            "dev_priority": "low",
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
            "verified": {"date": "2026-09-15", "version": "1.2.3", "scope": "one real session"},
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
    devPriority: 'normal',
    knownIssue: 'one caveat | with a pipe',
    lastVerified: {
      date: '2026-09-15',
      version: '1.2.3',
      scope: 'one real conversation on a real page',
    },
    credibility: 'from-source',
    channel: 'stable',
  },
  {
    id: 'p-exp',
    origins: ['https://exp.example'],
    devPriority: 'low',
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

# The day every self-test rendering is judged against. A fixed date, so the
# probes never depend on the calendar of the day they run: the fixture dates
# above are fresh against it, and the freshness probes below pick dates at
# both edges of the threshold relative to it. The real gate keeps using the
# real today (see `resolve_as_of`); only these tests are pinned.
SELFTEST_AS_OF = date(2026, 10, 1)
# Exactly 90 days old on SELFTEST_AS_OF: the threshold day itself, which must
# still render as a plain date ("older than" is strict).
SELFTEST_FRESH_90 = "2026-07-03"
# 91 days old on SELFTEST_AS_OF: the first stale day.
SELFTEST_STALE_91 = "2026-07-02"

# Two browsers exercise every part of the browser section's shape live inside
# _SELFTEST_REGISTRY above ("browsers"): one whose three cells share the
# row-level source, and one whose every cell differs, including the third
# vocabulary word, which must never be spellable as a promise. They live in the
# registry dict itself so a deep-copied variant that mutates a harness cell
# still carries a well-formed browser section.


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
        browsers = load_browsers(tmp)
        probe("registry parsed", [h["id"] for h in harnesses] == ["alpha", "beta", "gamma", "delta"])
        probe("contract parsed", [p["id"] for p in platforms] == ["p-stable", "p-exp", "p-unver"])
        probe(
            "browsers parsed",
            [b["id"] for b in browsers] == ["b-chrome", "b-arc"],
        )
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
            harness_status(by_id["delta"]) == f"{STATUS_VERIFIED} (2026-09-15)",
        )
        web = {p["id"]: p for p in platforms}
        probe(
            "a platform with a recorded verification outranks its channel and credibility",
            web_status(web["p-stable"]) == f"{STATUS_VERIFIED} (2026-09-15)",
        )
        probe(
            "a stable from-source platform without a verified record stays supported",
            web_status({"channel": "stable", "credibility": "from-source", "lastVerified": None})
            == STATUS_SUPPORTED,
        )
        probe("experimental channel is experimental", web_status(web["p-exp"]) == STATUS_EXPERIMENTAL)
        probe("unverified credibility is uncertain", web_status(web["p-unver"]) == STATUS_UNCERTAIN)

        # The editorial overlay is read out of both sources with the same
        # shape: registry `verified`/`dev_priority`/`known_issue` beside the
        # harness id, contract `lastVerified`/`devPriority`/`knownIssue` beside
        # the platform row. A field that is absent must come back None, which
        # renders as ABSENT — never as a guess.
        probe(
            "registry editorial fields are parsed per harness",
            by_id["alpha"]["dev_priority"] == "normal"
            and by_id["alpha"]["known_issue"] == "a caveat with a | pipe, to prove escaping"
            and by_id["beta"]["known_issue"] is None
            and by_id["gamma"]["dev_priority"] is None,
        )
        probe(
            "the contract's resolved lastVerified object is parsed field by field",
            web["p-stable"]["lastVerified"] == {
                "date": "2026-09-15",
                "version": "1.2.3",
                "scope": "one real conversation on a real page",
            },
        )
        probe(
            "contract editorial fields are parsed per platform, absent where missing",
            web["p-exp"]["devPriority"] == "low"
            and web["p-exp"]["knownIssue"] is None
            and web["p-exp"]["lastVerified"] is None
            and web["p-unver"]["devPriority"] is None,
        )

        # The freshness rule (ADR-041, decision 7), on both sides of the
        # threshold. Known-answer probes first, on dates picked relative to
        # SELFTEST_AS_OF: 90 days old is still a plain date ("older than" is
        # strict), 91 days old is the first stale day, and "no record" is the
        # placeholder, a third state that is neither.
        probe(
            "a date exactly 90 days old still renders as the plain date",
            last_verified_cell({"date": SELFTEST_FRESH_90, "scope": "x"}, SELFTEST_AS_OF)
            == SELFTEST_FRESH_90,
        )
        probe(
            "a date 91 days old renders as needing a re-check with its date",
            last_verified_cell({"date": SELFTEST_STALE_91, "scope": "x"}, SELFTEST_AS_OF)
            == f"{RECHECK_LABEL} ({SELFTEST_STALE_91})",
        )
        probe(
            "no record renders the absent placeholder",
            last_verified_cell(None, SELFTEST_AS_OF) == ABSENT,
        )
        stale_platform = {
            "id": "p-stale",
            "origins": ["https://stale.example"],
            "channel": "stable",
            "credibility": "from-source",
            "lastVerified": {"date": SELFTEST_STALE_91, "scope": "an old run"},
        }
        stale_short = render_short([], [stale_platform], [], SELFTEST_AS_OF)
        probe(
            "a stale record is flagged in the short table's Last verified column",
            f"| p-stale | {STATUS_VERIFIED} ({SELFTEST_STALE_91}) "
            f"| {RECHECK_LABEL} ({SELFTEST_STALE_91}) |" in stale_short,
        )
        probe(
            "the status keeps the historical fact while the column carries the flag",
            f"{STATUS_VERIFIED} ({SELFTEST_STALE_91})" in stale_short,
        )
        stale_full = render_full([], [stale_platform], [], SELFTEST_AS_OF)
        probe(
            "a stale record is flagged in the editorial section",
            f"| Web | p-stale | {RECHECK_LABEL} ({SELFTEST_STALE_91}) |" in stale_full,
        )
        probe(
            "the same record is flagged when judged against a day past the threshold",
            f"{RECHECK_LABEL} ({SELFTEST_FRESH_90})" in render_short(
                [],
                [{**stale_platform, "lastVerified": {"date": SELFTEST_FRESH_90, "scope": "x"}}],
                [],
                date(2026, 10, 3),
            ),
        )

        short = render_short(harnesses, platforms, browsers, SELFTEST_AS_OF)
        full = render_full(harnesses, platforms, browsers, SELFTEST_AS_OF)
        probe("short table carries the fixed 5+ platforms headline", "5+ platforms" in short)
        probe("short table has no generated count in prose", not re.search(r"\b\d+ (?:platform|harness|tool)", short))
        probe("short table renders every status word", all(
            s in short for s in (STATUS_SUPPORTED, STATUS_UNCERTAIN, STATUS_UNSUPPORTED, STATUS_EXPERIMENTAL)
        ))
        probe("full table carries a path template", "~/.alpha/<uuid>.jsonl" in full)
        probe("full table carries the verified date", "(2026-09-15)" in full)
        probe(
            "the short legend names the re-check rule beside its threshold",
            f"a date older than {RECHECK_DAYS} days is shown as {RECHECK_LABEL} (DATE)" in short,
        )

        # The editorial section: one row per tool and per platform, the same
        # three fields both families carry, and the placeholder wherever a
        # field was not written. A pipe in a known issue must not break the
        # table cell it renders into.
        probe(
            "the editorial section renders a row with every field present",
            "| Web | p-stable | 2026-09-15 | normal | one caveat \\| with a pipe |" in full,
        )
        probe(
            "the editorial section renders a verified harness with no priority or issue",
            f"| Local | Delta | 2026-09-15 | {ABSENT} | {ABSENT} |" in full,
        )
        probe(
            "the editorial section keeps the absent placeholder for unwritten fields",
            f"| Local | Beta | {ABSENT} | low | {ABSENT} |" in full,
        )
        probe(
            "the editorial section escapes a pipe inside a known issue",
            "a caveat with a \\| pipe, to prove escaping" in full,
        )

        # The browser grid: one row per browser, one cell per OS, in the row
        # order the registry carries (tier-grouped, never alphabetical, so a
        # reader comparing machines finds the promised browsers together).
        probe(
            "the short browser grid renders a row whose cells share the row source",
            "| B-Chrome | supported | supported | supported |" in short,
        )
        probe(
            "the short browser grid renders the no-promise third word",
            "| B-Arc | supported | no native build | unverified |" in short,
        )
        probe(
            "a full browser cell with its own source keeps that source",
            "| B-Arc | Linux | no native build | https://example.com/b-arc-linux |" in full,
        )
        probe(
            "a full browser cell with no cell source falls back to the row source",
            "| B-Chrome | Windows | supported | https://example.com/b-chrome |" in full,
        )
        probe(
            "a full browser cell's override wins over the row source",
            "| B-Arc | Windows | unverified | https://example.com/b-arc-windows |" in full,
        )
        probe(
            "the short legend states the Firefox exception to the tier vocabulary",
            "Firefox is carried as supported outside these Chromium-family tiers" in short,
        )

        # The rendered tables are DATA that gets copied into other documents, so
        # a glyph has to survive whatever font and pipeline it lands in. The
        # placeholder for "no value recorded" is therefore plain ASCII, defined
        # once as ABSENT, and a typographic dash must not appear anywhere in a
        # rendering — U+2014 as a field value reads as punctuation and is not
        # something a reader can grep for. These probes are the only guard: the
        # gate byte-compares the committed tables against this renderer, so a
        # dash emitted here would be committed and checked-in green.
        probe(
            "no rendered table contains an em or en dash",
            not any(ch in (short + full) for ch in ("—", "–")),
        )
        probe("the absent-value placeholder is plain ASCII", ABSENT.isascii() and ABSENT.isprintable())
        probe("the placeholder is a single documented glyph", ABSENT == "-")

        # Rows are selected by their Surface column, not by position: the
        # separator row `|---|---|` matches a `startswith("| ")` filter only by
        # accident of spacing, and slicing around it silently drops a row.
        data_rows = [
            ln for ln in short.splitlines()
            if ln.startswith("| Local | ") or ln.startswith("| Web | ")
        ]
        probe(
            "every short-table row without a verification date ends in the placeholder",
            bool(data_rows) and all(ln.endswith(f"| {ABSENT} |") for ln in data_rows if "2026-09-15" not in ln),
        )
        probe(
            "a verified row keeps its date instead of the placeholder",
            any(ln.endswith("| 2026-09-15 |") for ln in data_rows),
        )
        probe(
            "a full-table cell with no registry entry renders the placeholder",
            f"| Alpha | Linux | {ABSENT} | {ABSENT} | {ABSENT} | not supported | {ABSENT} |" in full,
        )
        probe(
            "a full-table cell whose source carries no URL renders the placeholder",
            f"| Beta | macOS | `~/Library/Beta/sessions` | sqlite | community-claim-unverified "
            f"| uncertain (unverified) | {ABSENT} |" in full,
        )
        # `format` and `confidence` fall back separately, so each needs its own
        # cell; a harness whose only OS cell lacks either one exercises them.
        sparse_full = render_full(
            [
                {"id": "s-fmt", "display_name": "S-Fmt",
                 "paths": {"macos": {"template": "~/.s-fmt", "confidence": "source-confirmed"}}},
                {"id": "s-conf", "display_name": "S-Conf",
                 "paths": {"macos": {"template": "~/.s-conf", "format": "jsonl"}}},
            ],
            [],
            [],
            SELFTEST_AS_OF,
        )
        probe(
            "a cell with no format recorded renders the placeholder",
            f"| S-Fmt | macOS | `~/.s-fmt` | {ABSENT} | source-confirmed | supported | {ABSENT} |"
            in sparse_full,
        )
        probe(
            "a cell with no confidence recorded renders the placeholder",
            f"| S-Conf | macOS | `~/.s-conf` | jsonl | {ABSENT} |" in sparse_full,
        )

        # --update-fixtures makes a fresh checkout pass the check.
        probe("fixtures update cleanly", update_fixtures(tmp, SELFTEST_AS_OF) == 0)
        probe("fresh fixtures pass the check", run_check(tmp, SELFTEST_AS_OF) == 0)

        # A stale committed table must fail.
        short_path = os.path.join(tmp, SHORT_FIXTURE_REL)
        with open(short_path, "r", encoding="utf-8") as fh:
            original_short = fh.read()
        with open(short_path, "w", encoding="utf-8") as fh:
            fh.write(original_short.replace("supported", "SUPPORTED", 1))
        probe("a corrupted fixture fails the check", run_check(tmp, SELFTEST_AS_OF) == 1)
        with open(short_path, "w", encoding="utf-8") as fh:
            fh.write(original_short)

        # Changing the input without regenerating must fail. One probe per
        # family: a harness cell and a browser cell are different inputs of the
        # same kind of fact (both are registry data the tables derive from),
        # and a regression that misses one of the two sections would otherwise
        # prove the other still fails while quietly rendering the first.
        changed = json.loads(json.dumps(_SELFTEST_REGISTRY))
        changed["harnesses"][0]["paths"]["macos"]["template"] = "~/.alpha/changed/<uuid>.jsonl"
        _scaffold(tmp, registry=changed)
        probe("a changed registry fails the check", run_check(tmp, SELFTEST_AS_OF) == 1)
        changed_browsers = json.loads(json.dumps(_SELFTEST_REGISTRY))
        changed_browsers["browsers"][0]["tiers"]["windows"] = {"tier": "unverified"}
        _scaffold(tmp, registry=changed_browsers)
        probe("a changed browser tier fails the check", run_check(tmp, SELFTEST_AS_OF) == 1)
        _scaffold(tmp)

        # A changed editorial field is the same failure as a changed path cell
        # or a changed tier: the tables claim to be derived from the overlay,
        # so an edit that does not re-derive them must go red. One probe per
        # family of source, plus one per field kind, because a renderer that
        # silently dropped one field would otherwise pass its sibling's probe.
        edited_issue = json.loads(json.dumps(_SELFTEST_REGISTRY))
        edited_issue["harnesses"][0]["known_issue"] = "a changed caveat"
        _scaffold(tmp, registry=edited_issue)
        probe("a changed known_issue fails the check", run_check(tmp, SELFTEST_AS_OF) == 1)
        edited_priority = json.loads(json.dumps(_SELFTEST_REGISTRY))
        edited_priority["harnesses"][1]["dev_priority"] = "high"
        _scaffold(tmp, registry=edited_priority)
        probe("a changed dev_priority fails the check", run_check(tmp, SELFTEST_AS_OF) == 1)
        edited_verified = json.loads(json.dumps(_SELFTEST_REGISTRY))
        edited_verified["harnesses"][3]["verified"] = {
            "date": "2026-08-01",
            "scope": "a different run",
        }
        _scaffold(tmp, registry=edited_verified)
        probe("a changed verified record fails the check", run_check(tmp, SELFTEST_AS_OF) == 1)
        _scaffold(tmp, contract=_SELFTEST_CONTRACT.replace(
            "date: '2026-09-15',",
            "date: '2026-08-01',",
        ))
        probe("a changed platform lastVerified fails the check", run_check(tmp, SELFTEST_AS_OF) == 1)
        _scaffold(tmp)

        # A malformed editorial record is an input error (exit 2), never a
        # rendered guess: a priority outside the vocabulary, a date that is
        # not a full day, a date nobody could have verified yet, a record
        # with no scope, and a line the parser does not recognize inside a
        # lastVerified object. Each is its own probe because each is caught
        # by its own validation, and a regression would otherwise be covered
        # by whichever sibling still fires.
        bad_priority = json.loads(json.dumps(_SELFTEST_REGISTRY))
        bad_priority["harnesses"][0]["dev_priority"] = "urgent"
        _scaffold(tmp, registry=bad_priority)
        probe("an unknown dev_priority in the registry is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        bad_date = json.loads(json.dumps(_SELFTEST_REGISTRY))
        bad_date["harnesses"][3]["verified"] = {"date": "2026-09", "scope": "one real session"}
        _scaffold(tmp, registry=bad_date)
        probe("a verified date that is not a full ISO day is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        bad_future = json.loads(json.dumps(_SELFTEST_REGISTRY))
        bad_future["harnesses"][3]["verified"] = {"date": "2066-01-01", "scope": "one real session"}
        _scaffold(tmp, registry=bad_future)
        probe("a verified date in the future is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        no_scope = json.loads(json.dumps(_SELFTEST_REGISTRY))
        no_scope["harnesses"][3]["verified"] = {"date": "2026-09-15"}
        _scaffold(tmp, registry=no_scope)
        probe("a verified record with no scope is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        _scaffold(tmp, contract=_SELFTEST_CONTRACT.replace("devPriority: 'low'", "devPriority: 'urgent'"))
        probe("an unknown devPriority in the contract is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        _scaffold(tmp, contract=_SELFTEST_CONTRACT.replace(
            "      version: '1.2.3',",
            "      vversion: '1.2.3',",
        ))
        probe("an unrecognized line inside a lastVerified object is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        _scaffold(tmp)

        # A browsers section that cannot be read at all is an error, never a
        # guess: absent, half-filled, or with a tier this vocabulary does not
        # know. `check` exits 2 (input error), which the check distinguishes
        # from 1 (stale tables) and reports as such.
        no_browsers = json.loads(json.dumps(_SELFTEST_REGISTRY))
        del no_browsers["browsers"]
        _scaffold(tmp, registry=no_browsers)
        probe("a registry with no browsers section is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        gappy = json.loads(json.dumps(_SELFTEST_REGISTRY))
        del gappy["browsers"][1]["tiers"]["linux"]
        _scaffold(tmp, registry=gappy)
        probe("a browser row missing an OS cell is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        weird_tier = json.loads(json.dumps(_SELFTEST_REGISTRY))
        weird_tier["browsers"][0]["tiers"]["linux"] = {"tier": "probably-fine"}
        _scaffold(tmp, registry=weird_tier)
        probe("an unknown browser tier is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        _scaffold(tmp)

        # Markers in a document are checked, and a half pair is loud.
        readme = os.path.join(tmp, "README.md")
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\n{render_short(harnesses, platforms, browsers, SELFTEST_AS_OF).strip()}\n{SHORT_END}\n")
        probe("a correct in-document block passes", run_check(tmp, SELFTEST_AS_OF) == 0)
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\nwrong\n{SHORT_END}\n")
        probe("a stale in-document block fails", run_check(tmp, SELFTEST_AS_OF) == 1)
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\nno end marker\n")
        probe("a half marker pair fails loudly", run_check(tmp, SELFTEST_AS_OF) == 1)

        # --write-* refuses to create or guess.
        probe("write refuses when markers are absent", write_into_file(tmp, "README.md", "short", SELFTEST_AS_OF) == 2)
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\nold\n{SHORT_END}\n")
        probe("write replaces between markers", write_into_file(tmp, "README.md", "short", SELFTEST_AS_OF) == 0)
        probe("write result passes the check", run_check(tmp, SELFTEST_AS_OF) == 0)
        probe("write refuses a missing file", write_into_file(tmp, "NOPE.md", "short", SELFTEST_AS_OF) == 2)

        # Malformed inputs must fail loudly, never render a partial matrix.
        _scaffold(tmp, contract="export const ALL_PLATFORMS: readonly ChatPlatform[] = [\n"
                                 "  {\n    id: 'p',\n    origins: ['https://x.example'],\n  },\n];\n")
        probe("a platform with no channel is an error", run_check(tmp, SELFTEST_AS_OF) == 2)
        os.remove(os.path.join(tmp, REGISTRY_REL))
        probe("a missing registry is an error", run_check(tmp, SELFTEST_AS_OF) == 2)

        # Advice a failure text prints is only advice if the CLI accepts it.
        # Stale the fixtures, a short block and a full block at once, so the
        # probe sees every remediation message `check` can emit, then read the
        # flags back out of those messages and hold them against argparse. This
        # is the guard for the day one of them named a flag that does not exist.
        _scaffold(tmp)
        update_fixtures(tmp, SELFTEST_AS_OF)
        with open(short_path, "w", encoding="utf-8") as fh:
            fh.write("stale\n")
        with open(readme, "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{SHORT_START}\nwrong\n{SHORT_END}\n")
        os.makedirs(os.path.join(tmp, "docs-dev"), exist_ok=True)
        with open(os.path.join(tmp, "docs-dev", "support.md"), "w", encoding="utf-8") as fh:
            fh.write(f"# t\n\n{FULL_START}\nwrong\n{FULL_END}\n")
        printed_flags = {
            flag for text in check(tmp, SELFTEST_AS_OF)[0] for flag in re.findall(r"--[a-z][a-z-]*", text)
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
    ap.add_argument("--apply", action="store_true", help="update fixtures, README.md, and docs/support.md")
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
        if args.apply:
            rc = update_fixtures(root)
            if rc != 0:
                return rc
            rc = write_into_file(root, "README.md", "short")
            if rc != 0:
                return rc
            return write_into_file(root, "docs/support.md", "full")
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
