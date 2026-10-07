#!/usr/bin/env python3
"""extval-stress.py — the EXT-VAL wipe / random-half / re-collect stress driver.

WHAT THIS IS
------------
27-ORACLE §0 step 3 asks for a durability test, not a coverage measurement:
wipe all (or a random half) of the browser-side state and the stage for one
platform, let the extension back-fill again, and re-run the oracle. The
back-fill half needs a browser; the harness does not. This script is the
harness, and the procedure that drives it is written in 27-ORACLE §5.

It never touches the real archive. It only ever operates on an **isolated
root** — a throwaway directory under the isolated base (the e2e-matrix
pattern, `~/scratch/chat-stasher-e2e/`). A root that is the real chat-stasher
data directory, or sits inside it, is refused; so is a root outside the
isolated base, a `--stage-subdir` or `--browser-state-subdir` that escapes the
root, and a root that is an ancestor of the base rather than inside it. A
refusal changes nothing and exits 3, never 0: "we did not do it" is not "there
was nothing to do".

THE CYCLE
---------
  1. snapshot    per-session shard count, byte total and content digest, plus
                 one `digest-root` over the whole platform
  2. wipe        every *web* session dir for the platform, or a deterministic
                 random half of them; the same choice applied to the root's
                 `browser-state/` directory. Local-harness dirs in the same
                 bucket (`<platform>.<machine>.<id>`, 27-ORACLE §3.3) are never
                 touched: a browser re-backfill cannot return one, so wiping one
                 would report a loss that is not a loss
  3. re-collect  the collect path is re-run against the isolated root (an
                 external command; see --collect-cmd)
  4. re-compare  the oracle (`compare.py`) is re-run against the isolated stage
  5. delta       a before/after table — kept / restored / changed / wiped /
                 lost / appeared — and the oracle's recall / RED / SHORT /
                 extras before and after

A step that was not configured is reported `not-run` and the whole run is
`incomplete`. A run is `complete` only when every step ran. Nothing is ever
reported as zero in place of unknown.

DRY RUN
-------
`--dry-run` prints exactly which session directories and browser-state entries
would be removed, and where the collect and the oracle would run, and removes
nothing. A dry run, like a refusal, exits 3: it establishes nothing. It is the only mode the 2026-10-07 prep run used, because the stress
test it prepares still needs a browser.

WHAT IS CONFINED, AND WHAT IS NOT
---------------------------------
Every path this driver **writes or removes** must be inside the isolated root,
and that is enforced before anything is touched. The oracle and the export files
are **read-only inputs** and are not confined: `compare.py` and a real takeout
live outside the root, and reading them cannot change the archive.

PRIVACY
-------
Output carries counts, byte sizes, dates, 8-char id prefixes and 12-char
digests only — never conversation text, never a title, never a full session id.

USAGE
-----
  python3 scripts/extval-stress.py --self-test

  python3 scripts/extval-stress.py --platform deepseek [--mode random-half] \\
        [--root DIR] \\
        --collect-cmd '{bin} ingest --inbox {root}/inbox' \\
        --oracle /path/to/oracle/compare.py --export /path/to/export.json

Omit `--root` and the driver creates `extval-<platform>-<timestamp>/` under the
isolated base and deletes it again unless `--keep`.

EXIT CODES
----------
  0  the cycle completed and no count that must not move moved
  1  a count that must not move did move (recall fell, or RED/SHORT rose), or a
     session was lost, or the content of a kept session changed — or the
     self-test failed
  2  usage error
  3  not established: refused, a dry run, or a step left `not-run`
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import os
import random
import shlex
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone

EXIT_OK = 0
EXIT_FAIL = 1
EXIT_USAGE = 2
EXIT_NOT_ESTABLISHED = 3

DEFAULT_STAGE_SUBDIR = os.path.join("stage", "sessions")
DEFAULT_BROWSER_STATE_SUBDIR = "browser-state"
SHARD_SUFFIX = ".jsonl"
DIGEST_CHARS = 12
ID_PREFIX_CHARS = 8
STDERR_TAIL_CHARS = 200


class Refusal(Exception):
    """A path that is not an isolated root. Nothing has been touched."""


# --------------------------------------------------------------------- paths


def _real(path: str) -> str:
    return os.path.realpath(os.path.abspath(os.path.expanduser(path)))


def _inside(path: str, base: str) -> bool:
    """True when `path` is strictly inside `base` (not equal to it)."""
    p, b = _real(path), _real(base)
    if p == b:
        return False
    try:
        return os.path.commonpath([p, b]) == b
    except ValueError:  # different drives (Windows): the two are not comparable
        return False


def default_isolated_base() -> str:
    return os.path.join(os.path.expanduser("~"), "scratch", "chat-stasher-e2e")


def default_real_data_root() -> str:
    xdg = os.environ.get("XDG_DATA_HOME")
    if xdg:
        return os.path.join(xdg, "chat-stasher")
    return os.path.join(os.path.expanduser("~"), ".local", "share", "chat-stasher")


def require_isolated_root(root: str, base: str, real_data_root: str) -> str:
    """The three ways a path can fail to be an isolated root.

    Each is checked by name because the message is the deliverable: a refusal
    that does not say *which* rule refused leaves the reader guessing whether
    the base or the path was wrong.
    """
    r = _real(root)
    real = _real(real_data_root)
    if r == real or _inside(r, real):
        raise Refusal(
            f"refused: {r} is the real chat-stasher data root (or inside {real}); "
            "this driver only ever operates on an isolated root"
        )
    if r == _real(base) or _inside(base, r):
        raise Refusal(
            f"refused: {r} contains the isolated base {_real(base)} rather than "
            "being a root inside it"
        )
    if not _inside(r, base):
        raise Refusal(
            f"refused: {r} is not inside the isolated base {_real(base)}"
        )
    return r


def require_inside_root(path: str, root: str, what: str) -> str:
    """A path this driver will write or remove must stay inside the root."""
    if _real(path) == _real(root) or _inside(path, root):
        return _real(path)
    raise Refusal(
        f"refused: {what} {_real(path)} is outside the isolated root {_real(root)}"
    )


# ----------------------------------------------------------------- snapshots


def sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def snapshot_tree(dirpath: str) -> dict | None:
    """Counts and a content digest of every file under `dirpath`.

    None means the directory is not there at all — a measurement, not an error.
    An unreadable file inside it is recorded as its own state and never folded
    into a count, so an unreadable shard cannot read as a missing one.
    """
    if not os.path.isdir(dirpath):
        return None
    lines: list[str] = []
    total = 0
    shards = 0
    for base, _dirs, files in os.walk(dirpath):
        for name in files:
            full = os.path.join(base, name)
            rel = os.path.relpath(full, dirpath)
            try:
                size = os.path.getsize(full)
                digest = sha256_file(full)
            except OSError:
                lines.append(f"{rel}\tUNREADABLE")
                continue
            total += size
            if name.endswith(SHARD_SUFFIX):
                shards += 1
            lines.append(f"{rel}\t{size}\t{digest}")
    lines.sort()
    tree = hashlib.sha256("\n".join(lines).encode()).hexdigest()
    return {"files": len(lines), "shards": shards, "bytes": total, "digest": tree}


def platform_entries(platform_dir: str, platform: str) -> tuple[list[str], list[str]]:
    """(web session dirs, local-harness dirs) for one platform, sorted.

    The split is `compare.py`'s `stage_dirs`, and it is load-bearing here: a
    `<platform>.<id>` dir is a web capture, a `<platform>.<machine>.<id>` dir is
    a **local harness** session — a different product in the same platform
    bucket, never web coverage (§3.3). Only the web dirs are ever wiped. A
    browser re-backfill cannot return a harness dir, so wiping one would report
    a loss that is not a loss and quietly measure the wrong thing.
    """
    web: list[str] = []
    harness: list[str] = []
    if not os.path.isdir(platform_dir):
        return web, harness
    prefix = platform + "."
    for name in sorted(os.listdir(platform_dir)):
        if not name.startswith(prefix):
            continue
        rest = name[len(prefix):]
        (harness if "." in rest else web).append(name)
    return web, harness


def snapshot_platform(stage_sessions: str, machine: str, platform: str) -> dict:
    """{web session dir name -> tree snapshot} for one platform, sorted."""
    platform_dir = os.path.join(stage_sessions, machine)
    web, _harness = platform_entries(platform_dir, platform)
    return {name: snapshot_tree(os.path.join(platform_dir, name)) for name in web}


def digest_root(snaps: dict) -> str | None:
    """One digest over the whole platform — the e2e run's `digest-root`.

    None when the platform holds nothing at all: "we measured nothing" is not
    the digest of an empty set.
    """
    if not snaps:
        return None
    lines = [
        f"{name}:{snap['digest'] if snap else 'absent'}"
        for name, snap in sorted(snaps.items())
    ]
    return hashlib.sha256("\n".join(lines).encode()).hexdigest()[:DIGEST_CHARS]


def rollup(snaps: dict) -> dict:
    present = {name: snap for name, snap in snaps.items() if snap}
    return {
        "sessions": len(present),
        "shards": sum(snap["shards"] for snap in present.values()),
        "bytes": sum(snap["bytes"] for snap in present.values()),
    }


# ------------------------------------------------------------------- wiping


def plan_wipe(names: list[str], mode: str, seed: int) -> list[str]:
    """The exact set of entries a wipe removes.

    `random-half` is half, rounded down, and never a silent no-op on a
    non-empty set: one entry is wiped when flooring would remove none. The
    choice is seeded, so a rerun with the same `--seed` removes the same half
    and the run is reproducible.
    """
    ordered = sorted(names)
    if mode == "wipe-all":
        return ordered
    count = len(ordered) // 2
    if count == 0 and ordered:
        count = 1
    return sorted(random.Random(seed).sample(ordered, count))


def wipe_entries(targets: list[str]) -> list[str]:
    removed = []
    for path in targets:
        if os.path.isdir(path):
            shutil.rmtree(path)
            removed.append(path)
        elif os.path.lexists(path):
            os.remove(path)
            removed.append(path)
    return removed


# -------------------------------------------------------------- subprocesses


def run_collect(collect_cmd: str | None, root: str) -> dict:
    """Re-run the collect path inside the isolated root.

    The command is split with `shlex` and run without a shell, so a root path
    with a quote or a space cannot become a second command. XDG and the rustic
    cache are pointed into the root, so the collect cannot read or write the
    real state even by accident. Nothing but the exit code and one bounded
    stderr line is reported: stdout is where a tool would print conversation
    material.
    """
    if not collect_cmd:
        return {"status": "not-run"}
    argv = shlex.split(collect_cmd.replace("{root}", root))
    if not argv:
        return {"status": "not-run"}
    env = dict(os.environ)
    env.update(
        {
            "XDG_DATA_HOME": root,
            "XDG_CONFIG_HOME": os.path.join(root, "config"),
            "XDG_STATE_HOME": os.path.join(root, "state-home"),
            "CHAT_STASHER_RUSTIC_CACHE_DIR": os.path.join(root, "cache", "rustic"),
        }
    )
    try:
        proc = subprocess.run(argv, cwd=root, env=env, capture_output=True, text=True)
    except OSError as error:
        return {"status": "failed", "detail": str(error)}
    tail = [line for line in proc.stderr.strip().splitlines() if line.strip()]
    return {
        "status": "ran",
        "rc": proc.returncode,
        "stderr_tail": tail[-1][:STDERR_TAIL_CHARS] if tail else "",
    }


def run_oracle(args: argparse.Namespace, root: str, stage_sessions: str, tag: str) -> dict:
    """Re-run compare.py against the isolated stage and read its JSON back."""
    if not args.oracle or not args.export:
        return {"status": "not-run"}
    out_json = os.path.join(root, "out", f"oracle-{tag}.json")
    os.makedirs(os.path.dirname(out_json), exist_ok=True)
    argv = [
        sys.executable,
        _real(args.oracle),
        "--platform",
        args.platform,
        "--stage-root",
        stage_sessions,
        "--machine",
        args.machine,
        "--out-json",
        out_json,
    ]
    for export in args.export:
        argv += ["--export", _real(export)]
    if args.deep:
        argv.append("--deep")
    try:
        proc = subprocess.run(argv, capture_output=True, text=True)
    except OSError as error:
        return {"status": "failed", "detail": str(error)}
    if proc.returncode != 0:
        return {"status": "failed", "rc": proc.returncode}
    if not os.path.exists(out_json):
        return {"status": "failed", "rc": proc.returncode, "detail": "no --out-json written"}
    try:
        with open(out_json, encoding="utf-8") as handle:
            return {"status": "ran", "rc": proc.returncode, "result": json.load(handle)}
    except (OSError, json.JSONDecodeError) as error:
        return {"status": "failed", "detail": str(error)}


# --------------------------------------------------------------------- delta


def session_verdict(
    before: dict | None, after: dict | None, wiped: bool, collect_ran: bool
) -> str:
    if before is None and after is None:
        return "absent"
    if before is None:
        return "appeared"
    if after is None:
        if not wiped:
            return "anomaly"
        return "lost" if collect_ran else "wiped"
    if before["digest"] == after["digest"]:
        return "restored" if wiped else "kept"
    return "changed"


MUST_NOT_MOVE = (
    # (label, path into the oracle result, direction that is a regression)
    ("recall.found", ("recall", "found"), "down"),
    ("content_short.RED", ("content_short_counts", "RED"), "up"),
    ("content_short.SHORT", ("content_short_counts", "SHORT"), "up"),
)


def oracle_metric(result: dict | None, path: tuple[str, ...]):
    node = result
    for key in path:
        if not isinstance(node, dict) or key not in node:
            return None
        node = node[key]
    return node


def oracle_extras(result: dict | None):
    """`extras` is a count in the weak modes and a {count, ids} dict in the id ones."""
    value = oracle_metric(result, ("extras",))
    if isinstance(value, dict):
        return value.get("count")
    return value


def oracle_regressions(before: dict | None, after: dict | None) -> list[str]:
    """Which must-not-move counts moved the wrong way between two oracle runs."""
    if before is None or after is None:
        return []
    moved = []
    for label, path, direction in MUST_NOT_MOVE:
        old, new = oracle_metric(before, path), oracle_metric(after, path)
        if old is None or new is None:
            moved.append(f"{label}: not comparable ({old!r} -> {new!r})")
        elif direction == "down" and new < old:
            moved.append(f"{label}: {old} -> {new}")
        elif direction == "up" and new > old:
            moved.append(f"{label}: {old} -> {new}")
    return moved


def describe(snap: dict | None) -> str:
    if snap is None:
        return "absent"
    return f"{snap['shards']} shard(s) {snap['bytes']} B"


def short_id(name: str) -> str:
    """The part after the platform prefix, cut to 8 chars for the report."""
    _, _dot, rest = name.partition(".")
    return (rest or name)[:ID_PREFIX_CHARS]


# ----------------------------------------------------------------------- run


def execute(args: argparse.Namespace) -> tuple[int, dict]:
    base = args.isolated_base
    real_data_root = args.real_data_root
    created = False
    if args.root:
        root = require_isolated_root(args.root, base, real_data_root)
        if not os.path.isdir(root):
            raise Refusal(f"refused: {root} does not exist; seed it first or omit --root")
    else:
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        root = require_isolated_root(
            os.path.join(base, f"extval-{args.platform}-{stamp}"), base, real_data_root
        )
        os.makedirs(root, exist_ok=True)
        created = True

    stage_sessions = require_inside_root(
        os.path.join(root, args.stage_subdir), root, "--stage-subdir"
    )
    browser_state = require_inside_root(
        os.path.join(root, args.browser_state_subdir), root, "--browser-state-subdir"
    )

    try:
        platform_dir = os.path.join(stage_sessions, args.machine)
        web_names, harness_names = platform_entries(platform_dir, args.platform)
        before = {
            name: snapshot_tree(os.path.join(platform_dir, name)) for name in web_names
        }
        browser_before = snapshot_tree(browser_state)
        session_names = [
            name for name in before if os.path.isdir(os.path.join(platform_dir, name))
        ]
        browser_entries = (
            sorted(os.listdir(browser_state)) if os.path.isdir(browser_state) else []
        )
        wiped_sessions = plan_wipe(session_names, args.mode, args.seed)
        wiped_browser = plan_wipe(browser_entries, args.mode, args.seed)

        report: dict = {
            "platform": args.platform,
            "mode": args.mode,
            "seed": args.seed,
            "dry_run": bool(args.dry_run),
            "root": root,
            "root_created": created,
            "machine": args.machine,
            "isolated_base": _real(base),
            "real_data_root": _real(real_data_root),
            "date_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "digest_root_before": digest_root(before),
            "before": rollup(before),
            "browser_state_before": browser_before,
            "would_wipe": [short_id(name) for name in wiped_sessions],
            "would_wipe_browser": wiped_browser,
            "local_harness_skipped": len(harness_names),
        }

        if args.dry_run:
            report["verdict"] = "dry-run"
            report["steps"] = {
                "snapshot": "ran",
                "wipe": "not-run",
                "collect": "not-run",
                "compare": "not-run",
            }
            return EXIT_NOT_ESTABLISHED, report

        # The "before" oracle reading is the pre-wipe baseline: measuring it
        # after the wipe (or after the re-collect) would compare the run with
        # itself and could never show a recall that failed to come back.
        oracle_before = run_oracle(args, root, stage_sessions, "before")

        removed_sessions = wipe_entries(
            [os.path.join(platform_dir, name) for name in wiped_sessions]
        )
        removed_browser = wipe_entries(
            [os.path.join(browser_state, name) for name in wiped_browser]
        )

        collect = run_collect(args.collect_cmd, root)
        collect_ran = collect.get("status") == "ran" and collect.get("rc") == 0

        after = snapshot_platform(stage_sessions, args.machine, args.platform)
        oracle_after = run_oracle(args, root, stage_sessions, "after")

        wiped_set = set(wiped_sessions)
        rows = []
        for name in sorted(set(before) | set(after)):
            rows.append(
                {
                    "id": short_id(name),
                    "wiped": name in wiped_set,
                    "before": describe(before.get(name)),
                    "after": describe(after.get(name)),
                    "verdict": session_verdict(
                        before.get(name), after.get(name), name in wiped_set, collect_ran
                    ),
                }
            )

        broken = [row for row in rows if row["verdict"] in ("lost", "changed", "anomaly")]
        metric_moves = oracle_regressions(
            oracle_before.get("result"), oracle_after.get("result")
        )

        steps = {
            "snapshot": "ran",
            "wipe": "ran",
            "collect": collect.get("status", "not-run"),
            "compare": oracle_after.get("status", "not-run"),
        }
        complete = (
            collect_ran
            and oracle_before.get("status") == "ran"
            and oracle_after.get("status") == "ran"
        )
        report.update(
            {
                "steps": steps,
                "collect": collect,
                "wiped_removed": {
                    "sessions": len(removed_sessions),
                    "browser": len(removed_browser),
                },
                "digest_root_after": digest_root(after),
                "after": rollup(after),
                "browser_state_after": snapshot_tree(browser_state),
                "sessions": rows,
                "broken": [row["id"] for row in broken],
                "oracle_before": oracle_before.get("result"),
                "oracle_after": oracle_after.get("result"),
                "must_not_move_moved": metric_moves,
                "complete": complete,
            }
        )

        if broken or metric_moves:
            report["verdict"] = "regressed"
            return EXIT_FAIL, report
        if not complete:
            report["verdict"] = "incomplete"
            return EXIT_NOT_ESTABLISHED, report
        report["verdict"] = "held"
        return EXIT_OK, report
    finally:
        if created and not args.keep:
            # Re-validate immediately before removing: the only path this
            # process ever deletes is one already proven to be inside the
            # isolated base.
            require_isolated_root(root, base, real_data_root)
            shutil.rmtree(root, ignore_errors=True)


# -------------------------------------------------------------------- output


def render(report: dict) -> str:
    lines = []
    add = lines.append
    add(
        f"extval-stress — platform {report['platform']} · mode {report['mode']} · "
        f"seed {report['seed']}"
    )
    add(
        f"  isolated root : {report['root']}"
        + (" (created, self-deleting)" if report.get("root_created") else "")
    )
    add(f"  isolated base : {report['isolated_base']}")
    add(f"  real archive  : {report['real_data_root']} (never touched)")
    add(
        f"  digest-root   : before {report['digest_root_before']} · "
        f"after {report.get('digest_root_after', 'not-run')}"
    )
    before = report["before"]
    add(
        f"  stage         : before {before['sessions']} session(s) / {before['shards']} "
        f"shard(s) / {before['bytes']} B"
    )
    if "after" in report:
        after = report["after"]
        add(
            f"                  after  {after['sessions']} session(s) / {after['shards']} "
            f"shard(s) / {after['bytes']} B"
        )
    if report.get("local_harness_skipped"):
        add(
            f"  harness dirs  : {report['local_harness_skipped']} skipped "
            "(<platform>.<machine>.<id>, §3.3 — never web coverage)"
        )
    add(f"  browser-state : before {describe(report.get('browser_state_before'))}")
    if "browser_state_after" in report:
        add(f"                  after  {describe(report['browser_state_after'])}")
    add(f"  steps         : {', '.join(f'{k}={v}' for k, v in report['steps'].items())}")

    if report["dry_run"]:
        add("")
        add("  would wipe (nothing removed — dry run):")
        for sid in report["would_wipe"]:
            add(f"    - {sid}")
        for entry in report["would_wipe_browser"]:
            add(f"    - browser-state/{entry}")
        if not report["would_wipe"] and not report["would_wipe_browser"]:
            add("    (nothing matched the platform prefix)")
        add("")
        add(f"verdict: {report['verdict']} — a plan establishes nothing")
        return "\n".join(lines)

    add("")
    add(f"  {'session':<10} {'wiped':<6} {'before':<22} {'after':<22} verdict")
    for row in report["sessions"]:
        add(
            f"  {row['id']:<10} {'yes' if row['wiped'] else 'no':<6} "
            f"{row['before']:<22} {row['after']:<22} {row['verdict']}"
        )
    if not report["sessions"]:
        add("  (no sessions matched the platform prefix)")

    add("")
    ob, oa = report.get("oracle_before"), report.get("oracle_after")
    if ob or oa:
        add("  oracle (must-not-move: recall down, RED/SHORT up; extras may grow)")
        for label, path, _direction in MUST_NOT_MOVE:
            add(
                f"    {label:<20} before {oracle_metric(ob, path)}  "
                f"after {oracle_metric(oa, path)}"
            )
        add(f"    {'extras':<20} before {oracle_extras(ob)}  after {oracle_extras(oa)}")
    else:
        add("  oracle: not-run (no --oracle/--export, or it failed)")

    if report["must_not_move_moved"]:
        add("")
        for move in report["must_not_move_moved"]:
            add(f"  MUST-NOT-MOVE MOVED: {move}")
    if report["broken"]:
        add(f"  sessions not returned: {', '.join(report['broken'])}")
    add("")
    add(f"verdict: {report['verdict']}")
    return "\n".join(lines)


# ---------------------------------------------------------------------- argv


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="EXT-VAL wipe / random-half / re-collect stress driver "
        "(isolated root only).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the hermetic suite (no browser, no archive, no network)",
    )
    parser.add_argument("--platform", help="platform prefix, e.g. deepseek")
    parser.add_argument("--mode", choices=("wipe-all", "random-half"), default="wipe-all")
    parser.add_argument(
        "--seed",
        type=int,
        default=20261007,
        help="seed for random-half, so a rerun removes the same half",
    )
    parser.add_argument("--root", help="an existing isolated root (default: create one)")
    parser.add_argument("--isolated-base", default=default_isolated_base())
    parser.add_argument("--real-data-root", default=default_real_data_root())
    parser.add_argument("--stage-subdir", default=DEFAULT_STAGE_SUBDIR)
    parser.add_argument("--browser-state-subdir", default=DEFAULT_BROWSER_STATE_SUBDIR)
    parser.add_argument(
        "--machine",
        default="extval-isolated",
        help="partition name inside the isolated stage",
    )
    parser.add_argument(
        "--collect-cmd",
        help="the collect path, run inside the root; {root} is substituted",
    )
    parser.add_argument("--oracle", help="path to oracle/compare.py (read-only input)")
    parser.add_argument(
        "--export",
        action="append",
        default=[],
        help="export file for the oracle (read-only input; repeatable)",
    )
    parser.add_argument("--deep", action="store_true", help="pass --deep to the oracle")
    parser.add_argument("--dry-run", action="store_true", help="print the plan; remove nothing")
    parser.add_argument("--keep", action="store_true", help="keep a self-created root")
    parser.add_argument("--json", action="store_true", help="emit one JSON object on stdout")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.self_test:
        return run_self_test()
    if not args.platform:
        parser.error("--platform is required unless --self-test")

    try:
        code, report = execute(args)
    except Refusal as refusal:
        print(str(refusal), file=sys.stderr)
        return EXIT_NOT_ESTABLISHED

    if args.json:
        print(json.dumps(report, sort_keys=False))
    else:
        print(render(report))
    return code


# ----------------------------------------------------------------- self-test

STUB_COLLECT = r'''#!/usr/bin/env python3
"""Stand-in for the collect path: re-seal the seed corpus into the stage."""
import os
import shutil
import sys

root, machine = sys.argv[1], sys.argv[2]
src = os.path.join(root, "seed", machine)
dst = os.path.join(root, "stage", "sessions", machine)
if os.path.isdir(src):
    os.makedirs(dst, exist_ok=True)
    for name in sorted(os.listdir(src)):
        shutil.copytree(os.path.join(src, name), os.path.join(dst, name), dirs_exist_ok=True)
sys.exit(0)
'''

STUB_ORACLE = r'''#!/usr/bin/env python3
"""Stand-in for compare.py: emit the result keys the driver reads."""
import argparse
import json
import os

ap = argparse.ArgumentParser()
ap.add_argument("--platform")
ap.add_argument("--stage-root")
ap.add_argument("--machine")
ap.add_argument("--export", action="append", default=[])
ap.add_argument("--deep", action="store_true")
ap.add_argument("--out-json")
a = ap.parse_args()
base = os.path.join(a.stage_root, a.machine)
names = sorted(os.listdir(base)) if os.path.isdir(base) else []
found = len([n for n in names if n.startswith(a.platform + ".")])
expected = 3
res = {
    "platform": a.platform,
    "mode": "id",
    "recall": {"expected": expected, "found": found,
               "missing": expected - found,
               "recall": round(found / expected, 4) if expected else None},
    "content_short_counts": {"total": 0, "SHORT": 0, "RED": 0},
    "extras": 0,
    "stage_sessions": len(names),
}
if a.out_json:
    with open(a.out_json, "w") as fh:
        json.dump(res, fh)
print(json.dumps(res))
'''


def _quiet(argv: list[str]) -> tuple[int, str]:
    """Run main() with both streams captured, so the suite's output is its own."""
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer), contextlib.redirect_stderr(buffer):
        code = main(argv)
    return code, buffer.getvalue()


