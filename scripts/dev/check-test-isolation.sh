#!/usr/bin/env bash
# Fail a test run that touches this machine's real chat-stasher user data
# (W306; redesigned by W306b after CI run 37023093243).
#
#     bash scripts/dev/check-test-isolation.sh -- cargo test
#
# runs <command> (default: `cargo test -p chat-stasher`).
#
# A guard is only worth having if the people who must obey it can see it go
# green. The first version of this guard snapshotted the whole data, config and
# state root and failed on *any* change. That is correct on a quiet machine and
# wrong on the machine that matters: the author runs the live product (the
# extension and its native host) on the box the guard gates, and it writes the
# real `stage/ext-status/…` every few minutes. CI run 37023093243 then showed
# the same shape from two more directions:
#
#   * ubuntu-latest: `~/.local/state` did not exist before the run and did
#     after it. The product never writes there (`collect::default_state_dir`
#     is `<data root>/state`); the writer is the `opendal` SFTP backend inside
#     one test, which parks its ControlMaster socket under `$XDG_STATE_HOME`.
#     A guard that reds on that is red on a dependency's scratch directory.
#   * windows-latest: the branch watched `%LOCALAPPDATA%\chat-stasher` as the
#     *data and config* root. That is the product's **cache** root there
#     (`scanner::user_cache_dirs`), so the body cache the suite legitimately
#     warms — `%LOCALAPPDATA%\chat-stasher\body\…`, hundreds of entries — was
#     reported as a leak.
#
# A guard that reds when the product does its job is a guard people learn to
# re-run until it is green. So this version stops asking *where* a change
# landed and asks *what the change carries*: a test write is identifiable by a
# reserved fixture identity or by a per-run marker the shared test sandbox
# stamps into everything it hands a test, and a product write is identifiable
# by carrying neither.
#
# What it watches:
#
#   * Fingerprint scan over the **data, config, cache and state-homed** roots.
#     After the run every entry the run created or modified is inspected, and
#     is a leak when
#       - its name carries a reserved fixture token (`synthetic`, `fixture`,
#         `probe`, `dummy`, case-insensitive, as a whole token) and that path
#         did not already exist before the run, or
#       - its name carries the run marker, or
#       - a small file's *content* carries the run marker, or
#       - a small file *under `<data root>/state`* carries a reserved fixture
#         identity this run introduced — the coordination database is where the
#         2026-10-02 incident left its `synthetic-install` row, and it holds no
#         conversation text, so the token scan cannot mistake an archived
#         conversation for a fixture.
#     The state-store content rule is a case-insensitive **substring** search,
#     not the whole-token boundary rule the names use. A store is not text with
#     delimiters around every value: SQLite lays a record's columns down with no
#     separator, so the incident's row is the bytes `chatgptsynthetic-install`
#     and a boundary search (`[^alnum]` before the token) is blind to it —
#     `...tgpt` + `synthetic-install`. Matching the substring catches that row,
#     while a conversation file is never read because the scan is confined to
#     `<data root>/state`. The unit is the bare token, not the enclosing word:
#     the word is not stable, since any alnum byte a store glues on (the next
#     column of the same record) rewrites `synthetic-install` into a different
#     string, which would defeat the baseline below.
#     Because a machine may already carry debris from an *earlier* leak — the
#     2026-10-02 incident's row lived in the real coordination database until it
#     was removed by hand, and its `chatgpt.synthetic-session` shard is still in
#     the real stage — the run is compared against a **per-run baseline**: the
#     fixture tokens already present in the state store, and the
#     fixture-named paths already present under the watched roots, are recorded
#     before the wrapped command starts and subtracted from the verdict. A run
#     that merely rewrites the file holding old debris (the live product's
#     heartbeat does exactly that) is green; an identity this run *introduces*
#     is red. Subtracting a *path* removes only that path: an entry the run adds
#     inside a residue directory is still a leak, because the baseline names the
#     entries that were there, not the directory's descendants in general.
#     What that cannot separate is a test write that re-uses a fixture token the
#     machine already carries — a new `synthetic-…` value in the state store
#     while debris holding `synthetic` is still on disk. That residual is stated
#     below with the other honest limits.
#     The state home (`~/.local/state`) is scanned for the **marker only**: it
#     is a shared directory the product never writes, so a token or structure
#     rule would red on whatever else uses it, while the run marker can only
#     come from this run's own sandbox.
#     A fixture-named entry that is *removed* during the run is a leak too, so
#     the names are diffed as well; a name the run deleted is only a leak when
#     it carries a fingerprint.
#     The marker is a random token this guard generates per run and exports as
#     `CHAT_STASHER_TEST_ISOLATION_MARKER`. The shared `Sandbox` fixture
#     (`crates/chat-stasher/src/test_support.rs`) puts it in the name of the
#     temp root it hands every test, so a value a test derives from its sandbox
#     — a path echoed into a config file, a status record, an audit row —
#     carries the marker even when the write itself lands in a real root.
#   * Structure diff over the **native-messaging manifest directories and the
#     inbox**: any entry appearing, vanishing, renaming, or appearing and
#     vanishing inside the run is a leak. Nothing the product does on its own
#     writes these during a test run, so for them the old whole-snapshot rule
#     is still the right one.
#
# What this deliberately does not catch, stated so it is a decision and not an
# oversight: a write into a real root that carries no fingerprint at all — a
# realistic-looking identity, no sandbox-derived value in it — is not
# distinguishable from the live product writing, and is not flagged. Neither is
# a write that re-uses a fixture token the baseline already found on the
# machine: subtracting the baseline to keep pre-existing debris from redding
# every run is what makes the two indistinguishable. The
# code-level fail-safe (`crates/chat-stasher/src/test_identity_guard.rs`) is
# the layer for a fixture identity; this guard is the layer for the run's own
# traces, and its selftest pins both sides of that boundary with a probe that
# writes a realistic file into the real data root and must stay green.
#
# Roots are the product's own resolutions, not a second spelling:
#
#   * home        `$HOME` else `$USERPROFILE`      (`config::home_from_env`)
#   * data root   `$XDG_DATA_HOME/chat-stasher` else `~/.local/share/chat-stasher`
#                                                  (`config::default_data_root`)
#   * config root `$XDG_CONFIG_HOME/chat-stasher` else `~/.config/chat-stasher`
#                                                  (`config::config_path`)
#   * cache root  `dirs::cache_dir()/chat-stasher`  (`scanner::user_cache_dirs`):
#                 `~/Library/Caches` on macOS, `$XDG_CACHE_HOME` else `~/.cache`
#                 on Linux, `%LOCALAPPDATA%` else `%USERPROFILE%\AppData\Local`
#                 on Windows. The product's own body cache and activity index
#                 live here, and neither guard watched it on macOS/Linux before
#                 W306b.
#   * state home  `$XDG_STATE_HOME` else `~/.local/state` (`scanner::xdg_state_home`).
#   * inbox       `~/Downloads/chat-stasher/inbox` — the historical drop point.
#                 The extension no longer downloads there, so nothing writes it
#                 during a run; it is kept as a structure root because a test
#                 that *did* write it would be exactly the class this guards.
#   * manifests   read from the filesystem, not hard-coded per browser: the set
#                 that exists now is snapshotted, so a test that writes a
#                 manifest into a browser the guard found is caught, and a
#                 browser directory that appears during the run is itself a
#                 diff.
#
# A root that cannot be named is a refusal (exit 1), never a skip: a guard that
# cannot see is red, not absent. The recursive walk is the expensive part and is
# bounded by the archive's size; the fingerprint scan only reads the files the
# run touched, so a quiet run reads nothing.
#
# Exit codes: 0 = the run left every watched root untouched; 1 = it did not (or
# the snapshot itself failed); 2 = usage. The wrapped command's own exit code is
# reported as part of the verdict and does not bypass it: a green suite that
# dirtied the real data is a red guard run.

