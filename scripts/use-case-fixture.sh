#!/usr/bin/env bash
#
# use-case-fixture.sh — build the synthetic archive the use-case pages quote.
#
# The two pages under docs/use-cases/ paste real command output. "Real" means
# run against an archive, so this script builds one, shaped like the situation
# the pages describe: a machine that is gone, whose conversations survive only
# in a destination, next to a second machine that is still here.
#
# Everything in it is invented. The machine partition is an obvious placeholder,
# the session ids are fabricated, the conversation text is one sentence about a
# fictional greenhouse sensor, and the search token (`kettleloop`) is a made-up
# word. No real conversation, account, hostname or path appears anywhere.
#
# The archive it builds is deliberately imperfect, because a fixture that only
# shows the happy path makes the pages lie about the tool:
#
#   * one session's body is reclaimed from the stage before the second push, so
#     it lives only in an OLDER snapshot — the shape that used to make a whole
#     machine's history invisible to `search` and `read --session`;
#   * one session carries no timestamp at all, so its conversation time is
#     genuinely unknown and a date query must report it rather than drop it.
#
# Usage:
#   bash scripts/use-case-fixture.sh [--binary PATH] [--work DIR]
#
#   --binary PATH   use this binary instead of target/debug/chat-stasher
#   --work DIR      build under DIR instead of a fresh temp directory. The
#                   pages quote command output, so a fixed path is what makes
#                   the quoted lines match what a reader gets.
#
# What it is NOT: it talks to no network, opens no socket, reads no real tool's
# session files and writes nothing outside its own temp directory. It does not
# make a repository that any page command writes to: `push` builds it, and
# everything the pages run afterwards is read-only.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$ROOT/target/debug/chat-stasher"
WORK_OVERRIDE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BINARY="$2"; shift 2 ;;
    --work) WORK_OVERRIDE="$2"; shift 2 ;;
    -h|--help) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "use-case-fixture: unknown argument $1" >&2; exit 2 ;;
  esac
done

if [ ! -x "$BINARY" ]; then
  echo "use-case-fixture: no binary at $BINARY (build it, or pass --binary)" >&2
  exit 2
fi

# The two machines. `attic-laptop` is the one that is gone, so its partition is
# a generator-shaped id and the name it is known by has to be written down by
# the machine that is still here.
LOST="0123456789abcdef0123456789abcdef"
HERE="desk-mini"

