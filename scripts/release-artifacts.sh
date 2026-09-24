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

ARTIFACT_ARM64="chat-stasher-darwin-arm64"
ARTIFACT_X86="chat-stasher-darwin-x86_64"

say "CLI · cargo build --release (macOS arm64 + x86_64)"
( cd "$ROOT" && cargo build --release --target aarch64-apple-darwin -q && cargo build --release --target x86_64-apple-darwin -q )
check "cargo build --release for both macOS targets" 0 "$?"
cp "$ROOT/target/aarch64-apple-darwin/release/chat-stasher" "$OUT/$ARTIFACT_ARM64" 2>/dev/null
check "copy $ARTIFACT_ARM64" 0 "$?"
cp "$ROOT/target/x86_64-apple-darwin/release/chat-stasher" "$OUT/$ARTIFACT_X86" 2>/dev/null
check "copy $ARTIFACT_X86" 0 "$?"

EXT_VERSION="$(node -p "require('$ROOT/apps/extension/package.json').version" 2>/dev/null)"
EXT_ASSET="chat-stasher-extension-$EXT_VERSION.zip"
say "extension · pnpm zip (stable channel)"
( cd "$ROOT/apps/extension" && unset CS_RELEASE_CHANNEL && pnpm zip >/dev/null 2>&1 ); check "pnpm zip with stable default" 0 "$?"
ZIP="$(ls -t "$ROOT/apps/extension/.output/"*.zip 2>/dev/null | head -1)"
if [ -n "$ZIP" ]; then cp "$ZIP" "$OUT/$EXT_ASSET"; check "copy $EXT_ASSET" 0 "$?"
else check "extension zip produced" 0 1; fi
if [ -n "$ZIP" ] && [ -s "$OUT/$EXT_ASSET" ]; then
  unzip -p "$OUT/$EXT_ASSET" manifest.json | python3 -c 'import json,sys; m=json.load(sys.stdin); s=json.dumps(m).lower(); bad=("perplexity.ai", "kimi.com"); found=[x for x in bad if x in s]; print("[release]   FAIL · stable manifest contains experimental origin(s): " + ", ".join(found)) if found else print("[release]   PASS · stable manifest excludes experimental origins"); sys.exit(bool(found))'
  check "stable extension manifest has no experimental origins" 0 "$?"
else check "stable extension manifest has no experimental origins" 0 1; fi

# 🔴 这一段才是「敢让陌生人用」的真判据：把二进制搬出仓库、给它一个空 HOME、
# 把 PATH 砍到只剩系统目录（没有 cargo、没有 node、没有这个项目的任何东西），
# 看它还跑不跑得动。在仓库里跑通不算数——那台机器上什么都有。
say "stranger check · run the binary outside the repo, empty HOME, bare PATH"
SBOX="$(mktemp -d)"; mkdir -p "$SBOX/home"
cp "$OUT/$ARTIFACT_ARM64" "$SBOX/cs" 2>/dev/null
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
  unzip -l "$OUT/$EXT_ASSET" 2>/dev/null | grep -q "$f"; check "zip contains $f" 0 "$?"
done

say "checksums"
( cd "$OUT" && shasum -a 256 "$ARTIFACT_ARM64" "$ARTIFACT_X86" "$EXT_ASSET" > SHA256SUMS 2>/dev/null ); check "SHA256SUMS" 0 "$?"
for art in "$ARTIFACT_ARM64" "$ARTIFACT_X86" "$EXT_ASSET"; do
  EXPECTED_SHA="$(awk -v a="$art" '$2 == a { print $1 }' "$OUT/SHA256SUMS")"
  [ -n "$EXPECTED_SHA" ]; check "SHA256SUMS contains $art" 0 "$?"
done

EXPECTED="$(printf '%s\n' "$ARTIFACT_ARM64" "$ARTIFACT_X86" "$EXT_ASSET" SHA256SUMS | sort)"
ACTUAL="$(ls "$OUT" | sort)"
if [ "$EXPECTED" = "$ACTUAL" ]; then say "asset set OK: exactly two CLI binaries, stable extension zip, and SHA256SUMS"
else say "unexpected asset set"; printf '%s\n' "$ACTUAL"; FAILED=1; fi

echo
if [ "$FAILED" = 0 ]; then ls -la "$OUT"; echo; echo "[release] RELEASE-ARTIFACTS: PASS"; exit 0
else echo "[release] RELEASE-ARTIFACTS: FAIL"; exit 1; fi