set -uo pipefail

TAG="[test-isolation]"

usage() {
  echo "Usage: bash scripts/dev/check-test-isolation.sh [-- <command...>]" >&2
  echo "Runs <command> (default: cargo test -p chat-stasher) and fails when it" >&2
  echo "leaves a test fingerprint in this machine's real chat-stasher data," >&2
  echo "config or cache root, or changes a native-messaging manifest directory" >&2
  echo "or the inbox." >&2
  exit 2
}

refuse() {
  echo "$TAG refusing: $*" >&2
  exit 1
}

COMMAND=("cargo" "test" "-p" "chat-stasher")
while [ "$#" -gt 0 ]; do
  case "$1" in
    --)
      shift
      if [ "$#" -gt 0 ]; then
        COMMAND=("$@")
      fi
      break
      ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
done

case "$(uname -s)" in
  Darwin*) PLATFORM=macos ;;
  Linux*) PLATFORM=linux ;;
  MINGW*|MSYS*|CYGWIN*) PLATFORM=windows ;;
  *) refuse "unsupported platform $(uname -s)" ;;
esac

to_unix_path() {
  local converted=""
  if command -v cygpath >/dev/null 2>&1 &&
    converted=$(cygpath -u "$1" 2>/dev/null) && [ -n "$converted" ]; then
    printf '%s' "$converted"
  else
    printf '%s' "$1" | tr '\\' '/'
  fi
}