def _seed_root(root: str, machine: str, sessions: dict, platform: str) -> None:
    """A seeded isolated root: stage sessions, a second platform, browser state."""
    stage = os.path.join(root, "stage", "sessions", machine)
    seed = os.path.join(root, "seed", machine)
    for sid, body in sessions.items():
        for parent in (stage, seed):
            target = os.path.join(parent, f"{platform}.{sid}", "b00")
            os.makedirs(target, exist_ok=True)
            with open(os.path.join(target, "0001.jsonl"), "w", encoding="utf-8") as fh:
                fh.write(body)
    # A local-harness dir in the same platform bucket: never web coverage, and
    # never something a browser re-backfill could restore, so the wipe must
    # leave it exactly where it is.
    harness = os.path.join(stage, f"{platform}.{machine}.localcli", "b00")
    os.makedirs(harness, exist_ok=True)
    with open(os.path.join(harness, "0001.jsonl"), "w", encoding="utf-8") as fh:
        fh.write('{"raw":{"text":"{\"local\":true}"}}\n')
    other = os.path.join(stage, "claude.keepme", "b00")
    os.makedirs(other, exist_ok=True)
    with open(os.path.join(other, "0001.jsonl"), "w", encoding="utf-8") as fh:
        fh.write('{"raw":{"text":"{\\"keep\\":true}"}}\n')
    browser = os.path.join(root, "browser-state")
    os.makedirs(browser, exist_ok=True)
    for name in ("install.json", "seen-cursor.json", "pending.jsonl"):
        with open(os.path.join(browser, name), "w", encoding="utf-8") as fh:
            fh.write(name + "\n")


