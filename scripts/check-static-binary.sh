#!/usr/bin/env bash
#
# check-static-binary.sh — decide whether a staged release binary really is a
# static executable for the machine it claims to be for.
#
# It lives here rather than inline in the workflow step for the same reason
# scripts/release-tag-gate.sh does: the rule has one owner, and a here-doc in a
# workflow cannot be driven locally. The workflow calls this script, and
# scripts/selftest-check-static-binary.sh drives the same script against
# recorded tool output — so the thing that is tested is the thing that runs, and
# there is no second copy to drift.
#
# The claim being checked is the one the release makes to a user: "one binary
# covers every distribution", i.e. it needs no libc from the target machine. A
# build that quietly used the runner's glibc would run *on this runner* — which
# has glibc — and fail on the user's machine, the one place nobody would connect
# back to this run.
#
# Four checks, and the reason each one is not the others:
#
#   1. **`file` names the machine.** Both Linux cells run native code, so a
#      build that produced the other architecture's image would otherwise be
#      caught only by an architecture check that ran it.
#   2. **`file` says the file is statically linked.** Two spellings, and they are
#      the same claim: a static PIE is an `ET_DYN` image with no program
#      interpreter, `file` calls that "static-pie linked", and it says so only
#      when the image also has no `DT_NEEDED` entries (`src/readelf.c`:
#      `if (dynamic) { if (pie && need == 0) str = "static-pie"; ... }`). A
#      substring test that knew only the first spelling is what refused a static
#      binary and failed run 36131443686. This check is kept because it names the
#      machine in the same sentence it uses, and because it is the human-readable
#      diagnosis a reader of the log wants first — not because it is the
#      authority. Checks 3 and 4 are the authority, and they are what the next
#      paragraph is about.
#   3. **The ELF headers.** No `PT_INTERP` program header and no `DT_NEEDED`
#      dynamic entry is the definition of static linkage; the presence of a
#      dynamic *section* is not, because a static PIE keeps one for
#      self-relocation. Judging by the header rather than by a sentence is what
#      makes the refusal independent of `file`'s wording, which has changed
#      before (`file` grew the "static-pie" wording in a later release) and can
#      change again without this gate going quiet.
#   4. **It runs.** A binary of the right shape that refuses to start is not a
#      release asset, and this is the cheapest place to find out.
#
# Why there is no `ldd` check. The step this replaces had one, and it was not
# redundant-but-harmless: it was a second instance of the same bug. On a static
# PIE `ldd` exits 0, so it refused the very binary check 2 was relaxed for. The
# chain, from glibc's own source: `ld.so --verify` exits 2 (not 1) for an object
# with a dynamic section and no interpreter (`elf/rtld.c`:
# `_exit (has_interp ? 0 : 2)`), `elf/ldd.bash.in` reads 2 as "the loader
# understands this file" and re-runs it through `try_trace`, that trace run
# prints "\tstatically linked" for a main map with no `DT_NEEDED`
# (`elf/rtld.c`, in `dl_main`) and returns 0, and `ldd` ends `exit $result` with
# `result` still 0. The ET_EXEC image the arm64 cell builds takes the other
# branch — no dynamic section at all means `_exit(1)` and "not a dynamic
# executable" — which is why that cell was green and is not evidence the check
# was sound. `no PT_INTERP and no DT_NEEDED` is strictly stronger than "`ldd`
# cannot resolve it" and is what the check now asks, so nothing is lost.
#
# Usage:
#   bash scripts/check-static-binary.sh --binary dist/chat-stasher-linux-x86_64 --machine "x86-64"
#   bash scripts/check-static-binary.sh --binary dist/chat-stasher-linux-arm64  --machine "ARM aarch64"
#
#   --binary PATH    required. The staged file to check.
#   --machine STRING required. The substring `file -b` must contain, e.g.
#                    `x86-64` or `ARM aarch64`.
#
# It prints `file -b`'s sentence and the binary's own `--version` output on
# stdout, so both survive into the run log. Every refusal goes to stderr and
# names the asset, the file and the header line that caused it.
#
# It reads one file, writes none, and touches no network.
#
# Exit codes: 0 = a static binary for that machine, and it runs · 1 = refused
# (including "readelf is not installed", which is a refusal rather than a skip:
# a check that cannot run must not read as a check that passed) · 2 = usage
# error.

set -euo pipefail

BINARY=""
MACHINE=""

usage() {
  echo "usage: bash scripts/check-static-binary.sh --binary PATH --machine STRING" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --binary)
      [ $# -ge 2 ] || { echo "check-static-binary: --binary needs a value" >&2; usage; exit 2; }
      BINARY="$2"; shift 2 ;;
    --machine)
      [ $# -ge 2 ] || { echo "check-static-binary: --machine needs a value" >&2; usage; exit 2; }
      MACHINE="$2"; shift 2 ;;
    *)
      echo "check-static-binary: unknown argument: $1" >&2
      usage
      exit 2 ;;
  esac
done

if [ -z "$BINARY" ] || [ -z "$MACHINE" ]; then
  echo "check-static-binary: --binary and --machine are both required" >&2
  usage
  exit 2
fi

if [ ! -f "$BINARY" ]; then
  echo "error: no such file: ${BINARY}" >&2
  exit 1
fi

