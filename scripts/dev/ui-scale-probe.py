#!/usr/bin/env python3
"""ui-scale-probe.py — measure `chat-stasher ui` page size and render time
against a synthetic archive at 1k / 5k / 10k sessions (UIA-9).

The design's performance budget (`29-UI-DESIGN.md` §7) is a set of hard page
ceilings: `/sessions` <= 400 KB, `/` <= 300 KB, a reader window <= 250 KB,
`/content` 1 MiB per window. None of those can be checked against a real archive
on a developer machine, and the budget's own note asks for the one thing that
makes a ceiling real rather than decorative: **a measurement at the size the
ceiling is supposed to hold at**, plus a sample or two whose answer is already
known. This is that measurement.

It runs the product end to end and mocks nothing:

    ui-scale-fixture.py   ->  synthetic stage trees (no real content)
    chat-stasher activity-index   ->  the sidecar the pages read times from
    chat-stasher push             ->  a real repository per size
    chat-stasher ui               ->  the real server, on a real loopback socket
    GET <route>                   ->  the size and latency recorded here

Per route it records the response body's byte count, its sha256, the HTTP status
and the cost of serving it (see "Measuring render time" below). The body itself is
deliberately **not** written to disk: at 10k sessions the pages under measurement
are several MB each, the artefact would dwarf the measurement, and the digest is
what makes a re-run comparable. Sizes and latencies are the measurement; the
digests say whether two runs measured the same bytes.

Measuring render time, and why the naive way measures something else
---------------------------------------------------------------
A single GET against this server does **not** measure how long the page takes to
render. `view::serve` accepts on a non-blocking listener and, when nothing is
pending, sleeps 50 ms before trying again (`view.rs:468-472`). A request that
arrives just after the server has entered that sleep waits out the rest of the
tick, so one-shot latency is `render + U(0, 50) ms` — the accept poll, not the
page. Measured on this machine at 120 sessions: a lone GET of `/` took 44.9 ms,
and its four pipelined siblings took 0.80 / 0.61 / 0.62 / 0.67 ms. Reporting the
first number as "render time" would overstate the page by ~50x and would move
with the poll, not with the page.

So each measurement here is a **burst**: N connections are opened first, their
requests sent, and only then are the responses read in order. The server drains
the backlog back to back, paying the poll once. The per-request cost is the
delta between consecutive response completions, which is the server's own work
for that page; the first delta is reported separately as `ms_cold` because that
one *is* what a reader clicking a link sees (poll included, ~25 ms average).

The server holds the whole metadata read in memory before it binds the socket
(`ui.rs` module docs), so a delta is HTML generation plus the loopback write, not
archive I/O. The one exception is `/content`, which reopens the repository and
decrypts the session on every request — its delta is dominated by that fetch, and
it is the route the design says is reachable only by an explicit click.

Startup is measured separately, from process launch to the URL it prints: that is
the cost of the archive read, which the design budgets as "startup to first
clickable".

Privacy line: this prints route names, byte counts, sha256 digests, latencies and
counts. It never prints a session id, a page body, or any conversation text — the
archive it measures is synthetic, and the reporting discipline is the same one
the repository's other gates keep.

Usage:
    python3 scripts/dev/ui-scale-probe.py --work DIR --out table.tsv
    python3 scripts/dev/ui-scale-probe.py --sizes 1000,5000,10000 --profiles timed,all

Exit codes: 0 = every size measured · 1 = a measurement could not be taken ·
2 = usage error.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def _load_fixture_module():
    """Import `ui-scale-fixture.py` from beside this file.

    Loaded by path rather than by name because every script in `scripts/dev/`
    is named with dashes, which no `import` statement can spell.

    Bytecode writing is off for the load: `.gitignore` covers
    `scripts/__pycache__/` but not `scripts/dev/__pycache__/`, so importing the
    sibling would otherwise leave an untracked directory behind every run. A
    probe that dirties the tree it measures is a worse neighbour than one that
    re-compiles 350 lines of Python each time.
    """
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ui-scale-fixture.py")
    spec = importlib.util.spec_from_file_location("ui_scale_fixture", path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"ui-scale-probe: cannot load the fixture generator at {path}")
    module = importlib.util.module_from_spec(spec)
    previous = sys.dont_write_bytecode
    sys.dont_write_bytecode = True
    try:
        spec.loader.exec_module(module)
    finally:
        sys.dont_write_bytecode = previous
    return module


fixture = _load_fixture_module()

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEFAULT_BIN = os.path.join(REPO_ROOT, "target", "debug", "chat-stasher")

# The design's page ceiling for "flag this" purposes. A page above it is
# reported as over budget on every run that sees it, whatever the reason.
FLAG_BYTES = 4 * 1024 * 1024

# Routes measured, in report order. `reader` names the two routes the design
# splits the reading surface into: the session page (metadata + the load link,
# the reader's entry) and the content route (the body, fetched only on a click).
# There is no `/reader` route in this revision of `ui.rs` — the probe measures
# what exists and says so in its output, rather than reporting a page that is
# still a design.
ROUTES = (
    ("overview", "/", "html"),
    ("session_list", "/sessions", "html"),
    ("reader_session", "/session?i={mid}", "html"),
    ("reader_content_typical", "/content?i={mid}", "html"),
    ("reader_content_largest", "/content?i={biggest}", "html"),
    ("api_overview", "/api/overview", "json"),
    ("api_sessions", "/api/sessions", "json"),
)


class ProbeError(RuntimeError):
    pass


# ------------------------------------------------------------------- helpers


def _isolated_env(work: str) -> dict:
    """Env for every product command this probe runs.

    `HOME` and the three XDG roots point inside the probe's own work directory so
    no command can read or write the operator's real config, registry or stage.
    The registry is an empty one written here: without it, a `push` stage check
    counts the sessions on the machine it happens to be running on, which would
    make the fixture depend on whose laptop it ran on.
    """
    root = os.path.join(work, "env")
    for sub in ("home", "config", "data", "state"):
        os.makedirs(os.path.join(root, sub), exist_ok=True)
    registry = os.path.join(root, "registry.json")
    if not os.path.exists(registry):
        with open(registry, "w", encoding="utf-8") as fh:
            json.dump(
                {"schema_version": 1, "generated": "ui-scale-probe", "harnesses": []}, fh
            )
    env = dict(os.environ)
    env.update(
        {
            "HOME": os.path.join(root, "home"),
            "XDG_CONFIG_HOME": os.path.join(root, "config"),
            "XDG_DATA_HOME": os.path.join(root, "data"),
            "XDG_STATE_HOME": os.path.join(root, "state"),
            "CHAT_STASHER_REGISTRY": registry,
        }
    )
    return env


def run(bin_path: str, work: str, args: list[str], log: str | None = None) -> str:
    """Run one product command, raising with its own output when it fails."""
    env = _isolated_env(work)
    proc = subprocess.run(
        [bin_path] + args,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    out = proc.stdout or ""
    if log:
        with open(log, "a", encoding="utf-8") as fh:
            fh.write(f"$ chat-stasher {' '.join(args)}\n{out}\n")
    if proc.returncode != 0:
        raise ProbeError(
            f"`chat-stasher {' '.join(args)}` exited {proc.returncode}:\n{out[-4000:]}"
        )
    return out


def build_repo(bin_path: str, work: str, stages_root: str, repo: str, key: str,
               machines: list[str], log: str) -> None:
    """activity-index then push, one stage per machine (a stage is one machine's
    outbox and `push` refuses one holding two partitions)."""
    for machine in machines:
        stage = os.path.join(stages_root, f"stage-{machine}")
        run(bin_path, work, ["activity-index", "--stage", stage, "--machine", machine], log)
        run(
            bin_path,
            work,
            [
                "push",
                "--stage", stage,
                "--repo", repo,
                "--key-file", key,
                "--machine", machine,
                "--keep-ssh-masters",
            ],
            log,
        )


class UiServer:
    """A running `chat-stasher ui`, plus the wall time it took to become usable.

    stdout goes to a file rather than a pipe: the serve loop outlives the URL
    line, and a reader that stops consuming a pipe can block the writer.
    """

    def __init__(self, bin_path: str, work: str, repo: str, key: str, idle: int, log: str):
        env = _isolated_env(work)
        self.log_path = log
        self.log = open(log, "a", encoding="utf-8")
        started = time.perf_counter()
        self.proc = subprocess.Popen(
            [
                bin_path,
                "ui",
                "--repo", repo,
                "--key-file", key,
                "--no-open",
                "--idle-timeout", str(idle),
                "--keep-ssh-masters",
            ],
            env=env,
            stdout=self.log,
            stderr=subprocess.STDOUT,
            text=True,
        )
        url = None
        deadline = started + 600.0
        while time.perf_counter() < deadline:
            if self.proc.poll() is not None:
                raise ProbeError(
                    "`chat-stasher ui` exited before printing a URL:\n"
                    + self._tail()
                )
            url = self._url_from_log()
            if url:
                break
            time.sleep(0.02)
        if not url:
            self.stop()
            raise ProbeError("`chat-stasher ui` never printed a URL:\n" + self._tail())
        self.startup_seconds = time.perf_counter() - started
        addr, _, query = url[len("http://"):].partition("/")
        self.host, _, port = addr.rpartition(":")
        self.port = int(port)
        self.token = ""
        for kv in query.lstrip("?").split("&"):
            if kv.startswith("token="):
                self.token = kv[len("token="):]
        if not self.token:
            self.stop()
            raise ProbeError(f"no token in the URL `chat-stasher ui` printed: {url!r}")

    def _tail(self, n: int = 60) -> str:
        try:
            with open(self.log_path, "r", encoding="utf-8", errors="replace") as fh:
                return "".join(fh.readlines()[-n:])
        except OSError as exc:
            return f"(log unreadable: {exc})"

    def _url_from_log(self) -> str | None:
        try:
            with open(self.log_path, "r", encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    line = line.strip()
                    if line.startswith("http://"):
                        return line
        except OSError:
            return None
        return None

    def _path(self, target: str) -> str:
        sep = "&" if "?" in target else "?"
        return f"{target}{sep}token={self.token}"

    def get(self, target: str) -> dict:
        """One request on its own, for warm-up. See `burst` for the measurement."""
        return self.burst(target, 1)["samples"][0]

    def burst(self, target: str, n: int) -> dict:
        """N pipelined requests; per-request cost is the delta between
        consecutive completions.

        All N sockets are connected and their requests sent *before* any
        response is read, so the server accepts them back to back out of the
        listen backlog and pays its 50 ms idle poll at most once. Reading the
        responses in order is safe: the server serves one connection at a time,
        and the kernel's accept queue is FIFO.
        """
        path = self._path(target)
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {self.host}:{self.port}\r\n"
            f"Connection: close\r\n\r\n"
        ).encode()

        sockets = []
        for _ in range(n):
            sock = socket.create_connection((self.host, self.port), timeout=300)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            sockets.append(sock)
        for sock in sockets:
            sock.sendall(request)

        started = time.perf_counter()
        samples = []
        previous = started
        for sock in sockets:
            raw = bytearray()
            while True:
                chunk = sock.recv(1 << 16)
                if not chunk:
                    break
                raw.extend(chunk)
            completed = time.perf_counter()
            sock.close()
            samples.append(self._parse(bytes(raw), (completed - previous) * 1000.0))
            previous = completed
        return {"target": path, "samples": samples}

    @staticmethod
    def _parse(raw: bytes, ms: float) -> dict:
        head, _, body = raw.partition(b"\r\n\r\n")
        lines = head.split(b"\r\n")
        status = 0
        if len(lines[0].split(b" ")) > 1:
            try:
                status = int(lines[0].split(b" ")[1])
            except ValueError:
                status = 0
        declared = None
        content_type = ""
        for line in lines[1:]:
            key, _, value = line.partition(b":")
            key = key.strip().lower()
            if key == b"content-length":
                try:
                    declared = int(value.strip())
                except ValueError:
                    declared = None
            elif key == b"content-type":
                content_type = value.strip().decode("latin-1")
        return {
            "status": status,
            "bytes": len(body),
            "sha256": hashlib.sha256(body).hexdigest()[:16],
            "ms": ms,
            "content_type": content_type,
            # The w15 socket test's rule, kept here: a body shorter than its own
            # Content-Length is a truncation, and a truncation must never read as
            # a small page.
            "content_length_matches": declared is None or declared == len(body),
        }

    def get_json(self, target: str) -> dict:
        sep = "&" if "?" in target else "?"
        url = f"http://{self.host}:{self.port}{target}{sep}token={self.token}"
        with urllib.request.urlopen(url, timeout=300) as resp:
            return json.loads(resp.read().decode("utf-8", "replace"))

    def stop(self) -> None:
        """Terminate the process this probe started — and only that one."""
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=20)
        try:
            self.log.close()
        except OSError:
            pass


# ------------------------------------------------------------------ one size


def median(values: list[float]) -> float:
    ordered = sorted(values)
    n = len(ordered)
    if n % 2:
        return ordered[n // 2]
    return (ordered[n // 2 - 1] + ordered[n // 2]) / 2.0


def measure_size(
    bin_path: str,
    work: str,
    profile: str,
    sessions: int,
    machines_count: int,
    repeats: int,
    big_bytes: int,
) -> tuple[list[dict], dict]:
    """Build one archive and measure every route on it. Returns (rows, facts)."""
    key = f"{profile}-{sessions}"
    root = os.path.join(work, key)
    os.makedirs(root, exist_ok=True)
    log = os.path.join(root, "commands.log")
    if os.path.exists(log):
        os.remove(log)

    stages_root = os.path.join(root, "stages")
    summary = fixture.build(
        out=stages_root,
        sessions=sessions,
        machines=machines_count,
        profile=profile,
        seed=166,
        big_sessions=2,
        big_bytes=big_bytes,
        clean=True,
    )
    with open(os.path.join(root, "fixture.json"), "w", encoding="utf-8") as fh:
        json.dump(summary, fh, indent=1, sort_keys=True)

    repo = os.path.join(root, "repo")
    masterkey = os.path.join(root, "masterkey.json")
    build_repo(bin_path, work, stages_root, repo, masterkey, summary["machines"], log)

    server = UiServer(
        bin_path,
        work,
        repo,
        masterkey,
        idle=300,
        log=os.path.join(root, "ui.log"),
    )
    rows: list[dict] = []
    try:
        # The two indices the routes need. `/api/sessions` is the archive's own
        # answer to "which row is which", so the probe asks the server rather
        # than assuming an order — and reads the largest row off the measured
        # bytes instead of a hard-coded position.
        index = server.get_json("/api/sessions")
        listed = index.get("sessions") or []
        if not listed:
            raise ProbeError("the dashboard listed no sessions for a non-empty archive")
        mid = listed[len(listed) // 2]["index"]
        biggest = max(listed, key=lambda s: s["bytes"])["index"]

        for name, target, kind in ROUTES:
            path = target.format(mid=mid, biggest=biggest)
            # One warm-up request, not recorded: the first GET of a route pays
            # for the server's own first-touch allocations, and folding that into
            # the reported latency would report a one-off as the steady state.
            server.get(path)

            burst = server.burst(path, repeats)
            samples = burst["samples"]
            cold = samples[0]
            rest = samples[1:] or samples
            statuses = sorted({s["status"] for s in samples})
            render = [s["ms"] for s in rest]
            rows.append(
                {
                    "profile": profile,
                    "sessions": sessions,
                    "route": name,
                    "kind": kind,
                    "target": burst["target"],
                    "status": statuses[0] if len(statuses) == 1 else statuses,
                    "bytes": cold["bytes"],
                    "bytes_stable": len({s["bytes"] for s in samples}) == 1,
                    "sha_stable": len({s["sha256"] for s in samples}) == 1,
                    "length_ok": all(s["content_length_matches"] for s in samples),
                    # The burst's own render cost: the median and worst delta
                    # between consecutive completions, with the accept poll
                    # excluded (it is paid once, by `cold`).
                    "ms_render_median": median(render),
                    "ms_render_max": max(render),
                    # What a reader clicking a link actually waits for: the first
                    # response out of an idle server, poll included.
                    "ms_cold": cold["ms"],
                    "repeats": repeats,
                    "samples": samples,
                }
            )

        overview = server.get_json("/api/overview")
        facts = {
            "profile": profile,
            "sessions_requested": sessions,
            "startup_ms": server.startup_seconds * 1000.0,
            "sessions_in_view": (overview.get("summary") or {}).get("sessions_in_view"),
            "time_unknown": (overview.get("summary") or {}).get("time_unknown"),
            "machines": (overview.get("summary") or {}).get("machines"),
            "sources": (overview.get("summary") or {}).get("sources"),
            "complete": overview.get("complete"),
            "index_files_read": overview.get("index_files_read"),
            "data_blobs_read": overview.get("data_blobs_read"),
            "generator_expected_known_time": summary["expected_known_time"],
            "generator_expected_unknown_time": summary["expected_unknown_time"],
            "fixture_total_bytes": summary["total_bytes"],
            "mid_index": mid,
            "largest_index": biggest,
            "largest_session_bytes": max(s["bytes"] for s in listed),
        }
    finally:
        server.stop()
    return rows, facts


# ---------------------------------------------------------------------- main


def fmt_bytes(n: int) -> str:
    if n >= 1024 * 1024:
        return f"{n / (1024 * 1024):.2f} MiB"
    if n >= 1024:
        return f"{n / 1024:.1f} KiB"
    return f"{n} B"


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description="Measure `chat-stasher ui` page size at scale.")
    ap.add_argument("--bin", default=DEFAULT_BIN, help="path to the chat-stasher binary")
    ap.add_argument(
        "--work",
        default=None,
        help="work directory for fixtures, repos and logs (default: a temp dir, removed)",
    )
    ap.add_argument("--sizes", default="1000,5000,10000")
    ap.add_argument("--profiles", default="timed,all")
    ap.add_argument("--machines", type=int, default=3)
    ap.add_argument(
        "--repeats",
        type=int,
        default=9,
        help="requests per burst; the first pays the accept poll, the rest are "
        "the render samples",
    )
    ap.add_argument("--big-bytes", type=int, default=5 * 1024 * 1024)
    ap.add_argument("--out", default=None, help="write the TSV table here as well as stdout")
    ap.add_argument("--json", dest="json_out", default=None, help="write raw samples here")
    ap.add_argument("--keep", action="store_true", help="keep a temp --work directory")
    args = ap.parse_args(argv)

    if not os.path.isfile(args.bin):
        print(
            f"ui-scale-probe: no binary at {args.bin}\n"
            "ui-scale-probe: run `cargo build` first, or pass --bin",
            file=sys.stderr,
        )
        return 2
    try:
        sizes = [int(s) for s in args.sizes.split(",") if s.strip()]
        profiles = [p.strip() for p in args.profiles.split(",") if p.strip()]
    except ValueError:
        print("ui-scale-probe: --sizes takes comma-separated integers", file=sys.stderr)
        return 2
    for p in profiles:
        if p not in ("timed", "all"):
            print(f"ui-scale-probe: unknown profile {p!r}", file=sys.stderr)
            return 2

    tmp = None
    work = args.work
    if not work:
        tmp = tempfile.mkdtemp(prefix="ui-scale-probe-")
        work = tmp
    os.makedirs(work, exist_ok=True)

    rows: list[dict] = []
    facts: list[dict] = []
    try:
        for profile in profiles:
            for size in sizes:
                print(f"# measuring profile={profile} sessions={size}", file=sys.stderr)
                try:
                    r, f = measure_size(
                        args.bin, work, profile, size, args.machines, args.repeats,
                        args.big_bytes,
                    )
                except ProbeError as exc:
                    print(f"ui-scale-probe: {exc}", file=sys.stderr)
                    return 1
                rows.extend(r)
                facts.append(f)
                fpath = os.path.join(work, f"{profile}-{size}", "facts.json")
                with open(fpath, "w", encoding="utf-8") as fh:
                    json.dump(f, fh, indent=1, sort_keys=True)
    finally:
        if tmp and not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)

    # ------------------------------------------------------------------ table
    header = [
        "profile", "sessions", "route", "status", "bytes", "human",
        "ms_render_p50", "ms_render_max", "ms_cold", "n", "stable", "over_4MiB",
    ]
    lines = ["\t".join(header)]
    over = []
    for row in rows:
        flag = row["bytes"] > FLAG_BYTES
        if flag:
            over.append(row)
        lines.append(
            "\t".join(
                [
                    row["profile"],
                    str(row["sessions"]),
                    row["route"],
                    str(row["status"]),
                    str(row["bytes"]),
                    fmt_bytes(row["bytes"]),
                    f"{row['ms_render_median']:.2f}",
                    f"{row['ms_render_max']:.2f}",
                    f"{row['ms_cold']:.1f}",
                    str(row["repeats"]),
                    "yes"
                    if (row["bytes_stable"] and row["sha_stable"] and row["length_ok"])
                    else "NO",
                    "OVER" if flag else "",
                ]
            )
        )
    table = "\n".join(lines) + "\n"
    sys.stdout.write(table)

    print("\n# archive facts (counts and sizes only)", file=sys.stderr)
    fhead = [
        "profile", "sessions", "in_view", "time_unknown", "known_expected",
        "startup_ms", "machines", "sources", "complete", "index_files", "fixture_bytes",
    ]
    flines = ["\t".join(fhead)]
    for f in facts:
        flines.append(
            "\t".join(
                [
                    f["profile"],
                    str(f["sessions_requested"]),
                    str(f["sessions_in_view"]),
                    str(f["time_unknown"]),
                    str(f["generator_expected_known_time"]),
                    f"{f['startup_ms']:.0f}",
                    str(f["machines"]),
                    str(f["sources"]),
                    str(f["complete"]),
                    str(f["index_files_read"]),
                    str(f["fixture_total_bytes"]),
                ]
            )
        )
    ftable = "\n".join(flines) + "\n"
    sys.stderr.write(ftable)

    if over:
        print(f"\n# PAGES OVER {fmt_bytes(FLAG_BYTES)} ({FLAG_BYTES} B):", file=sys.stderr)
        for row in over:
            print(
                f"#   {row['profile']}/{row['sessions']} {row['route']} "
                f"{row['bytes']} B ({fmt_bytes(row['bytes'])})",
                file=sys.stderr,
            )
    else:
        print(f"\n# no page over {fmt_bytes(FLAG_BYTES)}", file=sys.stderr)

    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(table)
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as fh:
            json.dump({"rows": rows, "facts": facts, "work": work}, fh, indent=1, sort_keys=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
