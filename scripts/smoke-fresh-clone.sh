#!/usr/bin/env bash
# M1 · A1/A2 —— 一个陌生人 clone 下来能不能跑起来。
#
# 为什么要脚本而不是「我照着 README 走了一遍」：后者不可复算。这个脚本每一步都
# 打印退出码，任何人任何时候重跑都能得到同一份判据。
#
# 🔴 它 clone 的是【远端】，不是本地工作区 —— 本地有很多 gitignored 的东西
# （`.output/`、`target/`、`.private/`），只有从远端 clone 才知道陌生人拿到的是什么。
#
# 用法：bash scripts/smoke-fresh-clone.sh [--with-extension]
#   --with-extension  也跑 A2（pnpm install + build，慢很多）
set -u
REMOTE="${SMOKE_REMOTE:-https://github.com/dimpurr/chat-stasher.git}"
WORK="$(mktemp -d)"
WITH_EXT=0
[ "${1:-}" = "--with-extension" ] && WITH_EXT=1
FAILED=0

step() { printf '\n[smoke] %s\n' "$1"; }
check() { # check <label> <expected-rc> <actual-rc>
  if [ "$2" = "$3" ]; then printf '[smoke]   PASS · %s (rc=%s)\n' "$1" "$3"
  else printf '[smoke]   FAIL · %s (rc=%s, want %s)\n' "$1" "$3" "$2"; FAILED=1; fi
}
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

step "clone a fresh copy of $REMOTE"
git clone -q "$REMOTE" "$WORK/repo"; check "git clone" 0 "$?"
[ "$FAILED" = 1 ] && { echo "[smoke] SMOKE: FAIL"; exit 1; }

cd "$WORK/repo" || exit 1

step "A1 · cargo build (a stranger has no target/ cache)"
cargo build -q >"$WORK/build.log" 2>&1; check "cargo build" 0 "$?"

step "A1 · the binary answers --help"
./target/debug/chat-stasher --help >/dev/null 2>&1; check "chat-stasher --help" 0 "$?"

# `doctor` is read-only, but point HOME at an empty dir anyway: this asserts the
# command *runs on a machine with nothing on it*, which is the stranger's case,
# and it keeps the smoke test from reading the operator's real sessions.
step "A1 · doctor runs on an empty HOME (the stranger's first machine)"
EMPTY_HOME="$WORK/empty-home"; mkdir -p "$EMPTY_HOME"
env HOME="$EMPTY_HOME" XDG_DATA_HOME="$EMPTY_HOME/.local/share" \
    XDG_CONFIG_HOME="$EMPTY_HOME/.config" XDG_STATE_HOME="$EMPTY_HOME/.local/state" \
    ./target/debug/chat-stasher doctor >/dev/null 2>&1
check "chat-stasher doctor (empty HOME)" 0 "$?"

if [ "$WITH_EXT" = 1 ]; then
  step "A2 · the extension builds from a fresh clone (.output/ is gitignored)"
  ( cd apps/extension && pnpm install --frozen-lockfile >"$WORK/pnpm-i.log" 2>&1 ); check "pnpm install" 0 "$?"
  ( cd apps/extension && pnpm build >"$WORK/pnpm-b.log" 2>&1 ); check "pnpm build" 0 "$?"
  for f in manifest.json background.js popup.html; do
    [ -f "apps/extension/.output/chrome-mv3/$f" ]; check "built artifact $f exists" 0 "$?"
  done
fi

if [ "$FAILED" = 0 ]; then echo; echo "[smoke] SMOKE: PASS"; exit 0
else echo; echo "[smoke] SMOKE: FAIL"; exit 1; fi
