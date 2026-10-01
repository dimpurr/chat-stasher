#!/usr/bin/env bash
# Report — and with --apply, delete — the test leftovers in this machine's
# real rustic cache directory (W289).
#
#     bash scripts/dev/prune-test-rustic-cache.sh             # dry run: report only
#     bash scripts/dev/prune-test-rustic-cache.sh --apply     # delete exactly the reported set
#
# What this is about. Every repository open with default options makes rustic
# create `<cache root>/rustic/<repository id>/…` on the machine that ran the
# open. The test suite opened thousands of throwaway repositories before the
# caches were isolated (W289): every `cargo test` run left another handful of
# per-repository directories in ~/Library/Caches/rustic (Linux:
# ~/.cache/rustic), each holding the metadata packs of a repository that no
# longer exists. On one machine that accumulated 36,000+ directories, which
# is what the cache-walking tests then had to sweep and what this script
# exists to reclaim.
#
# Safety rule, in one sentence: a directory is removable only when it is
# demonstrably NOT the cache of a repository the user's real chat-stasher
# config still declares, and when anything about that claim cannot be
# verified the script refuses rather than guessing.
#
# Concretely, the keep-set is built from the config itself — the local
# single-destination repository and every `[destinations.<name>]` entry —
# using `cargo run -p chat-stasher --example repo-config-id`: that helper
# opens each declared repository READ-ONLY (`no_cache` is forced on so even
# the cache is not written) and prints the repository id its config file
# carries, which is exactly the name of its cache directory. This is
# deliberate where a shortcut would be fatal: the id cannot be read out of a
# repository's config file by a text tool (rustic stores it encrypted), so
# anything cheaper than opening the repository would be a guess wearing the
# costume of a fact.
#
# Three ways this fails closed:
#
#   * the config cannot be loaded, or the id helper could not be built →
#     no keep-set → nothing is deletable, and --apply is refused;
#   * a declared repository could not be opened (remote backend unreachable
#     in this shell, a missing credential) → its id is unknown → it cannot be
#     proved not to own one of the listed directories → --apply is refused
#     unless `--allow-unresolved` is passed too, which shifts the decision to
#     a human who has just read which repositories were unreadable;
#   * every deletion is by exact 64-hex directory name directly under the
#     cache root — never a glob, never a pattern that could match a path
#     outside it, never anything that is not a directory (CACHEDIR.TAG at
#     the root, stray files: reported, never deleted).
#
# What is NOT protected, stated here rather than discovered later: a real
# repository that used to be configured and no longer is; a repository some
# OTHER tool on this machine opens (any other rustic/restic user shares this
# root — those directories look exactly like test leftovers). Review the
# dry-run list before --apply; the default is the dry run precisely so that
# the list is the thing that gets reviewed.
#
# Dry-run only report first appeared in nm/W289-OUT.md; nothing in
# chat-stasher's archive is ever touched by this script — the metadata cache
# is disposable by design (deleting it costs a re-download of metadata, never
# any data), and no repository is ever opened for writing.

set -uo pipefail

TAG="[prune-test-rustic-cache]"

usage() {
  echo "Usage: bash scripts/dev/prune-test-rustic-cache.sh [--apply] [--allow-unresolved]" >&2
  echo "Reports the rustic cache directories that belong to no repository the" >&2
  echo "user's real chat-stasher config declares. --apply deletes exactly those;" >&2
  echo "the default is a dry run that deletes nothing. --allow-unresolved lets" >&2
  echo "--apply proceed even when a declared repository could not be opened (its" >&2
  echo "cache cannot be identified then, so its cache directories would be pruned" >&2
  echo "too and rebuilt from that repository on its next open)." >&2
  exit 2
}

APPLY=0
ALLOW_UNRESOLVED=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1 ;;
    --allow-unresolved) ALLOW_UNRESOLVED=1 ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
  shift
done

case "$(uname -s)" in
  Darwin*) ;;
  Linux*) ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "$TAG refusing: on Windows the cache root comes from the Known Folder" \
      "API and this script cannot resolve or safely enumerate it. See the header" \
      "of scripts/dev/check-test-cache-isolation.sh for the same gap on the guard." >&2
    exit 1
    ;;
  *)
    echo "$TAG refusing: unsupported platform $(uname -s)" >&2
    exit 1
    ;;
esac

if [ -z "${HOME:-}" ]; then
  echo "$TAG refusing: no \$HOME, so neither cache root nor the config can be named" >&2
  exit 1
fi

# Same cache-root spelling the product walks (user_cache_dirs_on): Linux
# honours $XDG_CACHE_HOME when absolute, macOS is $HOME/Library/Caches.
case "$(uname -s)" in
  Darwin*) ROOT=$HOME/Library/Caches ;;
  *)
    if [ -n "${XDG_CACHE_HOME:-}" ] && [ "${XDG_CACHE_HOME#\/}" != "$XDG_CACHE_HOME" ]; then
      ROOT=$XDG_CACHE_HOME
    else
      ROOT=$HOME/.cache
    fi
    ;;