if [ -n "$WORK_OVERRIDE" ]; then
  # `--work` deletes the directory it is given, so it refuses anything that is
  # not a path someone obviously made for this: a bare `/`, a relative name, or
  # a direct child of the filesystem root.
  case "$WORK_OVERRIDE" in
    /*) ;;
    *) echo "use-case-fixture: --work must be an absolute path" >&2; exit 2 ;;
  esac
  if [ "$WORK_OVERRIDE" = "/" ] || [ "$(dirname "$WORK_OVERRIDE")" = "/" ]; then
    echo "use-case-fixture: refusing --work $WORK_OVERRIDE" >&2
    exit 2
  fi
  WORK="$WORK_OVERRIDE"
  rm -rf "$WORK"
else
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/chat-stasher-use-cases.XXXXXX")"
fi
HOME_DIR="$WORK/home"
STAGE="$WORK/stage"
DESK_STAGE="$WORK/desk-stage"
REPO="$WORK/offsite"
KEYS="$WORK/keys"

mkdir -p "$HOME_DIR" "$STAGE" "$DESK_STAGE" "$REPO" "$KEYS" \
         "$WORK/config/chat-stasher"

cat > "$WORK/config/chat-stasher/config.toml" <<EOF
# Written by scripts/use-case-fixture.sh. Synthetic: no real machine, account
# or path appears in this file.
machine = "$LOST"

[destinations.offsite]
repo = "$REPO"
key_file = "$KEYS/masterkey.json"
EOF

run() {
  HOME="$HOME_DIR" \
  USERPROFILE="$HOME_DIR" \
  XDG_CONFIG_HOME="$WORK/config" \
  XDG_DATA_HOME="$WORK/data" \
  XDG_STATE_HOME="$WORK/state" \
  XDG_CACHE_HOME="$WORK/cache" \
  "$BINARY" "$@"
}

# One claude-code transcript line, in the shape that tool writes.
line() {
  local ts="$1" text="$2"
  printf '{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user",'\
'"message":{"role":"user","content":"%s"},"uuid":"u-%s","timestamp":"%s","cwd":"/x","version":"1.0.31"}\n' \
    "$text" "$ts" "$ts"
}

# One line with no timestamp field at all, which is what makes a session's
# conversation time unknown rather than zero.
undated_line() {
  printf '{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user",'\
'"message":{"role":"user","content":"%s"},"uuid":"u-undated","cwd":"/x","version":"1.0.31"}\n' "$1"
}

# <stage> <machine> <session-dir> then the lines on stdin.
plant() {
  local stage="$1" machine="$2" session="$3"
  local dir="$stage/sessions/$machine/$session/000"
  mkdir -p "$dir"
  cat > "$dir/000001.jsonl"
}

A="claude-code.$LOST.aaaa0001-1111-4111-8111-111111111111"
B="claude-code.$LOST.aaaa0002-2222-4222-8222-222222222222"
C="claude-code.$LOST.aaaa0003-3333-4333-8333-333333333333"
D="claude-code.$LOST.aaaa0004-4444-4444-8444-444444444444"
E="claude-code.$HERE.bbbb0001-1111-4111-8111-111111111111"

# A: the session whose body is reclaimed before the second push. Only the token
# `kettleloop` appears here, so a text search for it has exactly one answer.
{
  line "2026-03-02T08:00:00Z" "the kettleloop rig reads the greenhouse sensor every minute"
  line "2026-03-02T09:30:00Z" "kettleloop again: the sensor drifts when the door is open"
} | plant "$STAGE" "$LOST" "$A"

# B: an ordinary session the machine archived before it went away.
line "2026-03-05T14:20:00Z" "notes on the greenhouse sensor calibration" \
  | plant "$STAGE" "$LOST" "$B"

# C: a session the tool can read and cannot date. Its conversation is real; its
# time is not derivable from anything the archive holds.
undated_line "the rig log has no timestamp on this entry" \
  | plant "$STAGE" "$LOST" "$C"

# The per-session activity index the archive carries beside the shards. A real
# machine gets it from `collect`; here the stage is the source, so it is built
# from the stage.
run activity-index --stage "$STAGE" > /dev/null
run push --stage "$STAGE" --destination offsite > /dev/null

# The field shape: the destination proved it holds A's bytes, so the tool
# deleted its staged copy. A now lives only in the snapshot pushed above, and
# the next push — which carries a newer session — leaves its body there.
run reclaim-stage --stage "$STAGE" --apply > /dev/null

# D: the session the machine archived between the reclaim and the moment it
# went away. Its push is what makes a second snapshot.
line "2026-03-09T11:05:00Z" "closing notes on the greenhouse sensor rig" \
  | plant "$STAGE" "$LOST" "$D"
run activity-index --stage "$STAGE" > /dev/null
run push --stage "$STAGE" --destination offsite > /dev/null

# The surviving machine names the one that is gone. A label is written into a
# stage and travels to the destination on a later push, which is why this needs
# a machine that is still here.
cat > "$WORK/config/chat-stasher/config.toml" <<EOF
machine = "$HERE"

[destinations.offsite]
repo = "$REPO"
key_file = "$KEYS/masterkey.json"
EOF

run machine-label --stage "$DESK_STAGE" --target "$LOST" --label "attic-laptop" > /dev/null

# This machine's own work, so its push has something of its own to carry.
line "2026-03-09T16:40:00Z" "sensor schematic review before the rig moves" \
  | plant "$DESK_STAGE" "$HERE" "$E"
run activity-index --stage "$DESK_STAGE" > /dev/null
run push --stage "$DESK_STAGE" --destination offsite > /dev/null

cat <<EOF
use-case-fixture: built

  work tree     $WORK
  destination   offsite  (repo $REPO)
  lost machine  $LOST   labelled attic-laptop
  this machine  $HERE

Run a page's commands with the environment above, for example:

  HOME=$HOME_DIR XDG_CONFIG_HOME=$WORK/config XDG_DATA_HOME=$WORK/data \\
  XDG_STATE_HOME=$WORK/state XDG_CACHE_HOME=$WORK/cache \\
  $BINARY overview --destination offsite

  rm -rf $WORK   # when you are done
EOF
