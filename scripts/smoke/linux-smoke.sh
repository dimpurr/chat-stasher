#!/usr/bin/env bash
#
# linux-smoke.sh — does the installed CLI work end to end?
#
# One question, one exit code: given a machine that has never run chat-stasher
# before, and a set of *synthetic* agent histories planted for the harnesses
# this build parses, does the whole chain work — detect, init, collect, push,
# read, overview, and a scheduler unit that points at a binary which survives
# `cargo clean`?
#
# Why a script and not "I ran it once": the same reason scripts/smoke-fresh-clone.sh
# exists. A manual walk-through is not recomputable; this prints a verdict at
# every step, so anyone can re-run it and get the same answer.
#
# What it is NOT: it talks to no chat platform, logs in nowhere, opens no socket,
# and writes nothing outside a temp directory. Every fixture is synthetic
# one-liners — no real conversation content, no real account, no path from the
# developer's machine. The throwaway HOME is the whole world it sees.
#
# Usage:
#   bash scripts/smoke/linux-smoke.sh [--binary PATH] [--keep] [--platform NAME]
#
#   --binary PATH    use this binary instead of target/debug/chat-stasher
#   --keep           keep the throwaway HOME and print its path
#   --platform NAME  run the *seeding and assertion* logic as if this were
#                    NAME (`macos` / `linux` / `windows`), by planting that
#                    platform's registry cells in the slot this build reads.
#                    A development aid: it is how the Linux path of this
#                    script is proven on the machine it was written on, using
#                    the same instrument the Rust tests use for a foreign
#                    platform (`registry_default_path_shape_test`). CI never
#                    passes it — there the platform is the real one.
#
# Runs on Linux (that is the point) and on macOS (so it can be developed and
# proven where the owner works). Nothing here is Linux-only: the scheduler
# artefact is *rendered*, never installed, so the systemd unit is exercised on
# both platforms.
#
# ---------------------------------------------------------------------------
# Which harnesses this smoke seeds, and why the rest are not seeded.
#
# The seeded set is derived at run time from the shipped registry
# (crates/chat-stasher/data/harness-registry-v1.json) for the current platform,
# so this script cannot drift from the data file it is about. On Linux the
# shipped cells leave five harnesses seedable:
#
#   claude-code · codex · gemini-cli · opencode · cursor
#
# `codex` needs one configured line on Linux/Windows and this script writes it
# (see the CODEX note at the init step). The other seven are *not* seeded, each
# for a reason this script names out loud, rather than by planting a shape the
# tool would then mis-read:
#
#   grok, kimi-code    the Linux cell's confidence is `unascertained`, so the
#                      tool refuses to scan that path at all — the registry
#                      declines to guess, and `doctor` must report `unknown`,
#                      never `0`. The macOS cells are confirmed/measured, so on
#                      macOS these two *are* seeded.
#   aider, crush       the template is `$CWD/...` — there is no fixed location
#                      under HOME to plant anything at.
#   zed                the Linux cell declares `sqlite` but no `sql_table`, and
#                      no cell on any platform declares Zed's schema, so the
#                      probe falls back to opencode's `session` table, which is
#                      not Zed's layout. Planting one would invent a shape.
#   continue           the cell's format is `json` with no `session_pattern`, and
#                      the name-only fail-safe rejects a patternless JSON cell
#                      outright (scanner.rs, build_record) — the shipped cell
#                      enumerates nothing by design.
#   github-copilot-cli the documented layout is
#                      `session-state/<session-id>/events.jsonl`, so every
#                      session's *filename* is the constant `events` and the id
#                      comes from that filename: every session collapses into one
#                      archive slot. Two such sessions were planted on
#                      2026-09-25 and both were archived under one id, the second
#                      shard appended to the first. Deliberately not seeded while
#                      that holds — the fixture would encode a shape the tool
#                      mishandles.
#
# Each of those is asserted below as a *state*, not left implicit: a harness the
# registry declines to scan has to come back `unknown`, and the script fails if it
# ever comes back as a claimed `0`.
#
# The assertions read `doctor --json`, not the human table. The JSON is the
# documented interface, and its tri-state tags are precisely what is being
# checked (`{"kind":"known","count":N}` vs `{"kind":"unknown","why":...}`);
# grepping the pretty table would re-implement that distinction with string
# matching and lose it in the process.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REGISTRY="$ROOT/crates/chat-stasher/data/harness-registry-v1.json"

