#!/usr/bin/env bash
#
# The selftest for scripts/check-static-binary.sh: proof that the gate accepts
# both shapes a static Linux release asset can have, and refuses everything that
# is not one — including the two inputs that make `file` and the ELF headers
# disagree, which is what shows the headers rather than the sentence are the
# authority.
#
# It drives the real script, with `file` and `readelf` shimmed on PATH, the way
# scripts/self-test-install.sh shims `uname` and scripts/check-workflows.sh
# shims `curl`: the host is a Mac, which has neither tool and cannot read a
# Linux ELF at all, and the branches that matter must still be exercised. The
# shims answer out of fixture files keyed by the basename of the file they are
# asked about, so a probe is a stub "binary" plus a directory of recorded tool
# output. A fixture that is absent is how a probe says "readelf could not read
# this".
#
# **Fixture provenance, because it is not uniform.** The four `file` fixtures
# that carry the regression are verbatim from the failed run — the two that the
# real cells produced are marked below with the job they came from, and the
# static-pie one is the exact sentence that failed run 36131443686. The
# `readelf` fixtures are NOT copied from a specific binary: they are the lines
# this gate reads, in GNU readelf's line shapes, and the fields the gate does not
# read (offsets, sizes, build ids) are plausible rather than recorded. That is
# stated here rather than left to look like more than it is. Generating them for
# real would mean building a Linux musl binary, which the host cannot do; the
# tokens the gate greps for (`^[[:space:]]*INTERP`, `(NEEDED)`,
# `^[[:space:]]*LOAD`) are what the fixtures pin, and they are anchored on the
# token rather than on a column precisely so that a readelf whose column widths
# differ cannot change the verdict.
#
# Probes (expected verdict, and the message that must carry it):
#
#   accepted  linux-x86_64   static-pie, ET_DYN, no PT_INTERP, no DT_NEEDED
#                            — the *exact* file(1) sentence that failed rc.1
#             linux-arm64    classic static, ET_EXEC, no dynamic section at all
#                            — the file(1) sentence from the cell that passed
#   refused   dynamically-linked      file says it names an interpreter
#             wrong-machine           an arm64 sentence asked about as x86-64
#             interp-and-needed       file says static-pie, headers say PT_INTERP
#             needed-only             file says static-pie, headers say DT_NEEDED
#             object-file             file says statically linked, Type: REL
#             unreadable              readelf cannot read the ELF header
#             does-not-run            right shape, `--version` exits non-zero
#   usage     no arguments, an unknown flag, a missing value       -> exit 2
#
# The two "file says one thing, headers say another" probes are the ones that
# matter most: they are what a string-based gate cannot pass, and they are the
# probes that were shown going green against the old step (see W181's report).
#
# Exit codes: 0 = every probe behaved · 1 = at least one did not · 2 = usage.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="$ROOT/scripts/check-static-binary.sh"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cs-static-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

SHIM="$TMP/shim"
BINS="$TMP/bins"
FIX="$TMP/fixtures"
mkdir -p "$SHIM" "$BINS" "$FIX/file" "$FIX/readelf/header" "$FIX/readelf/program" "$FIX/readelf/dynamic"

PROBES=0
FAILED=0

# ---------------------------------------------------------------------------
# The shims. Both resolve a fixture by the basename of the file they are asked
# about, and both fail the way the real tool fails when they have no answer:
# `file` writes to stderr and exits 1, and so does `readelf`.
# ---------------------------------------------------------------------------
cat >"$SHIM/file" <<'SHIM_EOF'
#!/bin/sh
# file [-b] PATH
path=""
for arg in "$@"; do
  case "$arg" in
    -*) ;;
    *) path="$arg" ;;
  esac
done
fixture="${CS_FIXTURES}/file/$(basename "${path:-none}")"
if [ ! -f "$fixture" ]; then
  echo "file: cannot open \`${path}' (No such file or directory)" >&2
  exit 1
fi
cat "$fixture"
SHIM_EOF