SNAP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cs-test-isolation.XXXXXX") || exit 1
cleanup() { rm -rf "$SNAP_DIR"; }
trap cleanup EXIT

# The reserved fixture namespace, restated from
# `test_identity_guard::FIXTURE_IDENTITY_TOKENS`. The selftest reads that
# constant out of the Rust source and fails if the two ever disagree, so the
# copy cannot drift silently.
FIXTURE_TOKENS="synthetic fixture probe dummy"
# A file larger than this is not read for fingerprints. The marker is a random
# token and the token scan is confined to `<data root>/state`, so a cap only
# bounds the cost of a pathological write; it is not a correctness boundary.
CONTENT_SCAN_CAP=$((4 * 1024 * 1024))

HOME_DIR=""
DATA_ROOT=""
CONFIG_ROOT=""
CACHE_ROOT=""
STATE_HOME=""
INBOX_ROOT=""
LOCALAPPDATA_UNIX=""

# `%LOCALAPPDATA%` (Windows) converted once; empty elsewhere. The product's
# cache root and manifest directory both hang off it there.
if [ "$PLATFORM" = "windows" ] && [ -n "${LOCALAPPDATA:-}" ]; then
  LOCALAPPDATA_UNIX="$(to_unix_path "$LOCALAPPDATA")"
fi

# The product's home: `$HOME` first, `$USERPROFILE` second (config::home_from_env).
if [ -n "${HOME:-}" ]; then
  HOME_DIR="$HOME"
elif [ "$PLATFORM" = "windows" ] && [ -n "${USERPROFILE:-}" ]; then
  HOME_DIR="$(to_unix_path "$USERPROFILE")"
fi
[ -n "$HOME_DIR" ] || refuse \
  "neither \$HOME nor (on Windows) \$USERPROFILE is set, so the real user-data" \
  "roots cannot be named; the run would be unguarded"

# data root — `config::default_data_root`.
if [ -n "${XDG_DATA_HOME:-}" ]; then
  DATA_ROOT="$XDG_DATA_HOME/chat-stasher"
else
  DATA_ROOT="$HOME_DIR/.local/share/chat-stasher"
fi

# config root — `config::config_path` (the directory holding `config.toml`).
if [ -n "${XDG_CONFIG_HOME:-}" ]; then
  CONFIG_ROOT="$XDG_CONFIG_HOME/chat-stasher"
else
  CONFIG_ROOT="$HOME_DIR/.config/chat-stasher"
fi

# state home — `scanner::xdg_state_home`.
STATE_HOME="${XDG_STATE_HOME:-$HOME_DIR/.local/state}"

# cache root — `dirs::cache_dir()` as `scanner::user_cache_dirs` spells it. On
# Windows that is `%LOCALAPPDATA%`, which is *not* a child of home; the
# `%USERPROFILE%\AppData\Local` fallback is the documented second candidate.
case "$PLATFORM" in
  macos) CACHE_ROOT="$HOME_DIR/Library/Caches/chat-stasher" ;;
  linux)
    if [ -n "${XDG_CACHE_HOME:-}" ]; then
      CACHE_ROOT="$XDG_CACHE_HOME/chat-stasher"
    else
      CACHE_ROOT="$HOME_DIR/.cache/chat-stasher"
    fi
    ;;
  windows)
    if [ -n "$LOCALAPPDATA_UNIX" ]; then
      CACHE_ROOT="$LOCALAPPDATA_UNIX/chat-stasher"
    else
      CACHE_ROOT="$HOME_DIR/AppData/Local/chat-stasher"
    fi
    ;;