BIN=""
KEEP=0
PLATFORM_SIM=""
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BIN="${2:-}"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --platform) PLATFORM_SIM="${2:-}"; shift 2 ;;
    -h|--help) awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "[smoke] unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$PLATFORM_SIM" in
  ""|macos|linux|windows) ;;
  *) echo "[smoke] --platform must be macos, linux or windows" >&2; exit 2 ;;
esac

step() { printf '\n[smoke] %s\n' "$1"; }
say()  { printf '[smoke]   %s\n' "$1"; }
# check <label> <expected-rc> <actual-rc>
check() {
  if [ "$2" = "$3" ]; then printf '[smoke]   PASS · %s (rc=%s)\n' "$1" "$3"
  else printf '[smoke]   FAIL · %s (rc=%s, want %s)\n' "$1" "$3" "$2"; FAILED=1; fi
}
# assert <label> <rc> — for tests that are "did this match", not a process exit code
assert() {
  if [ "$2" = 0 ]; then printf '[smoke]   PASS · %s\n' "$1"
  else printf '[smoke]   FAIL · %s\n' "$1"; FAILED=1; fi
}
fail_now() { printf '[smoke]   FAIL · %s\n' "$1"; echo "[smoke] SMOKE: FAIL"; exit 1; }

FAILED=0

case "$(uname -s)" in
  Darwin) REAL_PLATFORM="macos" ;;
  Linux)  REAL_PLATFORM="linux" ;;
  *)      REAL_PLATFORM="windows" ;;
esac
PLATFORM="${PLATFORM_SIM:-$REAL_PLATFORM}"