def run_self_test() -> int:
    failures: list[str] = []

    def check(label: str, ok: bool, detail: str = "") -> None:
        print(("PASS " if ok else "FAIL ") + label + ("" if ok else f": {detail}"))
        if not ok:
            failures.append(label)

    sessions = {
        "aaaa1111": '{"raw":{"text":"{\\"data\\":{\\"biz_data\\":{\\"chat_messages\\":[1,2,3]}}}"}}\n',
        "bbbb2222": '{"raw":{"text":"{\\"data\\":{\\"biz_data\\":{\\"chat_messages\\":[1]}}}"}}\n',
        "cccc3333": '{"raw":{"text":"{\\"data\\":{\\"biz_data\\":{\\"chat_messages\\":[1,2]}}}"}}\n',
    }

    with tempfile.TemporaryDirectory(prefix="extval-selftest-") as tmp:
        base = os.path.join(tmp, "isolated")
        os.makedirs(base)
        real = os.path.join(tmp, "real-data")
        os.makedirs(os.path.join(real, "stage", "sessions", "extval-isolated"))
        with open(os.path.join(real, "stage", "sessions", "extval-isolated", "keep"), "w") as fh:
            fh.write("real\n")
        outside = os.path.join(tmp, "outside")
        os.makedirs(outside)
        collect = os.path.join(tmp, "stub-collect.py")
        oracle = os.path.join(tmp, "stub-oracle.py")
        export = os.path.join(tmp, "export.json")
        for path, body in ((collect, STUB_COLLECT), (oracle, STUB_ORACLE)):
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(body)
        with open(export, "w", encoding="utf-8") as fh:
            fh.write("[]\n")

        machine = "extval-isolated"
        platform = "deepseek"
        common = [
            "--platform", platform,
            "--isolated-base", base,
            "--real-data-root", real,
            "--machine", machine,
            "--oracle", oracle,
            "--export", export,
        ]
        collect_cmd = f"{sys.executable} {collect} {{root}} {machine}"

        def fresh_root(name: str) -> str:
            root = os.path.join(base, name)
            _seed_root(root, machine, sessions, platform)
            return root

        # 1. dry run removes nothing and establishes nothing
        root = fresh_root("dry")
        before_files = snapshot_tree(os.path.join(root, "stage"))
        code, out = _quiet([*common, "--root", root, "--dry-run"])
        after_files = snapshot_tree(os.path.join(root, "stage"))
        check("dry-run exits 3 (a plan establishes nothing)", code == EXIT_NOT_ESTABLISHED,
              f"exit {code}")
        check("dry-run removes nothing", before_files == after_files)
        check("dry-run names the sessions it would wipe", "aaaa1111" in out, out)

        # 2. wipe-all, full cycle: everything returns identically
        root = fresh_root("full")
        code, out = _quiet([*common, "--root", root, "--collect-cmd", collect_cmd, "--json"])
        report = json.loads(out)
        check("wipe-all exits 0", code == EXIT_OK, f"exit {code}")
        check("wipe-all verdict held", report.get("verdict") == "held", str(report.get("verdict")))
        check(
            "wipe-all restores the same digest-root",
            report.get("digest_root_before") == report.get("digest_root_after"),
            f"{report.get('digest_root_before')} -> {report.get('digest_root_after')}",
        )
        check(
            "wipe-all restores every session",
            all(row["verdict"] == "restored" for row in report["sessions"])
            and len(report["sessions"]) == 3,
            str(report["sessions"]),
        )
        check(
            "wipe-all leaves the other platform alone",
            snapshot_tree(os.path.join(root, "stage", "sessions", machine, "claude.keepme"))
            is not None,
        )
        harness_dir = os.path.join(
            root, "stage", "sessions", machine, f"{platform}.{machine}.localcli"
        )
        check(
            "wipe-all reports the local-harness dir it skipped",
            report.get("local_harness_skipped") == 1,
            str(report.get("local_harness_skipped")),
        )
        check(
            "wipe-all leaves the local-harness dir alone",
            snapshot_tree(harness_dir) is not None,
        )
        check(
            "wipe-all wipes the browser state too",
            report.get("browser_state_before", {}).get("files") == 3
            and report.get("browser_state_after", {}).get("files") == 0,
            f"{report.get('browser_state_before')} -> {report.get('browser_state_after')}",
        )

        # 3. random-half wipes a deterministic half, and it still returns
        root = fresh_root("half")
        code, out = _quiet(
            [*common, "--root", root, "--mode", "random-half", "--collect-cmd", collect_cmd,
             "--json"]
        )
        report = json.loads(out)
        wiped = [row for row in report["sessions"] if row["wiped"]]
        check("random-half exits 0", code == EXIT_OK, f"exit {code}")
        check("random-half wipes half (1 of 3)", len(wiped) == 1, str(len(wiped)))
        check(
            "random-half restores the wiped one",
            all(row["verdict"] == "restored" for row in wiped),
            str(wiped),
        )
        check(
            "random-half leaves the kept ones byte-identical",
            all(row["verdict"] == "kept" for row in report["sessions"] if not row["wiped"]),
        )

        # 4. the check can fail: a collect that restores nothing is caught
        root = fresh_root("lost")
        code, out = _quiet([*common, "--root", root, "--collect-cmd", "true", "--json"])
        report = json.loads(out)
        check("a no-op collect exits 1", code == EXIT_FAIL, f"exit {code}")
        check("a no-op collect is regressed", report.get("verdict") == "regressed")
        check(
            "a no-op collect shows recall falling",
            any("recall.found" in move for move in report.get("must_not_move_moved", [])),
            str(report.get("must_not_move_moved")),
        )

        # 5. refusals change nothing
        real_keep = os.path.join(real, "stage", "sessions", "extval-isolated", "keep")
        code, out = _quiet([*common, "--root", real, "--collect-cmd", collect_cmd])
        check("the real data root is refused", code == EXIT_NOT_ESTABLISHED, f"exit {code}")
        check(
            "the refusal names the real data root",
            "real chat-stasher data root" in out,
            out.strip(),
        )
        check("the refused real root is untouched", os.path.exists(real_keep))

        code, out = _quiet([*common, "--root", outside, "--collect-cmd", collect_cmd])
        check("a root outside the base is refused", code == EXIT_NOT_ESTABLISHED, f"exit {code}")
        check("the outside refusal names the base", "isolated base" in out, out.strip())

        code, out = _quiet(
            [*common, "--root", fresh_root("escape"), "--stage-subdir", os.path.join("..", "esc"),
             "--collect-cmd", collect_cmd]
        )
        check("an escaping --stage-subdir is refused", code == EXIT_NOT_ESTABLISHED, f"exit {code}")
        check("the escape refusal names the root", "outside the isolated root" in out, out.strip())

        # 6. a self-created root is deleted; --keep keeps it
        code, _out = _quiet([*common, "--collect-cmd", collect_cmd])
        check("a self-created root runs", code == EXIT_OK, f"exit {code}")
        check(
            "a self-created root is deleted",
            not [n for n in os.listdir(base) if n.startswith("extval-deepseek-")],
            str(sorted(os.listdir(base))),
        )
        code, _out = _quiet([*common, "--collect-cmd", collect_cmd, "--keep"])
        kept = [n for n in os.listdir(base) if n.startswith("extval-deepseek-")]
        check("--keep keeps the self-created root", code == EXIT_OK and len(kept) == 1, str(kept))
        for name in kept:
            shutil.rmtree(os.path.join(base, name), ignore_errors=True)

    print("ALL PASS" if not failures else f"{len(failures)} FAILED")
    return EXIT_OK if not failures else EXIT_FAIL


if __name__ == "__main__":
    sys.exit(main())
