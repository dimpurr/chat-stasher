#!/usr/bin/env bash
# install-matrix.sh — MEN-3: menu bar app × CLI install matrix (one machine).
#
# Drives the REAL app binary (apps/menubar, built below) against the states
# MEN-3 accepts, entirely in throwaway HOME/prefix dirs — never the user's
# real ~/.local/bin, /Applications, or launchd:
#
#   (a)  app installed first, then the CLI arrives:
#        (a1) no CLI anywhere on PATH
#        (a2) the CLI is installed by the REAL scripts/install.sh (a mock
#             release served over file://, the same trick as
#             scripts/self-test-install.sh) into its default destination
#             $HOME/.local/bin, $HOME being the throwaway home
#   (b)  CLI first, then the app
#   (c)  a stale OLDER CLI earlier on PATH than a newer one (both orders)
#   (up) upgrade direction: the real install.sh puts an older CLI down, the
#        app reports it too old, then the real install.sh upgrades it in
#        place and the app reports the new one healthy
#   (*)  two evidence scenarios from driving a real machine:
#        a CLI answering status --json without the contracted `local`
#        section (the 0.4.x shape), and a CLI answering garbage — the app
#        must classify both as the too-old family, never as a config
#        problem.
#
# The app is asked through its own headless handshake, `--resolve-cli`,
# which runs the same spawnCli/readStatus path the panel runs and prints
# `cli=<absolute path|none> version=<v|unknown> state=<token>`:
#   ok · too-old · cli-missing · no-status-document · setup ·
#   credentials · unreadable
# That one line is the acceptance's three answers at once — which CLI was
# found, what version it reported, and the too-old verdict. The About
# sheet renders the same LocalSnapshot (cliHandshakeCaption), and the
# classification there is covered by unit tests.
#
# The OS half is cross-checked per scenario: what an execvp PATH search
# would run (command -v under /bin/sh, plus the classic env-127 refusal
# when nothing resolves) must name the same binary the app reports, so
# the app's PATH walk (cliOnPath) can never drift from what a shell would
# find. An independent semver floor comparison mirrors the too-old verdict
# so the app is never grading its own homework.
#
# Not covered here (stated, not skipped silently): the GUI rendering of
# the About sheet and the status card, which needs eyes on screen — this
# verifies the facts they render; and https downloads of real releases,
# which the file:// mock replaces because the network is not a property of
# this machine. Repeatable as-is on m3/i7 later; every run builds its own
# CLIs, so no older host artifact is needed.
#
# Temp-dir values inside `cli=` fields assume paths without whitespace
# (mktemp on macOS/Linux guarantees this in TMPDIR; the script creates its
# own work dir and never inspects anything outside it).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
APP_DIR="$ROOT/apps/menubar"
FLOOR="0.5.0-rc.2"
OLD_VERSION="0.4.0"
NEW_VERSION="0.5.0-rc.2"

PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }

# ---------------------------------------------------------------------------
# 0. Build the real app binary. The first run fetches the Sparkle
#    dependency; afterwards this is incremental and near-instant.
# ---------------------------------------------------------------------------
echo "## build: chat-stasher-menubar (swift build)"
BUILD_LOG="$(mktemp)"
if (cd "$APP_DIR" && swift build >"$BUILD_LOG" 2>&1); then
  ok "app binary built (swift build exit 0)"
else
  tail -20 "$BUILD_LOG" >&2
  exit 1
fi
rm -f "$BUILD_LOG"
APP_BIN="$APP_DIR/.build/debug/chat-stasher-menubar"
[ -x "$APP_BIN" ] || { echo "no app binary at $APP_BIN" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Fakes and harness
# ---------------------------------------------------------------------------
HOST_OS="$(uname -s | tr 'A-Z' 'a-z')"
HOST_ARCH="$(uname -m | tr 'A-Z' 'a-z')"
[ "$HOST_ARCH" = "aarch64" ] && HOST_ARCH="arm64"
HOST_TARGET="$HOST_OS-$HOST_ARCH"

# make_shim DIR VERSION SHAPE writes a `chat-stasher` executable whose
# `status --json` answer is one of the shapes the matrix names:
#   contracted   — the 0.5.0-rc.2-era document: cli_version + local section
#   precontract  — the 0.4.x document: it decodes (schema, command), but
#                  neither `local` nor `cli_version` exists, the exact
#                  shape a real stale install has
#   garbage      — prose, nothing parseable
# The whole body is one file with the version baked in, because the real
# install.sh moves exactly the one artifact file — a sibling would not
# survive installation.
make_shim() {
  local dir="$1" version="$2" shape="${3:-contracted}" body
  case "$shape" in
    contracted)
      body='{"schema_version":1,"command":"status","exit_code":0,"cli_version":"'"$version"'","config_source":"file","config_error":null,"config_error_kind":null,"scanner":{"kind":"ok","why":null},"local":{"schedule":{"kind":"launchd","installed":true,"units":[]},"last_run":null,"stage":{"waiting_to_upload":{"kind":"known","value":0,"why":null}},"destination_names":[]}}'
      ;;
    precontract)
      body='{"schema_version":1,"command":"status","exit_code":0,"config_source":"file","healthy":true,"run_state":{"kind":"known","outcome":"ok"},"scanner":{"kind":"ok"}}'
      ;;
    garbage)
      body='This CLI only speaks an older status format.'
      ;;
  esac
  mkdir -p "$dir"
  cat > "$dir/chat-stasher" <<SHIM