# A fixed machine partition, so the name this smoke asserts on later is a
# constant rather than something it has to discover first. Choosing the
# partition explicitly is a documented flag, not a workaround; the
# identity-generation path for an unconfigured machine is covered by the
# identity tests, not here.
SMOKE_MACHINE="smoke-$PLATFORM"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/chat-stasher-linux-smoke.XXXXXX")"
cleanup() {
  if [ "$KEEP" = 1 ]; then echo "[smoke] kept: $WORK"
  else rm -rf "$WORK"; fi
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------------- the world
# Everything the CLI could read or write is redirected into $WORK. HOME alone is
# not enough on Linux: the XDG base directories are separate variables, and a
# runner that exports them would send the config and the repository to the real
# user's home instead.
HOME_DIR="$WORK/home"
STAGE="$WORK/stage"
mkdir -p "$HOME_DIR"
export HOME="$HOME_DIR"
export XDG_DATA_HOME="$HOME_DIR/.local/share"
export XDG_CONFIG_HOME="$HOME_DIR/.config"
export XDG_STATE_HOME="$HOME_DIR/.local/state"
export XDG_CACHE_HOME="$HOME_DIR/.cache"
# ... and every per-harness override this build knows about is cleared, so the
# registry templates (plus the one configured line below) are the only thing
# deciding where anything is. A leftover OPENCODE_DB or KIMI_CODE_HOME on the
# runner would otherwise silently move a store out of the throwaway HOME.
unset CODEX_HOME GEMINI_CLI_HOME OPENCODE_DB KIMI_CODE_HOME CURSOR_USER_DIR || true
# A scratch registry from another test must not leak in either.
unset CHAT_STASHER_REGISTRY || true

# --platform: put the named platform's cells where this build reads them, so the
# foreign platform's templates travel the identical code path here. The `sql_*`
# declarations are kept in a slot this run does not read, because that is where
# they are in a real install of that platform (Linux borrows Cursor's and Grok's
# schema from the macOS cell — scanner.rs, `schema_cell`); dropping them would
# make the simulation fail for a reason the real platform does not have.
if [ -n "$PLATFORM_SIM" ] && [ "$PLATFORM_SIM" != "$REAL_PLATFORM" ]; then
  SIM_REGISTRY="$WORK/harness-registry-simulated-$PLATFORM_SIM.json"
  python3 - "$REGISTRY" "$SIM_REGISTRY" "$REAL_PLATFORM" "$PLATFORM_SIM" <<'PY'
import json, sys
src, dst, real, sim = sys.argv[1:5]
registry = json.load(open(src))
for harness in registry["harnesses"]:
    cells = harness["paths"]
    if sim not in cells:
        continue
    borrow = {k: v for k, v in (cells.get(real) or {}).items() if k.startswith("sql_")}
    if borrow:
        spare = "windows" if real != "windows" else "linux"
        cells[spare] = {**(cells.get(spare) or {}), **borrow}
    cells[real] = cells[sim]
json.dump(registry, open(dst, "w"), indent=2)
PY
  export CHAT_STASHER_REGISTRY="$SIM_REGISTRY"
  say "** SIMULATED PLATFORM: $PLATFORM (running on $REAL_PLATFORM; CI never does this) **"
  say "   registry in use: $SIM_REGISTRY"
fi

# ------------------------------------------------------------------- step 0/8
step "0/8 · binary"
if [ -z "$BIN" ]; then
  BIN="${CHAT_STASHER_BIN:-$ROOT/target/debug/chat-stasher}"
  if [ ! -x "$BIN" ]; then
    say "not found, building: $BIN"
    ( cd "$ROOT" && cargo build -q ) || fail_now "cargo build failed"
  fi
fi
[ -x "$BIN" ] || fail_now "binary is not executable: $BIN"
say "binary   : $BIN"
say "version  : $("$BIN" --version 2>&1)"

# The binary is copied to a path that is NOT under a `target/` directory. That is
# what makes the scheduler assertion in step 7 mean something rather than
# nothing: `schedule` embeds the running executable's path by default, and a
# build artifact is exactly the thing that does not survive `cargo clean`.
INSTALLED="$WORK/bin/chat-stasher"
mkdir -p "$WORK/bin"
cp "$BIN" "$INSTALLED"
chmod +x "$INSTALLED"
say "installed: $INSTALLED (stable path, not under target/)"

# ------------------------------------------------------------------- step 1/8
step "1/8 · synthetic histories in a throwaway HOME"
# The fixtures are planted by a small python3 helper because three of the five
# are SQLite files, and python3's sqlite3 module is in the stdlib everywhere this
# runs — no sqlite3 CLI, no extra dependency.
#
# Each recipe reproduces a shape that already exists in this repository's own
# tests; the comment names the file it came from, so a shape change there is a
# change here too. Nothing is invented, and no fixture is conversation data:
# every one is `{}` or a single synthetic line.
python3 - "${CHAT_STASHER_REGISTRY:-$REGISTRY}" "$HOME_DIR" "$WORK/seeded.tsv" "$WORK/codex-root.txt" "$PLATFORM" <<'PY'
import json, os, pathlib, sqlite3, sys

registry_path, home_arg, manifest_path, codex_root_path, platform = sys.argv[1:6]
home = pathlib.Path(home_arg)

# Session ids are repository test constants: shaped like real ids, and not real.
UUID = "019bf00d-97b6-7eb2-9bf8-eacbacc0{:04d}"


def resolve(template, env):
    """Reduce a registry template to the root the CLI would probe.

    Deliberately narrow: `~`, `$HOME`, `$XDG_DATA_HOME`, `$XDG_CONFIG_HOME` and a
    leading placeholder are all this knows. Anything else — e.g. `$CODEX_HOME` —
    returns None, and the caller reacts by NOT planting rather than by guessing a
    location. A guess would drop the fixture where the CLI never looks and then
    blame the CLI for not finding it.
    """
    out = []
    rest = template
    while rest:
        if rest.startswith("<"):
            break
        if rest.startswith("~/"):
            # Drop the `~` only: the `/` that follows is a real separator and
            # has to survive, or every tilde template lands in a sibling
            # directory called `home.claude` and the harness reads as missing.
            out.append(env["HOME"]); rest = rest[1:]; continue
        if rest.startswith("$"):
            name = rest[1:].split("/")[0]
            if name not in env:
                return None
            out.append(env[name]); rest = rest[len(name) + 1:]; continue
        out.append(rest[0]); rest = rest[1:]
    return "".join(out).rstrip("/")


def write(rel, text):
    p = home / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text)


def sqlite_at(rel):
    p = home / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    if p.exists():
        p.unlink()
    return sqlite3.connect(p)


def plant_claude_code(root, n):
    # tests/doctor_consistency_test.rs (plant_dir_sessions): one .jsonl per
    # session under <root>/<sanitized-cwd>/.
    for i in range(n):
        write(f"{root}/-home-user-smoke/{UUID.format(i)}.jsonl", "{}\n")


def plant_codex(root, n):
    # Same file; the nested date directory is the real Codex rollout layout.
    for i in range(n):
        write(f"{root}/2026-08-01/{UUID.format(100 + i)}.jsonl", "{}\n")