cat >"$SHIM/readelf" <<'SHIM_EOF'
#!/bin/sh
# readelf -hW|-lW|-dW PATH
case "$1" in
  -hW) key=header ;;
  -lW) key=program ;;
  -dW) key=dynamic ;;
  *)
    echo "readelf: unrecognized option '$1'" >&2
    exit 1
    ;;
esac
path="$2"
fixture="${CS_FIXTURES}/readelf/${key}/$(basename "${path:-none}")"
if [ ! -f "$fixture" ]; then
  # No fixture is how a probe says readelf could not read this file.
  echo "readelf: Error: '${path}': Failed to read file's ELF header" >&2
  exit 1
fi
cat "$fixture"
SHIM_EOF

chmod +x "$SHIM/file" "$SHIM/readelf"

# ---------------------------------------------------------------------------
# Stub "binaries". The gate runs the asset's own `--version`, so a probe's
# asset is a shell script that answers it.
# ---------------------------------------------------------------------------
write_binary() {
  # $1 = asset name, $2 = the exit status `--version` gives
  cat >"$BINS/$1" <<EOF
#!/bin/sh
if [ "\$1" = "--version" ]; then
  echo "chat-stasher 0.5.0-rc.2"
  exit $2
fi
echo "usage: chat-stasher" >&2
exit 2
EOF
  chmod +x "$BINS/$1"
}

write_fixture() {
  # $1 = path under $FIX, $2... = lines
  dest="$FIX/$1"
  mkdir -p "$(dirname "$dest")"
  shift
  printf '%s\n' "$@" >"$dest"
}

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

# --- linux-x86_64: the rc.1 regression. Static-PIE: an ET_DYN image that keeps
# a dynamic section for self-relocation, has no program interpreter, and names
# no shared libraries. `file` says "static-pie linked" and says so only because
# `need == 0`; the old gate's substring test knew one spelling and refused this.
# The sentence below is verbatim from run 36131443686, job 108060226314.
write_binary linux-x86_64 0
write_fixture "file/linux-x86_64" \
  'ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, BuildID[sha1]=4025392ed62f5cc2eb11c835c2a411f02b0d2599, not stripped'
write_fixture "readelf/header/linux-x86_64" \
  'ELF Header:' \
  '  Magic:   7f 45 4c 46 02 01 01 00 00 00 00 00 00 00 00 00' \
  '  Class:                             ELF64' \
  '  Data:                              2'"'"'s complement, little endian' \
  '  Version:                           1 (current)' \
  '  OS/ABI:                            UNIX - System V' \
  '  ABI Version:                       0' \
  '  Type:                              DYN (Position-Independent Executable file)' \
  '  Machine:                           Advanced Micro Devices X86-64' \
  '  Version:                           0x1' \
  '  Entry point address:               0x8f60' \
  '  Start of program headers:          64 (bytes into file)' \
  '  Start of section headers:          10747096 (bytes into file)' \
  '  Flags:                             0x0' \
  '  Size of this header:               64 (bytes)' \
  '  Size of program headers:           56 (bytes)' \
  '  Number of program headers:         10' \
  '  Size of section headers:           64 (bytes)' \
  '  Number of section headers:         29' \
  '  Section header string table index: 28'
