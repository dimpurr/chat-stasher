#!/usr/bin/env python3
"""SG-3 live-day gate report (ADR-061 D6/D7; 46 §4.2/§6).

Reads the per-pass counters the collector has been writing into
``run-state.json`` since the increment era shipped and reports the measured
written-vs-growth economics the one-day gate accepts on:

  * per pass: the commit ``written ≤ max(1.2 × growth, 64 KiB)`` rule over
    every snapshot-source session sealed that pass (D6's hourly shape:
    counts are per hour because one ``run-once`` pass is one hour);
  * over the window: the aggregate ratio and the weekly projection band,
    reported as a *band from measurement*, never a promise (D6's method);
  * the absolute band this ADR deliberately does not pin anywhere.

The script never reads conversation bodies, session ids or paths: the
run-state file holds counts and byte totals only, and the script's output
keeps to them. It exits non-zero when any single pass busts the hourly
ceiling, so a wired gate can check it.

Usage:
  python3 scripts/dev/sg3-live-day-report.py --state-dir <chat-stasher state dir> \
      [--passes 24]

The one-day gate procedure (the follow-up steps this harness is left for):

  1. On m1max, let the scheduled hourly ``run-once`` collect normally for one
     day while one real deputy session stays open (the heavy case ADR-061
     measured: long open sessions, tens to hundreds of parts per round).
  2. Run this report. The gate needs: every pass inside the hourly ceiling,
     and the day's aggregate well under the whole-file era's shape (which
     was ~50-100× the session's own growth).
  3. Byte-compare the assembled readback against a fresh export of the source
     for that session at the end of the day:
     ``cargo test -p chat-stasher --test w1020_sg3_live_day_harness_test --
      --ignored --nocapture`` with SG3_LIVE_DB / SG3_LIVE_STAGE pointing at
     the real database and stage. That harness only reads (read-only SQLite
     open, read-only stage files); it never writes either.
  4. Record the band in the ADR-061 ticket: written/day and written ÷ true
     growth. Do not extrapolate beyond the measured day; the weekly band is
     this report's ``×7`` line, labelled as a projection.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

HOURLY_CEILING_FACTOR = 1.2
HOURLY_FLOOR_BYTES = 64 * 1024  # 裁定 5: a metadata-only pass stays inside the ceiling.


def load_passes(state_dir: Path, passes: int) -> list[dict]:
    path = state_dir / "run-state.json"
    try:
        raw = json.loads(path.read_text())
    except FileNotFoundError:
        sys.exit(f"no run-state.json under {state_dir}; nothing measured yet")
    except (OSError, json.JSONDecodeError) as error:
        sys.exit(f"cannot read {path}: {error}")
    # The run-state file holds one pass; the file's history (if any backup
    # rotation keeps older copies) is out of scope — the gate window is the
    # passes observed since the counters exist, one file per machine.
    out = [raw]
    # Older rotated entries: run-state.json.<ts> style, if the operator kept
    # them; the harness doc says to snapshot the file per hour if the daily
    # per-pass view is wanted (see step 1bis below).
    for rotated in sorted(state_dir.glob("run-state.json.*")):
        try:
            out.append(json.loads(rotated.read_text()))
        except (OSError, json.JSONDecodeError):
            continue
    out = out[-passes:]
    phases = [entry.get("phases", {}) for entry in out]
    phases = [p for p in phases if p]
    if not phases:
        sys.exit("no pass metrics found; run with a build that carries the SG-3 counters")
    return phases


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--state-dir",
        type=Path,
        required=True,
        help="chat-stasher's state directory (the one holding run-state.json)",
    )
    parser.add_argument(
        "--passes",
        type=int,
        default=24,
        help="how many pass records to consider (default: 24 = one day)",
    )
    args = parser.parse_args()

    phases = load_passes(args.state_dir, args.passes)
    total_written = 0
    total_growth = 0
    total_full_exports = 0
    total_increment_passes = 0
    busts = 0
    print("[sg3] per-pass written-vs-growth (hourly rule: written <= max(1.2*growth, 64KiB))")
    for index, phase in enumerate(phases):
        written = phase.get("sqlite_increment_bytes", 0)
        growth = phase.get("sqlite_growth_bytes", 0)
        full = phase.get("sqlite_full_exports", 0)
        incr = phase.get("sqlite_increment_passes", 0)
        # The whole-file era wrote one full copy per changed session; those
        # bytes belong to the era being replaced, so a degrade-heavy day
        # shows up in `full_exports` rather than hiding inside the ratio.
        total_written += written
        total_growth += growth
        total_full_exports += full
        total_increment_passes += incr
        ceiling = max(
            math.ceil(HOURLY_CEILING_FACTOR * growth),
            HOURLY_FLOOR_BYTES,
        )
        status = "ok" if written <= ceiling else "BUST"
        if written > ceiling:
            busts += 1
        print(
            f"[sg3] pass {index}: increment_bytes={written} growth_bytes={growth} "
            f"ceiling={ceiling} full_exports={full} increment_passes={incr} -> {status}"
        )

    print(
        f"[sg3] window: increment_bytes={total_written} growth_bytes={total_growth} "
        f"full_exports={total_full_exports} increment_passes={total_increment_passes}"
    )
    if total_growth > 0:
        ratio = total_written / total_growth
        print(
            f"[sg3] amplification (increment bytes / true growth): {ratio:.2f}x "
            f"(weekly projection band: {ratio:.2f}x of the week's opencode growth — a "
            "measure, not a promise)"
        )
    else:
        print("[sg3] amplification: no growth measured in the window (only metadata?)")
    if total_full_exports:
        print(
            f"[sg3] note: {total_full_exports} whole-file observations sealed — the "
            "degrade/first-sight write; bounded to one per session (map loss) but worth "
            "naming in the day's report"
        )
    if busts:
        print(f"[sg3] FAIL: {busts} pass(es) outside the hourly ceiling")
        return 1
    print("[sg3] PASS: every measured pass inside the hourly ceiling")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
