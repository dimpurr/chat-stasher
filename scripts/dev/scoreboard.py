#!/usr/bin/env python3
"""scoreboard.py — generate the platform scoreboard as markdown.

SB-2 generator (public code, private OUTPUT). This script is pure plumbing:
it reads saved inputs and renders markdown. It never calls a model, never
opens the archive itself, and never writes to a chat-stasher destination or
stage — the only paths it writes are `--out` and the small state file kept
next to it. Every input is opened read-only.

The board covers every platform the shipped product knows: the local AI
coding tools from `crates/chat-stasher/data/harness-registry-v1.json` and the
web chat platforms from `apps/extension/lib/contract.ts`'s `ALL_PLATFORMS` —
the same two sources `gen-support-matrix.py` renders, loaded through that
script so the two can never drift.

Four inputs, each optional: a missing input is shown as "source unavailable"
and is never folded into a zero. The editorial fields are the exception to
being handed in — they already live in the two platform sources this script
loads, so they are read from there by default and `--editorial` only overrides
them (see below).

  --ext-status DIR    The native host's per-install status reports: the
                      `ext-status` directory of a machine's local stage.
                      Records are keyed `(machine, install_id)`; EXT-13
                      writes `<machine>/<install_id>.json` and keeps the
                      legacy flat `<install_id>.json` on purpose, so readers
                      resolve the two layouts to one row per key, preferring
                      the keyed record (`contracts/nativehost-protocol.md`
                      §6.7). Pending counts are per install and are NEVER
                      added across installs — two installs may be backfilling
                      one account, and a platform total would count that
                      account's debt twice.

  --overview FILE     The saved stdout of `chat-stasher overview
                      --destination <name> --json` (the full document with
                      the per-session `sessions` rows, not the `--summary`
                      variant). From it: per machine × local tool session
                      counts, the time-unknown share of each cell, and each
                      tool's newest known session. Where a local tool's id
                      is also a web platform's id, the archive's id space
                      mixes the two and the table says so.

  --oracle PATH       Reference-corpus ("oracle") comparison results: a
                      directory of the checker's JSON outputs, or one file
                      (repeatable). Each result carries recall, content-short
                      counts and its own generated-at time. The export
                      window travels with every number: a recall figure
                      without the export it was measured against is
                      unreadable.

  (editorial)         Read by default from the two sources under `--root`
                      that already carry these fields, so the board's editorial
                      columns are never a second copy somebody has to keep in
                      step: each harness row's `verified` / `dev_priority` /
                      `known_issue` from the registry, and each platform row's
                      `lastVerified` / `devPriority` / `knownIssue` from the
                      extension contract, both normalized by
                      `gen-support-matrix.py` before this script sees them. The
                      two id spaces are kept apart rather than unioned: where
                      one id names both a local tool and a web platform (grok),
                      each row reads its own family's record, and a list that
                      has to name such a row says which of the two it means. A
                      row carrying no field at all is absent from the reading,
                      which is what "nothing recorded" means; it is not the
                      same state as a source that could not be read.

  --editorial FILE    Override for the editorial fields above, a JSON object of
                      the shape {"platforms": {"<platform-id>": {"status": str,
                      "dev_priority": str, "last_verified": "YYYY-MM-DD",
                      "known_issues": str}}}; every field optional per
                      platform. An override REPLACES the sources (a named file
                      that cannot be read is unavailable, never silently
                      swapped for the built-in reading), and its rows are keyed
                      by bare id, so one row covers every row carrying that id.
                      `last_verified` drives the re-verification rule; a date
                      that will not parse is shown as written and proves
                      nothing. Rows for ids this product does not know are
                      listed, never dropped.

Output: markdown on stdout, or to `--out`. Sections, top to bottom:

  # chat-stasher platform scoreboard
  Generated at: …                        the clock every age is measured against
  ## ANOMALIES                           the rules, then rules that could not
                                         be judged this run (a visibility
                                         list, never an all-clear)
  ## Sources                             availability + freshness per source
                                         and for the previous-run state
  ## Web platforms                       per platform (per-install counts,
                                         never a pending total), per install
  ## Local tools                         the machine × tool session matrix with
                                         the time-unknown share in each cell
  ## Oracle results                      every result, newest first

ANOMALIES rules and their knobs:

  no-new-capture        (web) no install's `captured_by_this_browser` counter
                        has been observed to advance for N days
                        (--quiet-days, default 3; "observed to advance" is
                        measured between two runs of this script — the first
                        run only establishes a baseline, and a first run
                        never reports this rule as either clean or dirty);
                        (local tools) a tool's newest known session content
                        timestamp, across machines, is older than N days.
  pending-rising        Some (machine, install, platform) pending count is
                        higher than in the previous run, compared through the
                        state file kept next to `--out` (`<out>.state.json`),
                        redirected with `--state` or disabled with
                        `--no-state`.
  time-unknown-share    A machine × tool cell has more than --unknown-share
                        (default 0.5) of its conversations with a time the
                        archive could not read. No-conversation-content
                        sessions stay outside numerator and denominator
                        (ADR-035).
  verification-stale   A platform's editorial `last_verified` is more than
                        --verify-days (default 90) days before the run. Rows
                        that carry editorial fields but no readable date are
                        named as unjudged rather than passed: "nobody has
                        recorded a verification" is not "verified recently".
  source-stale /        One of the four inputs is missing or unreadable
  source-unavailable    (unavailable), or older than the freshness limit
                        (stale): each install's own status report, whatever
                        the install's cadence — a silent daily reporter is
                        named as one, and no cadence exempts a report from
                        the limit — or the whole ext-status source, the
                        overview snapshot's file mtime, or the newest oracle
                        run, against --stale-hours (default 48, the same
                        threshold `overview --json` marks a status record
                        stale with). A status record or a whole source with
                        no readable timestamp also counts as stale. The
                        editorial source's own age is deliberately not judged
                        here: its rows carry per-platform verification dates
                        (and, by default, it is the same two source files the
                        platform axes come from), so the 90-day rule above is
                        where that age shows up.

Exit codes (the CLI contract, kept for a plumbing tool): 0 = the board was
generated, including a board whose anomalies are the point of the run;
1 = the board could not be delivered (an output or state write failed, after
the inputs were read); 2 = usage error (an unparseable --now, a repository
root without the two platform sources, argparse's refusals).

Recommended pipeline (the commands carry the private paths, this script
carries none):

  chat-stasher overview --destination <name> --json > overview-snapshot.json
  python3 scripts/dev/scoreboard.py \\
      --root <checkout> \\
      --ext-status <stage>/ext-status \\
      --overview overview-snapshot.json \\
      --oracle oracle/out \\
      --out SCOREBOARD.md

The editorial fields come from `--root`, so the pipeline has nothing to
produce for them; add `--editorial <file>` only to overrule the sources.

The board itself carries machine names, browser/profile labels and
per-platform counts — treat wherever `--out` points the way you treat any
other local working notes about your own archive. This script ships none of
that and knows no paths of its own; every input is handed to it.
"""

from __future__ import annotations

import argparse
import datetime as dt
import importlib.util
import json
import os
import sys
import tempfile
from dataclasses import dataclass, field
from typing import Any

STATE_SUFFIX = ".state.json"
STATE_SCHEMA = 1
EXT_STATUS_SCHEMA = "chat-stasher/ext-status@1"
UNAVAILABLE = "source unavailable"
ABSENT = "-"
KNOWN_TIME_SOURCES = {"exact", "inferred", "messages", "list-updated"}
DEFAULT_STALE_HOURS = 48.0
DEFAULT_QUIET_DAYS = 3.0
DEFAULT_UNKNOWN_SHARE = 0.5
DEFAULT_VERIFY_DAYS = 90.0


class UsageError(Exception):
    """The run cannot start: a bad argument or unreadable platform sources."""


# ---------------------------------------------------------------------------
# Time and number helpers
# ---------------------------------------------------------------------------


def parse_rfc3339(text: Any) -> dt.datetime | None:
    if not isinstance(text, str):
        return None
    try:
        parsed = dt.datetime.fromisoformat(text.strip())
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.astimezone(dt.timezone.utc)


def parse_date(text: Any) -> dt.date | None:
    if not isinstance(text, str):
        return None
    try:
        return dt.date.fromisoformat(text.strip())
    except ValueError:
        return None


def iso_z(moment: dt.datetime) -> str:
    return moment.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def fmt_age(seconds: float) -> str:
    if seconds < 0:
        return "in the future"
    if seconds < 60:
        return f"{seconds:.0f} s"
    if seconds < 48 * 3600:
        return f"{seconds / 3600:.1f} h"
    return f"{seconds / 86400:.1f} d"


def when(moment: dt.datetime | None, now: dt.datetime) -> str:
    if moment is None:
        return "time unreadable"
    delta = (now - moment).total_seconds()
    if delta >= 0:
        return f"{iso_z(moment)} ({fmt_age(delta)} ago)"
    return f"{iso_z(moment)} (ahead of this run's clock by {fmt_age(-delta)})"


def fmt_int(value: int) -> str:
    return f"{value:,}"


def pct(part: int, whole: int) -> str:
    return f"{100.0 * part / whole:.1f}%"


def machine_label(machine: str, display: str | None) -> str:
    """`display (machine)` when the two differ, else the one name twice not."""
    if display and display != machine:
        return f"{display} ({machine})"
    return machine


def esc(text: Any) -> str:
    if text is None:
        return ABSENT
    flat = " ".join(str(text).split())
    return flat.replace("|", "\\|")


# ---------------------------------------------------------------------------
# Platform catalog (the same two sources as the support matrix)
# ---------------------------------------------------------------------------


# The two families a row can belong to. A row's editorial record is filed under
# (family, id): the families carry the same field names because the generator
# normalizes both sources to one shape, but they are two id spaces, and an id
# in both (grok) is two rows with two different records, never one row unioned
# from two files.
FAMILY_LOCAL = "local"
FAMILY_WEB = "web"


@dataclass(frozen=True)
class LocalTool:
    id: str
    display: str
    # The SB-1 editorial fields as the registry states them, already validated
    # and normalized by gen-support-matrix.py (None = nothing recorded).
    verified: dict[str, Any] | None = None
    dev_priority: str | None = None
    known_issue: str | None = None


@dataclass(frozen=True)
class WebPlatform:
    id: str
    channel: str
    credibility: str
    verified: dict[str, Any] | None = None
    dev_priority: str | None = None
    known_issue: str | None = None


@dataclass(frozen=True)
class Catalog:
    local: list[LocalTool]
    web: list[WebPlatform]
    # The two source files this catalog was read from, so the editorial reading
    # can report their age without a second hard-coded copy of the paths.
    sources: dict[str, str] = field(default_factory=dict)

    @property
    def local_ids(self) -> set[str]:
        return {t.id for t in self.local}

    @property
    def web_ids(self) -> set[str]:
        return {p.id for p in self.web}

    @property
    def shared_ids(self) -> set[str]:
        return self.local_ids & self.web_ids