write_fixture "readelf/program/linux-x86_64" \
  'Elf file type is DYN (Position-Independent Executable file)' \
  'Entry point 0x8f60' \
  'There are 10 program headers, starting at offset 64' \
  '' \
  'Program Headers:' \
  '  Type           Offset   VirtAddr           PhysAddr           FileSiz  MemSiz   Flg Align' \
  '  PHDR           0x000040 0x0000000000000040 0x0000000000000040 0x000230 0x000230 R   0x8' \
  '  LOAD           0x000000 0x0000000000000000 0x0000000000000000 0x0c3e04 0x0c3e04 R   0x1000' \
  '  LOAD           0x0c4000 0x00000000000c4000 0x00000000000c4000 0x1f9d21 0x1f9d21 R E 0x1000' \
  '  LOAD           0x2bdd21 0x00000000002bdd21 0x00000000002bdd21 0x0a2ec8 0x0a2ec8 RW  0x1000' \
  '  DYNAMIC        0x33f1d0 0x000000000033f1d0 0x000000000033f1d0 0x0001e0 0x0001e0 RW  0x8' \
  '  NOTE           0x000238 0x0000000000000238 0x0000000000000238 0x000040 0x000040 R   0x4' \
  '  TLS            0x0a0a00 0x00000000003b9a00 0x00000000003b9a00 0x000000 0x000040 R   0x40' \
  '  GNU_PROPERTY   0x000278 0x0000000000000278 0x0000000000000278 0x000020 0x000020 R   0x8' \
  '  GNU_EH_FRAME   0x0a2ec8 0x00000000000a2ec8 0x00000000000a2ec8 0x011b7c 0x011b7c R   0x4' \
  '  GNU_RELRO      0x2bdd21 0x00000000002bdd21 0x00000000002bdd21 0x0802df 0x0802df R   0x1' \
  '' \
  ' Section to Segment mapping:' \
  '  Segment Sections...' \
  '   00     ' \
  '   01     .init .plt .text .rodata .eh_frame_hdr .eh_frame ' \
  '   02     ' \
  '   03     .init_array .data.rel.ro .got .data .bss ' \
  '   04     .dynamic .got ' \
  '   05     .note.gnu.property ' \
  '   06     ' \
  '   07     .note.gnu.property ' \
  '   08     .eh_frame_hdr ' \
  '   09     .init_array .data.rel.ro .dynamic .got '
write_fixture "readelf/dynamic/linux-x86_64" \
  'Dynamic section at offset 0x33f1d0 contains 20 entries:' \
  '  Tag        Type                         Name/Value' \
  ' 0x000000000000000c (INIT)               0xc4000' \
  ' 0x000000000000000d (FINI)               0x2bdd20' \
  ' 0x0000000000000019 (INIT_ARRAY)         0x33eef0' \
  ' 0x000000000000001b (INIT_ARRAYSZ)       56 (bytes)' \
  ' 0x0000000000000007 (RELA)               0x1a8' \
  ' 0x0000000000000008 (RELASZ)             12168 (bytes)' \
  ' 0x0000000000000009 (RELAENT)            24 (bytes)' \
  ' 0x000000006ffffff9 (RELACOUNT)          507' \
  ' 0x000000006ffffffb (FLAGS_1)            Flags: PIE' \
  ' 0x0000000000000000 (NULL)               0x0'

# --- linux-arm64: the cell that passed. A classic static image — ET_EXEC, and
# no dynamic section at all, which is the other way to be static and the reason
# the gate must not ask whether a dynamic section exists.
# The sentence below is verbatim from run 36131443686, job 108060226384.
write_binary linux-arm64 0
write_fixture "file/linux-arm64" \
  'ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, BuildID[sha1]=19a6f7caf069be3abc8c0648c81f10d8ac6608f2, not stripped'
write_fixture "readelf/header/linux-arm64" \
  'ELF Header:' \
  '  Magic:   7f 45 4c 46 02 01 01 00 00 00 00 00 00 00 00 00' \
  '  Class:                             ELF64' \
  '  Data:                              2'"'"'s complement, little endian' \
  '  Version:                           1 (current)' \
  '  OS/ABI:                            UNIX - System V' \
  '  ABI Version:                       0' \
  '  Type:                              EXEC (Executable file)' \
  '  Machine:                           AArch64' \
  '  Version:                           0x1' \
  '  Entry point address:               0x4094d0' \
  '  Start of program headers:          64 (bytes into file)' \
  '  Start of section headers:          7598408 (bytes into file)' \
  '  Flags:                             0x0' \
  '  Size of this header:               64 (bytes)' \
  '  Size of program headers:           56 (bytes)' \
  '  Number of program headers:         6' \
  '  Size of section headers:           64 (bytes)' \
  '  Number of section headers:         26' \
  '  Section header string table index: 25'