def plant_gemini(root, n):
    # tests/doctor_consistency_test.rs: session-a.json + session-b.jsonl counted,
    # settings.json rejected by the cell's `session-*` pattern. Planted here
    # exactly as that test plants it — the real layout nests one level deeper
    # (<projectId>/chats/) and the walk is recursive, so both shapes are found.
    write(f"{root}/session-a.json", "{}\n")
    write(f"{root}/session-b.jsonl", "{}\n")
    write(f"{root}/settings.json", "{}\n")  # negative control: must NOT be counted


def plant_opencode(root, n):
    # src/sqlite_probe.rs (opencode_export_is_one_session_line_with_nested_parts):
    # session + message + part. The message/part tables are load-bearing, not
    # decoration: with only the `session` table the doctor count is right but no
    # SessionRecord can be produced, and `run-once` ends in ERROR
    # (collect_incomplete) with an archive gap. All three tables are what makes
    # this harness archivable.
    conn = sqlite_at(root)
    conn.executescript(
        "CREATE TABLE session(id TEXT PRIMARY KEY, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);"
        "CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,"
        " time_updated INTEGER NOT NULL, data TEXT NOT NULL);"
        "CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,"
        " time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);"
    )
    for i in range(n):
        t = 1760000000000 + i
        conn.execute("INSERT INTO session VALUES (?,?,?)", (f"session-{i}", t, t))
        conn.execute(
            "INSERT INTO message VALUES (?,?,?,?,?)",
            (f"message-{i}", f"session-{i}", t, t, '{"role":"user"}'),
        )
        conn.execute(
            "INSERT INTO part VALUES (?,?,?,?,?,?)",
            (f"part-{i}", f"message-{i}", f"session-{i}", t, t, '{"type":"text"}'),
        )
    conn.commit(); conn.close()


def plant_cursor(root, n):
    # tests/doctor_consistency_test.rs (plant_cursor_db): cursorDiskKV with
    # `composerData:%` keys and a `createdAt` inside the JSON value.
    conn = sqlite_at(root)
    conn.execute("CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")
    for i in range(n):
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES (?,?)",
            (
                f"composerData:00000000-0000-4000-8000-{i:012}",
                '{"composerId":"c%d","createdAt":%d,"fullConversationHeadersOnly":[{}]}'
                % (i, 1760000000000 + i),
            ),
        )
    conn.commit(); conn.close()


def plant_grok(root, n):
    # tests/doctor_consistency_test.rs (plant_grok_db): `session_docs`, all rows,
    # `updated_at` in seconds.
    conn = sqlite_at(root)
    conn.execute(
        "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL,"
        " updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL,"
        " content_hash TEXT NOT NULL, last_indexed_offset INTEGER NOT NULL DEFAULT 0)"
    )
    for i in range(n):
        conn.execute(
            "INSERT INTO session_docs (session_id,cwd,updated_at,title,content,content_hash)"
            " VALUES (?,?,?,?,?,?)",
            (UUID.format(200 + i), "/tmp/smoke", 1784924765 + i, "t", "c", "h"),
        )
    conn.commit(); conn.close()


def plant_kimi(root, n):
    # tests/w37_kimi_test.rs: the transcript is always named `wire.jsonl`, so the
    # id must come from the `session_*` directory above it. One file per session
    # directory is the whole point of that layout.
    for i in range(n):
        write(
            f"{root}/wd_smoke_{i:014d}/session_00000000-0000-4000-8000-{i:012d}"
            "/agents/main/wire.jsonl",
            '{"role":"user","text":"synthetic"}\n',
        )


# harness id -> (sessions the recipe plants, the planting function)
RECIPES = {
    "claude-code": (2, plant_claude_code),
    "codex": (1, plant_codex),
    "gemini-cli": (2, plant_gemini),
    "opencode": (3, plant_opencode),
    "cursor": (2, plant_cursor),
    "grok": (1, plant_grok),
    "kimi-code": (1, plant_kimi),
}

# Harnesses with no recipe, each with the reason it has none.
NO_RECIPE = {
    "aider": "template_unresolvable",
    "crush": "template_unresolvable",
    "zed": "no_schema_in_build",
    "continue": "cell_rejects_json",
    "github-copilot-cli": "id_not_keyable",
}