def default_root() -> str:
    return os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def _load_generator(root: str) -> Any:
    path = os.path.join(root, "scripts", "gen-support-matrix.py")
    if not os.path.isfile(path):
        raise UsageError(f"cannot load scripts/gen-support-matrix.py under {root!r}")
    spec = importlib.util.spec_from_file_location("scoreboard_support_matrix_gen", path)
    if spec is None or spec.loader is None:
        raise UsageError(f"cannot load scripts/gen-support-matrix.py under {root!r}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_catalog(root: str) -> Catalog:
    gen = _load_generator(root)
    try:
        harnesses = gen.load_harnesses(root)
        contract = gen.parse_contract_platforms(root)
    except Exception as exc:
        raise UsageError(f"cannot read the platform sources under {root!r}: {exc}") from exc
    local = [
        LocalTool(
            id=h["id"],
            display=h.get("display_name") or h["id"],
            verified=h.get("verified"),
            dev_priority=h.get("dev_priority"),
            known_issue=h.get("known_issue"),
        )
        for h in harnesses
    ]
    # The contract spells the same three fields in camelCase; they are renamed
    # once, here, so the editorial reading has one vocabulary to build from.
    web = [
        WebPlatform(
            id=p["id"],
            channel=p["channel"],
            credibility=p["credibility"],
            verified=p.get("lastVerified"),
            dev_priority=p.get("devPriority"),
            known_issue=p.get("knownIssue"),
        )
        for p in contract
    ]
    # An id CAN appear in both lists (grok the xAI CLI and grok.com the chat
    # platform share it) — the archive's id space itself mixes them, and the
    # board marks those cells rather than pretending the two are one.
    sources = {
        "harness registry": os.path.join(root, gen.REGISTRY_REL),
        "extension contract": os.path.join(root, gen.CONTRACT_REL),
    }
    return Catalog(local=local, web=web, sources=sources)


# ---------------------------------------------------------------------------
# Input 1: the native host's ext-status reports
# ---------------------------------------------------------------------------


@dataclass
class PlatformReading:
    platform: str
    captured: int
    pending: int
    paused_reason: str | None


@dataclass
class InstallRecord:
    machine: str
    install_id: str
    browser: str
    profile_label: str | None
    extension_version: str
    reported_at_raw: str
    reported_at: dt.datetime | None
    reported_daily: bool
    rows: dict[str, PlatformReading]
    identity_conflict: bool
    conflict_evidence: str | None
    legacy_migration: bool

    @property
    def label(self) -> str:
        label = self.browser or "(no browser name)"
        if self.profile_label:
            label = f"{label} · {self.profile_label}"
        return label

    @property
    def short_id(self) -> str:
        return self.install_id[:8] + "…" if len(self.install_id) > 8 else self.install_id


@dataclass
class RecordProblem:
    where: str
    why: str


@dataclass
class ExtStatusReading:
    available: bool
    reason: str | None
    records: list[InstallRecord] = field(default_factory=list)
    unreadable: list[RecordProblem] = field(default_factory=list)
    superseded_legacy: int = 0
    shared_install_ids: set[str] = field(default_factory=set)
    unknown_platform_ids: set[str] = field(default_factory=set)

    def reporting(self, platform: str) -> list[InstallRecord]:
        return [r for r in self.records if platform in r.rows]


def _parse_platform_rows(value: Any) -> dict[str, PlatformReading] | None:
    if not isinstance(value, list):
        return None
    rows: dict[str, PlatformReading] = {}
    for row in value:
        if not isinstance(row, dict):
            return None
        platform = row.get("platform")
        captured = row.get("captured_by_this_browser")
        pending = row.get("pending")
        paused = row.get("paused_reason")
        if not isinstance(platform, str) or not platform:
            return None
        if not isinstance(captured, int) or isinstance(captured, bool) or captured < 0:
            return None
        if not isinstance(pending, int) or isinstance(pending, bool) or pending < 0:
            return None
        if paused is not None and not isinstance(paused, str):
            return None
        rows[platform] = PlatformReading(platform, captured, pending, paused)
    return rows


def _read_one_status_record(
    path: str, machine_dir: str | None
) -> tuple[InstallRecord | None, RecordProblem | None]:
    """Read one record file. The identity checks mirror the archive reader in
    the CLI (`main.rs` `read_overview_indexes`): schema, the file name's
    install id, and the machine key. A mismatch is a visible problem, never an
    imagined row."""
    name = os.path.basename(path)
    where = f"{machine_dir}/{name}" if machine_dir is not None else name
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
    except OSError as exc:
        return None, RecordProblem(where, f"unreadable file: {exc.strerror or exc}")
    try:
        parsed = json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        return None, RecordProblem(where, f"not valid JSON: {exc}")
    if not isinstance(parsed, dict):
        return None, RecordProblem(where, "not a JSON object")
    schema = parsed.get("schema")
    if schema != EXT_STATUS_SCHEMA:
        return None, RecordProblem(where, f"schema is {schema!r}, not {EXT_STATUS_SCHEMA}")
    install_id = parsed.get("install_id")
    stem = os.path.splitext(name)[0]
    if not isinstance(install_id, str) or not install_id:
        return None, RecordProblem(where, "no install_id")
    if install_id != stem:
        return None, RecordProblem(where, "install_id does not match its file name")
    recorded_machine = parsed.get("machine")
    if not isinstance(recorded_machine, str) or not recorded_machine:
        return None, RecordProblem(where, "no machine recorded in the report")
    if machine_dir is not None and recorded_machine != machine_dir:
        return (
            None,
            RecordProblem(
                where,
                f"record names machine {recorded_machine!r} but is filed under {machine_dir!r}",
            ),
        )
    rows = _parse_platform_rows(parsed.get("platforms"))
    if rows is None:
        return None, RecordProblem(where, "a platform row is malformed")
    browser = parsed.get("browser")
    if not isinstance(browser, str):
        browser = ""
    profile = parsed.get("profile_label")
    if not isinstance(profile, str):
        profile = None
    version = parsed.get("extension_version")
    if not isinstance(version, str):
        version = ""
    reported_raw = parsed.get("reported_at")
    reported_at = parse_rfc3339(reported_raw)
    reported_daily = parsed.get("reported_daily") is True
    conflict = parsed.get("identity_conflict")
    evidence = parsed.get("identity_conflict_evidence")
    migration = parsed.get("legacy_migration")
    record = InstallRecord(
        machine=recorded_machine,
        install_id=install_id,
        browser=browser,
        profile_label=profile,
        extension_version=version,
        reported_at_raw=reported_raw if isinstance(reported_raw, str) else "",
        reported_at=reported_at,
        reported_daily=reported_daily,
        rows=rows,
        identity_conflict=(conflict is True),
        conflict_evidence=evidence if isinstance(evidence, str) else None,
        legacy_migration=(migration is True),
    )
    return record, None


def read_ext_status(path: str | None) -> ExtStatusReading:
    if path is None:
        return ExtStatusReading(False, "not provided (--ext-status)")
    if not os.path.isdir(path):
        return ExtStatusReading(False, f"{path} is not a directory")
    try:
        top = sorted(os.listdir(path))
    except OSError as exc:
        return ExtStatusReading(False, f"cannot list {path}: {exc.strerror or exc}")
    problems: list[RecordProblem] = []
    keyed_files: list[tuple[str, str]] = []
    flat_files: list[str] = []
    for name in top:
        if name.startswith("."):
            continue
        full = os.path.join(path, name)
        if os.path.isdir(full):
            try:
                listing = sorted(os.listdir(full))
            except OSError as exc:
                problems.append(RecordProblem(name, f"cannot list machine directory: {exc.strerror or exc}"))
                continue
            for file_name in listing:
                if file_name.startswith(".") or not file_name.endswith(".json"):
                    continue
                keyed_files.append((name, os.path.join(full, file_name)))
        elif name.endswith(".json"):
            flat_files.append(full)
    records: dict[tuple[str, str], tuple[InstallRecord, bool]] = {}
    order: list[tuple[str, str]] = []
    superseded = 0
    for machine_dir, full in keyed_files:
        record, problem = _read_one_status_record(full, machine_dir)
        if record is None:
            problems.append(problem)
            continue
        key = (record.machine, record.install_id)
        if key not in records:
            records[key] = (record, False)
            order.append(key)
    for full in flat_files:
        record, problem = _read_one_status_record(full, None)
        if record is None:
            problems.append(problem)
            continue
        key = (record.machine, record.install_id)
        if key in records:
            # The keyed record is the later observation of the same
            # (machine, install); the flat file survives on disk on purpose
            # and is not listed twice.
            superseded += 1
            continue
        records[key] = (record, True)
        order.append(key)
    listed = [records[key][0] for key in order]
    listed.sort(key=lambda r: (r.machine, r.install_id))
    by_id: dict[str, set[str]] = {}
    for record in listed:
        by_id.setdefault(record.install_id, set()).add(record.machine)
    shared = {install_id for install_id, machines in by_id.items() if len(machines) > 1}
    reading = ExtStatusReading(
        True,
        None,
        records=listed,
        unreadable=problems,
        superseded_legacy=superseded,
        shared_install_ids=shared,
    )
    return reading


# ---------------------------------------------------------------------------
# Input 2: the saved overview document
# ---------------------------------------------------------------------------


@dataclass
class SessionObservation:
    machine: str
    machine_display: str
    harness: str
    known: bool
    no_content: bool
    anchor: dt.datetime | None


@dataclass
class OverviewReading:
    available: bool
    reason: str | None
    mtime: dt.datetime | None
    sessions: list[SessionObservation] = field(default_factory=list)
    machines: list[str] = field(default_factory=list)
    display: dict[str, str] = field(default_factory=dict)
    conversations_by_harness: dict[str, int] | None = None
    malformed_rows: int = 0

    def sessions_for_harness(self, harness: str) -> list[SessionObservation]:
        return [s for s in self.sessions if s.harness == harness]


def _classify_time(session: dict[str, Any]) -> tuple[bool, bool, dt.datetime | None]:
    """Mirrors the CLI's `has_known_time` / ADR-35 split, as read back from
    the serialized tri-state."""
    source = session.get("time_source")
    kind = source.get("kind") if isinstance(source, dict) else None
    known_bounds: list[int] = []
    for key in ("first_unix", "last_unix"):
        value = session.get(key)
        if isinstance(value, dict) and value.get("kind") == "known" and isinstance(value.get("unix"), int):
            known_bounds.append(value["unix"])
    known = kind in KNOWN_TIME_SOURCES and bool(known_bounds)
    if known:
        return True, False, dt.datetime.fromtimestamp(min(known_bounds), tz=dt.timezone.utc)
    kind_is_no_content = kind == "no_conversation_content"
    if kind_is_no_content:
        return False, True, None
    return False, False, None


def read_overview(path: str | None) -> OverviewReading:
    if path is None:
        return OverviewReading(False, "not provided (--overview)", None)
    if not os.path.isfile(path):
        return OverviewReading(False, f"{path} is not a file", None)
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
    except OSError as exc:
        return OverviewReading(False, f"unreadable: {exc.strerror or exc}", None)
    try:
        parsed = json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        return OverviewReading(False, f"not valid JSON: {exc}", None)
    if not isinstance(parsed, dict):
        return OverviewReading(False, "not a JSON object", None)
    command = parsed.get("command")
    if command not in (None, "overview"):
        return OverviewReading(False, f"not an overview document (command={command!r})", None)
    exit_code = parsed.get("exit_code")
    # exit 3 ships overview_error_json: the archive was not read to
    # completion, and nothing below may be read as a measured zero. exit 1 is
    # a real empty ("read to the end and no index exists anywhere"). exit 2 is
    # the CLI contract's usage error and is named as one; a missing exit code
    # is named as missing rather than guessed, in either direction.
    if not isinstance(exit_code, int) or exit_code not in (0, 1):
        if exit_code == 2:
            lead = "usage error"
        elif isinstance(exit_code, int):
            lead = "did not finish reading"
        else:
            lead = "no readable exit code"
        reason = f"{lead} (exit {exit_code!r}"
        kind = parsed.get("error_kind")
        error = parsed.get("error")
        if isinstance(kind, str):
            reason += f", {kind}"
        if isinstance(error, str):
            reason += f": {error}"
        return OverviewReading(False, reason + ")", None)
    if not isinstance(parsed.get("sessions"), list):
        return OverviewReading(
            False,
            f"the overview document has no per-session rows (variant {parsed.get('variant')!r}); "
            "the scoreboard needs the full --json document, not --summary",
            None,
        )
    reading = OverviewReading(True, None, None)
    reading.mtime = dt.datetime.fromtimestamp(os.stat(path).st_mtime, tz=dt.timezone.utc)
    seen_machines: list[str] = []
    malformed = 0
    for raw_session in parsed["sessions"]:
        if not isinstance(raw_session, dict):
            malformed += 1
            continue
        machine = raw_session.get("machine")
        harness = raw_session.get("harness")
        display = raw_session.get("machine_display")
        if not isinstance(machine, str) or not machine or not isinstance(harness, str) or not harness:
            malformed += 1
            continue
        if machine not in reading.display:
            reading.display[machine] = display if isinstance(display, str) and display else machine
            seen_machines.append(machine)
        known, no_content, anchor = _classify_time(raw_session)
        reading.sessions.append(
            SessionObservation(
                machine=machine,
                machine_display=reading.display[machine],
                harness=harness,
                known=known,
                no_content=no_content,
                anchor=anchor,
            )
        )
    reading.machines = seen_machines
    reading.malformed_rows = malformed
    conversations = parsed.get("conversations")
    if isinstance(conversations, list):
        counts: dict[str, int] = {}
        for conversation in conversations:
            if isinstance(conversation, dict) and isinstance(conversation.get("harness"), str):
                harness = conversation["harness"]
                counts[harness] = counts.get(harness, 0) + 1
        reading.conversations_by_harness = counts
    return reading


# ---------------------------------------------------------------------------
# Input 3: oracle comparison results
# ---------------------------------------------------------------------------


@dataclass
class OracleResult:
    platform: str
    file_name: str
    recall_text: str
    short: int
    red: int
    window_text: str
    generated_at: dt.datetime | None
    missing_class: str | None


@dataclass
class OracleReading:
    available: bool
    reason: str | None
    results: list[OracleResult] = field(default_factory=list)
    unreadable: list[str] = field(default_factory=list)
    newest_generated: dt.datetime | None = None

    def for_platform(self, platform: str) -> list[OracleResult]:
        return sorted(
            [r for r in self.results if r.platform == platform],
            key=lambda r: (r.generated_at is None, r.generated_at or dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc)),
            reverse=True,
        )


def read_oracle(paths: list[str] | None) -> OracleReading:
    if not paths:
        return OracleReading(False, "not provided (--oracle)")
    files: list[str] = []
    problems: list[str] = []
    for path in paths:
        if os.path.isdir(path):
            try:
                listing = sorted(os.listdir(path))
            except OSError as exc:
                problems.append(f"{path}: cannot list ({exc.strerror or exc})")
                continue
            files.extend(
                os.path.join(path, name)
                for name in listing
                if not name.startswith(".") and name.endswith(".json")
            )
        elif os.path.isfile(path):
            files.append(path)
        else:
            problems.append(f"{path} is not a file or directory")
    if problems and not files:
        return OracleReading(False, "; ".join(problems))
    if not files:
        return OracleReading(False, "the path holds no .json result files")
    reading = OracleReading(True, None)
    for full in files:
        name = os.path.basename(full)
        try:
            with open(full, "rb") as handle:
                parsed = json.loads(handle.read())
        except (OSError, json.JSONDecodeError, UnicodeDecodeError) as exc:
            problems.append(f"{name}: unreadable ({exc})")
            continue
        if not isinstance(parsed, dict) or not isinstance(parsed.get("platform"), str):
            problems.append(f"{name}: not a comparison result (no platform)")
            continue
        recall = parsed.get("recall")
        if not isinstance(recall, dict):
            problems.append(f"{name}: no recall object")
            continue
        found = recall.get("found")
        expected = recall.get("expected")
        if not isinstance(found, int) or isinstance(found, bool) or not isinstance(expected, int) or isinstance(expected, bool):
            problems.append(f"{name}: recall counts are not integers")
            continue
        if expected > 0:
            recall_text = f"{fmt_int(found)}/{fmt_int(expected)} = {pct(found, expected)}"
        else:
            recall_text = f"n/a (found {fmt_int(found)}, expected 0)"
        short = red = 0
        counts = parsed.get("content_short_counts")
        if isinstance(counts, dict):
            if isinstance(counts.get("total"), int):
                short = counts["total"]
            if isinstance(counts.get("RED"), int):
                red = counts["RED"]
        window = parsed.get("window")
        if isinstance(window, list) and window:
            window_text = " → ".join(b if isinstance(b, str) else "…" for b in window)
        else:
            window_text = ABSENT
        generated_at = parse_rfc3339(parsed.get("generated_at"))
        if generated_at is not None and (reading.newest_generated is None or generated_at > reading.newest_generated):
            reading.newest_generated = generated_at
        missing_class = parsed.get("missing_class")
        reading.results.append(
            OracleResult(
                platform=parsed["platform"],
                file_name=name,
                recall_text=recall_text,
                short=short,
                red=red,
                window_text=window_text,
                generated_at=generated_at,
                missing_class=missing_class if isinstance(missing_class, str) else None,
            )
        )
    reading.unreadable = problems
    return reading