# The asset name, not the path: it is what the Release is about, and a message
# that names dist/chat-stasher-linux-x86_64 as `chat-stasher-linux-x86_64` is
# the one the upload step and the asset-set check can be grepped for together.
ASSET="$(basename "$BINARY")"

# ---- 1 and 2. what `file` says ------------------------------------------------
FILE_OUT="$(file -b "$BINARY")"
printf '%s\n' "$FILE_OUT"

case "$FILE_OUT" in
  *"$MACHINE"*) ;;
  *)
    {
      echo "error: ${ASSET} is not a ${MACHINE} binary:"
      echo "       ${FILE_OUT}"
    } >&2
    exit 1
    ;;
esac

# `dynamically linked` matches neither branch below, which is the point.
case "$FILE_OUT" in
  *"statically linked"* | *"static-pie linked"*) ;;
  *)
    {
      echo "error: ${ASSET} is not statically linked — it would need a libc the target machine may not have:"
      echo "       ${FILE_OUT}"
    } >&2
    exit 1
    ;;
esac

# ---- 3. the ELF headers, which are the authority ------------------------------
if ! command -v readelf >/dev/null 2>&1; then
  {
    echo "error: readelf is not on PATH, so ${ASSET} could not be checked."
    echo "       It comes from binutils; the release job installs it explicitly."
    echo "       Refusing rather than skipping: a check that did not run is not a"
    echo "       check that passed."
  } >&2
  exit 1
fi

# The line the header checks below are read out of. Each of these three
# invocations must exit 0 — readelf can exit 0 on a file it could not fully
# read, which is why the facts are asserted and not merely the status (see the
# `Type:` and `LOAD` requirements); a non-zero status is the case where readelf
# refused outright.
readelf_field() {
  # $1 = one readelf flag, $2 = the part of the file it reads, named for the
  # refusal. Prints the output on stdout; returns 1 after printing a refusal.
  local out
  if ! out="$(readelf "$1" "$BINARY" 2>/dev/null)"; then
    {
      echo "error: readelf could not read the ${2} of ${ASSET}."
      echo "       A file whose ELF ${2} cannot be read is not a release asset."
    } >&2
    return 1
  fi
  printf '%s\n' "$out"
}

if ! HEADER="$(readelf_field -hW "ELF header")"; then
  exit 1
fi

TYPE_LINE="$(printf '%s\n' "$HEADER" | grep -E '^[[:space:]]*Type:' || true)"
case "$TYPE_LINE" in
  *EXEC* | *DYN*) ;;
  *)
    {
      echo "error: ${ASSET} is not an executable ELF:"
      echo "       readelf reports no Type: of EXEC or DYN."
      if [ -n "$TYPE_LINE" ]; then
        printf '%s\n' "$TYPE_LINE" | sed 's/^[[:space:]]*/       /'
      fi
    } >&2
    exit 1
    ;;
esac

if ! PROGRAM="$(readelf_field -lW "program headers")"; then
  exit 1
fi

# A program header table with no LOAD is not a program image, whatever else it
# holds. Asserted before INTERP is looked for so that "no INTERP found" means
# the table was read, not that it was empty.
if ! printf '%s\n' "$PROGRAM" | grep -qE '^[[:space:]]*LOAD([[:space:]]|$)'; then
  {
    echo "error: ${ASSET} has no PT_LOAD segment, so it is not a program image."
    echo "       Its program headers could not be read in full."
  } >&2
  exit 1
fi

INTERP_LINE="$(printf '%s\n' "$PROGRAM" | grep -E '^[[:space:]]*INTERP([[:space:]]|$)' || true)"
if [ -n "$INTERP_LINE" ]; then
  {
    echo "error: ${ASSET} is a dynamic executable:"
    echo "       it names a program interpreter (PT_INTERP):"
    printf '%s\n' "$INTERP_LINE"
  } >&2
  exit 1
fi

if ! DYNAMIC="$(readelf_field -dW "dynamic section")"; then
  exit 1
fi

# A static-PIE has a dynamic section and no DT_NEEDED in it; a classic static
# binary has no dynamic section at all, and readelf says so on stdout. Only the
# DT_NEEDED entries decide, which is why this does not ask whether the section
# exists.
NEEDED_LINES="$(printf '%s\n' "$DYNAMIC" | grep -E '\(NEEDED\)' || true)"
if [ -n "$NEEDED_LINES" ]; then
  {
    echo "error: ${ASSET} is a dynamic executable:"
    echo "       it names shared libraries it needs at run time (DT_NEEDED):"
    printf '%s\n' "$NEEDED_LINES"
  } >&2
  exit 1
fi

# ---- 4. and it runs ----------------------------------------------------------
set +e
RUN_OUT="$("$BINARY" --version 2>&1)"
RUN_RC=$?
set -e

if [ "$RUN_RC" -ne 0 ]; then
  {
    echo "error: ${ASSET} does not run: '${BINARY} --version' exited ${RUN_RC}:"
    if [ -n "$RUN_OUT" ]; then
      printf '%s\n' "$RUN_OUT" | sed 's/^/       /'
    fi
  } >&2
  exit 1
fi

printf '%s\n' "$RUN_OUT"

echo "check-static-binary: ${ASSET} is a static ${MACHINE} binary (no PT_INTERP, no DT_NEEDED) and runs" >&2