# Where a harness lives when the registry cell for this platform cannot anchor
# it, and the config key that says so. This is the tool's *documented* remedy for
# a cell it will not guess at, and each path is the registry's own stated default
# for an unset override — not a location invented here.
#
# It is a fallback, not an override: a platform whose cell does resolve (macOS
# spells codex's `~/.codex/sessions/`) keeps using the registry, so the shipped
# cell is what is exercised wherever it can be.
CONFIGURED_ROOTS = {
    "codex": (".codex/sessions", "codex_sessions_dir"),
}

registry = json.loads(pathlib.Path(registry_path).read_text())
env = {
    "HOME": str(home),
    "XDG_DATA_HOME": os.environ["XDG_DATA_HOME"],
    "XDG_CONFIG_HOME": os.environ["XDG_CONFIG_HOME"],
}

counts = {}
configured = []
manifest = []
for harness in registry["harnesses"]:
    hid = harness["id"]
    cell = harness["paths"].get(platform)
    if cell is None:
        manifest.append((hid, "not_seeded", "no_cell_for_platform"))
        continue
    recipe = RECIPES.get(hid)
    if recipe is None:
        manifest.append((hid, "not_seeded", NO_RECIPE.get(hid, "no_fixture_recipe")))
        continue
    # The registry's own gate: an `unascertained` cell is one this build refuses
    # to scan, whatever sits on disk. Planting there would create an expectation
    # the tool is designed not to meet, so the harness is reported, not seeded.
    # A *configured* root is not gated that way — it is the user stating a fact,
    # not the registry guessing — but the fallback below is only reached for a
    # template this build cannot anchor, which is a different case.
    if cell.get("confidence") == "unascertained":
        manifest.append((hid, "not_seeded", "confidence_unascertained"))
        continue
    root = resolve(cell["template"], env)
    if root is None:
        # The cell's template starts with a per-install override (`$CODEX_HOME`,
        # `$GEMINI_CLI_HOME`, ...) that this build deliberately does not resolve.
        # Fall back to the documented configured root, when there is one.
        fallback = CONFIGURED_ROOTS.get(hid)
        if fallback is None:
            manifest.append((hid, "not_seeded", "template_unresolvable"))
            continue
        rel, config_key = fallback
        root = str(home / rel)
        configured.append((hid, config_key, root))
    n, fn = recipe
    fn(root, n)
    counts[hid] = n
    manifest.append((hid, "seeded", str(n)))

pathlib.Path(manifest_path).write_text(
    "".join(f"{hid}\t{status}\t{note}\n" for hid, status, note in manifest)
)
# Handed back to the shell, which appends them to the config after `init`.
pathlib.Path(codex_root_path).write_text(
    "".join(f"{hid}\t{key}\t{path}\n" for hid, key, path in configured)
)

print(f"[smoke]   platform: {platform}")
print(f"[smoke]   planted {sum(counts.values())} synthetic sessions across {len(counts)} harnesses:")
for hid, n in sorted(counts.items()):
    print(f"[smoke]     {hid:22} {n}")
if configured:
    print("[smoke]   seeded through a configured root, because this platform's")
    print("[smoke]   registry cell cannot be anchored without one:")
    for hid, key, path in configured:
        print(f"[smoke]     {hid:22} {key} = {path}")
print("[smoke]   not seeded (each reason asserted in step 8):")
for hid, status, note in manifest:
    if status == "not_seeded":
        print(f"[smoke]     {hid:22} {note}")
PY
[ -s "$WORK/seeded.tsv" ] || fail_now "fixture manifest was not written"
SEEDED_IDS="$(awk -F'\t' '$2=="seeded"{print $1}' "$WORK/seeded.tsv" | tr '\n' ' ')"
SEEDED_TOTAL="$(awk -F'\t' '$2=="seeded"{s+=$3} END{print s+0}' "$WORK/seeded.tsv")"
[ -n "$SEEDED_IDS" ] || fail_now "no harness could be seeded — the recipes and the registry have diverged"
# A floor on coverage, so a recipe cannot be dropped (or a cell's confidence
# flipped) and take the smoke's reach with it silently. These four are the
# harnesses this smoke is expected to cover on Linux; if one drops out, that is
# a change to look at, not a quieter smoke.
for required in claude-code codex gemini-cli opencode; do
  echo " $SEEDED_IDS " | grep -q " $required " \
    || fail_now "required harness $required could not be seeded on $PLATFORM — coverage would silently shrink"
done
say "seeded harnesses: $SEEDED_IDS"
say "seeded sessions : $SEEDED_TOTAL"

