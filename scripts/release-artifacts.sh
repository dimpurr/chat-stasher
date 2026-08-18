#!/usr/bin/env bash
# 产出「陌生人拿了就能用」的两件东西 —— 二进制 + 扩展 zip，各带 sha256。
#
# 🔴 为什么不是「照 README 从源码编译」：那是给开发者的门槛，不是给用户的。
# 一个陌生人应该下载一个文件就能跑，而不是先装 Rust 工具链。
#
# 用法：bash scripts/release-artifacts.sh [outdir]   （默认 dist/）
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
mkdir -p "$OUT" || exit 1
FAILED=0
say() { printf '[release] %s\n' "$1"; }
check() { if [ "$2" = "$3" ]; then printf '[release]   PASS · %s (rc=%s)\n' "$1" "$3"
          else printf '[release]   FAIL · %s (rc=%s, want %s)\n' "$1" "$3" "$2"; FAILED=1; fi; }

say "CLI · cargo build --release"
( cd "$ROOT" && cargo build --release -q ); check "cargo build --release" 0 "$?"

HOST="$(uname -s | tr 'A-Z' 'a-z')-$(uname -m)"
BIN="$OUT/chat-stasher-$HOST"
cp "$ROOT/target/release/chat-stasher" "$BIN" 2>/dev/null; check "copy binary to $BIN" 0 "$?"

say "extension · pnpm zip"
( cd "$ROOT/apps/extension" && pnpm zip >/dev/null 2>&1 ); check "pnpm zip" 0 "$?"
ZIP="$(ls -t "$ROOT/apps/extension/.output/"*.zip 2>/dev/null | head -1)"
if [ -n "$ZIP" ]; then cp "$ZIP" "$OUT/"; check "copy $(basename "$ZIP")" 0 "$?"
else check "extension zip produced" 0 1; fi

# 🔴 这一段才是「敢让陌生人用」的真判据：把二进制搬出仓库、给它一个空 HOME、
# 把 PATH 砍到只剩系统目录（没有 cargo、没有 node、没有这个项目的任何东西），
# 看它还跑不跑得动。在仓库里跑通不算数——那台机器上什么都有。
say "stranger check · run the binary outside the repo, empty HOME, bare PATH"
SBOX="$(mktemp -d)"; mkdir -p "$SBOX/home"
cp "$BIN" "$SBOX/cs" 2>/dev/null
env -i HOME="$SBOX/home" PATH=/usr/bin:/bin \
    XDG_DATA_HOME="$SBOX/home/.local/share" XDG_CONFIG_HOME="$SBOX/home/.config" \
    XDG_STATE_HOME="$SBOX/home/.local/state" "$SBOX/cs" --help >/dev/null 2>&1
check "stranger: --help" 0 "$?"
env -i HOME="$SBOX/home" PATH=/usr/bin:/bin \
    XDG_DATA_HOME="$SBOX/home/.local/share" XDG_CONFIG_HOME="$SBOX/home/.config" \
    XDG_STATE_HOME="$SBOX/home/.local/state" "$SBOX/cs" doctor >/dev/null 2>&1
check "stranger: doctor" 0 "$?"
rm -rf "$SBOX"

say "stranger check · the extension zip carries a loadable package"
for f in manifest.json background.js popup.html; do
  unzip -l "$OUT/$(basename "$ZIP")" 2>/dev/null | grep -q "$f"; check "zip contains $f" 0 "$?"
done

say "checksums"
( cd "$OUT" && shasum -a 256 chat-stasher-* *.zip > SHA256SUMS 2>/dev/null ); check "SHA256SUMS" 0 "$?"

echo
if [ "$FAILED" = 0 ]; then ls -la "$OUT"; echo; echo "[release] RELEASE-ARTIFACTS: PASS"; exit 0
else echo "[release] RELEASE-ARTIFACTS: FAIL"; exit 1; fi