# ---------------------------------------------------------------------------
# Input 4: SB-1 editorial fields
# ---------------------------------------------------------------------------


@dataclass
class EditorialEntry:
    platform: str
    # Which row this entry belongs to: FAMILY_LOCAL / FAMILY_WEB for a row read
    # from its own source, "" for an override row, which is keyed by bare id
    # and governs every row carrying that id.
    family: str = ""
    fields: dict[str, str] = field(default_factory=dict)
    last_verified: dt.date | None = None
    last_verified_raw: str | None = None

    @property
    def dated(self) -> bool:
        return self.last_verified is not None


@dataclass
class EditorialReading:
    available: bool
    reason: str | None
    mtime: dt.datetime | None
    # Keyed (family, id) so a lookup never has to ask which source answered.
    entries: dict[tuple[str, str], EditorialEntry] = field(default_factory=dict)
    # One row per recorded entry, in source order, for the rules and notes that
    # are about the reading as a whole rather than one table cell.
    rows: list[EditorialEntry] = field(default_factory=list)
    newest_last_verified: dt.date | None = None
    # Where the fields came from — the override file, or the two SB-1 sources.
    origin: str | None = None
    derived: bool = False

    def for_row(self, family: str, platform: str) -> EditorialEntry | None:
        if not self.available:
            return None
        return self.entries.get((family, platform))

    def label(self, entry: EditorialEntry, catalog: Catalog) -> str:
        """The entry's name in a list.

        An id that is both a local tool and a web platform is two rows with two
        records; a sentence that names one of them has to say which, exactly as
        the tables mark that pair rather than pretending they are one.
        """
        if entry.family and entry.platform in catalog.shared_ids:
            kind = "local tool" if entry.family == FAMILY_LOCAL else "web platform"
            return f"{entry.platform} ({kind})"
        return entry.platform


def _editorial_entry(
    family: str,
    platform: str,
    verified: dict[str, Any] | None,
    dev_priority: str | None,
    known_issue: str | None,
) -> EditorialEntry | None:
    """One entry from a source record, or None when nothing is recorded.

    None is "this row carries no editorial field at all", which the tables
    render as a dash — never as a guess, and never as the same thing as a
    source that could not be read.
    """
    fields: dict[str, str] = {}
    if dev_priority:
        fields["dev_priority"] = dev_priority
    if known_issue:
        # One internal vocabulary for both families, so a note or a cell reads
        # the same whichever source answered.
        fields["known_issues"] = known_issue
    raw_date = (verified or {}).get("date")
    if not fields and not isinstance(raw_date, str):
        return None
    entry = EditorialEntry(platform=platform, family=family, fields=fields)
    if isinstance(raw_date, str) and raw_date:
        # The generator validates this shape at load time; it is kept as
        # raw-plus-parsed anyway, so a date that somehow will not parse shows
        # as written instead of turning into an absent one.
        entry.last_verified_raw = raw_date
        entry.last_verified = parse_date(raw_date)
    return entry


def _finish_reading(reading: EditorialReading) -> EditorialReading:
    for entry in reading.rows:
        if entry.last_verified is not None and (
            reading.newest_last_verified is None or entry.last_verified > reading.newest_last_verified
        ):
            reading.newest_last_verified = entry.last_verified
    return reading


def _read_editorial_override(path: str) -> EditorialReading:
    if not os.path.isfile(path):
        return EditorialReading(False, f"{path} is not a file", None)
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
    except OSError as exc:
        return EditorialReading(False, f"unreadable: {exc.strerror or exc}", None)
    try:
        parsed = json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        return EditorialReading(False, f"not valid JSON: {exc}", None)
    if not isinstance(parsed, dict) or not isinstance(parsed.get("platforms"), dict):
        return EditorialReading(False, "no `platforms` object", None)
    reading = EditorialReading(True, None, None, origin=f"override file {path}")
    reading.mtime = dt.datetime.fromtimestamp(os.stat(path).st_mtime, tz=dt.timezone.utc)
    for platform, entry in parsed["platforms"].items():
        if not isinstance(platform, str) or not isinstance(entry, dict):
            continue
        value = EditorialEntry(platform=platform)
        for key in ("status", "dev_priority", "known_issues"):
            if isinstance(entry.get(key), str) and entry[key]:
                value.fields[key] = entry[key]
        raw_date = entry.get("last_verified")
        if isinstance(raw_date, str) and raw_date:
            value.last_verified_raw = raw_date
            value.last_verified = parse_date(raw_date)
        reading.rows.append(value)
        # An override is keyed by bare id, so it answers for every row that
        # carries the id — including both rows of a shared one.
        reading.entries[(FAMILY_LOCAL, platform)] = value
        reading.entries[(FAMILY_WEB, platform)] = value
    return _finish_reading(reading)


def _read_editorial_from_sources(catalog: Catalog, root: str) -> EditorialReading:
    reading = EditorialReading(True, None, None, origin=f"the SB-1 sources under {root}", derived=True)
    for tool in catalog.local:
        entry = _editorial_entry(FAMILY_LOCAL, tool.id, tool.verified, tool.dev_priority, tool.known_issue)
        if entry is not None:
            reading.entries[(FAMILY_LOCAL, tool.id)] = entry
            reading.rows.append(entry)
    for platform in catalog.web:
        entry = _editorial_entry(
            FAMILY_WEB, platform.id, platform.verified, platform.dev_priority, platform.known_issue
        )
        if entry is not None:
            reading.entries[(FAMILY_WEB, platform.id)] = entry
            reading.rows.append(entry)
    mtimes = []
    for path in catalog.sources.values():
        try:
            mtimes.append(os.stat(path).st_mtime)
        except OSError:
            continue
    if mtimes:
        reading.mtime = dt.datetime.fromtimestamp(max(mtimes), tz=dt.timezone.utc)
    return _finish_reading(reading)


def read_editorial(path: str | None, catalog: Catalog, root: str) -> EditorialReading:
    if path is not None:
        # An override that cannot be read is unavailable. Falling back to the
        # built-in reading would answer a question nobody asked.
        return _read_editorial_override(path)
    return _read_editorial_from_sources(catalog, root)


# ---------------------------------------------------------------------------
# Previous-run state
# ---------------------------------------------------------------------------


@dataclass
class StateReading:
    path: str | None
    enabled: bool
    previous: dict[str, Any] | None
    problem: str | None


def resolve_state(args: argparse.Namespace) -> StateReading:
    if args.no_state:
        return StateReading(None, False, None, None)
    if args.state:
        return StateReading(args.state, True, None, None)
    if args.out:
        return StateReading(args.out + STATE_SUFFIX, True, None, None)
    return StateReading(None, False, None, None)


def load_state(state: StateReading) -> StateReading:
    if not state.enabled or state.path is None:
        return state
    if not os.path.isfile(state.path):
        return StateReading(state.path, True, None, "no previous state file — first run against this output")
    try:
        with open(state.path, "r", encoding="utf-8") as handle:
            parsed = json.load(handle)
    except (OSError, json.JSONDecodeError, UnicodeDecodeError) as exc:
        return StateReading(state.path, True, None, f"previous state unreadable ({exc}); replaced below")
    if (
        not isinstance(parsed, dict)
        or parsed.get("schema_version") != STATE_SCHEMA
        or not isinstance(parsed.get("installs"), dict)
    ):
        return StateReading(state.path, True, None, "previous state has an unknown shape; replaced below")
    return StateReading(state.path, True, parsed, None)


# ---------------------------------------------------------------------------
# Evaluation
# ---------------------------------------------------------------------------


@dataclass
class PlatformTrack:
    captured: int | None = None
    pending: int | None = None
    first_seen: dt.datetime | None = None
    last_seen: dt.datetime | None = None
    last_advanced: dt.datetime | None = None
    currently_reported: bool = False


@dataclass
class InstallTrack:
    first_seen: dt.datetime | None = None
    last_seen: dt.datetime | None = None
    present: bool = False
    platforms: dict[str, PlatformTrack] = field(default_factory=dict)


@dataclass
class Evaluation:
    anomalies: list[str] = field(default_factory=list)
    not_evaluable: list[str] = field(default_factory=list)
    tracks: dict[tuple[str, str], InstallTrack] = field(default_factory=dict)
    tool_newest: dict[str, tuple[dt.datetime, str] | None] = field(default_factory=dict)
    cell_stats: dict[tuple[str, str], tuple[int, int, int]] = field(default_factory=dict)
    new_state: dict[str, Any] | None = None
    sources: list[tuple[str, str, str]] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)


def _restore_tracks(previous: dict[str, Any] | None) -> dict[tuple[str, str], InstallTrack]:
    tracks: dict[tuple[str, str], InstallTrack] = {}
    if not previous:
        return tracks
    installs = previous.get("installs")
    if not isinstance(installs, dict):
        return tracks
    for machine, per_install in installs.items():
        if not isinstance(machine, str) or not isinstance(per_install, dict):
            continue
        for install_id, entry in per_install.items():
            if not isinstance(install_id, str) or not isinstance(entry, dict):
                continue
            track = InstallTrack(
                first_seen=parse_rfc3339(entry.get("first_seen")),
                last_seen=parse_rfc3339(entry.get("last_seen")),
                present=False,
            )
            platform_entries = entry.get("platforms")
            if isinstance(platform_entries, dict):
                for platform, platform_entry in platform_entries.items():
                    if not isinstance(platform, str) or not isinstance(platform_entry, dict):
                        continue
                    track.platforms[platform] = PlatformTrack(
                        captured=platform_entry.get("captured") if isinstance(platform_entry.get("captured"), int) else None,
                        pending=platform_entry.get("pending") if isinstance(platform_entry.get("pending"), int) else None,
                        first_seen=parse_rfc3339(platform_entry.get("first_seen")),
                        last_seen=parse_rfc3339(platform_entry.get("last_seen")),
                        last_advanced=parse_rfc3339(platform_entry.get("last_advanced")),
                    )
            tracks[(machine, install_id)] = track
    return tracks


