#!/usr/bin/env python3
"""Summarize recorded push retries without opening an archive or destination.

Input is an offline JSONL stream with one object per run-once push attempt:

  {"event":"run_once_push","attempt_id":"a1","retry_of":null,
   "outcome":"interrupted"}
  {"event":"run_once_push","attempt_id":"a2","retry_of":"a1",
   "outcome":"completed","retried_bytes":4096,"retried_sessions":2}

`retry_of: null` explicitly marks an attempt that is not a retry; an omitted
`retry_of` leaves the relationship unknown. `retried_bytes` and
`retried_sessions` are optional measurements from the recording system, not
estimates from stage size. Only attempts explicitly linked to an earlier
interrupted attempt count as confirmed retries. Missing measurements remain
unknown. Identifiers are used only to match rows and are never printed.

Usage:
    python3 scripts/push-retry-report.py --input events.jsonl
    python3 scripts/push-retry-report.py --input - < events.jsonl
    python3 scripts/push-retry-report.py --selftest

The script reads only the supplied stream, makes no network calls, and never
opens a chat-stasher archive or destination. Exit codes: 0 = report generated ·
1 = invalid/unreadable event stream · 2 = usage error.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from io import StringIO
from typing import Any, TextIO

EVENT = "run_once_push"
OUTCOMES = {"interrupted", "completed", "failed"}


class InputError(Exception):
    """The supplied event stream is not valid according to this schema."""


@dataclass(frozen=True)
class Attempt:
    attempt_id: str
    retry_of: str | None
    retry_link_known: bool
    outcome: str
    retried_bytes: int | None
    retried_sessions: int | None


@dataclass(frozen=True)
class Report:
    records: int
    interrupted: int
    retries: int
    unmatched_retries: int
    retry_link_unknown: int
    byte_total: int
    byte_unknown: int
    session_total: int
    session_unknown: int


def measurement(row: dict[str, Any], key: str, line: int) -> int | None:
    if key not in row or row[key] is None:
        return None
    value = row[key]
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise InputError(f"line {line}: {key} must be a non-negative integer or null")
    return value


def read_events(handle: TextIO) -> list[Attempt]:
    attempts: list[Attempt] = []
    seen: dict[str, Attempt] = {}
    for line, raw in enumerate(handle, 1):
        if not raw.strip():
            continue
        try:
            row = json.loads(raw)
        except (json.JSONDecodeError, UnicodeError):
            raise InputError(f"line {line}: invalid JSON") from None
        if not isinstance(row, dict) or row.get("event") != EVENT:
            raise InputError(f"line {line}: expected a {EVENT} object")
        attempt_id = row.get("attempt_id")
        if not isinstance(attempt_id, str) or not attempt_id:
            raise InputError(f"line {line}: attempt_id must be a non-empty string")
        if attempt_id in seen:
            raise InputError(f"line {line}: duplicate attempt_id")
        outcome = row.get("outcome")
        if not isinstance(outcome, str) or outcome not in OUTCOMES:
            raise InputError(f"line {line}: outcome must be interrupted, completed, or failed")
        retry_of = row.get("retry_of")
        if retry_of is not None and (not isinstance(retry_of, str) or not retry_of):
            raise InputError(f"line {line}: retry_of must be a non-empty string or null")
        attempt = Attempt(
            attempt_id=attempt_id,
            retry_of=retry_of,
            retry_link_known="retry_of" in row,
            outcome=outcome,
            retried_bytes=measurement(row, "retried_bytes", line),
            retried_sessions=measurement(row, "retried_sessions", line),
        )
        attempts.append(attempt)
        seen[attempt_id] = attempt
    return attempts


def summarize(attempts: list[Attempt]) -> Report:
    interrupted = sum(attempt.outcome == "interrupted" for attempt in attempts)
    retries = 0
    unmatched = 0
    retry_link_unknown = 0
    byte_total = byte_unknown = 0
    session_total = session_unknown = 0
    earlier: dict[str, Attempt] = {}
    for attempt in attempts:
        if not attempt.retry_link_known:
            retry_link_unknown += 1
        elif attempt.retry_of is not None:
            previous = earlier.get(attempt.retry_of)
            if previous is None or previous.outcome != "interrupted":
                unmatched += 1
            else:
                retries += 1
                if attempt.retried_bytes is None:
                    byte_unknown += 1
                else:
                    byte_total += attempt.retried_bytes
                if attempt.retried_sessions is None:
                    session_unknown += 1
                else:
                    session_total += attempt.retried_sessions
        earlier[attempt.attempt_id] = attempt
    return Report(
        records=len(attempts),
        interrupted=interrupted,
        retries=retries,
        unmatched_retries=unmatched,
        retry_link_unknown=retry_link_unknown,
        byte_total=byte_total,
        byte_unknown=byte_unknown,
        session_total=session_total,
        session_unknown=session_unknown,
    )


def measurement_text(
    total: int, unknown: int, retries: int, link_unknown: int, unmatched: int
) -> str:
    if retries == 0:
        unresolved = link_unknown + unmatched
        if unresolved:
            return f"unknown (no confirmed retries; {unresolved} attempt records have unresolved retry linkage)"
        return "0 (no retry links recorded)"
    if unknown == retries:
        result = f"unknown ({unknown}/{retries} linked retry records lack this measurement)"
    elif unknown:
        result = f"at least {total} ({unknown}/{retries} linked retry records lack this measurement)"
    else:
        result = str(total)
    unresolved = link_unknown + unmatched
    if unresolved:
        if unknown == retries:
            return f"unknown ({unresolved} attempt records have unresolved retry linkage; " \
                f"{result})"
        return f"at least {total} ({unresolved} attempt records have unresolved retry linkage; {result})"
    return result


def render(report: Report) -> str:
    if report.records == 0:
        retries = "unknown (no attempt records)"
        byte_measure = "unknown (no attempt records)"
        session_measure = "unknown (no attempt records)"
    else:
        retries = str(report.retries)
        byte_measure = measurement_text(
            report.byte_total, report.byte_unknown, report.retries,
            report.retry_link_unknown, report.unmatched_retries
        )
        session_measure = measurement_text(
            report.session_total, report.session_unknown, report.retries,
            report.retry_link_unknown, report.unmatched_retries
        )
    return "\n".join(
        (
            "[push-retry] offline event summary",
            f"[push-retry] attempt_records={report.records}",
            f"[push-retry] interrupted_attempts={report.interrupted}",
            f"[push-retry] linked_retries={retries}",
            f"[push-retry] attempts_without_retry_link={report.retry_link_unknown}",
            f"[push-retry] retries_without_interrupted_parent={report.unmatched_retries}",
            f"[push-retry] retried_bytes={byte_measure}",
            f"[push-retry] retried_sessions={session_measure}",
            "[push-retry] note=measurements are explicit retry-record fields; no stage-size estimate",
        )
    )


def selftest() -> int:
    cases: list[tuple[str, str, tuple[str, ...]]] = [
        (
            "measured retry",
            '{"event":"run_once_push","attempt_id":"opaque-a","retry_of":null,"outcome":"interrupted"}\n'
            '{"event":"run_once_push","attempt_id":"opaque-b","retry_of":"opaque-a",'
            '"outcome":"completed","retried_bytes":4096,"retried_sessions":2}\n',
            ("linked_retries=1", "retried_bytes=4096", "retried_sessions=2"),
        ),
        (
            "missing measurements stay unknown",
            '{"event":"run_once_push","attempt_id":"x","retry_of":null,"outcome":"interrupted"}\n'
            '{"event":"run_once_push","attempt_id":"y","retry_of":"x","outcome":"failed"}\n',
            (
                "linked_retries=1",
                "retried_bytes=unknown (1/1 linked retry records lack this measurement)",
                "retried_sessions=unknown (1/1 linked retry records lack this measurement)",
            ),
        ),
        (
            "partial measurements are lower bounds",
            '{"event":"run_once_push","attempt_id":"p","retry_of":null,"outcome":"interrupted"}\n'
            '{"event":"run_once_push","attempt_id":"q","retry_of":"p","outcome":"failed",'
            '"retried_bytes":512}\n',
            ("retried_bytes=512", "retried_sessions=unknown (1/1 linked retry records lack this measurement)"),
        ),
        (
            "unmatched retry link",
            '{"event":"run_once_push","attempt_id":"z","retry_of":"missing",'
            '"outcome":"completed","retried_bytes":8,"retried_sessions":1}\n',
            ("linked_retries=0", "retries_without_interrupted_parent=1", "retried_bytes=unknown (no confirmed retries; 1 attempt records have unresolved retry linkage)"),
        ),
        (
            "missing retry linkage stays unknown",
            '{"event":"run_once_push","attempt_id":"m","outcome":"interrupted"}\n',
            (
                "attempts_without_retry_link=1",
                "retried_bytes=unknown (no confirmed retries; 1 attempt records have unresolved retry linkage)",
            ),
        ),
    ]
    failures = 0
    for label, source, expected in cases:
        try:
            output = render(summarize(read_events(StringIO(source))))
            ok = all(fragment in output for fragment in expected)
            # Opaque identifiers must never enter the report.
            ok = ok and "opaque-a" not in output and "opaque-b" not in output
        except InputError:
            ok = False
        failures += not ok
        print(f"[push-retry] selftest {'ok' if ok else 'WRONG'}: {label}")

    try:
        read_events(
            StringIO('{"event":"run_once_push","attempt_id":"x","retry_of":null,"outcome":"interrupted",'
                     '"retried_bytes":-1}\n')
        )
        rejected_negative = False
    except InputError:
        rejected_negative = True
    failures += not rejected_negative
    print(f"[push-retry] selftest {'ok' if rejected_negative else 'WRONG'}: rejects invalid measurement")
    print(f"[push-retry] SELFTEST: {'PASS' if failures == 0 else 'FAIL'} ({failures} wrong)")
    return 0 if failures == 0 else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Summarize offline push-attempt JSONL records.")
    parser.add_argument("--input", help="JSONL event file, or - for stdin")
    parser.add_argument("--selftest", action="store_true", help="run synthetic checks")
    args = parser.parse_args(argv)
    if args.selftest:
        if args.input is not None:
            parser.error("--input cannot be combined with --selftest")
        return selftest()
    if args.input is None:
        parser.error("--input is required")
    try:
        if args.input == "-":
            attempts = read_events(sys.stdin)
        else:
            with open(args.input, encoding="utf-8") as handle:
                attempts = read_events(handle)
    except (OSError, UnicodeError, InputError) as error:
        message = str(error) if isinstance(error, InputError) else "cannot read input"
        print(f"[push-retry] {message}", file=sys.stderr)
        return 1
    print(render(summarize(attempts)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