esac

INBOX_ROOT="$HOME_DIR/Downloads/chat-stasher/inbox"

# The product's state store: the one place the fingerprint scan reads content
# from, and the only place the 2026-10-02 incident left a row rather than a
# name. Kept apart from the roots above because the content rule — substring,
# binary-safe — is deliberately narrower than the name rule.
STATE_STORE_ROOT="$DATA_ROOT/state"

# Every root the guard watches structurally: the inbox plus every
# native-messaging manifest directory that currently exists. Re-derived before
# and after the run, so a manifest directory that appears or disappears is part
# of the diff rather than invisible to it.
collect_structure_roots() {
  printf '%s\n' "$INBOX_ROOT"
  # `find -name`, not a `*/NativeMessagingHosts` glob: the Chrome path is
  # `Google/Chrome/NativeMessagingHosts` (and Arc's is `Arc/User Data/…`), two
  # components deep, and `*` does not cross `/` — the glob silently missed the
  # one directory the incident's own browser uses. Depth 4 covers every layout
  # in `nativehost.rs`'s table.
  case "$PLATFORM" in
    macos)
      find "$HOME_DIR/Library/Application Support" -maxdepth 4 -type d \
        -name NativeMessagingHosts 2>/dev/null
      ;;
    linux)
      find "$HOME_DIR/.config" -maxdepth 3 -type d -name NativeMessagingHosts 2>/dev/null
      [ -d "$HOME_DIR/.mozilla/native-messaging-hosts" ] &&
        printf '%s\n' "$HOME_DIR/.mozilla/native-messaging-hosts"
      ;;
    windows)
      [ -n "$LOCALAPPDATA_UNIX" ] &&
        find "$LOCALAPPDATA_UNIX" -maxdepth 3 -type d -name NativeMessagingHosts 2>/dev/null
      ;;
  esac
}

# The roots the fingerprint scan covers, one `mode<TAB>root` line each. The
# state home is `marker`-mode: the product never writes it, so a token or
# structure rule would red on a dependency's scratch directory (the
# ubuntu-latest failure of run 37023093243 was exactly that), while the run
# marker still catches a test trace there.
scan_roots() {
  printf 'full\t%s\n' "$DATA_ROOT"
  printf 'full\t%s\n' "$CONFIG_ROOT"
  printf 'full\t%s\n' "$CACHE_ROOT"
  printf 'marker\t%s\n' "$STATE_HOME"
}

# Snapshot the roots named in `$2` (one path per line) into `$1`, one stat line
# per entry, keyed by path so a changed root is a readable diff. An absent root
# is recorded as `ABSENT <path>` so a run that creates it is a diff.
snapshot_all() {
  # $1 = output file, $2 = roots file.
  : >"$1"
  while IFS= read -r root; do
    [ -n "$root" ] || continue
    if [ ! -e "$root" ]; then
      printf 'ABSENT %s\n' "$root" >>"$1"
      continue
    fi
    if ! find "$root" -print0 2>"$SNAP_DIR/find.err" |
      LC_ALL=C sort -z >"$SNAP_DIR/names.raw"; then
      echo "$TAG refusing: could not list $root" >&2
      cat "$SNAP_DIR/find.err" >&2
      exit 1
    fi
    while IFS= read -r -d '' entry; do
      # inode, mtime, size — a rename or an in-place rewrite is a diff even
      # when the name set is unchanged.
      stat -c '%i %Y %s' "$entry" 2>/dev/null ||
        stat -f '%i %m %z' "$entry" 2>/dev/null ||
        printf 'stat-unavailable'
      printf ' %s\n' "$entry"
    done <"$SNAP_DIR/names.raw" >>"$1"
  done <"$2"
  LC_ALL=C sort -o "$1" "$1"
}