write_fixture "readelf/program/linux-arm64" \
  'Elf file type is EXEC (Executable file)' \
  'Entry point 0x4094d0' \
  'There are 6 program headers, starting at offset 64' \
  '' \
  'Program Headers:' \
  '  Type           Offset   VirtAddr           PhysAddr           FileSiz  MemSiz   Flg Align' \
  '  LOAD           0x000000 0x0000000000400000 0x0000000000400000 0x2414e0 0x2414e0 R E 0x10000' \
  '  LOAD           0x2414e0 0x00000000007414e0 0x00000000007414e0 0x4a1e8 0x4d908 RW  0x10000' \
  '  NOTE           0x000158 0x0000000000400158 0x0000000000400158 0x000020 0x000020 R   0x8' \
  '  TLS            0x2421d0 0x000000000074b1d0 0x000000000074b1d0 0x000000 0x000048 R   0x40' \
  '  GNU_PROPERTY   0x000178 0x0000000000400178 0x0000000000400178 0x000024 0x000024 R   0x8' \
  '  GNU_RELRO      0x2414e0 0x00000000007414e0 0x00000000007414e0 0x000410 0x000410 R   0x1' \
  '' \
  ' Section to Segment mapping:' \
  '  Segment Sections...' \
  '   00     .init .text .rodata ' \
  '   01     .data.rel.ro .got .data .bss '
write_fixture "readelf/dynamic/linux-arm64" \
  'There is no dynamic section in this file.'

# --- dynamically-linked: an ordinary glibc PIE. Refused by the `file` sentence
# before readelf is asked anything.
write_binary dynamically-linked 0
write_fixture "file/dynamically-linked" \
  'ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), dynamically linked, interpreter /lib64/ld-linux-x86-64.so.2, BuildID[sha1]=1f2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d, for GNU/Linux 3.2.0, not stripped'
write_fixture "readelf/header/dynamically-linked" \
  'ELF Header:' \
  '  Class:                             ELF64' \
  '  Type:                              DYN (Position-Independent Executable file)' \
  '  Machine:                           Advanced Micro Devices X86-64'
write_fixture "readelf/program/dynamically-linked" \
  'Program Headers:' \
  '  Type           Offset   VirtAddr           PhysAddr           FileSiz  MemSiz   Flg Align' \
  '  PHDR           0x000040 0x0000000000000040 0x0000000000000040 0x0002d8 0x0002d8 R   0x8' \
  '  INTERP         0x000318 0x0000000000000318 0x0000000000000318 0x00001c 0x00001c R   0x1' \
  '      [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]' \
  '  LOAD           0x000000 0x0000000000000000 0x0000000000000000 0x001000 0x001000 R   0x1000' \
  '  DYNAMIC        0x0002c0 0x00000000000002c0 0x00000000000002c0 0x0001e0 0x0001e0 RW  0x8'
write_fixture "readelf/dynamic/dynamically-linked" \
  'Dynamic section at offset 0x2c0 contains 27 entries:' \
  '  Tag        Type                         Name/Value' \
  ' 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]' \
  ' 0x000000000000000c (INIT)               0x1000' \
  ' 0x0000000000000000 (NULL)               0x0'

# --- wrong-machine: the arm64 sentence, asked about as an x86-64 asset.
write_binary wrong-machine 0
write_fixture "file/wrong-machine" \
  'ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, BuildID[sha1]=19a6f7caf069be3abc8c0648c81f10d8ac6608f2, not stripped'

# --- interp-and-needed: `file` says static-pie; the headers say otherwise. A
# string-based gate accepts this file. The header check is what refuses it.
write_binary interp-and-needed 0
write_fixture "file/interp-and-needed" \
  'ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, BuildID[sha1]=4025392ed62f5cc2eb11c835c2a411f02b0d2599, not stripped'
write_fixture "readelf/header/interp-and-needed" \
  'ELF Header:' \
  '  Class:                             ELF64' \
  '  Type:                              DYN (Position-Independent Executable file)' \
  '  Machine:                           Advanced Micro Devices X86-64'
write_fixture "readelf/program/interp-and-needed" \
  'Elf file type is DYN (Position-Independent Executable file)' \
  'Program Headers:' \
  '  Type           Offset   VirtAddr           PhysAddr           FileSiz  MemSiz   Flg Align' \
  '  LOAD           0x000000 0x0000000000000000 0x0000000000000000 0x001000 0x001000 R   0x1000' \
  '  INTERP         0x000318 0x0000000000000318 0x0000000000000318 0x00001c 0x00001c R   0x1' \
  '      [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]'