def evaluate(args: argparse.Namespace, ext: ExtStatusReading, overview: OverviewReading,
             oracle: OracleReading, editorial: EditorialReading, state: StateReading) -> Evaluation:
    now = args.parsed_now
    ev = Evaluation()
    tracks = _restore_tracks(state.previous)
    ev.tracks = tracks

    # -- pending rises and capture advancement (web, through the state) -----
    previous = state.previous or {}
    for record in ext.records:
        key = (record.machine, record.install_id)
        track = tracks.setdefault(key, InstallTrack())
        track.present = True
        if track.first_seen is None:
            track.first_seen = now
        track.last_seen = now
        previous_install = (
            previous.get("installs", {}).get(record.machine, {}).get(record.install_id)
            if isinstance(previous, dict)
            else None
        )
        previous_platforms = (
            previous_install.get("platforms")
            if isinstance(previous_install, dict) and isinstance(previous_install.get("platforms"), dict)
            else {}
        )
        previous_seen = parse_rfc3339(previous_install.get("last_seen")) if isinstance(previous_install, dict) else None
        for platform_name, reading in record.rows.items():
            platform_track = track.platforms.setdefault(platform_name, PlatformTrack())
            platform_track.currently_reported = True
            if platform_track.first_seen is None:
                platform_track.first_seen = now
            platform_track.last_seen = now
            before = previous_platforms.get(platform_name)
            if isinstance(before, dict):
                old_pending = before.get("pending")
                old_captured = before.get("captured")
                if isinstance(old_pending, int) and reading.pending > old_pending:
                    gap = f" {fmt_age((now - previous_seen).total_seconds())} after the previous run" if previous_seen else ""
                    ev.anomalies.append(
                        f"pending-rising: {record.machine} / install {record.short_id} ({record.label}) / "
                        f"{platform_name} — pending {fmt_int(old_pending)} → {fmt_int(reading.pending)}{gap}"
                    )
                if isinstance(old_captured, int) and reading.captured > old_captured:
                    platform_track.last_advanced = now
            platform_track.captured = reading.captured
            platform_track.pending = reading.pending
        for platform_name, platform_track in track.platforms.items():
            if platform_name not in record.rows:
                platform_track.currently_reported = False

    if state.enabled and state.path is not None:
        installs_state: dict[str, Any] = {}
        for (machine, install_id), track in tracks.items():
            platforms_state: dict[str, Any] = {}
            for platform_name, p in track.platforms.items():
                platforms_state[platform_name] = {
                    "captured": p.captured,
                    "pending": p.pending,
                    "first_seen": iso_z(p.first_seen) if p.first_seen else None,
                    "last_seen": iso_z(p.last_seen) if p.last_seen else None,
                    "last_advanced": iso_z(p.last_advanced) if p.last_advanced else None,
                }
            installs_state.setdefault(machine, {})[install_id] = {
                "first_seen": iso_z(track.first_seen) if track.first_seen else None,
                "last_seen": iso_z(track.last_seen) if track.last_seen else None,
                "present": track.present,
                "platforms": platforms_state,
            }
        ev.new_state = {
            "schema_version": STATE_SCHEMA,
            "updated_at": iso_z(now),
            "installs": installs_state,
        }

    # -- no-new-capture (web platforms) -------------------------------------
    # Judged only when the ext-status source was readable this run: memory
    # from previous runs may summarize, but a missing input must not decide.
    quiet_seconds = args.quiet_days * 86400
    web_never_tracked: list[str] = []
    web_too_young: list[str] = []
    if ext.available:
        for platform in args.catalog.web:
            platform_tracks = [t.platforms[platform.id] for t in tracks.values() if platform.id in t.platforms]
            if not platform_tracks:
                web_never_tracked.append(platform.id)
                continue
            advances = [p.last_advanced for p in platform_tracks if p.last_advanced is not None]
            firsts = [p.first_seen for p in platform_tracks if p.first_seen is not None]
            if advances:
                last = max(advances)
                age = (now - last).total_seconds()
                if age > quiet_seconds:
                    reporting = ext.reporting(platform.id)
                    ev.anomalies.append(
                        f"no-new-capture: {platform.id} — last capture advance observed {fmt_age(age)} ago "
                        f"({iso_z(last)}); {len(reporting)} install(s) report this platform now"
                    )
                continue
            if not firsts:
                continue
            tracked_age = (now - min(firsts)).total_seconds()
            if tracked_age > quiet_seconds:
                ev.anomalies.append(
                    f"no-new-capture: {platform.id} — no capture observed in any run while tracked "
                    f"({fmt_age(tracked_age)}, since {iso_z(min(firsts))})"
                )
            else:
                web_too_young.append(f"{platform.id} (tracked {fmt_age(tracked_age)})")

    # -- no-new-capture (local tools) + per-machine xy cells -----------------
    for session in overview.sessions:
        key = (session.machine, session.harness)
        total, unknown, no_content = ev.cell_stats.get(key, (0, 0, 0))
        ev.cell_stats[key] = (total + 1, unknown + (0 if session.known or session.no_content else 1), no_content + (1 if session.no_content else 0))
    for tool in args.catalog.local:
        rows = overview.sessions_for_harness(tool.id)
        anchors = [(s.anchor, s.machine) for s in rows if s.known and s.anchor is not None]
        if not anchors:
            ev.tool_newest[tool.id] = None
            continue
        newest = max(anchors, key=lambda pair: pair[0])
        ev.tool_newest[tool.id] = newest
        age = (now - newest[0]).total_seconds()
        if age > quiet_seconds:
            ev.anomalies.append(
                f"no-new-capture: {tool.id} — newest archived session content is {fmt_age(age)} old "
                f"({iso_z(newest[0])}, machine {newest[1]})"
            )
    for (machine, harness), (total, unknown, no_content) in sorted(ev.cell_stats.items()):
        denominator = total - no_content
        if denominator <= 0 or unknown == 0:
            continue
        share = unknown / denominator
        if share > args.unknown_share:
            label = machine_label(machine, overview.display.get(machine))
            ev.anomalies.append(
                f"time-unknown-share: {label} / {harness} — "
                f"{fmt_int(unknown)} of {fmt_int(denominator)} conversations time-unknown ({pct(unknown, denominator)})"
            )

    # -- verification age -----------------------------------------------------
    missing_verified: list[str] = []
    if editorial.available:
        for entry in editorial.rows:
            label = editorial.label(entry, args.catalog)
            if entry.last_verified is None:
                if entry.last_verified_raw:
                    missing_verified.append(f"{label} (unparseable {entry.last_verified_raw!r})")
                else:
                    missing_verified.append(label)
                continue
            age = (now.date() - entry.last_verified).days
            if age > args.verify_days:
                ev.anomalies.append(
                    f"verification-stale: {label} — last verified {entry.last_verified.isoformat()} "
                    f"({fmt_age(age * 86400.0)} ago, above the {args.verify_days:.0f} d window)"
                )

    # -- source availability, freshness, and their anomalies ------------------
    ev.sources, source_anomalies = _source_rows(args, ext, overview, oracle, editorial, state, now)
    ev.anomalies.extend(source_anomalies)

    # -- per-install report age (a status report past the freshness limit) ----
    # Any install's report is judged, whatever its cadence: the limit comes
    # from the freshness contract, not from how often the install promised to
    # speak. `reported_daily` only names a silent reporter that had built a
    # daily streak; it never exempts anyone from the limit. A record with no
    # readable time is stale here too, not only as part of a whole stale
    # source — a missing measurement is never freshness.
    stale_seconds = args.stale_hours * 3600
    for record in ext.records:
        if record.reported_at is None:
            ev.anomalies.append(
                f"source-stale: install “{record.label}” on {record.machine} — report time unreadable, "
                "counted as stale"
            )
            continue
        report_age = (now - record.reported_at).total_seconds()
        if report_age > stale_seconds:
            daily_clause = "; this install used to report daily" if record.reported_daily else ""
            ev.anomalies.append(
                f"source-stale: install “{record.label}” on {record.machine} — no report for "
                f"{fmt_age(report_age)} (limit {args.stale_hours:.0f} h{daily_clause})"
            )

    # -- notes ------------------------------------------------------------------
    cat_web_ids = args.catalog.web_ids
    for record in ext.records:
        for platform_name in record.rows:
            if platform_name not in cat_web_ids:
                ext.unknown_platform_ids.add(platform_name)
    if ext.shared_install_ids:
        names = ", ".join(sorted(i[:8] + "…" for i in ext.shared_install_ids))
        ev.notes.append(
            f"install id(s) reported from more than one machine: {names}. Every record is listed "
            "separately and never merged (the reader-side rule of the extension protocol)"
        )
    for problem in ext.unreadable:
        ev.notes.append(f"unreadable status record: {problem.where} — {problem.why} (kept visible, never counted as a row)")
    for (machine, install_id), track in sorted(tracks.items()):
        if not track.present:
            ev.notes.append(
                f"install {install_id[:8]}… on {machine} appears in previous-run state but not in this run's "
                f"input (last seen {when(track.last_seen, now)}; kept visible, never guessed)"
            )
    for record in ext.records:
        if record.identity_conflict and record.conflict_evidence:
            ev.notes.append(
                f"identity conflict reported by the host for install {record.short_id} on {record.machine}: "
                f"{record.conflict_evidence}"
            )
    if ext.available and ext.unknown_platform_ids:
        names = ", ".join(sorted(ext.unknown_platform_ids))
        ev.notes.append(f"platform rows with ids outside the extension's table: {names} (listed, not judged)")
    for problem in oracle.unreadable:
        ev.notes.append(f"unreadable oracle file: {problem}")
    if editorial.available:
        unknown_editorial = [
            e
            for e in editorial.rows
            if e.platform not in cat_web_ids and e.platform not in args.catalog.local_ids
        ]
        for value in sorted(unknown_editorial, key=lambda e: e.platform):
            parts = [f"{k}={v}" for k, v in sorted(value.fields.items())]
            shown = f" ({'; '.join(parts)})" if parts else ""
            ev.notes.append(f"editorial row for an id this product does not know: {value.platform}{shown}")

    # -- not-evaluable bookkeeping ----------------------------------------------
    if not ext.available:
        ev.not_evaluable.append("pending-rising + no-new-capture (web) — extension status reports are unavailable")
    elif not state.enabled:
        ev.not_evaluable.append(
            "pending-rising + no-new-capture (web) — no state file (state disabled: --no-state, or stdout without --out or --state)"
        )
    elif state.previous is None:
        if state.problem and "first run" not in state.problem:
            ev.not_evaluable.append(f"pending-rising + no-new-capture (web) — {state.problem}")
        else:
            ev.not_evaluable.append(
                "pending-rising + no-new-capture (web) — first run against this output: counters are baseline only, "
                "movement is measured from the next run"
            )
    if web_never_tracked:
        ev.not_evaluable.append(
            "no-new-capture (web) — platforms no install has ever reported: " + ", ".join(web_never_tracked)
        )
    if web_too_young:
        ev.not_evaluable.append("no-new-capture (web) — tracking younger than the quiet window: " + ", ".join(web_too_young))
    if not overview.available:
        ev.not_evaluable.append("time-unknown-share + no-new-capture (local tools) — overview document is unavailable")
    else:
        no_known_time: list[str] = []
        for tool in args.catalog.local:
            rows = overview.sessions_for_harness(tool.id)
            if rows and ev.tool_newest.get(tool.id) is None:
                no_known_time.append(tool.id)
        if no_known_time:
            ev.not_evaluable.append(
                "no-new-capture (local tools) — sessions exist but none carries a known time, so no age can be read: "
                + ", ".join(no_known_time)
            )
    if not editorial.available:
        ev.not_evaluable.append("verification-stale — editorial fields are unavailable")
    elif missing_verified:
        ev.not_evaluable.append("verification-stale — no readable last_verified date for: " + ", ".join(missing_verified))
    elif not editorial.rows:
        ev.not_evaluable.append(
            f"verification-stale — the editorial source ({editorial.origin}) records no editorial row at all"
        )
    if not oracle.available:
        ev.not_evaluable.append("oracle completeness — oracle comparison results are unavailable")

    return ev


def _source_rows(
    args: argparse.Namespace,
    ext: ExtStatusReading,
    overview: OverviewReading,
    oracle: OracleReading,
    editorial: EditorialReading,
    state: StateReading,
    now: dt.datetime,
) -> tuple[list[tuple[str, str, str]], list[str]]:
    rows: list[tuple[str, str, str]] = []
    anomalies: list[str] = []
    stale_limit = args.stale_hours * 3600

    # extension status reports
    if not ext.available:
        rows.append(("extension status reports", f"{UNAVAILABLE} — {ext.reason}", "n/a"))
        anomalies.append(f"source-unavailable: extension status reports — {ext.reason}")
    else:
        machines = {r.machine for r in ext.records}
        state_text = f"available · {len(ext.records)} install(s) on {len(machines)} machine(s)"
        if ext.superseded_legacy:
            state_text += f" · {ext.superseded_legacy} superseded legacy record(s) kept on disk"
        if ext.unreadable:
            state_text += f" · {len(ext.unreadable)} unreadable record(s)"
        report_times = [r.reported_at for r in ext.records if r.reported_at is not None]
        unreadable_time = len(ext.records) - len(report_times)
        if report_times:
            newest = max(report_times)
            freshness = f"newest report {when(newest, now)}"
            if (now - newest).total_seconds() > stale_limit:
                freshness += " — stale"
                anomalies.append(
                    f"source-stale: extension status reports — newest report is "
                    f"{fmt_age((now - newest).total_seconds())} old (limit {args.stale_hours:.0f} h)"
                )
            if unreadable_time:
                freshness += f" · {unreadable_time} report(s) with an unreadable time"
        elif ext.records:
            freshness = "no readable report time — stale"
            anomalies.append(
                f"source-stale: extension status reports — {len(ext.records)} record(s) and none "
                "carries a readable time"
            )
        elif ext.unreadable:
            freshness = "no readable record at all"
        else:
            freshness = "0 record(s) (a measured empty status directory)"
        rows.append(("extension status reports", state_text, freshness))
        if ext.unreadable:
            first = ext.unreadable[0]
            anomalies.append(
                f"source-unavailable: extension status reports — {len(ext.unreadable)} of "
                f"{len(ext.records) + len(ext.unreadable)} record(s) unreadable (first: {first.where}: {first.why})"
            )

    # overview document
    if not overview.available:
        rows.append(("overview document", f"{UNAVAILABLE} — {overview.reason}", "n/a"))
        anomalies.append(f"source-unavailable: overview document — {overview.reason}")
    else:
        age = (now - overview.mtime).total_seconds() if overview.mtime else 0.0
        freshness = f"snapshot saved {fmt_age(age)} ago (file mtime {iso_z(overview.mtime)})"
        if age > stale_limit:
            freshness += " — stale"
            anomalies.append(
                f"source-stale: overview document — snapshot file is {fmt_age(age)} old "
                f"(limit {args.stale_hours:.0f} h)"
            )
        state_text = f"available · {len(overview.sessions)} session row(s) on {len(overview.machines)} machine(s)"
        if overview.malformed_rows:
            state_text += f" · {overview.malformed_rows} malformed row(s) left uncounted"
        rows.append(("overview document", state_text, freshness))

    # oracle comparison
    if not oracle.available:
        rows.append(("oracle comparison", f"{UNAVAILABLE} — {oracle.reason}", "n/a"))
        anomalies.append(f"source-unavailable: oracle comparison — {oracle.reason}")
    else:
        state_text = f"available · {len(oracle.results)} result(s)"
        if oracle.unreadable:
            state_text += f" · {len(oracle.unreadable)} unreadable file(s)"
        if oracle.newest_generated is not None:
            age = (now - oracle.newest_generated).total_seconds()
            freshness = f"newest run {when(oracle.newest_generated, now)}"
            if age > stale_limit:
                freshness += " — stale"
                anomalies.append(
                    f"source-stale: oracle comparison — newest run is {fmt_age(age)} old "
                    f"(limit {args.stale_hours:.0f} h)"
                )
        else:
            freshness = "no readable generated-at in any result"
        rows.append(("oracle comparison", state_text, freshness))

    # editorial fields
    if not editorial.available:
        rows.append(("editorial fields", f"{UNAVAILABLE} — {editorial.reason}", "n/a"))
        anomalies.append(f"source-unavailable: editorial fields — {editorial.reason}")
    else:
        dated = sum(1 for entry in editorial.rows if entry.dated)
        state_text = (
            f"available · {len(editorial.rows)} editorial row(s) · {dated} with a last_verified date"
            f" · {editorial.origin}"
        )
        if editorial.mtime:
            freshness = f"{'newest source' if editorial.derived else 'file'} mtime {iso_z(editorial.mtime)}"
        else:
            freshness = ABSENT
        if editorial.newest_last_verified is not None:
            freshness += f" · newest last_verified {editorial.newest_last_verified.isoformat()}"
        else:
            freshness += " · no readable last_verified date in the source"
        rows.append(("editorial fields", state_text, freshness))

    # previous-run state (its own row: rule 2 lives or dies here)
    if state.enabled:
        if state.path is None:
            rows.append(("previous-run state", f"{UNAVAILABLE} — no path resolved", "n/a"))
        elif state.previous is not None:
            updated = parse_rfc3339(state.previous.get("updated_at"))
            rows.append(("previous-run state", f"available · {state.path}", when(updated, now)))
        else:
            rows.append(("previous-run state", f"absent — {state.problem}", "n/a"))
    else:
        rows.append(("previous-run state", "disabled (--no-state, or stdout without --out or --state)", "n/a"))
    return rows, anomalies


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def editorial_cell(entry: EditorialEntry | None, field_name: str) -> str:
    if entry is None or not entry.fields.get(field_name):
        return ABSENT
    return esc(entry.fields[field_name])