esac
WATCH="$ROOT/rustic"

if [ ! -d "$WATCH" ]; then
  echo "$TAG nothing to do: no rustic cache directory at $WATCH"
  exit 0
fi

# Locate the checkout that contains this script, for the manifest passed to
# cargo. Running from a worktree or the main checkout both work; running a
# copied-out script does not, and using $0 keeps it working no matter the cwd.
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd) || {
  echo "$TAG refusing: could not resolve this script's directory" >&2
  exit 1
}
MANIFEST="$SCRIPT_DIR/../../Cargo.toml"
if [ ! -f "$MANIFEST" ]; then
  echo "$TAG refusing: no Cargo.toml at $MANIFEST — this script must run from a checkout" >&2
  exit 1
fi

# The id helper must read *the user's real config*, so XDG_CONFIG_HOME is
# explicitly dropped: a dev shell that points it at a sandbox would silently
# prune against an empty keep-set. Bounded wait: a remote backend that hangs
# instead of failing must not turn a cleanup into an overnight job.
ID_TIMEOUT_SECS=180
REPORT=$(mktemp "${TMPDIR:-/tmp}/cs-prune-ids.XXXXXX") || exit 1
CLEANUP_FILES=("$REPORT")
cleanup() { rm -f "${CLEANUP_FILES[@]}"; }
trap cleanup EXIT

echo "$TAG deriving the repository ids of every repository in the real config" \
  "(read-only opens; a remote backend may take a moment)…"
(
  env -u XDG_CONFIG_HOME cargo run --quiet --manifest-path "$MANIFEST" \
    --example repo-config-id >"$REPORT" 2>&1 &
  runner=$!
  waited=0
  while kill -0 "$runner" 2>/dev/null; do
    if [ "$waited" -ge "$ID_TIMEOUT_SECS" ]; then
      echo "$TAG refusing: the id helper did not finish within ${ID_TIMEOUT_SECS}s" \
        "(a configured remote backend did not answer). Nothing was deleted." >&2
      kill "$runner" 2>/dev/null
      exit 1
    fi
    sleep 2
    waited=$((waited + 2))
  done
  wait "$runner" && exit 0 || exit 1
)

helper_status=$?
if [ "$helper_status" -ne 0 ] || ! grep -q '^end$' "$REPORT"; then
  echo "$TAG FAIL: the repository-id helper produced no report — there is no keep-set," \
    "so no directory can be proved removable. Nothing was deleted. Helper output:" >&2
  sed "s/^/$TAG   /" "$REPORT" >&2
  exit 1
fi

names_raw=$(mktemp "${TMPDIR:-/tmp}/cs-prune-names.XXXXXX") || exit 1
CLEANUP_FILES+=("$names_raw")
if ! find "$WATCH" -mindepth 1 -maxdepth 1 >"$names_raw"; then
  echo "$TAG FAIL: could not list $WATCH" >&2
  exit 1
fi

# Keep-set and problems, from the helper's lines. `absent` (declared but
# never initialised) owns no cache directory by construction; `error` is the
# opposite: an id that could not be established, so the keep-set has a hole.
keep=$(mktemp "${TMPDIR:-/tmp}/cs-prune-keep.XXXXXX") || exit 1
CLEANUP_FILES+=("$keep")
grep -E '^id[[:space:]]' "$REPORT" | awk '{print $3}' >"$keep"
# `error` lines are the hole in the keep-set; `absent` lines (declared but
# never initialised) own no cache directory by construction, so they are fine
# to fold into the summary only — but both quotes of the helper output above
# show them, which is why neither is a variable here.
unresolved=$(grep -cE '^error[[:space:]]' "$REPORT" || true)
keep_count=$(grep -c . "$keep" || true)

echo
echo "=== configured repositories (the keep-set) ==="
sed "s/^/$TAG   /" "$REPORT"
if [ "$keep_count" -eq 0 ]; then
  echo "$TAG refusing: the config declares no repository whose id could be derived," \
    "so nothing can be safely classed as a test leftover. Nothing was deleted." >&2
  exit 1
fi

# Classify. A cache entry is removable only when it is a directory whose name
# is exactly 64 lowercase hex characters (RepositoryId::to_hex) and is not in
# the keep-set. Anything else — CACHEDIR.TAG at the root, files, odd spellings
# — is reported as "other" and never touched.
removable=$(mktemp "${TMPDIR:-/tmp}/cs-prune-removable.XXXXXX") || exit 1
CLEANUP_FILES+=("$removable")
other_count=0
while IFS= read -r full; do
  name=${full#"$WATCH"/}
  if [ -d "$full" ] && printf '%s' "$name" | grep -qE '^[0-9a-f]{64}$' &&
    ! grep -qxF "$name" "$keep"; then
    printf '%s\n' "$full" >>"$removable"
  else
    other_count=$((other_count + 1))
  fi
done <"$names_raw"
removable_count=$(grep -c . "$removable" || true)

# Sizes: allocated kilobytes (du -k), summed; 4 du:s in parallel keep a
# 36,000-directory walk from being a single-threaded crawl.
total_kib=0
if [ "$removable_count" -gt 0 ]; then
  cpus=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)
  total_kib=$(xargs -P "$cpus" -n 32 du -sk <"$removable" | awk '{s += $1} END {printf "%d", s}')