#!/bin/sh
case "\$1" in
  status)
    printf '%s\n' '$body'
    exit 0 ;;
  overview)
    printf '%s\n' '{"schema_version":1,"command":"overview","variant":"summary","exit_code":0,"totals":{"machines":0,"sources":0,"sessions":0,"unknown_time_sessions":0,"no_conversation_content_sessions":0},"machines":[],"sources":[],"days":[]}'
    exit 0 ;;
  --version)
    printf 'chat-stasher $version\n'
    exit 0 ;;
  *) echo "usage" >&2; exit 2 ;;
esac
SHIM
  chmod +x "$dir/chat-stasher"
}

# A mock release dist, the self-test-install.sh arrangement: one artifact
# named for this host target, checksummed, served over file:// so the REAL
# scripts/install.sh runs end to end with no network. install.sh runs the
# artifact's `--version` before installing it; the shim answers.
mock_dist() {
  local dist="$1" version="$2" hash
  make_shim "$dist.stage" "$version" contracted
  mv "$dist.stage/chat-stasher" "$dist/chat-stasher-$HOST_TARGET"
  rmdir "$dist.stage"
  hash="$(shasum -a 256 "$dist/chat-stasher-$HOST_TARGET" | awk '{print $1}')"
  ( cd "$dist" && printf '%s  %s\n' "$hash" "chat-stasher-$HOST_TARGET" > SHA256SUMS )
}

# The real installer, aimed at a throwaway home by the environment alone
# (HOME decides the default destination; nothing outside $HOME/$WORK is
# touched). The install log stays inside the throwaway home.
run_real_install() {
  local dist="$1" version="$2" home="$3"
  mkdir -p "$home"
  env -i PATH=/usr/bin:/bin HOME="$home" \
    CHAT_STASHER_BASE_URL="file://$dist" CHAT_STASHER_VERSION="$version" \
    bash "$ROOT/scripts/install.sh" >"$home/install.log" 2>&1
}

# What an execvp search would run on the given PATH: none, or a path.
execvp_finds() {
  local found=""
  found="$(env -i PATH="$1" /bin/sh -c 'command -v chat-stasher' 2>/dev/null)" || found=""
  if [ -n "$found" ]; then printf '%s' "$found"; else printf none; fi
}

# What the app reports on the given PATH: the --resolve-cli line. HEREDIR
# is a scratch home so the run cannot read the invoker's real state.
app_reports() {
  local path="$1" heredir="$2"
  env -i PATH="$path" HOME="$heredir" TMPDIR="$WORK/state/app-tmp" \
    "$APP_BIN" --resolve-cli
}

# The value of KEY= in a `cli=a version=b state=c` line.
line_field() {
  local line="$1" key="$2" pair
  for pair in $line; do
    case "$pair" in
      "$key="*) printf '%s' "${pair#"$key"=}"; return 0 ;;
    esac
  done
  printf absent
}