write_fixture "readelf/dynamic/interp-and-needed" \
  'Dynamic section at offset 0x2c0 contains 2 entries:' \
  '  Tag        Type                         Name/Value' \
  ' 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]'

# --- needed-only: no interpreter, but it still names a shared library it will
# want at run time. This is the shape rust-lang/rust#82912 reported — static
# according to every sentence, dynamically linked according to the binary.
write_binary needed-only 0
write_fixture "file/needed-only" \
  'ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, BuildID[sha1]=4025392ed62f5cc2eb11c835c2a411f02b0d2599, not stripped'
write_fixture "readelf/header/needed-only" \
  'ELF Header:' \
  '  Class:                             ELF64' \
  '  Type:                              DYN (Position-Independent Executable file)' \
  '  Machine:                           Advanced Micro Devices X86-64'
write_fixture "readelf/program/needed-only" \
  'Elf file type is DYN (Position-Independent Executable file)' \
  'Program Headers:' \
  '  Type           Offset   VirtAddr           PhysAddr           FileSiz  MemSiz   Flg Align' \
  '  LOAD           0x000000 0x0000000000000000 0x0000000000000000 0x001000 0x001000 R   0x1000' \
  '  DYNAMIC        0x0002c0 0x00000000000002c0 0x00000000000002c0 0x0001e0 0x0001e0 RW  0x8'
write_fixture "readelf/dynamic/needed-only" \
  'Dynamic section at offset 0x2c0 contains 3 entries:' \
  '  Tag        Type                         Name/Value' \
  ' 0x0000000000000001 (NEEDED)             Shared library: [libz.so.1]' \
  ' 0x000000006ffffffb (FLAGS_1)            Flags: PIE'

# --- object-file: a relocatable object, which is not a program image whatever
# the sentence above it says. Fail-closed: this is the branch that catches a
# wrong file being staged.
write_binary object-file 0
write_fixture "file/object-file" \
  'ELF 64-bit LSB relocatable, x86-64, version 1 (SYSV), statically linked, not stripped'
write_fixture "readelf/header/object-file" \
  'ELF Header:' \
  '  Class:                             ELF64' \
  '  Type:                              REL (Relocatable file)' \
  '  Machine:                           Advanced Micro Devices X86-64'

# --- unreadable: no readelf fixtures at all, so the shim fails the way readelf
# fails on a file it cannot parse. Nothing must be read as "no INTERP found".
write_binary unreadable 0
write_fixture "file/unreadable" \
  'ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, BuildID[sha1]=4025392ed62f5cc2eb11c835c2a411f02b0d2599, not stripped'

# --- does-not-run: every header fact is right and the binary refuses to start.
write_binary does-not-run 3
write_fixture "file/does-not-run" \
  'ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, BuildID[sha1]=4025392ed62f5cc2eb11c835c2a411f02b0d2599, not stripped'
write_fixture "readelf/header/does-not-run" \
  'ELF Header:' \
  '  Type:                              DYN (Position-Independent Executable file)' \
  '  Machine:                           Advanced Micro Devices X86-64'
write_fixture "readelf/program/does-not-run" \
  'Program Headers:' \
  '  Type           Offset   VirtAddr           PhysAddr           FileSiz  MemSiz   Flg Align' \
  '  LOAD           0x000000 0x0000000000000000 0x0000000000000000 0x001000 0x001000 R   0x1000'
write_fixture "readelf/dynamic/does-not-run" \
  'Dynamic section at offset 0x2c0 contains 1 entry:' \
  ' 0x000000006ffffffb (FLAGS_1)            Flags: PIE'