fi

human() {
  kib=$1
  if [ "$kib" -ge 1048576 ]; then
    awk -v k="$kib" 'BEGIN {printf "%.1f GiB", k / 1048576}'
  elif [ "$kib" -ge 1024 ]; then
    awk -v k="$kib" 'BEGIN {printf "%.0f MiB", k / 1024}'
  else
    printf '%s KiB' "$kib"
  fi
}

total_entries=$(grep -c . "$names_raw" || true)
if [ "$removable_count" -gt 0 ]; then
  # Dialect picked once, not probed per file: a 36,000-entry count is exactly
  # the workload where per-file probing turns a report into a benchmark.
  if stat -f '%m' . >/dev/null 2>&1; then
    EPOCH=(stat -f '%m')
  else
    EPOCH=(stat -c '%Y')
  fi
  newest_epoch=$(xargs "${EPOCH[@]}" <"$removable" 2>/dev/null | sort -rn | head -1 || true)
  oldest_epoch=$(xargs "${EPOCH[@]}" <"$removable" 2>/dev/null | sort -n | head -1 || true)
  if date -r 0 +%F >/dev/null 2>&1; then
    DATE_BSD=1
  else
    DATE_BSD=0
  fi
  if [ -n "${newest_epoch:-}" ]; then
    if [ "$DATE_BSD" -eq 1 ]; then
      newest=$(date -r "$newest_epoch" +%Y-%m-%d || true)
      oldest=$(date -r "$oldest_epoch" +%Y-%m-%d || true)
    else
      newest=$(date -d "@$newest_epoch" +%Y-%m-%d || true)
      oldest=$(date -d "@$oldest_epoch" +%Y-%m-%d || true)
    fi
  fi
fi

echo
echo "=== summary (dry run: nothing is deleted) ==="
echo "cache root:              $WATCH"
echo "entries under the root: $total_entries — $removable_count removable, $other_count other (never deleted: root files, non-hex names); $keep_count configured ids are kept, whichever of them currently have a directory under the root"
printf 'disk taken by removable: %s\n' "$(human "$total_kib")"
[ "${newest:-}" ] && echo "removable newest mtime:  $newest (the last test-run that wrote a real cache)"
[ "${oldest:-}" ] && echo "removable oldest mtime:  $oldest (the oldest surviving test leftover)"
echo
echo "kept ids (${keep_count}):"
sed 's/^/  /' "$keep"
echo
echo "removable: the $removable_count per-repository directories listed " \
  "below belong to no repository in the config."
if [ "$removable_count" -gt 0 ]; then
  echo "first 20 names:"
  head -20 "$removable" | sed 's/^/  /'
  [ "$removable_count" -gt 20 ] && echo "  … ($(( removable_count - 20 )) more)"
fi

if [ "$ALLOW_UNRESOLVED" -eq 0 ] && [ "$unresolved" -gt 0 ]; then
  echo
  echo "$TAG --apply REFUSED: $unresolved configured repositories could not be" \
    "opened, so their cache directories cannot be identified and could be in the" \
    "removable set. Fix the reason (bring a remote online / export its credential)" \
    "and re-run, or pass --allow-unresolved to accept pruning them too (their" \
    "caches rebuild from the repositories on next open; no data is ever lost)." >&2
  [ "$APPLY" -eq 1 ] && exit 1
fi

if [ "$APPLY" -eq 0 ]; then
  echo
  echo "$TAG dry run complete. Re-run with --apply to delete exactly the" \
    "$removable_count directories listed as removable."
  exit 0
fi

# --apply: one directory at a time, re-verified by name pattern before each
# rm, so a list corrupted mid-flight or an argument-quoting accident can only
# ever hit a 64-hex-named directory directly under the cache root.
deleted=0
failed=0
while IFS= read -r full; do
  name=${full#"$WATCH"/}
  if printf '%s' "$name" | grep -qE '^[0-9a-f]{64}$' && [ -d "$full" ]; then
    if rm -rf -- "$full"; then
      deleted=$(( deleted + 1 ))
    else
      failed=$(( failed + 1 ))
      echo "$TAG could not delete $name" >&2
    fi
  else
    failed=$(( failed + 1 ))
    echo "$TAG refusing to delete non-directory or malformed entry: $name" >&2
  fi
done <"$removable"
echo "$TAG deleted $deleted directories ($(human "$total_kib") reclaimed); $failed failures."
[ "$failed" -gt 0 ] && exit 1
[ "$deleted" -ne "$removable_count" ] && {
  echo "$TAG count mismatch: expected $removable_count deletions" >&2
  exit 1
}
exit 0