# An independent oracle mirroring the app's versionAtLeast prerelease
# ordering, so the too-old verdict is cross-checked, not just echoed.
at_floor() {
  python3 - "$1" <<'PY'
import sys
def parse(v):
    core, _, pre = v.partition("-")
    nums = tuple(int(x) for x in core.split("."))
    prerelease = pre.split(".") if pre else None
    return nums, prerelease
def cmpver(a, b):
    for i in range(max(len(a[0]), len(b[0]))):
        x = a[0][i] if i < len(a[0]) else 0
        y = b[0][i] if i < len(b[0]) else 0
        if x != y: return (x > y) - (x < y)
    pa, pb = a[1], b[1]
    if pa is None and pb is None: return 0
    if pa is None: return 1
    if pb is None: return -1
    for x, y in zip(pa, pb):
        if x == y: continue
        return (int(x) > int(y)) - (int(x) < int(y)) if x.isdigit() and y.isdigit() \
            else ((x > y) - (x < y))
    return (len(pa) > len(pb)) - (len(pa) < len(pb))
floor = parse("0.5.0-rc.2")
sys.exit(0 if cmpver(parse(sys.argv[1]), floor) >= 0 else 1)
PY
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/menubar-matrix.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/state/app-tmp"

OLD_DIR="$WORK/old-cli"
NEW_DIR="$WORK/new-cli"
PRECONTRACT_DIR="$WORK/precontract-cli"
GARBAGE_DIR="$WORK/garbage-cli"
HOME_A="$WORK/home-a"        # (a2): the app exists first; CLI installed later
HOME_UP="$WORK/home-up"      # (up): older CLI, upgraded in place later
OLD_DIST="$WORK/dist-old"
NEW_DIST="$WORK/dist-new"
mkdir -p "$OLD_DIST" "$NEW_DIST"

make_shim "$OLD_DIR" "$OLD_VERSION" contracted
make_shim "$NEW_DIR" "$NEW_VERSION" contracted
make_shim "$PRECONTRACT_DIR" "$OLD_VERSION" precontract
make_shim "$GARBAGE_DIR" "$OLD_VERSION" garbage
mock_dist "$NEW_DIST" "$NEW_VERSION"
mock_dist "$OLD_DIST" "$OLD_VERSION"

echo ""
echo "## scenario (a): app first, then the CLI"
echo "-- (a1) no chat-stasher anywhere on PATH"
a1_execvp_rc=0
a1_out="$(env -i PATH=/usr/bin:/bin chat-stasher status --json 2>/dev/null)" || a1_execvp_rc=$?
if [ "$a1_execvp_rc" -eq 127 ] && [ -z "$a1_out" ]; then
  ok "execvp search refuses (exit 127, empty stdout)"
else
  bad "wanted env exit 127 + empty stdout, got rc=$a1_execvp_rc out=$(printf %q "$a1_out")"
fi
if [ "$(execvp_finds /usr/bin:/bin)" = none ]; then
  ok "command -v finds none"
else
  bad "command -v found something on an empty PATH"
fi
a1_line="$(app_reports /usr/bin:/bin "$HOME_A")"
if [ "$(line_field "$a1_line" cli)" = none ] && [ "$(line_field "$a1_line" version)" = unknown ] \
   && [ "$(line_field "$a1_line" state)" = cli-missing ]; then
  ok "app reports the missing CLI: $a1_line"
else
  bad "app line on an empty PATH: $a1_line"
fi

echo "-- (a2) the real install.sh puts CLI $NEW_VERSION at \$HOME/.local/bin (throwaway HOME)"
if run_real_install "$NEW_DIST" "$NEW_VERSION" "$HOME_A"; then
  ok "install.sh exit 0 — $(tail -1 "$HOME_A/install.log")"
else
  bad "install.sh failed: $(tail -3 "$HOME_A/install.log")"
fi
A2_BIN="$HOME_A/.local/bin/chat-stasher"
A2_PATH="$HOME_A/.local/bin:/usr/bin:/bin"
if [ -x "$A2_BIN" ]; then
  ok "installed binary present + executable at \$HOME/.local/bin/chat-stasher"
else
  bad "no executable landed in \$HOME/.local/bin"
fi
a2_line="$(app_reports "$A2_PATH" "$HOME_A")"
if [ "$(execvp_finds "$A2_PATH")" = "$A2_BIN" ] && [ "$(line_field "$a2_line" cli)" = "$A2_BIN" ]; then
  ok "app and the execvp search agree on the found CLI"
else
  bad "app said cli=$(line_field "$a2_line" cli); execvp said $(execvp_finds "$A2_PATH")"
fi
if [ "$(line_field "$a2_line" version)" = "$NEW_VERSION" ] && at_floor "$NEW_VERSION" \
   && [ "$(line_field "$a2_line" state)" = ok ]; then
  ok "version $NEW_VERSION reported healthy (>= floor $FLOOR)"
else
  bad "a2 app line: $a2_line"
fi

echo ""
echo "## scenario (b): CLI first, then the app"
b_line="$(app_reports "$NEW_DIR:/usr/bin:/bin" "$HOME_A")"
if [ "$(execvp_finds "$NEW_DIR:/usr/bin:/bin")" = "$NEW_DIR/chat-stasher" ] \
   && [ "$(line_field "$b_line" cli)" = "$NEW_DIR/chat-stasher" ] \
   && [ "$(line_field "$b_line" version)" = "$NEW_VERSION" ] \
   && [ "$(line_field "$b_line" state)" = ok ]; then
  ok "app finds the pre-installed CLI: v$(line_field "$b_line" version), healthy"
else
  bad "b app line: $b_line (execvp: $(execvp_finds "$NEW_DIR:/usr/bin:/bin"))"
fi

echo ""
echo "## scenario (c): a stale OLDER CLI earlier on PATH than a newer one"
echo "-- old precedes new"
c_line="$(app_reports "$OLD_DIR:$NEW_DIR:/usr/bin:/bin" "$HOME_A")"
if [ "$(execvp_finds "$OLD_DIR:$NEW_DIR:/usr/bin:/bin")" = "$OLD_DIR/chat-stasher" ] \
   && [ "$(line_field "$c_line" cli)" = "$OLD_DIR/chat-stasher" ] \
   && [ "$(line_field "$c_line" version)" = "$OLD_VERSION" ]; then
  ok "app runs the EARLIER stale CLI"
else
  bad "c app line: $c_line (execvp: $(execvp_finds "$OLD_DIR:$NEW_DIR:/usr/bin:/bin"))"
fi
if [ "$(line_field "$c_line" state)" = too-old ] && ! at_floor "$OLD_VERSION"; then
  ok "stale $OLD_VERSION reported too old (independent floor agrees)"
else
  bad "too-old verdict wrong: $c_line"
fi
echo "-- new precedes old on PATH"
c2_line="$(app_reports "$NEW_DIR:$OLD_DIR:/usr/bin:/bin" "$HOME_A")"
if [ "$(execvp_finds "$NEW_DIR:$OLD_DIR:/usr/bin:/bin")" = "$NEW_DIR/chat-stasher" ] \
   && [ "$(line_field "$c2_line" cli)" = "$NEW_DIR/chat-stasher" ] \
   && [ "$(line_field "$c2_line" state)" = ok ]; then
  ok "reversed order picks the newer CLI: v$(line_field "$c2_line" version), healthy"
else
  bad "c2 app line: $c2_line"
fi

echo ""
echo "## upgrade direction: older CLI -> newer, via the real install.sh"
if run_real_install "$OLD_DIST" "$OLD_VERSION" "$HOME_UP"; then
  ok "install.sh exit 0 installing $OLD_VERSION"
else
  bad "install.sh failed for $OLD_VERSION: $(tail -3 "$HOME_UP/install.log")"
fi
UP_PATH="$HOME_UP/.local/bin:/usr/bin:/bin"
UP_BIN="$HOME_UP/.local/bin/chat-stasher"
up_line="$(app_reports "$UP_PATH" "$HOME_UP")"
if [ "$(line_field "$up_line" cli)" = "$UP_BIN" ] && [ "$(line_field "$up_line" version)" = "$OLD_VERSION" ] \
   && [ "$(line_field "$up_line" state)" = too-old ]; then
  ok "older CLI reported too old: $up_line"
else
  bad "pre-upgrade app line: $up_line"
fi
UP_BEFORE_HASH="$(shasum -a 256 "$UP_BIN" | awk '{print $1}')"
if run_real_install "$NEW_DIST" "$NEW_VERSION" "$HOME_UP"; then
  ok "install.sh exit 0 upgrading to $NEW_VERSION in place"
else
  bad "upgrade install.sh failed: $(tail -3 "$HOME_UP/install.log")"
fi
UP_AFTER_HASH="$(shasum -a 256 "$UP_BIN" | awk '{print $1}')"
if [ "$UP_BEFORE_HASH" != "$UP_AFTER_HASH" ]; then
  ok "upgrade replaced the binary bytes at the same path"
else
  bad "upgrade left the old bytes in place"
fi
up2_line="$(app_reports "$UP_PATH" "$HOME_UP")"
if [ "$(line_field "$up2_line" cli)" = "$UP_BIN" ] && [ "$(line_field "$up2_line" version)" = "$NEW_VERSION" ] \
   && [ "$(line_field "$up2_line" state)" = ok ] && at_floor "$NEW_VERSION"; then
  ok "upgraded CLI reported healthy by a fresh app process"
else
  bad "post-upgrade app line: $up2_line"
fi

echo ""
echo "## evidence scenarios from the real machine: stale shapes map to the too-old family"
echo "-- a CLI answering status without the contracted local section (0.4.x shape)"
p_line="$(app_reports "$PRECONTRACT_DIR:/usr/bin:/bin" "$HOME_A")"
if [ "$(line_field "$p_line" cli)" = "$PRECONTRACT_DIR/chat-stasher" ] \
   && [ "$(line_field "$p_line" state)" = no-status-document ]; then
  ok "pre-contract document classified as too old, not a config problem"
else
  bad "precontract app line: $p_line"
fi
echo "-- a CLI answering garbage"
g_line="$(app_reports "$GARBAGE_DIR:/usr/bin:/bin" "$HOME_A")"
if [ "$(line_field "$g_line" cli)" = "$GARBAGE_DIR/chat-stasher" ] \
   && [ "$(line_field "$g_line" state)" = no-status-document ]; then
  ok "garbage answer classified as too old"
else
  bad "garbage app line: $g_line"
fi

echo ""
echo "matrix result: $PASS passed, $FAIL failed"
if [ "$FAIL" -ne 0 ]; then
  printf 'work dir (kept for inspection): %s\n' "$WORK" >&2
  trap - EXIT
  exit 1
fi