# Names only, one per line — the cheap snapshot the fingerprint scan diffs to
# find a fixture name the run deleted.
snapshot_names() {
  # $1 = output file, $2 = roots file.
  local out="$1"
  : >"$out"
  while IFS= read -r root; do
    [ -n "$root" ] || continue
    [ -e "$root" ] || continue
    if ! find "$root" 2>"$SNAP_DIR/find.err" | LC_ALL=C sort >"$SNAP_DIR/names.lines"; then
      echo "$TAG refusing: could not list $root" >&2
      cat "$SNAP_DIR/find.err" >&2
      exit 1
    fi
    cat "$SNAP_DIR/names.lines" >>"$out"
  done <"$2"
  LC_ALL=C sort -o "$out" "$out"
}

lower() { printf '%s' "$1" | LC_ALL=C tr '[:upper:]' '[:lower:]'; }

# Does the basename carry a reserved fixture token as a whole token?
name_is_fixture() {
  local lowered token
  lowered="$(lower "$1")"
  for token in $(printf '%s' "$lowered" | LC_ALL=C tr -c 'a-z0-9' ' '); do
    case " $FIXTURE_TOKENS " in
      *" $token "*) return 0 ;;
    esac
  done
  return 1
}

# The run marker, generated once and exported so every test process (and the
# `Sandbox` fixture it builds) can stamp it. A random token cannot occur in a
# user's real data by accident, which is what makes it a safe content signal.
MARKER=$(od -An -N16 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')
[ -n "$MARKER" ] || refuse "could not generate the per-run test marker (/dev/urandom)"
export CHAT_STASHER_TEST_ISOLATION_MARKER="$MARKER"

name_has_marker() {
  case "$1" in *"$MARKER"*) return 0 ;; esac
  return 1
}

# A small regular file whose content carries the marker.
file_has_marker() {
  local size
  [ -f "$1" ] || return 1
  size=$(wc -c <"$1" 2>/dev/null) || return 1
  [ "$size" -le "$CONTENT_SCAN_CAP" ] || return 1
  LC_ALL=C grep -qF -- "$MARKER" "$1" 2>/dev/null
}

# The reserved fixture tokens a file's bytes carry, one per line, lowercased
# and sorted unique. The search is a case-insensitive *substring* match (`-a`
# so a store is read as bytes, never skipped as binary): a SQLite record lays
# its columns down with no separator, so the incident's row is
# `chatgptsynthetic-install` and a whole-token boundary rule cannot see the
# `synthetic` glued to the `chatgpt` before it. The *token*, not the enclosing
# word, is the unit, because the word is not stable: any alnum byte the store
# glues to the end of the value (`...install` + the next column) changes it, so
# a baseline keyed on the word would miss the same row on the next run.
# Files past the cap are skipped by design, not here. Confined by the caller to
# `<data root>/state`, so an archived conversation — arbitrary user text that
# can contain the word "probe" — is never read for tokens.
file_fixture_tokens() {
  local path="$1" size token
  [ -f "$path" ] || return 0
  size=$(wc -c <"$path" 2>/dev/null) || return 0
  [ "$size" -le "$CONTENT_SCAN_CAP" ] || return 0
  for token in $FIXTURE_TOKENS; do
    if LC_ALL=C grep -qaiF -- "$token" "$path" 2>/dev/null; then
      printf '%s\n' "$token"
    fi
  done
}

# ---- structure snapshot -----------------------------------------------------

collect_structure_roots | LC_ALL=C sort -u >"$SNAP_DIR/structure.roots"
snapshot_all "$SNAP_DIR/structure.before" "$SNAP_DIR/structure.roots"

# The scan-root names before the run, so a fixture name deleted during it is a
# diff. Names only: a stat of the whole archive is not needed to notice a
# removal. The state home is excluded — a shared directory's entries vanish
# under unrelated writers.
scan_roots | cut -d'	' -f2- | grep -v -x -F "$STATE_HOME" |
  LC_ALL=C sort -u >"$SNAP_DIR/scan.roots"
snapshot_names "$SNAP_DIR/scan.before" "$SNAP_DIR/scan.roots"