# ------------------------------------------------------------------- step 2/8
step "2/8 · doctor on a machine with nothing configured"
# This run is the honest-absence half of the smoke. Nothing is configured yet, so
# the registry alone decides which harnesses get looked at — and the invariant
# this repository exists for has to hold here: a harness the registry declines to
# probe is `unknown`, and an unknown is never rendered as `0`.
DOCTOR_JSON="$WORK/doctor-pristine.json"
rc=0; "$INSTALLED" doctor --json >"$DOCTOR_JSON" 2>"$WORK/doctor-pristine.err" || rc=$?
check "doctor --json exits 0" 0 "$rc"
"$INSTALLED" doctor >"$WORK/doctor-pristine.txt" 2>&1 || true
sed 's/^/[smoke]   | /' "$WORK/doctor-pristine.txt"
rc=0
python3 - "$DOCTOR_JSON" "$REGISTRY" <<'PY' || rc=$?
import json, pathlib, sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text())
registry = json.loads(pathlib.Path(sys.argv[2]).read_text())
bad = []

if report.get("scan_failed"):
    bad.append("scan_failed is true — the registry scan did not complete")

probes = {p["harness"]: p for p in report["probes"]}
missing = [h["id"] for h in registry["harnesses"] if h["id"] not in probes]
if missing:
    bad.append(f"registry harnesses absent from the probe table: {missing}")

# Invariant 1, applied to the smoke itself: a skipped probe must stay `unknown`,
# and a probe that ran must carry a real count. Either direction can break, and
# either break would make every number below mean nothing.
for hid, p in sorted(probes.items()):
    state = p["state"]
    sc = p["session_count"]
    if state.startswith("skip"):
        if sc["kind"] != "unknown":
            bad.append(f"{hid}: state={state} yet session_count={sc} (a skip must stay unknown)")
    elif sc["kind"] != "known":
        bad.append(f"{hid}: state={state} yet session_count={sc} (a probe that ran must count)")

print("[smoke]   probes: %d · skipped: %s" % (
    len(probes), sorted(h for h, p in probes.items() if p["state"].startswith("skip"))))
if bad:
    for line in bad:
        print(f"[smoke]   FAIL · {line}")
    sys.exit(1)
print("[smoke]   PASS · every registry harness has a probe row; skip states stay 'unknown', never 0")
PY
assert "doctor's own invariants hold on an unconfigured machine" "$rc"

# ------------------------------------------------------------------- step 3/8
step "3/8 · init"
rc=0; "$INSTALLED" init >"$WORK/init.txt" 2>&1 || rc=$?
check "init exits 0" 0 "$rc"
sed 's/^/[smoke]   | /' "$WORK/init.txt"
CONFIG="$XDG_CONFIG_HOME/chat-stasher/config.toml"
assert "init wrote a config file ($CONFIG)" "$([ -s "$CONFIG" ] && echo 0 || echo 1)"

# Codex note. On Linux and Windows the registry's codex cell is
# `$CODEX_HOME/sessions/`. `$CODEX_HOME` is a per-install override this build does
# not resolve (scanner.rs, `static_prefix_root`), and its marker table does not
# recognise the Linux spelling either, so exporting CODEX_HOME does not help
# (scanner.rs, `root_from_env_override`). With nothing configured, codex is
# therefore `sessions=unknown` on Linux, while the macOS cell —
# `~/.codex/sessions/` — resolves unaided. The remedy the tool documents is a
# configured root, and the path written below is the registry's own stated
# default for an unset CODEX_HOME, i.e. where a real Linux codex install keeps
# its rollouts. Seeding it any other way would drop the fixture where nothing
# looks. Without this line the codex leg of the chain is not exercised at all.
if [ -s "$WORK/codex-root.txt" ]; then
  while IFS=$'\t' read -r hid key path; do
    [ -n "$hid" ] || continue
    printf '\n# written by scripts/smoke/linux-smoke.sh — see the Codex note in that file\n%s = "%s"\n' \
      "$key" "$path" >>"$CONFIG"
    say "configured $key = $path (registry cell for $hid cannot be anchored here)"
  done <"$WORK/codex-root.txt"
fi

# ------------------------------------------------------------------- step 4/8
step "4/8 · run-once (collect -> seal -> push)"
RUN_LOG="$WORK/run-once.txt"
rc=0; "$INSTALLED" run-once --stage "$STAGE" --machine "$SMOKE_MACHINE" >"$RUN_LOG" 2>&1 || rc=$?
check "run-once exits 0" 0 "$rc"
grep -E '^\[(collect|push|run-once)\]' "$RUN_LOG" | sed 's/^/[smoke]   | /' || true
assert "run-once reports result: COMPLETED (a snapshot was created)" \
  "$(grep -q 'result: COMPLETED' "$RUN_LOG" && echo 0 || echo 1)"