def last_verified_cell(entry: EditorialEntry | None) -> str:
    if entry is None:
        return ABSENT
    if entry.last_verified is not None:
        return entry.last_verified.isoformat()
    if entry.last_verified_raw:
        return f"unparseable: {esc(entry.last_verified_raw)}"
    return ABSENT


def oracle_cell(oracle: OracleReading, platform_id: str) -> str:
    if not oracle.available:
        return UNAVAILABLE
    results = oracle.for_platform(platform_id)
    if not results:
        return "no result"
    newest = results[0]
    measured = iso_z(newest.generated_at) if newest.generated_at else "time unreadable"
    text = f"recall {newest.recall_text} · short {fmt_int(newest.short)} · red {fmt_int(newest.red)} (window {newest.window_text}, measured {measured})"
    if len(results) > 1:
        text += f"; {len(results) - 1} older result(s) in the results section"
    return text


def render_markdown(
    args: argparse.Namespace,
    ext: ExtStatusReading,
    overview: OverviewReading,
    oracle: OracleReading,
    editorial: EditorialReading,
    state: StateReading,
    ev: Evaluation,
) -> str:
    now = args.parsed_now
    catalog = args.catalog
    out: list[str] = []
    out.append("# chat-stasher platform scoreboard")
    out.append("")
    clock = "explicit --now" if args.now else "system clock"
    out.append(
        f"Generated at: {iso_z(now)} ({clock}). Every age below is measured against this "
        "moment, and against nothing else."
    )
    out.append("")
    out.append("## ANOMALIES")
    out.append("")
    if ev.anomalies:
        out.extend(f"- {line}" for line in ev.anomalies)
    else:
        out.append("None.")
    if ev.not_evaluable:
        out.append("")
        out.append("Rules that could not be judged this run (a visibility list, never an all-clear):")
        out.append("")
        out.extend(f"- {line}" for line in ev.not_evaluable)
    out.append("")
    out.append("## Sources")
    out.append("")
    out.append("| Source | State | Freshness |")
    out.append("|---|---|---|")
    for name, state_text, freshness in ev.sources:
        out.append(f"| {esc(name)} | {esc(state_text)} | {esc(freshness)} |")
    out.append("")
    if ev.notes:
        out.extend(f"- {esc(note)}" for note in ev.notes)
        out.append("")

    out.append("## Web platforms")
    out.append("")
    out.append(
        "Counts are per install and are never summed. `pending` is each install's own debt; two "
        "installs may be backfilling the same account, and a platform total would count that "
        "account twice — the only aggregates the extension's topology rules allow are counts of "
        "installs. “Last capture advance” is the newest cached observation between two runs of "
        "this script; one run alone cannot see capture movement."
    )
    if not ext.available:
        out.append("")
        out.append(
            f"extension status reports: **{UNAVAILABLE}** — {esc(ext.reason)}. Capture columns are not "
            "shown; an unavailable source is not a zero."
        )
    out.append("")
    header = [
        "Platform", "Installs reporting", "Still owe", "Paused", "Newest report",
        "Last capture advance", "Status", "Dev priority", "Last verified", "Oracle",
    ]
    out.append("| " + " | ".join(header) + " |")
    out.append("|" + "---|" * len(header))
    for platform in catalog.web:
        entry = editorial.for_row(FAMILY_WEB, platform.id)
        cells: list[str] = [esc(platform.id)]
        reporting = ext.reporting(platform.id)
        if ext.available:
            if reporting:
                owe = sum(1 for r in reporting if r.rows[platform.id].pending > 0)
                paused = sum(1 for r in reporting if r.rows[platform.id].paused_reason)
                times = [r.reported_at for r in reporting if r.reported_at is not None]
                newest_report = when(max(times), now) if times else "time unreadable"
                cells += [
                    f"{len(reporting)} install(s)",
                    f"{owe} install(s)",
                    f"{paused} install(s)",
                    newest_report,
                ]
            else:
                cells += ["0 install(s)", ABSENT, ABSENT, "not reported in this run"]
            platform_tracks = [t.platforms[platform.id] for t in ev.tracks.values() if platform.id in t.platforms]
            current = [p.last_advanced for p in platform_tracks if p.last_advanced is not None and p.currently_reported]
            remembered = [p.last_advanced for p in platform_tracks if p.last_advanced is not None]
            if current or remembered:
                newest_advance = max(current or remembered)
                cells.append(when(newest_advance, now))
            elif platform_tracks:
                cells.append("none observed yet (baseline only)")
            else:
                cells.append(ABSENT)
        else:
            cells += [ABSENT, ABSENT, ABSENT, ABSENT, UNAVAILABLE]
        cells.append(editorial_cell(entry, "status"))
        cells.append(editorial_cell(entry, "dev_priority"))
        cells.append(last_verified_cell(entry))
        cells.append(oracle_cell(oracle, platform.id))
        out.append("| " + " | ".join(cells) + " |")
    out.append("")

    if ext.available and ext.records:
        out.append("### Per install")
        out.append("")
        out.append(
            "One row per (install, platform). `Captured by this browser` is the extension's own "
            "counter — it is never compared with, or added to, what the archive holds."
        )
        out.append("")
        per_install_header = [
            "Machine", "Install", "Browser · profile", "Platform",
            "Captured by this browser", "Pending", "Paused reason",
            "Reported at", "Last capture advance",
        ]
        out.append("| " + " | ".join(per_install_header) + " |")
        out.append("|" + "---|" * len(per_install_header))
        for record in sorted(ext.records, key=lambda r: (r.machine, r.install_id)):
            track = ev.tracks.get((record.machine, record.install_id))
            markers = []
            if record.identity_conflict or record.install_id in ext.shared_install_ids:
                markers.append("identity conflict")
            if record.legacy_migration:
                markers.append("legacy-migrated")
            # The same two states the ANOMALIES section judges per install:
            # past the limit, or no readable time at all. The marker text is
            # assembled with its leading space outside `esc()` — flattening
            # whitespace is for field content, and it would eat this space.
            if record.reported_at is None or (now - record.reported_at).total_seconds() > args.stale_hours * 3600:
                markers.append("stale")
            marker_text = f" ({esc('; '.join(markers))})" if markers else ""
            reported = when(record.reported_at, now)
            for platform_name in sorted(record.rows):
                row = record.rows[platform_name]
                platform_track = track.platforms.get(platform_name) if track else None
                advance = when(platform_track.last_advanced, now) if platform_track and platform_track.last_advanced else (
                    "baseline only" if platform_track and platform_track.first_seen and platform_track.last_advanced is None else ABSENT
                )
                out.append(
                    f"| {esc(record.machine)} | {esc(record.short_id)}{marker_text} | {esc(record.label)} | "
                    f"{esc(platform_name)} | {fmt_int(row.captured)} | {fmt_int(row.pending)} | "
                    f"{esc(row.paused_reason) or ABSENT} | {esc(reported)} | {esc(advance)} |"
                )
        out.append("")
    elif ext.available:
        out.append("No status records in the input directory (a measured empty, not an unreadable one).")
        out.append("")

    out.append("## Local tools")
    out.append("")
    out.append(
        "Sessions counted per machine × tool from the overview document; `Conversations (deduped)` "
        "is the archive's own conversation axis, so a conversation archived on two machines counts "
        "once there and twice in the per-machine cells. Time-unknown keeps the archive's split: "
        "conversations with no conversation content sit outside both numerator and denominator."
    )
    if not overview.available:
        out.append("")
        out.append(
            f"overview document: **{UNAVAILABLE}** — {esc(overview.reason)}. Session counts, shares "
            "and newest-session columns are not shown; an unavailable source is not a zero."
        )
    out.append("")
    machines = overview.machines
    header = ["Tool"] + [overview.display.get(m, m) for m in machines]
    header += ["Sessions (rows)", "Conversations (deduped)", "Newest session", "Status", "Dev priority", "Last verified"]
    out.append("| " + " | ".join(esc(h) for h in header) + " |")
    out.append("|" + "---|" * len(header))
    for tool in catalog.local:
        entry = editorial.for_row(FAMILY_LOCAL, tool.id)
        cells: list[str] = [esc(tool.display)]
        if tool.id in catalog.shared_ids:
            cells[0] += " 🔶"
        row_total = 0
        for machine in machines:
            stats = ev.cell_stats.get((machine, tool.id))
            if stats is None:
                cells.append("0")
                continue
            total, unknown, no_content = stats
            row_total += total
            denominator = total - no_content
            if denominator > 0:
                cells.append(f"{fmt_int(total)} sess · {pct(unknown, denominator)} time-unknown")
            else:
                cells.append(f"{fmt_int(total)} sess · share n/a (no conversation content)")
        if machines:
            cells.append(fmt_int(row_total))
        else:
            cells.append(ABSENT)
        if overview.available and overview.conversations_by_harness is not None:
            conversations = overview.conversations_by_harness.get(tool.id)
            cells.append(fmt_int(conversations) if conversations is not None else ABSENT)
        else:
            cells.append(ABSENT)
        newest = ev.tool_newest.get(tool.id)
        if newest is None:
            if not overview.available:
                cells.append(ABSENT)
            elif not overview.sessions_for_harness(tool.id):
                cells.append("0 sessions")
            else:
                cells.append("unknown (no known-time session)")
        else:
            cells.append(when(newest[0], now))
        cells.append(editorial_cell(entry, "status"))
        cells.append(editorial_cell(entry, "dev_priority"))
        cells.append(last_verified_cell(entry))
        out.append("| " + " | ".join(cells) + " |")
    out.append("")
    if overview.available:
        extra: dict[str, int] = {}
        for session in overview.sessions:
            if session.harness not in catalog.local_ids:
                extra[session.harness] = extra.get(session.harness, 0) + 1
        if extra:
            pairs = ", ".join(f"{h} ({fmt_int(c)})" for h, c in sorted(extra.items()))
            out.append(
                f"- session rows with harness ids outside the local-tool registry: {pairs} "
                "(listed; the registry defines this table, not the archive)"
            )
        if catalog.shared_ids:
            names = ", ".join(sorted(catalog.shared_ids))
            out.append(
                f"- 🔶 id(s) shared between a local tool and a web platform ({names}): the archive's "
                "id space mixes the local tool's sessions with that platform's web captures, and these "
                "cells cannot separate them"
            )
        if overview.malformed_rows:
            out.append(
                f"- {fmt_int(overview.malformed_rows)} malformed session row(s) in the overview "
                "document are counted nowhere"
            )
    out.append("")

    out.append("## Oracle results")
    out.append("")
    if not oracle.available:
        out.append(f"{UNAVAILABLE} — {esc(oracle.reason)}. The export window is part of every number; without it there is nothing to read here.")
        out.append("")
    else:
        out.append(
            "Every result, newest first per platform. Recall is `found/expected` against the "
            "official export of its window; a platform can appear more than once (different "
            "accounts or runs)."
        )
        out.append("")
        header = ["Platform", "Recall", "Short", "Red", "Export window", "Measured", "Account/notes", "Result file"]
        out.append("| " + " | ".join(header) + " |")
        out.append("|" + "---|" * len(header))
        if not oracle.results:
            out.append("| " + " | ".join(["(no readable results)"] + [ABSENT] * (len(header) - 1)) + " |")
        ordered = sorted(
            oracle.results,
            key=lambda r: (
                r.platform,
                r.generated_at is None,
                r.generated_at or dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc),
            ),
        )
        for result in ordered:
            measured = iso_z(result.generated_at) if result.generated_at else "time unreadable"
            out.append(
                f"| {esc(result.platform)} | recall {esc(result.recall_text)} | {fmt_int(result.short)} | "
                f"{fmt_int(result.red)} | {esc(result.window_text)} | {measured} | "
                f"{esc(result.missing_class) or ABSENT} | {esc(result.file_name)} |"
            )
        out.append("")
    return "\n".join(out).rstrip("\n") + "\n"


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="scoreboard.py",
        description="Generate the chat-stasher platform scoreboard as markdown.",
    )
    parser.add_argument("--out", help="write the scoreboard here (default: stdout)")
    parser.add_argument(
        "--state",
        help="path of the previous-run state file (default: <out>.state.json, beside --out)",
    )
    parser.add_argument(
        "--no-state",
        action="store_true",
        help="do not read or write the state file (disables pending-rise and capture-advance observation)",
    )
    parser.add_argument("--ext-status", help="the ext-status directory of a machine's local stage")
    parser.add_argument("--overview", help="a saved `chat-stasher overview --json` document")
    parser.add_argument(
        "--oracle", action="append", default=None, help="an oracle results directory or file (repeatable)"
    )
    parser.add_argument(
        "--editorial",
        help="override the editorial fields with this JSON file (default: read them from the two platform sources under --root)",
    )
    parser.add_argument("--now", help="the reference clock, RFC3339 (default: the system clock)")
    parser.add_argument(
        "--stale-hours", type=float, default=DEFAULT_STALE_HOURS,
        help="source staleness limit in hours (default: %(default)s)",
    )
    parser.add_argument(
        "--quiet-days", type=float, default=DEFAULT_QUIET_DAYS,
        help="no-new-capture window in days (default: %(default)s)",
    )
    parser.add_argument(
        "--unknown-share", type=float, default=DEFAULT_UNKNOWN_SHARE,
        help="time-unknown share threshold, 0..1 (default: %(default)s)",
    )
    parser.add_argument(
        "--verify-days", type=float, default=DEFAULT_VERIFY_DAYS,
        help="last-verified re-check window in days (default: %(default)s)",
    )
    parser.add_argument("--root", help="repository root holding the two platform sources (default: this checkout)")
    parser.add_argument("--selftest", action="store_true", help="run the fixture self-test")
    return parser