# The per-run baseline, so debris an earlier leak left on this machine does not
# red every run (see the header): the fixture-named paths that already exist
# under the watched roots, and the fixture tokens that already sit in the state
# store. Both are subtracted from the verdict, so only a name or a token this
# run introduces can be a leak.
# The name rule, applied in one pass rather than one `basename` per entry: the
# real archive is tens of thousands of entries, so a fork per name costs
# seconds of overhead on every guarded run — and the guard runs on every
# `cargo test`, in CONTRIBUTING's line and in CI's Test step alike. The token
# list is passed in from the shell copy so it cannot drift from
# `name_is_fixture` — and the selftest pins that copy against
# `test_identity_guard.rs`.
: >"$SNAP_DIR/baseline.fixture_paths"
LC_ALL=C awk -v list="$FIXTURE_TOKENS" '
  BEGIN { split(list, T, " "); for (i in T) token[T[i]] = 1 }
  {
    name = $0
    sub(/.*\//, "", name)          # basename, as `name_is_fixture` reads it
    name = tolower(name)
    gsub(/[^a-z0-9]/, " ", name)   # split into whole alphanumeric tokens
    count = split(name, parts, " ")
    for (i = 1; i <= count; i++)
      if (parts[i] in token) { print $0; next }
  }
' "$SNAP_DIR/scan.before" >"$SNAP_DIR/baseline.fixture_paths"

: >"$SNAP_DIR/baseline.identities"
if [ -e "$STATE_STORE_ROOT" ]; then
  if ! find "$STATE_STORE_ROOT" -type f -print0 2>"$SNAP_DIR/find.err" |
    LC_ALL=C sort -z >"$SNAP_DIR/state.raw"; then
    echo "$TAG refusing: could not list the state store at $STATE_STORE_ROOT" >&2
    cat "$SNAP_DIR/find.err" >&2
    exit 1
  fi
  while IFS= read -r -d '' stored; do
    file_fixture_tokens "$stored" >>"$SNAP_DIR/baseline.identities"
  done <"$SNAP_DIR/state.raw"
  LC_ALL=C sort -u -o "$SNAP_DIR/baseline.identities" "$SNAP_DIR/baseline.identities"
fi

# The run boundary: everything the run does happens with a timestamp after
# this, so `find -newer` catches a create-then-delete whose entry names cancel
# out, and bounds the fingerprint scan to what the run actually touched.
BOUNDARY="$SNAP_DIR/boundary"
: >"$BOUNDARY"

started=$(date +%s)
"${COMMAND[@]}"
cmd_status=$?
elapsed=$(( $(date +%s) - started ))

failed=0

# ---- structure diff: any change is a leak ----------------------------------

collect_structure_roots | LC_ALL=C sort -u >"$SNAP_DIR/structure.roots.after"
snapshot_all "$SNAP_DIR/structure.after" "$SNAP_DIR/structure.roots.after"
if ! diff -q "$SNAP_DIR/structure.before" "$SNAP_DIR/structure.after" >/dev/null; then
  failed=1
  echo "$TAG FAIL: this run changed a real inbox or native-messaging manifest" \
    "directory" >&2
  diff "$SNAP_DIR/structure.before" "$SNAP_DIR/structure.after" 2>&1 | head -40 || true
fi

# Unchanged entry set, but a structure directory was touched during the run: a
# create-then-delete. Reported only when the snapshot diff was clean, so one
# change is never announced twice.
if [ "$failed" -eq 0 ]; then
  while IFS= read -r root; do
    [ -n "$root" ] || continue
    [ -e "$root" ] || continue
    if [ -n "$(find "$root" -newer "$BOUNDARY" -print -quit 2>/dev/null)" ]; then
      echo "$TAG FAIL: this run modified $root during the run (an entry is newer" \
        "than the run boundary) while leaving the entry set unchanged —" \
        "a create-then-delete the snapshot diff cannot see" >&2
      failed=1
    fi
  done <"$SNAP_DIR/structure.roots.after"
fi

# ---- fingerprint scan: only what carries a test identity -------------------

report_leak() {
  echo "$TAG FAIL: $1" >&2
  failed=1
}

# A file under the state store whose bytes carry a fixture identity this run
# introduced. The baseline's identities are subtracted, so the live product
# rewriting the file that holds an earlier leak's row — or a SQLite journal for
# it — is not a leak by itself.
report_new_state_tokens() { # $1 path
  local path="$1" identity
  case "$path" in "$STATE_STORE_ROOT"/*) ;; *) return 0 ;; esac
  [ -f "$path" ] || return 0
  while IFS= read -r identity; do
    [ -n "$identity" ] || continue
    grep -qxF -- "$identity" "$SNAP_DIR/baseline.identities" ||
      report_leak "a file carrying a fixture identity this run introduced" \
        "($identity) was written to $path"
  done < <(file_fixture_tokens "$path")
}

# Is this path *under* a fixture-named path that already existed before the
# run? The pre-existing directory is residue the baseline subtracts, but an
# entry the run adds inside it is not residue, and the name rule alone would
# miss it (the directory's own mtime touch is baselined away, and the new
# entry's own name carries no token).
under_baseline_fixture_path() { # $1 path
  local path="$1" prefix
  while IFS= read -r prefix; do
    [ -n "$prefix" ] || continue
    case "$path" in "$prefix"/*) return 0 ;; esac
  done <"$SNAP_DIR/baseline.fixture_paths"
  return 1
}

# A fixture name that was there before the run and is gone now: a test deleted
# from the real archive (or the live product removed a file whose name happens
# to carry a token, which is a leak-shaped name either way).
snapshot_names "$SNAP_DIR/scan.after.names" "$SNAP_DIR/scan.roots"
while IFS= read -r gone; do
  [ -n "$gone" ] || continue
  name_is_fixture "$(basename "$gone")" && report_leak \
    "a fixture-named entry disappeared during the run: $gone"
done < <(LC_ALL=C comm -23 "$SNAP_DIR/scan.before" "$SNAP_DIR/scan.after.names")

while IFS='	' read -r mode root; do
  [ -n "$root" ] || continue
  [ -e "$root" ] || continue
  # `-newer` bounds the scan to entries this run created or modified: a quiet
  # run (everything sandboxed) touches nothing here and reads nothing. A walk
  # that fails is a refusal, not an empty set: a guard that cannot see is red.
  if ! find "$root" -newer "$BOUNDARY" -print0 2>"$SNAP_DIR/find.err" |
    LC_ALL=C sort -z >"$SNAP_DIR/touched.raw"; then
    echo "$TAG refusing: could not list the entries the run touched under $root" >&2
    cat "$SNAP_DIR/find.err" >&2
    exit 1
  fi
  while IFS= read -r -d '' entry; do
    base="$(basename "$entry")"
    if [ "$mode" = "full" ] && name_is_fixture "$base" &&
      ! grep -qxF -- "$entry" "$SNAP_DIR/baseline.fixture_paths"; then
      report_leak "a fixture-named entry appeared at $entry during the run"
      continue
    fi
    # An entry the run added inside a fixture-named directory that was already
    # there before the run: the directory is baseline residue, the new entry is
    # not. Skipped for an entry that existed before the run, so the incident's
    # own shards stay residue while a new one is a leak.
    if [ "$mode" = "full" ] &&
      ! grep -qxF -- "$entry" "$SNAP_DIR/scan.before" &&
      under_baseline_fixture_path "$entry"; then
      report_leak "a new entry appeared under fixture-named residue at $entry"
      continue
    fi
    if name_has_marker "$base"; then
      report_leak "a run-marked entry appeared at $entry during the run"
      continue
    fi
    if file_has_marker "$entry"; then
      report_leak "a file carrying this run's marker was written to $entry"
      continue
    fi
    if [ "$mode" = "full" ]; then
      report_new_state_tokens "$entry"
    fi
  done <"$SNAP_DIR/touched.raw"
done < <(scan_roots)

if [ "$failed" -ne 0 ]; then
  echo "$TAG FAIL: exit code of the wrapped command was $cmd_status — the guard" \
    "fails on the change regardless. A test resolved a real user-data location;" \
    "give it a per-test temp root through tests' shared fixture (W306)." >&2
  exit 1
fi

if [ "$cmd_status" -ne 0 ]; then
  echo "$TAG FAIL: wrapped command exited $cmd_status (real user data untouched)"
  exit 1
fi

echo "$TAG PASS: ${COMMAND[*]} left the real data, config, cache, inbox and" \
  "native-messaging manifest directories free of test fingerprints" \
  "(runtime=${elapsed}s)"