# The stage holds the sealed shard tree, one directory per session; the snapshot
# itself lands in the repository. Both halves are asserted, because "collected"
# and "archived" are two different claims.
STAGE_SESSIONS="$(find "$STAGE/sessions/$SMOKE_MACHINE" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')"
assert "the stage holds a sealed shard tree, one directory per session ($STAGE_SESSIONS)" \
  "$([ "$STAGE_SESSIONS" -gt 0 ] && echo 0 || echo 1)"
assert "every staged session directory holds at least one sealed shard" \
  "$(find "$STAGE/sessions/$SMOKE_MACHINE" -mindepth 3 -maxdepth 3 -name '*.jsonl' | grep -q . && echo 0 || echo 1)"

# ------------------------------------------------------------------- step 5/8
step "5/8 · read --all-machines (the snapshot is readable back)"
READ_LOG="$WORK/read.txt"
rc=0; "$INSTALLED" read --all-machines >"$READ_LOG" 2>&1 || rc=$?
check "read --all-machines exits 0" 0 "$rc"
grep -E '^\[read\]|^  machine' "$READ_LOG" | sed 's/^/[smoke]   | /' || true
assert "the repository holds exactly one snapshot" \
  "$(grep -q 'snapshots read : 1' "$READ_LOG" && echo 0 || echo 1)"
assert "the snapshot carries $STAGE_SESSIONS sessions, the same number the stage holds" \
  "$(grep -qE "^  machine $SMOKE_MACHINE .*sessions=$STAGE_SESSIONS\$" "$READ_LOG" && echo 0 || echo 1)"

# ------------------------------------------------------------------- step 6/8
step "6/8 · overview (machine x harness, read out of the repository)"
OVERVIEW_LOG="$WORK/overview.txt"
# `overview` deliberately has no default destination and this config declares
# none, so the repository is named explicitly — the documented way to open one
# for a single-destination setup.
rc=0; "$INSTALLED" overview --repo "$XDG_DATA_HOME/chat-stasher/repo" >"$OVERVIEW_LOG" 2>&1 || rc=$?
check "overview exits 0" 0 "$rc"
sed 's/^/[smoke]   | /' "$OVERVIEW_LOG"
assert "overview lists this machine ($SMOKE_MACHINE)" \
  "$(grep -q "$SMOKE_MACHINE" "$OVERVIEW_LOG" && echo 0 || echo 1)"
assert "overview sees one machine whose session total matches the stage" \
  "$(grep -q "^machines 1 · harnesses .* · sessions $STAGE_SESSIONS " "$OVERVIEW_LOG" && echo 0 || echo 1)"

# ------------------------------------------------------------------- step 7/8
step "7/8 · schedule renders a systemd user unit (renders only; installs nothing)"
SCHED_OK="$WORK/schedule-stable.txt"
rc=0; "$INSTALLED" schedule --format systemd --stage "$STAGE" --binary "$INSTALLED" >"$SCHED_OK" 2>&1 || rc=$?
check "schedule --format systemd exits 0" 0 "$rc"
sed 's/^/[smoke]   | /' "$SCHED_OK"
assert "a systemd service and timer are rendered" \
  "$(grep -q 'chat-stasher-run-once.service' "$SCHED_OK" && grep -q 'chat-stasher-run-once.timer' "$SCHED_OK" && echo 0 || echo 1)"
assert "ExecStart points at the stable binary path" \
  "$(grep -qF "ExecStart=\"$INSTALLED\"" "$SCHED_OK" && echo 0 || echo 1)"
assert "no rendered line points into a target/ directory" \
  "$(grep -q '/target/' "$SCHED_OK" && echo 1 || echo 0)"
assert "the build-artifact warning is NOT printed for a stable path" \
  "$(grep -q 'warning: resolved binary is a build artifact' "$SCHED_OK" && echo 1 || echo 0)"