def prepare(args: argparse.Namespace) -> None:
    if args.now is not None:
        parsed = parse_rfc3339(args.now)
        if parsed is None:
            raise UsageError(f"--now is not an RFC3339 timestamp: {args.now!r}")
        args.parsed_now = parsed
    else:
        args.parsed_now = dt.datetime.now(dt.timezone.utc)
    for dest, flag in (
        ("stale_hours", "--stale-hours"),
        ("quiet_days", "--quiet-days"),
        ("unknown_share", "--unknown-share"),
        ("verify_days", "--verify-days"),
    ):
        value = getattr(args, dest)
        if value is None or value < 0:
            raise UsageError(f"{flag} must be a non-negative number")
    args.catalog = load_catalog(args.root if args.root else default_root())


def atomic_write(path: str, body: str) -> None:
    directory = os.path.dirname(os.path.abspath(path))
    fd, temp_path = tempfile.mkstemp(dir=directory, prefix=".scoreboard-", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(body)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_path, path)
    except BaseException:
        try:
            os.unlink(temp_path)
        except OSError:
            pass
        raise


def main(argv: list[str]) -> int:
    args = build_parser().parse_args(argv)
    if args.selftest:
        return selftest()
    try:
        prepare(args)
    except UsageError as exc:
        print(f"[scoreboard] usage: {exc}", file=sys.stderr)
        return 2
    ext = read_ext_status(args.ext_status)
    overview = read_overview(args.overview)
    oracle = read_oracle(args.oracle)
    editorial = read_editorial(args.editorial, args.catalog, args.root or default_root())
    state = load_state(resolve_state(args))
    ev = evaluate(args, ext, overview, oracle, editorial, state)
    board = render_markdown(args, ext, overview, oracle, editorial, state, ev)
    try:
        if args.out:
            atomic_write(args.out, board)
        else:
            sys.stdout.write(board)
        if ev.new_state is not None and state.enabled and state.path is not None:
            atomic_write(state.path, json.dumps(ev.new_state, indent=1, sort_keys=True) + "\n")
    except OSError as exc:
        print(f"[scoreboard] could not write the output: {exc}", file=sys.stderr)
        return 1
    return 0


# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

NOW_DT = dt.datetime(2026, 9, 29, 18, 0, 0, tzinfo=dt.timezone.utc)
NOW = "2026-09-29T18:00:00Z"
UNIX_NOW = int(NOW_DT.timestamp())
UNIX_FRESH = UNIX_NOW - 3600
UNIX_QUIET = UNIX_NOW - 4 * 86400
INSTALL_A = "11111111-1111-4111-8111-111111111111"
INSTALL_B = "22222222-2222-4222-8222-222222222222"


def _write_json(path: str, payload: Any, mtime: float | None = None) -> str:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(payload, handle)
    if mtime is not None:
        os.utime(path, (mtime, mtime))
    return path


def _status_payload(
    install_id: str,
    machine: str,
    reported_at: str,
    platform_rows: list[dict[str, Any]],
    reported_daily: bool = True,
) -> dict[str, Any]:
    return {
        "install_id": install_id,
        "browser": "Chrome",
        "profile_label": "Personal",
        "extension_version": "0.2.0.19",
        "reported_at": reported_at,
        "platforms": platform_rows,
        "machine": machine,
        "schema": "chat-stasher/ext-status@1",
        "daily_report_streak": 5,
        "reported_daily": reported_daily,
        "identity_conflict": False,
    }


def _row(platform: str, captured: int, pending: int, paused: str | None = None) -> dict[str, Any]:
    return {
        "platform": platform,
        "captured_by_this_browser": captured,
        "pending": pending,
        "paused_reason": paused,
        "account_fingerprint": "ab" * 32,
    }


def _session(
    harness: str,
    machine: str,
    kind: str = "known",
    unix: int = UNIX_FRESH,
    session_index: int = 0,
) -> dict[str, Any]:
    if kind == "known":
        boundary = {"kind": "known", "unix": unix}
        time_source = {"kind": "exact"}
    elif kind == "no_conversation_content":
        boundary = {"kind": "no_conversation_content"}
        time_source = {"kind": "no_conversation_content"}
    else:
        boundary = {"kind": "unknown", "why": "no timestamp field found within the line"}
        time_source = {"kind": "unknown", "why": "…"}
    return {
        "session_id": f"{harness}.{machine}.0000000{session_index:02d}-0000-4000-8000-000000000001",
        "machine": machine,
        "machine_display": machine.replace("machine-", "Machine ").title(),
        "harness": harness,
        "line_count": 3,
        "time_source": time_source,
        "first_unix": boundary,
        "last_unix": boundary,
    }


def _overview_doc(
    sessions: list[dict[str, Any]],
    conversations: list[dict[str, Any]] | None = None,
    exit_code: int = 0,
) -> dict[str, Any]:
    if conversations is None:
        conversations = [{"session_id": s["session_id"], "harness": s["harness"]} for s in sessions]
    body = {
        "schema_version": 1,
        "command": "overview",
        "healthy": exit_code == 0,
        "exit_code": exit_code,
        "no_index_anywhere": not sessions,
        "sessions": sessions,
        "conversations": conversations,
    }
    if exit_code == 3:
        return {
            "schema_version": 1,
            "command": "overview",
            "healthy": False,
            "exit_code": 3,
            "error": "remote read failed mid-snapshot",
            "error_kind": "read",
        }
    return body


def _oracle_payload(
    platform: str, found: int, expected: int, generated_at: str = "2026-09-24T22:59:09Z"
) -> dict[str, Any]:
    return {
        "platform": platform,
        "generated_at": generated_at,
        "recall": {
            "expected": expected,
            "found": found,
            "missing": expected - found,
            "recall": (found / expected) if expected else None,
        },
        "content_short_counts": {"total": 1, "RED": 0, "SHORT": 1},
        "window": ["2025-01-05", "2026-06-29"],
        "missing_class": "truly-missing (logged-in account)",
    }


