#!/usr/bin/env python3
"""ui-scale-fixture.py — synthesise chat-stasher stage trees large enough to
measure the dashboard at scale (UIA-9).

The dashboard's performance budget (`/sessions` <= 400 KB, `/` <= 300 KB,
`/reader` <= 250 KB, `/content` 1 MiB per window) cannot be checked against a
real archive: nobody has 10 000 conversations to hand, and a fixture copied from
a real one is conversation content, which does not belong in a measurement
artefact at all. So this generates one — synthetic lines, no real content, no
real ids, no real timestamps, deterministic from a seed.

What it produces is a **stage tree** in the product's own sealed layout, one
directory per machine partition (a stage *is* one machine's outbox, and `push`
refuses a stage holding two partitions):

    <out>/stage-<machine>/sessions/<machine>/<session-id>/000/000001.jsonl

It does **not** write the activity sidecar, and it does not push. Both are the
product's own commands and re-running them here would be a second
implementation of a format that already has one:

    chat-stasher activity-index --stage <stage> --machine <machine>
    chat-stasher push --stage <stage> --repo <repo> --key-file <key> --machine <machine>

`scripts/dev/ui-scale-probe.py` drives all three and measures the pages.

Harness composition
-------------------
`--profile timed` uses only harnesses whose archived line `activity-index` can
read a conversation time from, so the resulting index covers every session.
That is the profile for measuring **page size against archive size**: nothing
below varies except N.

`--profile all` (the default) covers every harness id in the shipped registry
(`crates/chat-stasher/data/harness-registry-v1.json`) plus the browser-extension
web-chat group, so every harness type is exercised. Some of those harnesses have
no time extractor at all, so their sessions land in the index as
`time_source: unknown` and the overview reports them in its "Time unknown"
section — a property of the tool, not of this fixture, and one the probe records
as a measured coverage number on every run.

Privacy line: this writes only synthetic strings of its own making. It reads one
file, the harness registry shipped with the binary. It prints counts, byte sizes
and harness ids — never a line of its output.

Exit codes: 0 = trees written · 2 = usage error.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import sys
from datetime import datetime, timezone

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
REGISTRY_PATH = os.path.join(
    REPO_ROOT, "crates", "chat-stasher", "data", "harness-registry-v1.json"
)

# The browser-extension web-chat harnesses. Mirror of `WEB_HARNESSES` in
# crates/chat-stasher/src/activity.rs: these sessions are delivered by the
# extension as an inbox bundle rather than read from a file or SQLite store, and
# their ids reach the archive as `<harness>.<native id>` (see the `deepseek.…`
# session in crates/chat-stasher/tests/w15_ui_test.rs).
WEB_CHAT_HARNESSES = (
    "chatgpt",
    "deepseek",
    "claude",
    "grok",
    "gemini",
    "perplexity",
    "kimi",
)

# Harnesses this generator can synthesise a line for that `activity-index` reads
# a conversation time from. Each entry names the reader it is shaped against in
# crates/chat-stasher/src/activity.rs (`line_time`'s dispatch). Getting a shape
# wrong does not fail loudly here — the session simply becomes `unknown` in the
# index — which is why every probe run reports the measured coverage next to the
# page sizes instead of trusting this list.
TIMED_SHAPES = (
    "claude-code",  # top_level_timestamp: top-level `timestamp`, RFC 3339
    "codex",  # top_level_timestamp (codex is unclassified, so a time is read)
    "opencode",  # opencode_time: `messages[].time_created`, epoch millis
    "cursor",  # cursor_time: `session.value.createdAt`, RFC 3339
    "gemini-cli",  # gemini_time: `messages[].timestamp`, RFC 3339
    "grok",  # grok_time: `session.updated_at`, epoch seconds (CLI shape)
    "kimi-code",  # kimi_code_time: top-level `time`, epoch millis, on a
    #              conversation op (`context.append_message`)
)

# Web-chat harnesses whose archived payload this generator can also shape into
# something `web_time` reads a time out of. gemini and kimi are absent on
# purpose: their readers take a `)]}'`-guarded batchexecute stream and a
# shape-specific span respectively, and a fixture that guesses there would
# record a wrong time rather than an honest "unknown".
WEB_TIMED_SHAPES = ("chatgpt", "deepseek", "claude")

SHARD_SUFFIX = ".jsonl"
BUCKET = "000"  # store::shard_bucket_name(seq<=20) == "000"; every session here
#               has at most 3 shards, so one bucket is the whole story.

# A conversation-time window the fixture spreads across. Fixed rather than
# relative to "now" so two runs of the same seed produce the same bytes.
WINDOW_START = 1735689600  # 2025-01-01T00:00:00Z
WINDOW_SPAN = 20_000_000  # ~231 days


def rfc3339(unix: int) -> str:
    return datetime.fromtimestamp(unix, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# --------------------------------------------------------------- line shapes
#
# One builder per harness. Each returns one synthetic JSON object; the caller
# writes them one per line. The text is deliberately contentless: a size, a
# role and a timestamp is everything a page-size measurement needs and everything
# a privacy line allows.


def _line(harness: str, sid: str, i: int, unix: int) -> dict:
    role = "user" if i % 2 == 0 else "assistant"
    text = f"synthetic line {i}"
    if harness == "claude-code":
        return {
            "parentUuid": None,
            "isMeta": None,
            "sessionId": sid,
            "type": role,
            "message": {"role": role, "content": text},
            "uuid": f"{sid}-{i:06d}",
            "timestamp": rfc3339(unix),
            "cwd": "/synthetic",
            "version": "0.0.0",
        }
    if harness == "codex":
        return {
            "timestamp": rfc3339(unix),
            "type": "event_msg",
            "payload": {"type": "agent_message", "message": text},
        }
    if harness == "opencode":
        return {
            "schema": "opencode-v1",
            "session": {"time_created": unix * 1000, "time_updated": unix * 1000},
            "messages": [
                {
                    "role": role,
                    "time_created": unix * 1000,
                    "content": text,
                }
            ],
        }
    if harness == "cursor":
        return {
            "kind": "composer",
            "session": {"value": {"createdAt": rfc3339(unix), "text": text}},
        }
    if harness == "gemini-cli":
        return {
            "startTime": rfc3339(unix),
            "lastUpdated": rfc3339(unix),
            "messages": [{"timestamp": rfc3339(unix), "type": role, "content": text}],
        }
    if harness == "grok":
        return {
            "schema": "grok-cli-v1",
            "table": "session_docs",
            "session": {"session_id": sid, "updated_at": unix, "text": text},
        }
    if harness == "kimi-code":
        return {
            "type": "context.append_message",
            "role": role,
            "time": unix * 1000,
            "text": text,
        }
    if harness == "chatgpt":
        return {
            "mapping": {f"n{i}": {"message": {"create_time": unix, "author": {"role": role}}}},
            "create_time": unix,
            "update_time": unix,
        }
    if harness == "deepseek":
        return {
            "data": {
                "biz_data": {
                    "chat_messages": [{"inserted_at": unix, "role": role, "text": text}],
                    "chat_session": {"inserted_at": unix, "updated_at": unix},
                }
            }
        }
    if harness == "claude":  # claude.ai, the web-chat harness (not claude-code)
        return {
            "created_at": rfc3339(unix),
            "updated_at": rfc3339(unix),
            "chat_messages": [
                {"created_at": rfc3339(unix), "updated_at": rfc3339(unix), "text": text}
            ],
        }
    # No time extractor for this harness id (activity.rs `line_time` returns
    # NoTimestampField for it), or no shape this generator is willing to guess.
    # The line is valid JSON that carries no timestamp, which is exactly what the
    # tool would find in such an archive: the session is indexed, and its time is
    # recorded as unknown with a reason.
    return {"role": role, "seq": i, "text": text}


def _big_line(harness: str, sid: str, i: int, unix: int, pad: int) -> dict:
    line = _line(harness, sid, i, unix)
    line["padding"] = "x" * pad
    return line


# ------------------------------------------------------------------- fixture


def _harness_pool(profile: str, registry_ids: list[str]) -> list[str]:
    if profile == "timed":
        return sorted(TIMED_SHAPES)
    pool = sorted(set(registry_ids) | set(WEB_CHAT_HARNESSES))
    return pool


def _known_time(harness: str, profile: str) -> bool:
    if harness in TIMED_SHAPES:
        return True
    return profile == "all" and harness in WEB_TIMED_SHAPES


def _registry_ids() -> list[str]:
    """Harness ids from the registry the binary ships with.

    Read rather than hard-coded, so a harness added to the registry is exercised
    by the next fixture instead of being silently absent from it. A registry that
    cannot be read is an error, not an empty list: "we could not look" must not
    become "there are none".
    """
    try:
        with open(REGISTRY_PATH, "r", encoding="utf-8") as fh:
            doc = json.load(fh)
    except (OSError, ValueError) as exc:
        raise SystemExit(f"ui-scale-fixture: cannot read the harness registry: {exc}")
    harnesses = doc.get("harnesses")
    if not isinstance(harnesses, list) or not harnesses:
        raise SystemExit(
            f"ui-scale-fixture: {REGISTRY_PATH} carries no harness list; "
            "refusing to build a fixture that claims to cover every harness"
        )
    return [h["id"] for h in harnesses if isinstance(h, dict) and "id" in h]


def _machine_names(count: int) -> list[str]:
    if count < 1:
        raise SystemExit("ui-scale-fixture: --machines must be at least 1")
    # Single-letter partitions, the same shape the fixtures elsewhere in this
    # repository use (`mbp-a`), kept short because they appear in every URL.
    return [f"scale-{chr(ord('a') + i)}" for i in range(count)]


def build(
    out: str,
    sessions: int,
    machines: int,
    profile: str,
    seed: int,
    big_sessions: int,
    big_bytes: int,
    clean: bool,
) -> dict:
    """Write the stage trees and return a counts-only summary."""
    if sessions < 1:
        raise SystemExit("ui-scale-fixture: --sessions must be at least 1")
    if profile not in ("timed", "all"):
        raise SystemExit("ui-scale-fixture: --profile must be `timed` or `all`")

    rng = random.Random(seed)
    registry_ids = _registry_ids()
    pool = _harness_pool(profile, registry_ids)
    machine_names = _machine_names(machines)

    if clean and os.path.isdir(out):
        shutil.rmtree(out)
    os.makedirs(out, exist_ok=True)

    big_every = 0
    if big_sessions > 0:
        # Spread the large sessions across the machines that get one.
        big_every = max(1, sessions // big_sessions)

    per_machine: dict[str, list[str]] = {m: [] for m in machine_names}
    expected_known = 0
    expected_unknown = 0
    total_bytes = 0

    for i in range(sessions):
        # Round-robin over machines, but advance the harness only every
        # `machines` sessions, so every harness appears on every machine
        # instead of being correlated with the partition it lands in.
        machine = machine_names[i % machines]
        harness = pool[(i // machines) % len(pool)]

        native = "".join(rng.choice("0123456789abcdef") for _ in range(32))
        sid = f"{harness}.{machine}.{native}"
        first = WINDOW_START + rng.randrange(WINDOW_SPAN)
        # A session's own span: 2..6 lines spread over 1..4 hours.
        n_lines = 2 + (i % 5)
        span = 3600 * (1 + (i % 4))
        is_big = big_sessions > 0 and i % big_every == 0 and (i // big_every) < big_sessions
        n_shards = 1 + (i % 3)

        if is_big:
            # One large line per shard rather than thousands of small ones: the
            # reader walks the same bytes either way, and a handful of lines
            # keeps the fixture build cheap.
            n_shards = 3
            pad = max(1, big_bytes // n_shards - 512)
            lines = [
                _big_line(
                    harness,
                    sid,
                    k,
                    first + (span * k // n_shards),
                    pad,
                )
                for k in range(n_shards)
            ]
        else:
            lines = [
                _line(harness, sid, k, first + (span * k // max(1, n_lines - 1)))
                for k in range(n_lines)
            ]

        # Deal the lines into shards in order; every shard is non-empty.
        chunks = [[] for _ in range(n_shards)]
        for k, line in enumerate(lines):
            chunks[k * n_shards // len(lines)].append(line)

        session_dir = os.path.join(
            out, f"stage-{machine}", "sessions", machine, sid, BUCKET
        )
        os.makedirs(session_dir, exist_ok=True)
        for seq, chunk in enumerate(chunks, start=1):
            if not chunk:
                continue
            path = os.path.join(session_dir, f"{seq:06d}{SHARD_SUFFIX}")
            with open(path, "w", encoding="utf-8") as fh:
                for line in chunk:
                    fh.write(json.dumps(line) + "\n")
            total_bytes += os.path.getsize(path)

        per_machine[machine].append(sid)
        if _known_time(harness, profile):
            expected_known += 1
        else:
            expected_unknown += 1

    never_matched = sorted(set(WEB_CHAT_HARNESSES) - set(pool))
    return {
        "out": os.path.abspath(out),
        "profile": profile,
        "seed": seed,
        "sessions": sessions,
        "machines": machine_names,
        "sessions_per_machine": {m: len(v) for m, v in per_machine.items()},
        "harnesses": pool,
        "harnesses_absent_from_registry": never_matched,
        "registry_harness_count": len(registry_ids),
        "big_sessions": big_sessions,
        "big_bytes": big_bytes,
        "total_bytes": total_bytes,
        "expected_known_time": expected_known,
        "expected_unknown_time": expected_unknown,
    }


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(
        description="Build synthetic chat-stasher stage trees at scale (UIA-9)."
    )
    ap.add_argument("--out", required=True, help="output root; stage-<machine>/ go inside")
    ap.add_argument("--sessions", type=int, default=10000)
    ap.add_argument("--machines", type=int, default=3)
    ap.add_argument("--profile", choices=("timed", "all"), default="all")
    ap.add_argument("--seed", type=int, default=166)
    ap.add_argument(
        "--big-sessions",
        type=int,
        default=2,
        help="sessions holding a multi-MiB payload, to exercise the content route",
    )
    ap.add_argument("--big-bytes", type=int, default=5 * 1024 * 1024)
    ap.add_argument(
        "--no-clean",
        action="store_true",
        help="do not remove an existing --out tree first",
    )
    args = ap.parse_args(argv)

    summary = build(
        out=args.out,
        sessions=args.sessions,
        machines=args.machines,
        profile=args.profile,
        seed=args.seed,
        big_sessions=args.big_sessions,
        big_bytes=args.big_bytes,
        clean=not args.no_clean,
    )
    json.dump(summary, sys.stdout, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