# ... and the same renderer, pointed at a build artifact, must say so. Without
# this second run the assertion above would also pass if the warning had been
# deleted — it would prove nothing about the thing it claims to check.
ARTIFACT_DIR="$WORK/checked-out/target/debug"
mkdir -p "$ARTIFACT_DIR"
cp "$BIN" "$ARTIFACT_DIR/chat-stasher"
SCHED_BAD="$WORK/schedule-artifact.txt"
rc=0; "$INSTALLED" schedule --format systemd --stage "$STAGE" --binary "$ARTIFACT_DIR/chat-stasher" >"$SCHED_BAD" 2>&1 || rc=$?
check "schedule still exits 0 for a build artifact" 0 "$rc"
assert "a build-artifact path DOES raise the warning, so the check above is not vacuous" \
  "$(grep -q 'warning: resolved binary is a build artifact' "$SCHED_BAD" && echo 0 || echo 1)"

# ------------------------------------------------------------------- step 8/8
step "8/8 · every seeded harness was detected, with the count that was planted"
DOCTOR_JSON2="$WORK/doctor-configured.json"
rc=0; "$INSTALLED" doctor --json >"$DOCTOR_JSON2" 2>"$WORK/doctor-configured.err" || rc=$?
check "doctor --json exits 0 with the config in place" 0 "$rc"
"$INSTALLED" doctor >"$WORK/doctor-configured.txt" 2>&1 || true
sed 's/^/[smoke]   | /' "$WORK/doctor-configured.txt"

rc=0
python3 - "$DOCTOR_JSON2" "$WORK/seeded.tsv" <<'PY' || rc=$?
import json, pathlib, sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text())
rows = [l.split("\t") for l in pathlib.Path(sys.argv[2]).read_text().splitlines() if l]
seeded = {r[0]: int(r[2]) for r in rows if r[1] == "seeded"}
not_seeded = {r[0]: r[2] for r in rows if r[1] == "not_seeded"}
probes = {p["harness"]: p for p in report["probes"]}
bad = []

for hid, want in sorted(seeded.items()):
    p = probes.get(hid)
    if p is None:
        bad.append(f"{hid}: seeded but has no probe row")
        continue
    sc = p["session_count"]
    if sc["kind"] != "known":
        bad.append(f"{hid}: seeded {want} sessions but the count is {sc} — not detected")
    elif sc["count"] != want:
        bad.append(f"{hid}: seeded {want} sessions but doctor counted {sc['count']}")
    else:
        print(f"[smoke]   PASS · {hid:22} detected · sessions={sc['count']} · state={p['state']}")

# The gaps, asserted as states. For the two reasons that carry a defined state,
# the state AND the tri-state tag are both checked: "we did not look" must not
# come back dressed as "there was nothing there".
EXPECTED_STATE = {
    "confidence_unascertained": "skip_unascertained",
    "template_unresolvable": "skip_unresolvable",
}
for hid, reason in sorted(not_seeded.items()):
    p = probes.get(hid)
    if p is None:
        bad.append(f"{hid}: not seeded ({reason}) and no probe row either")
        continue
    want = EXPECTED_STATE.get(reason)
    if want is None:
        print(f"[smoke]   note · {hid:22} not seeded ({reason}); doctor reports "
              f"state={p['state']} sessions={p['session_count']}")
    elif p["state"] != want:
        bad.append(f"{hid}: not seeded because {reason}, yet doctor reports state={p['state']}")
    elif p["session_count"]["kind"] != "unknown":
        bad.append(f"{hid}: state={want} yet session_count={p['session_count']} (must stay unknown)")
    else:
        print(f"[smoke]   PASS · {hid:22} not seeded ({reason}) · doctor reports {want} and 'unknown'")

gaps = report.get("archive_gaps") or []
if gaps:
    bad.append(f"archive gaps: {[g['harness'] for g in gaps]} — recognised but not archivable")

if bad:
    for line in bad:
        print(f"[smoke]   FAIL · {line}")
    sys.exit(1)
print("[smoke]   PASS · every seeded harness is detected with the count that was planted")
print("[smoke]   PASS · every unseeded harness is reported as skipped/unknown, and no archive gap remains")
PY
assert "seeded harnesses detected; unseeded ones honestly reported" "$rc"

# ----------------------------------------------------------------------- verdict
echo
if [ "$FAILED" = 0 ]; then
  say "$SEEDED_TOTAL synthetic sessions from $(echo "$SEEDED_IDS" | wc -w | tr -d ' ') harness(es): detect -> init -> collect -> push -> read -> overview -> schedule"
  echo "[smoke] SMOKE: PASS"
  exit 0
fi
echo "[smoke] SMOKE: FAIL"
exit 1