def selftest() -> int:
    import subprocess

    here = os.path.abspath(__file__)
    failures = 0
    count = 0

    def expect(condition: bool, label: str) -> bool:
        nonlocal failures, count
        count += 1
        if condition:
            return True
        failures += 1
        print(f"[scoreboard] selftest WRONG: {label}")
        return False

    def run_cli(args_list: list[str]) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, here] + args_list, capture_output=True, text=True)

    def base_args(
        root: str,
        ext_status: str | None = None,
        overview: str | None = None,
        oracle: list[str] | None = None,
        editorial: str | None = None,
        quiet_days: str | None = None,
        unknown_share: str | None = None,
    ) -> list[str]:
        args = ["--root", root, "--now", NOW]
        if ext_status:
            args += ["--ext-status", ext_status]
        if overview:
            args += ["--overview", overview]
        if oracle:
            for path in oracle:
                args += ["--oracle", path]
        if editorial:
            args += ["--editorial", editorial]
        if quiet_days:
            args += ["--quiet-days", quiet_days]
        if unknown_share:
            args += ["--unknown-share", unknown_share]
        return args

    repo_root = default_root()
    catalog = load_catalog(repo_root)
    tool_ids = [t.id for t in catalog.local]
    web_ids = [p.id for p in catalog.web]
    first_platform = web_ids[0]
    second_platform = web_ids[1]
    first_tool = tool_ids[0]
    second_tool = tool_ids[1]
    quiet_tool = tool_ids[-1]

    with tempfile.TemporaryDirectory(prefix="scoreboard-selftest-") as tmp:
        # ---------------- fixtures ------------------------------------------
        ext_dir = os.path.join(tmp, "ext-fresh")
        _write_json(
            os.path.join(ext_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(
                INSTALL_A, "machine-a", NOW.replace("18:00:00", "17:00:00"),
                [_row(first_platform, 12, 3)], reported_daily=False,
            ),
        )
        _write_json(
            os.path.join(ext_dir, "machine-b", f"{INSTALL_B}.json"),
            _status_payload(
                INSTALL_B, "machine-b", NOW.replace("18:00:00", "16:00:00"),
                [_row(first_platform, 30, 9, paused="rate-limited")],
            ),
        )
        rising_dir = os.path.join(tmp, "ext-rising")
        _write_json(
            os.path.join(rising_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(INSTALL_A, "machine-a", "2026-09-29T17:55:00Z", [_row(first_platform, 12, 8)]),
        )
        falling_dir = os.path.join(tmp, "ext-falling")
        _write_json(
            os.path.join(falling_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(INSTALL_A, "machine-a", "2026-09-29T17:55:00Z", [_row(first_platform, 14, 1)]),
        )
        quiet_dir = os.path.join(tmp, "ext-quiet")
        _write_json(
            os.path.join(quiet_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(INSTALL_A, "machine-a", "2026-09-29T17:55:00Z", [_row(first_platform, 12, 3)]),
        )
        daily_silent_dir = os.path.join(tmp, "ext-daily-silent")
        _write_json(
            os.path.join(daily_silent_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(
                INSTALL_A, "machine-a", "2026-09-26T17:00:00Z",
                [_row(second_platform, 12, 3)], reported_daily=True,
            ),
        )
        nondaily_silent_dir = os.path.join(tmp, "ext-nondaily-silent")
        _write_json(
            os.path.join(nondaily_silent_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(
                INSTALL_A, "machine-a", "2026-09-24T17:00:00Z",
                [_row(first_platform, 12, 3)], reported_daily=False,
            ),
        )
        _write_json(
            os.path.join(nondaily_silent_dir, "machine-b", f"{INSTALL_B}.json"),
            _status_payload(
                INSTALL_B, "machine-b", "2026-09-29T17:00:00Z",
                [_row(first_platform, 30, 9)], reported_daily=False,
            ),
        )
        blank_time_dir = os.path.join(tmp, "ext-blank-time")
        _write_json(
            os.path.join(blank_time_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(INSTALL_A, "machine-a", "not-a-time", [_row(second_platform, 12, 3)]),
        )
        empty_dir = os.path.join(tmp, "ext-empty")
        os.makedirs(empty_dir, exist_ok=True)
        unreadable_dir = os.path.join(tmp, "ext-unreadable")
        _write_json(
            os.path.join(unreadable_dir, "machine-a", "wrong-schema.json"),
            _status_payload("33333333-3333-4333-8333-333333333333", "machine-a", NOW, []) | {"schema": "someone-else@1"},
        )
        bad_row_install = "44444444-4444-4444-8444-444444444444"
        _write_json(
            os.path.join(unreadable_dir, "machine-a", f"{bad_row_install}.json"),
            _status_payload(
                bad_row_install,
                "machine-a",
                NOW,
                [{"platform": first_platform, "captured_by_this_browser": "many", "pending": 3}],
            ),
        )
        legacy_dir = os.path.join(tmp, "ext-legacy")
        _write_json(
            os.path.join(legacy_dir, f"{INSTALL_A}.json"),
            _status_payload(INSTALL_A, "machine-a", "2026-09-20T17:00:00Z", [_row(first_platform, 5, 1)]),
        )
        _write_json(
            os.path.join(legacy_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(INSTALL_A, "machine-a", "2026-09-29T17:30:00Z", [_row(first_platform, 8, 2)]),
        )
        unknown_platform_dir = os.path.join(tmp, "ext-unknown-id")
        _write_json(
            os.path.join(unknown_platform_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(
                INSTALL_A, "machine-a", "2026-09-29T17:00:00Z",
                [_row("a-platform-we-do-not-ship", 12, 3)],
            ),
        )
        shared_install_dir = os.path.join(tmp, "ext-shared-id")
        _write_json(
            os.path.join(shared_install_dir, "machine-a", f"{INSTALL_A}.json"),
            _status_payload(INSTALL_A, "machine-a", "2026-09-29T17:00:00Z", [_row(first_platform, 12, 3)]),
        )
        _write_json(
            os.path.join(shared_install_dir, "machine-b", f"{INSTALL_A}.json"),
            _status_payload(INSTALL_A, "machine-b", "2026-09-29T17:00:00Z", [_row(first_platform, 7, 1)]),
        )

        overview_sessions: list[dict[str, Any]] = []
        for index in range(30):
            overview_sessions.append(_session(first_tool, "machine-a", "known", UNIX_FRESH, index))
        for index in range(20):
            overview_sessions.append(_session(first_tool, "machine-a", "unknown", session_index=100 + index))
        for index in range(10):
            overview_sessions.append(_session(first_tool, "machine-b", "known", UNIX_FRESH, 200 + index))
        overview_sessions.append(_session(first_tool, "machine-b", "no_conversation_content", session_index=299))
        overview_sessions.append(_session(quiet_tool, "machine-b", "known", UNIX_QUIET, session_index=300))
        for index in range(40):
            overview_sessions.append(_session(second_tool, "machine-a", "unknown", session_index=400 + index))
        for index in range(10):
            overview_sessions.append(_session(second_tool, "machine-a", "no_conversation_content", session_index=500 + index))
        for index in range(5):
            overview_sessions.append(_session(second_tool, "machine-b", "no_conversation_content", session_index=600 + index))
        overview_path = _write_json(
            os.path.join(tmp, "overview.json"), _overview_doc(overview_sessions), mtime=UNIX_NOW - 3600
        )
        dedup_path = _write_json(
            os.path.join(tmp, "overview-dedup.json"),
            _overview_doc(
                [_session(first_tool, "machine-a", "known", UNIX_FRESH), _session(first_tool, "machine-b", "known", UNIX_FRESH)],
                [{"session_id": "one-conversation", "harness": first_tool}],
            ),
            mtime=UNIX_NOW,
        )
        summary_path = _write_json(
            os.path.join(tmp, "overview-summary.json"),
            {
                "schema_version": 1,
                "command": "overview",
                "variant": "summary",
                "healthy": True,
                "exit_code": 0,
                "totals": {},
                "sources": [],
                "machines": [],
                "days": [],
            },
            mtime=UNIX_NOW,
        )
        error_path = _write_json(os.path.join(tmp, "overview-error.json"), _overview_doc([], exit_code=3), mtime=UNIX_NOW)
        old_overview_path = _write_json(
            os.path.join(tmp, "overview-old.json"),
            _overview_doc([_session(first_tool, "machine-a", "known", UNIX_FRESH)]),
            mtime=UNIX_NOW - 96 * 3600,
        )
        oracle_dir = os.path.join(tmp, "oracle")
        _write_json(
            os.path.join(oracle_dir, f"{first_platform}.json"),
            _oracle_payload(first_platform, 58, 234),
        )
        _write_json(
            os.path.join(oracle_dir, f"{first_platform}-old.json"),
            _oracle_payload(first_platform, 10, 234, generated_at="2026-06-24T22:59:09Z"),
        )
        empty_oracle_dir = os.path.join(tmp, "oracle-empty")
        os.makedirs(empty_oracle_dir, exist_ok=True)
        editorial_path = _write_json(
            os.path.join(tmp, "editorial.json"),
            {
                "schema_version": 1,
                "platforms": {
                    first_platform: {
                        "status": "flowing",
                        "dev_priority": "P0",
                        "last_verified": "2026-06-20",
                        "known_issues": "list misses some conversations",
                    },
                    quiet_tool: {"status": "flowing", "dev_priority": "P2", "last_verified": "2026-09-28"},
                    "an-id-nobody-knows": {"status": "orphan row"},
                },
            },
            mtime=UNIX_NOW,
        )
        editorial_badpath = _write_json(
            os.path.join(tmp, "editorial-baddate.json"),
            {
                "schema_version": 1,
                "platforms": {first_platform: {"status": "flowing", "last_verified": "when-was-that"}},
            },
            mtime=UNIX_NOW,
        )

        # ---------------- case: full run, every source present -------------
        out_path = os.path.join(tmp, "board.md")
        state_path = out_path + STATE_SUFFIX
        result = run_cli(
            base_args(repo_root, ext_dir, overview_path, [oracle_dir], editorial_path)
            + ["--out", out_path]
        )
        expect(result.returncode == 0, f"full run exits 0 (got {result.returncode}: {result.stderr})")
        with open(out_path, encoding="utf-8") as handle:
            text = handle.read()
        for tool in catalog.local:
            expect(tool.display in text, f"local tool row present: {tool.id}")
        for web in web_ids:
            expect(web in text, f"web platform row present: {web}")
        expect("Generated at: 2026-09-29T18:00:00Z (explicit --now)" in text, "generated-at names the reference clock")
        expect("- pending-rising:" not in text, "first run reports no pending rise")
        expect("first run against this output: counters are baseline only" in text, "first run explains the baseline")
        expect(
            f"time-unknown-share: Machine A (machine-a) / {first_tool} — 20 of 50 conversations time-unknown (40.0%)"
            not in text,
            "share below the threshold is not an anomaly",
        )
        expect(
            f"time-unknown-share: Machine A (machine-a) / {second_tool} — 40 of 40 conversations time-unknown (100.0%)"
            in text,
            "share above the threshold is an anomaly, excluding no-content rows",
        )
        expect("40.0%" in text, "a below-threshold share is still displayed")
        expect("share n/a (no conversation content)" in text, "no-content cells say share n/a, not 0%")
        expect(f"verification-stale: {first_platform} — last verified 2026-06-20" in text, "verification-stale fires past the 90-day window")
        expect(f"no-new-capture: {quiet_tool} — newest archived session content is 4.0 d old" in text, "quiet local tool is flagged by content age")
        expect(f"no-new-capture: {first_tool}" not in text, "fresh local tool is not flagged")
        expect("unknown (no known-time session)" in text, "a tool whose sessions carry no readable time says unknown, not a fabricated age")
        expect(
            "no-new-capture (local tools) — sessions exist but none carries a known time, so no age can be read: "
            f"{second_tool}" in text,
            "the no-known-time state is named in the not-evaluable list, never silently passed",
        )
        expect("recall 58/234 = 24.8%" in text, "newest oracle result supplies the platform cell")
        expect("1 older result(s) in the results section" in text, "older oracle results are referenced, not hidden")
        expect("window 2025-01-05 → 2026-06-29" in text, "the export window travels with the recall number")
        expect("an-id-nobody-knows" in text, "editorial rows for unknown ids are listed, never dropped")
        expect("source-unavailable:" not in text, "no unavailable-source anomaly when all four sources are present")
        expect("| 12 install(s)" not in text and "pending total" not in text, "pending is never summed into a platform total")
        expect("| 12 | 3 |" in text and "| 30 | 9 |" in text, "each install's own captured/pending pair is listed")
        expect("| 2 install(s) | 2 install(s) | 1 install(s) |" in text, "install counts aggregate, counts of installs")
        expect("baseline only" in text, "first-run capture-advance cells say baseline, not a fabricated time")
        expect(os.path.isfile(state_path), "state file is written next to --out")
        with open(state_path, encoding="utf-8") as handle:
            state_doc = json.load(handle)
        expect(
            state_doc["installs"]["machine-a"][INSTALL_A]["platforms"][first_platform]["last_advanced"] is None,
            "the first run baselines without claiming an advance",
        )
        expect(
            state_doc["installs"]["machine-b"][INSTALL_B]["platforms"][first_platform]["captured"] == 30,
            "state carries every install's counters",
        )


        # ---------------- case: the share threshold boundary --------------------
        boundary_sessions: list[dict[str, Any]] = []
        for index in range(5):
            boundary_sessions.append(_session(quiet_tool, "machine-a", "known", UNIX_FRESH, session_index=700 + index))
        for index in range(5):
            boundary_sessions.append(_session(quiet_tool, "machine-a", "unknown", session_index=800 + index))
        boundary_path = _write_json(
            os.path.join(tmp, "overview-boundary.json"), _overview_doc(boundary_sessions), mtime=UNIX_NOW
        )
        result = run_cli(base_args(repo_root, None, boundary_path) + ["--no-state"])
        text = result.stdout
        expect(f"time-unknown-share: Machine A (machine-a) / {quiet_tool}" not in text, "a share of exactly 50% does not cross a strictly-greater threshold")
        result = run_cli(base_args(repo_root, None, boundary_path) + ["--no-state", "--unknown-share", "0.4999"])
        text = result.stdout
        expect(f"time-unknown-share: Machine A (machine-a) / {quiet_tool} — 5 of 10 conversations time-unknown (50.0%)" in text, "a share above a lowered threshold is flagged")

        # ---------------- case: pending rising through the state -----------
        rise_now = "2026-09-29T19:00:00Z"
        result = run_cli(base_args(repo_root, rising_dir) + ["--now", rise_now, "--state", state_path])
        expect(result.returncode == 0, "rise run exits 0")
        text = result.stdout
        expect(
            f"pending-rising: machine-a / install {INSTALL_A[:8]}… (Chrome · Personal) / {first_platform}" in text,
            "the rising line names machine, install and platform",
        )
        expect("pending 3 → 8" in text, "the rising line carries both readings")
        expect("1.0 h after the previous run" in text, "the rising line says when the previous run was")
        fall_now = "2026-09-29T20:00:00Z"
        result = run_cli(base_args(repo_root, falling_dir) + ["--now", fall_now, "--state", state_path])
        expect(result.returncode == 0, "fall run exits 0")
        text = result.stdout
        expect("- pending-rising:" not in text, "a falling pending count is no rise")
        expect(
            f"no-new-capture: {first_platform}" not in text,
            "the advancing captured counter (12 → 14) clears the quiet rule",
        )
        expect("2026-09-29T20:00:00Z" in text, "an observed advance is recorded with the run's clock")

        # ---------------- case: quiet platform, never-advanced -------------
        quiet_state = os.path.join(tmp, "quiet.state.json")
        old_now = (NOW_DT - dt.timedelta(days=4)).strftime("%Y-%m-%dT%H:%M:%SZ")
        result = run_cli(base_args(repo_root, quiet_dir) + ["--now", old_now, "--state", quiet_state])
        expect(result.returncode == 0, "quiet baseline run exits 0")
        result = run_cli(base_args(repo_root, quiet_dir) + ["--state", quiet_state])
        expect(result.returncode == 0, "quiet comparison run exits 0")
        text = result.stdout
        expect(f"no-new-capture: {first_platform} — no capture observed in any run while tracked" in text, "a steady counter over the window is the quiet anomaly")
        expect("(4.0 d, since 2026-09-25T18:00:00Z)" in text, "the quiet line names when tracking began")
        expect("- pending-rising:" not in text, "counters that did not move report no rise (the quiet case's values are identical)")

        # ---------------- case: a daily reporter goes silent ------------------
        result = run_cli(base_args(repo_root, daily_silent_dir) + ["--no-state"])
        text = result.stdout
        expect(
            "source-stale: install “Chrome · Personal” on machine-a — no report for 3.0 d (limit 48 h; this install used to report daily)"
            in text,
            "a silent daily reporter is the topology's own warning sentence",
        )

        # ---------------- case: a non-daily reporter goes silent --------------
        result = run_cli(base_args(repo_root, nondaily_silent_dir) + ["--no-state"])
        expect(result.returncode == 0, "the non-daily silent run exits 0")
        text = result.stdout
        expect(
            "source-stale: install “Chrome · Personal” on machine-a — no report for 5.0 d (limit 48 h)"
            in text,
            "a report past the limit is stale whatever its install's cadence, and lands in ANOMALIES",
        )
        expect(
            "used to report daily" not in text,
            "the daily clause is only claimed for an install that said it reports daily",
        )
        expect(
            "source-stale: install “Chrome · Personal” on machine-b" not in text,
            "a fresh report beside a stale one is never called stale",
        )
        expect(f"{INSTALL_A[:8]}… (stale)" in text, "the stale marker keeps its separating space")
        expect(f"{INSTALL_B[:8]}… (stale)" not in text, "a fresh install carries no stale marker")

        # ---------------- case: report time unparseable ----------------------
        result = run_cli(base_args(repo_root, blank_time_dir) + ["--no-state"])
        text = result.stdout
        expect("source-stale: extension status reports — 1 record(s) and none carries a readable time" in text, "a blank report time counts as stale, never as fresh")
        expect(
            "source-stale: install “Chrome · Personal” on machine-a — report time unreadable, counted as stale"
            in text,
            "the unreadable-time record is judged per install, not only as a whole source",
        )
        expect("time unreadable" in text, "the per-install row shows the unreadable time")

        # ---------------- case: unreadable records stay visible --------------
        result = run_cli(base_args(repo_root, unreadable_dir) + ["--no-state"])
        text = result.stdout
        expect("unreadable status record: machine-a/wrong-schema.json — schema is 'someone-else@1'" in text, "a schema-mismatched record is listed by file and reason")
        expect(f"unreadable status record: machine-a/{bad_row_install}.json — a platform row is malformed" in text, "a malformed platform row is listed, not dropped")
        expect("source-unavailable: extension status reports — 2 of 2 record(s) unreadable" in text, "unreadable records surface as an anomaly")
        expect("no readable record at all" in text, "a directory with only unreadable records is not called a measured empty")

        # ---------------- case: legacy layouts -------------------------------
        result = run_cli(base_args(repo_root, legacy_dir) + ["--no-state"])
        text = result.stdout
        expect("1 superseded legacy record(s) kept on disk" in text, "the superseded flat file is counted once, never listed twice")
        expect("| 8 | 2 |" in text, "the keyed record (later observation) is the one listed")

        # ---------------- case: shared install id across machines ------------
        result = run_cli(base_args(repo_root, shared_install_dir) + ["--no-state"])
        text = result.stdout
        expect("reported from more than one machine" in text, "a shared install id is flagged, following the protocol")
        expect("identity conflict" in text, "each shared record shows the identity-conflict marker")

        # ---------------- case: unknown platform id in a report ---------------
        result = run_cli(base_args(repo_root, unknown_platform_dir) + ["--no-state"])
        text = result.stdout
        expect("platform rows with ids outside the extension's table: a-platform-we-do-not-ship" in text, "unknown platform rows are listed, not judged or dropped")

        # ---------------- case: empty ext-status directory --------------------
        result = run_cli(base_args(repo_root, empty_dir) + ["--no-state"])
        text = result.stdout
        expect("0 record(s) (a measured empty status directory)" in text, "an empty directory is a measured empty")
        expect("source-unavailable: extension status reports" not in text, "an empty directory is available, not unavailable")

        # ---------------- case: no inputs at all -------------------------------
        result = run_cli(base_args(repo_root) + ["--no-state"])
        expect(result.returncode == 0, "no-input run exits 0")
        text = result.stdout
        expect(text.count(UNAVAILABLE) >= 4, "every one of the four sources says source unavailable")
        for web in web_ids:
            expect(web in text, f"web platform row exists with zero inputs: {web}")
        for tool in catalog.local:
            expect(tool.display in text, f"local tool row exists with zero inputs: {tool.id}")
        expect("pending-rising + no-new-capture (web) — extension status reports are unavailable" in text, "the rise rule explains itself when its input is missing")
        # The editorial fields are not an absent input here: they are read from
        # the same two sources the platform axes come from, so a run with no
        # other input still judges every row that carries a verification date.
        expect("source-unavailable: editorial fields" not in text, "the editorial fields are not unavailable when the two sources are present")
        expect(f"the SB-1 sources under {repo_root}" in text, "the editorial source says which reading answered")
        result = run_cli(
            base_args(repo_root, None, None, None, os.path.join(tmp, "not-a-file.json")) + ["--no-state"]
        )
        expect(
            "source-unavailable: editorial fields — " in result.stdout
            and "not-a-file.json is not a file" in result.stdout,
            "an override that cannot be read is unavailable, never silently the built-in reading",
        )
        expect("verification-stale — editorial fields are unavailable" in result.stdout, "the verify rule explains itself when its input is missing")
        expect("time-unknown-share + no-new-capture (local tools) — overview document is unavailable" in text, "the share rule explains itself when its input is missing")

        # ---------------- case: stale sources ----------------------------------
        result = run_cli(base_args(repo_root, None, old_overview_path) + ["--no-state"])
        text = result.stdout
        expect("source-stale: overview document — snapshot file is 4.0 d old (limit 48 h)" in text, "an old overview snapshot is stale")
        result = run_cli(base_args(repo_root, None, None, [empty_oracle_dir]) + ["--no-state"])
        expect("the path holds no .json result files" in result.stdout, "an empty oracle directory is named unavailable")
        result = run_cli(base_args(repo_root, None, None, [os.path.join(tmp, "oracle")]) + ["--no-state"])
        text = result.stdout
        expect("source-stale: oracle comparison — newest run is 4.8 d old" in text, "an old oracle run is stale")

        # ---------------- case: overview shape refusals -----------------------
        result = run_cli(base_args(repo_root, None, summary_path) + ["--no-state"])
        expect("no per-session rows (variant 'summary')" in result.stdout, "the summary variant is refused with its reason")
        result = run_cli(base_args(repo_root, None, error_path) + ["--no-state"])
        expect("did not finish reading (exit 3, read: remote read failed mid-snapshot)" in result.stdout, "an unfinished archive read is refused, never read as zero")
        expect("source-unavailable: overview document — did not finish reading" in result.stdout, "the refusal is an unavailable-source anomaly")

        # ---------------- case: dedup axis --------------------------------------
        result = run_cli(base_args(repo_root, None, dedup_path) + ["--no-state"])
        text = result.stdout
        rows = [line for line in text.splitlines() if line.startswith("| Claude Code ")]
        expect(any("| 1 sess" in line and "| 2 |" in line and "| 1 |" in line for line in rows), "one conversation on two machines reads 2 rows and 1 conversation")
        # The dedup document holds two machines and one tool, so a tool with no
        # sessions at all reads 0 in each machine cell and 0 sessions overall.
        # The row no longer ends in three dashes: its editorial columns come
        # from the real registry now, and a recorded priority is not a dash.
        second_display = catalog.local[1].display
        expect(
            any(
                line.startswith(f"| {second_display} ") and "| 0 | - | 0 sessions |" in line
                for line in text.splitlines()
            ),
            "a tool with no rows shows a measured 0 sessions, never a negative-space guess",
        )

        # ---------------- case: editorial fields from their own sources --------
        # A root built from the real registry and contract shapes, so the
        # reading is exercised through the same generator loaders the real
        # sources go through. Dates are picked against NOW: one fresh, one 120
        # days old (past the 90-day window), and rows with no date at all.
        fixture_root = os.path.join(tmp, "fixture-repo")
        gen = _load_generator(repo_root)
        # --root has to hold the generator too: it is what reads the two
        # sources, so the fixture exercises the real loaders and their
        # validation, not a fixture-only shortcut.
        with open(os.path.join(repo_root, "scripts", "gen-support-matrix.py"), "r", encoding="utf-8") as handle:
            generator_source = handle.read()
        os.makedirs(os.path.join(fixture_root, "scripts"), exist_ok=True)
        with open(os.path.join(fixture_root, "scripts", "gen-support-matrix.py"), "w", encoding="utf-8") as handle:
            handle.write(generator_source)
        stale_date = (NOW_DT - dt.timedelta(days=120)).date().isoformat()
        fresh_date = (NOW_DT - dt.timedelta(days=4)).date().isoformat()
        _write_json(
            os.path.join(fixture_root, gen.REGISTRY_REL),
            {
                "schema_version": 1,
                "generated": "2026-01-01",
                "harnesses": [
                    {
                        "id": "tool-fresh",
                        "display_name": "Tool Fresh",
                        "verified": {
                            "date": fresh_date,
                            "scope": "a real session archived end to end",
                        },
                        "dev_priority": "high",
                        "known_issue": "one caveat with a pointer",
                        "paths": {
                            "macos": {
                                "template": "~/.tool-fresh/<uuid>.jsonl",
                                "format": "jsonl",
                                "confidence": "source-confirmed",
                                "source": "measured",
                            }
                        },
                    },
                    {
                        "id": "tool-stale",
                        "display_name": "Tool Stale",
                        "verified": {"date": stale_date, "scope": "a real session archived end to end"},
                        "dev_priority": "normal",
                        "paths": {
                            "macos": {
                                "template": "~/.tool-stale",
                                "format": "sqlite",
                                "confidence": "from-source",
                                "source": "measured",
                            }
                        },
                    },
                    {
                        "id": "tool-silent",
                        "display_name": "Tool Silent",
                        "dev_priority": "low",
                        "known_issue": "no end-to-end run recorded yet",
                        "paths": {
                            "macos": {
                                "template": "~/.tool-silent",
                                "format": "jsonl",
                                "confidence": "unascertained",
                                "source": "not read",
                            }
                        },
                    },
                    # Two ids that name both a local tool and a web platform,
                    # with different records on each side: the CLI row must not
                    # inherit the platform's verification, or the reverse.
                    {
                        "id": "shared-local-silent",
                        "display_name": "Shared Local Silent",
                        "dev_priority": "low",
                        "known_issue": "the tool side has no verification",
                        "paths": {
                            "macos": {
                                "template": "~/.shared",
                                "format": "jsonl",
                                "confidence": "from-source",
                                "source": "measured",
                            }
                        },
                    },
                    {
                        "id": "shared-web-silent",
                        "display_name": "Shared Web Silent",
                        "verified": {"date": fresh_date, "scope": "the tool side was verified"},
                        "dev_priority": "high",
                        "paths": {
                            "macos": {
                                "template": "~/.shared-web",
                                "format": "jsonl",
                                "confidence": "from-source",
                                "source": "measured",
                            }
                        },
                    },
                ],
            },
        )
        os.makedirs(os.path.join(fixture_root, os.path.dirname(gen.CONTRACT_REL)), exist_ok=True)
        with open(os.path.join(fixture_root, gen.CONTRACT_REL), "w", encoding="utf-8") as handle:
            handle.write(
                "export const ALL_PLATFORMS: readonly ChatPlatform[] = [\n"
                "  {\n"
                "    id: 'web-fresh',\n"
                "    origins: ['https://web-fresh.example'],\n"
                "    devPriority: 'normal',\n"
                "    knownIssue: 'one caveat with a pointer',\n"
                "    lastVerified: {\n"
                f"      date: '{fresh_date}',\n"
                "      version: '1.2.3',\n"
                "      scope: 'a real conversation archived end to end',\n"
                "    },\n"
                "    credibility: 'from-source',\n"
                "    channel: 'stable',\n"
                "  },\n"
                "  {\n"
                "    id: 'web-stale',\n"
                "    origins: ['https://web-stale.example'],\n"
                "    lastVerified: {\n"
                f"      date: '{stale_date}',\n"
                "      scope: 'a real conversation archived end to end',\n"
                "    },\n"
                "    credibility: 'from-source',\n"
                "    channel: 'experimental',\n"
                "  },\n"
                "  {\n"
                "    id: 'shared-local-silent',\n"
                "    origins: ['https://shared-local-silent.example'],\n"
                "    devPriority: 'high',\n"
                "    lastVerified: {\n"
                f"      date: '{fresh_date}',\n"
                "      scope: 'the platform side was verified',\n"
                "    },\n"
                "    credibility: 'from-source',\n"
                "    channel: 'stable',\n"
                "  },\n"
                "  {\n"
                "    id: 'shared-web-silent',\n"
                "    origins: ['https://shared-web-silent.example'],\n"
                "    devPriority: 'low',\n"
                "    knownIssue: 'the platform side has no verification',\n"
                "    credibility: 'unverified',\n"
                "    channel: 'experimental',\n"
                "  },\n"
                "];\n"
            )
        result = run_cli(base_args(fixture_root) + ["--no-state"])
        expect(result.returncode == 0, f"the fixture-root run exits 0 (got {result.returncode}: {result.stderr})")
        text = result.stdout
        expect(
            f"available · 9 editorial row(s) · 6 with a last_verified date · the SB-1 sources under {fixture_root}" in text,
            "the editorial source row counts the real-shaped rows and names the reading that answered",
        )
        expect("source-unavailable: editorial fields" not in text, "the two sources are present, so the fields are available")
        expect(
            f"verification-stale: tool-stale — last verified {stale_date} (120.0 d ago, above the 90 d window)" in text,
            "a registry verification past the window is stale",
        )
        expect(
            f"verification-stale: web-stale — last verified {stale_date} (120.0 d ago, above the 90 d window)" in text,
            "a contract verification past the window is stale",
        )
        expect("verification-stale: tool-fresh" not in text and "verification-stale: web-fresh" not in text, "a fresh verification is not stale")
        expect(
            "verification-stale — no readable last_verified date for: "
            "tool-silent, shared-local-silent (local tool), shared-web-silent (web platform)" in text,
            "rows carrying editorial fields but no date are named, and a shared id says which of the two rows it means",
        )
        local_shared = [
            line for line in text.splitlines() if line.startswith("| Shared Local Silent")
        ]
        web_shared = [line for line in text.splitlines() if line.startswith("| shared-local-silent ")]
        expect(
            len(local_shared) == 1 and local_shared[0].endswith("| low | - |"),
            "the local row of a shared id reads the registry's record, not the contract's",
        )
        expect(
            len(web_shared) == 1 and f"| - | high | {fresh_date} |" in web_shared[0],
            "the web row of a shared id reads the contract's record",
        )
        reversed_local = [line for line in text.splitlines() if line.startswith("| Shared Web Silent")]
        reversed_web = [line for line in text.splitlines() if line.startswith("| shared-web-silent ")]
        expect(
            len(reversed_local) == 1 and reversed_local[0].endswith(f"| high | {fresh_date} |"),
            "the qualifier follows the row, not the id: the local row keeps the registry's date",
        )
        expect(
            len(reversed_web) == 1 and "| - | low | - |" in reversed_web[0],
            "the web row of the reversed pair has no date of its own",
        )
        # An override replaces that reading rather than adding to it.
        override_path = _write_json(
            os.path.join(tmp, "editorial-override.json"),
            {"platforms": {"tool-stale": {"dev_priority": "P0", "last_verified": fresh_date}}},
            mtime=UNIX_NOW,
        )
        result = run_cli(base_args(fixture_root, editorial=override_path) + ["--no-state"])
        expect(result.returncode == 0, "the override run exits 0")
        text = result.stdout
        expect(
            f"available · 1 editorial row(s) · 1 with a last_verified date · override file {override_path}" in text,
            "an override replaces the sources and says so",
        )
        expect("verification-stale: " not in text, "the override's fresh date leaves no stale row")
        expect("no readable last_verified date for" not in text, "rows absent from the override are not reported at all")

        # ---------------- case: read-only inputs -------------------------------
        input_paths: dict[str, bytes] = {}
        for base, dirs, names in os.walk(tmp):
            for name in names:
                if name == "board.md" or name.endswith(STATE_SUFFIX):
                    continue
                full = os.path.join(base, name)
                with open(full, "rb") as handle:
                    input_paths[full] = handle.read()
        result = run_cli(
            base_args(repo_root, ext_dir, overview_path, [oracle_dir], editorial_path)
            + ["--out", os.path.join(tmp, "readonly-board.md")]
        )
        expect(result.returncode == 0, "the read-only probe run exits 0")
        for path, body in input_paths.items():
            if "readonly-board" in path:
                continue
            try:
                with open(path, "rb") as handle:
                    same = handle.read() == body
            except OSError:
                same = False
            expect(same, f"input untouched by the run: {path}")
        expect(
            not any(name.endswith(STATE_SUFFIX) for name in os.listdir(ext_dir)),
            "no state file is ever written beside a stage directory's records",
        )

        # ---------------- case: stdout default, no state ------------------------
        before_state_files = sum(1 for _base, _dirs, names in os.walk(tmp) for name in names if name.endswith(STATE_SUFFIX))
        result = run_cli(base_args(repo_root, ext_dir) + ["--no-state"])
        expect(result.returncode == 0, "stdout run exits 0")
        expect(result.stdout.startswith("# chat-stasher platform scoreboard"), "stdout carries the board")
        after_state_files = sum(1 for _base, _dirs, names in os.walk(tmp) for name in names if name.endswith(STATE_SUFFIX))
        expect(before_state_files == after_state_files, "stdout mode without --state writes no state")

        # ---------------- case: usage errors and write failures -----------------
        result = run_cli(["--root", repo_root, "--now", "yesterday"])
        expect(result.returncode == 2, "an unparseable --now is a usage error (2)")
        result = run_cli(["--root", os.path.join(tmp, "definitely-not-a-repo")])
        expect(result.returncode == 2, "a root without the platform sources is a usage error (2)")
        result = run_cli(
            base_args(repo_root, ext_dir)
            + ["--out", os.path.join(tmp, "no-such-directory", "board.md"), "--no-state"]
        )
        expect(result.returncode == 1, "an output write failure exits 1")
        expect("could not write the output" in result.stderr, "a write failure says so on stderr")
        result = run_cli(base_args(repo_root, None, None, None, editorial_badpath) + ["--no-state"])
        expect("unparseable: when-was-that" in result.stdout, "an unparseable last_verified is shown as written, never math-ed")
        expect("verification-stale: " not in result.stdout.split("Rules that")[0], "an unparseable date never fires the verify rule")

    print(f"[scoreboard] SELFTEST: {'PASS' if failures == 0 else 'FAIL'} ({count} assertion(s), {failures} wrong)")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