# ---------------------------------------------------------------------------
# probe <description> <want-rc> <want-substring|-> <machine> <asset>
# The shimmed PATH carries no readelf and no file of the host's own, so the
# branch under test is the only one that can run.
# ---------------------------------------------------------------------------
probe() {
  desc="$1"; want_rc="$2"; want_text="$3"; machine="$4"; asset="$5"
  PROBES=$((PROBES + 1))
  set +e
  out="$(env PATH="$SHIM:/usr/bin:/bin" \
    CS_FIXTURES="$FIX" \
    bash "$GATE" --binary "$BINS/$asset" --machine "$machine" 2>&1)"
  rc=$?
  set -e
  bad=""
  [ "$rc" = "$want_rc" ] || bad="exit ${rc}, wanted ${want_rc}"
  if [ "$want_text" != "-" ] && ! printf '%s' "$out" | grep -qF -e "$want_text"; then
    bad="${bad:+${bad} and }did not mention ${want_text}"
  fi
  if [ -n "$bad" ]; then
    FAILED=$((FAILED + 1))
    echo "FAIL: ${desc}: ${bad}" >&2
    printf '%s\n' "$out" | sed 's/^/      /' >&2
  fi
}

# probe_usage <description> <want-rc> <want-substring> [args... as one string]
probe_usage() {
  desc="$1"; want_rc="$2"; want_text="$3"; shift 3
  PROBES=$((PROBES + 1))
  set +e
  out="$(env PATH="$SHIM:/usr/bin:/bin" CS_FIXTURES="$FIX" bash "$GATE" "$@" 2>&1)"
  rc=$?
  set -e
  bad=""
  [ "$rc" = "$want_rc" ] || bad="exit ${rc}, wanted ${want_rc}"
  if [ "$want_text" != "-" ] && ! printf '%s' "$out" | grep -qF -e "$want_text"; then
    bad="${bad:+${bad} and }did not mention ${want_text}"
  fi
  if [ -n "$bad" ]; then
    FAILED=$((FAILED + 1))
    echo "FAIL: ${desc}: ${bad}" >&2
    printf '%s\n' "$out" | sed 's/^/      /' >&2
  fi
}

# ---- accepted --------------------------------------------------------------
probe "a static-pie x86_64 asset is accepted (the rc.1 regression)" \
  0 "is a static x86-64 binary (no PT_INTERP, no DT_NEEDED) and runs" "x86-64" linux-x86_64
probe "a classic static aarch64 asset is accepted" \
  0 "is a static ARM aarch64 binary (no PT_INTERP, no DT_NEEDED) and runs" "ARM aarch64" linux-arm64
probe "  ... and its own --version reaches the log" \
  0 "chat-stasher 0.5.0-rc.2" "x86-64" linux-x86_64

# ---- refused ---------------------------------------------------------------
probe "a dynamically linked asset is refused before readelf is asked" \
  1 "is not statically linked" "x86-64" dynamically-linked
probe "an arm64 asset asked about as x86-64 is refused" \
  1 "is not a x86-64 binary" "x86-64" wrong-machine

# The two probes a string-based gate cannot pass.
probe "PT_INTERP is refused even though file(1) says static-pie" \
  1 "is a dynamic executable" "x86-64" interp-and-needed
probe "  ... and the refusal quotes the program header it found" \
  1 "INTERP" "x86-64" interp-and-needed
probe "DT_NEEDED is refused even though file(1) says static-pie" \
  1 "(NEEDED)" "x86-64" needed-only
probe "  ... and the refusal says it names shared libraries" \
  1 "it names shared libraries it needs at run time" "x86-64" needed-only

probe "a relocatable object is refused" \
  1 "is not an executable ELF" "x86-64" object-file
probe "an ELF whose header readelf cannot read is refused" \
  1 "readelf could not read the ELF header" "x86-64" unreadable
probe "  ... and the refusal is not a pass" \
  1 "-" "x86-64" unreadable
probe "an asset that does not run is refused" \
  1 "does not run" "x86-64" does-not-run

# ---- usage -----------------------------------------------------------------
probe_usage "no arguments is a usage error" 2 "both required"
probe_usage "an unknown argument is a usage error" 2 "unknown argument" --binary "$BINS/linux-x86_64" --bogus
probe_usage "--machine with no value is a usage error" 2 "--machine needs a value" --binary "$BINS/linux-x86_64" --machine

if [ "$FAILED" -ne 0 ]; then
  echo "selftest-check-static-binary: FAIL (${FAILED} of ${PROBES} probes failed)" >&2
  exit 1
fi
echo "selftest-check-static-binary: PASS (${PROBES} probes)"
