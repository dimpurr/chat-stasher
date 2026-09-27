#!/usr/bin/env bash
# The selftest for scripts/check-citation-drift.py: proof that it still catches
# drift. Nine probes.
#
# Every probe is SELF-CONTAINED: it builds its own throwaway fixture — a
# temp-dir tree with the documents, the target files they cite, and the
# citations.lock — and runs the checker against that tree by importing
# scripts/check-citation-drift.py and pointing its module globals (REPO,
# DOC_FILES, LOCK_PATH) at the fixture. No probe reads a live repo document,
# so a citation that moves in the real tree cannot void a probe; all nine stay
# reproducible on any branch, whatever the live docs say. The working tree is
# never touched, and the run ends by printing `git status` (which must be cold).
#
# The first version of the checker asked only two questions — is the line number
# in bounds, is that line non-empty. A citation moved to a line that exists and
# is non-empty but has nothing to do with the claim returned 0. The probes are
# aimed at that hole:
#
#   probe 1  move a citation to a line that exists, is non-empty, unrelated  => red
#   probe 2  leave the document alone, edit a line inside a cited range      => red
#   probe 3  change nothing                                                  => green
#   probe 4  add a dangling citation to contracts/ (W32)                     => red
#   probe 5  every contracts/*.md is in the scan set (W32)                    => green
#   probe 6  an extensionless citation is its own anchor (.gitignore, W35)   => green
#   probe 7  a continuation behind an unresolvable path token (W35)          => red
#   probe 8  code that precedes a :N but names no file (W35b)                => green
#   probe 9  a path-shaped citation of a missing file (W35b)                 => red
#
# 🔴 A probe must prove what it says it proves, or fail loudly. So every probe
#    first builds a GREEN fixture and asserts that green; only then does it
#    apply the mutation that must flip it red (or keep it green), and it checks
#    the fixture change landed before it judges the checker — the same "void
#    selftest" discipline the coordinate-based version used, but now against a
#    fixture the probe itself controls, so it cannot rot out from under the
#    probe when a real doc moves.
#
# Each probe returns 0 when its assertions all held, 1 otherwise; bash sums
# them and exits non-zero if any probe failed, so a single ✘ is a red run.

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO" || exit 2

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAILED=0

cat > "$TMP/common.py" <<'PY'
# Shared machinery for the self-contained citation-drift selftest. Runs the
# real checker as a module against a throwaway fixture tree: the module's REPO,
# DOC_FILES and LOCK_PATH globals are re-pointed at the fixture, so every parse
# and hash happens inside the fixture and the working tree is never touched.
import contextlib
import difflib
import glob
import importlib.util
import io
import os
import sys

REPO = sys.argv[1]
BASE = sys.argv[2]
OK = "✔"  # ✔
KO = "✘"  # ✘


def load_checker():
    spec = importlib.util.spec_from_file_location(
        "citation_drift", os.path.join(REPO, "scripts", "check-citation-drift.py")
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class Fixture:
    """A throwaway, repo-shaped tree the checker is pointed at."""

    def __init__(self, docfile="spec.md"):
        self.root = os.path.join(BASE, "fx_%d" % os.getpid())
        os.makedirs(os.path.join(self.root, "docs-dev"), exist_ok=True)
        os.makedirs(os.path.join(self.root, "contracts"), exist_ok=True)
        self.mod = load_checker()
        self.mod.REPO = self.root
        self.mod.LOCK_PATH = os.path.join(self.root, "docs-dev", "citations.lock")
        self.mod.DOC_FILES = [docfile]

    def write(self, rel, content):
        p = os.path.join(self.root, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w", encoding="utf-8") as fh:
            fh.write(content)

    def update(self):
        """Regenerate the fixture lock from its current docs; rc=0 if written."""
        self.mod._file_cache.clear()
        entries, problems = self.mod.collect()
        return cap(lambda: self.mod.cmd_update(entries, problems))[0]

    def check(self):
        """Re-parse and check current fixture state; returns (rc, combined text)."""
        self.mod._file_cache.clear()
        entries, problems = self.mod.collect()
        rc, out, err = cap(lambda: self.mod.cmd_check(entries, problems))
        return rc, out + err

    def listed(self):
        """--list of current fixture state; returns (rc, stdout text)."""
        self.mod._file_cache.clear()
        entries, problems = self.mod.collect()
        rc, out, _ = cap(lambda: self.mod.cmd_list(entries, problems))
        return rc, out


def cap(fn):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        rc = fn()
    return rc, out.getvalue(), err.getvalue()


def ck(label, got, want):
    if got == want:
        print("  %s %s" % (OK, label))
        return 0
    print("  %s %s (got rc=%s, want rc=%s)" % (KO, label, got, want))
    return 1


def ck_in(label, text, needle):
    if needle in text:
        print("  %s %s" % (OK, label))
        return 0
    print("  %s %s; '%s' not in:\n%s" % (KO, label, needle, text))
    return 1


def ck_not_in(label, text, needle):
    if needle in text:
        print("  %s %s; '%s' shows up:\n%s" % (KO, label, needle, text))
        return 1
    print("  %s %s" % (OK, label))
    return 0
PY

# append the nine probe bodies to common.py
cat >> "$TMP/common.py" <<'PY'


def probe_1(base):
    # A continuation (`:N` that inherits its file from the sentence before it)
    # moved onto a line that exists, is non-empty, and is unrelated to the
    # claim must go red — the hole the first checker let through.
    bad = 0
    fx = Fixture()
    fx.write("src/mod.rs",
             "// header comment: has nothing to do with the claim\n"
             "use std::fmt;\n"
             "pub fn fmt() {}\n")
    fx.write("spec.md", "See `src/mod.rs:2`, `:3`.\n")
    bad += ck("fixture builds a green lock holding `src/mod.rs:2`,`:3`", fx.update(), 0)
    rc, _ = fx.check()
    bad += ck("base continuation `:3` is green", rc, 0)
    fx.write("spec.md", "See `src/mod.rs:2`, `:1`.\n")
    rc, text = fx.check()
    bad += ck("continuation moved to an unrelated-but-legal line must be red", rc, 1)
    bad += ck_in("the red names the moved anchor src/mod.rs:1",
                 text, "src/mod.rs:1")
    return 0 if bad == 0 else 1


def probe_2(base):
    # Edit a line *inside* a cited multi-line range, leave the range's first
    # line (the snippet a reader/naive check compares) alone: only hashing the
    # whole range can catch it. Proven deterministically: after the inner edit
    # flips the run red, the range's first line is byte-for-byte unchanged, so
    # the red can only have come from the changed inner line.
    bad = 0
    fx = Fixture()
    body = "".join("        let v%d = %d;\n" % (i, i) for i in range(350))
    fx.write("src/store.rs", body)
    fx.write("spec.md", "See `src/store.rs:271-345`.\n")
    bad += ck("fixture builds a green lock for the cited range", fx.update(), 0)
    rc, _ = fx.check()
    bad += ck("base cited range 271-345 is green", rc, 0)
    lines = body.splitlines(True)
    first = lines[270]  # line 271 — the first line a reader/naive check compares
    lines[317] = "        // PROBE2: content changed inside the cited range\n"
    fx.write("src/store.rs", "".join(lines))
    rc, text = fx.check()
    bad += ck("content edited inside the cited range must be red", rc, 1)
    with open(os.path.join(fx.root, "src/store.rs"), encoding="utf-8") as fh:
        after = fh.read().splitlines(True)
    if after[270] == first and rc == 1:
        print("  %s the red is the inner-line digest: the range's first line is unchanged"
              % OK)
    else:
        print("  %s the probe did not isolate the inner line as the cause of the red" % KO)
        bad += 1
    return 0 if bad == 0 else 1


def probe_3(base):
    # Change nothing about a green fixture: it must stay green.
    fx = Fixture()
    fx.write("src/mod.rs", "pub fn f() {}\nuse std::fmt;\n")
    fx.write("spec.md", "See `src/mod.rs:2`.\n")
    bad = ck("fixture builds a green lock", fx.update(), 0)
    rc, _ = fx.check()
    bad += ck("a clean fixture must stay green", rc, 0)
    return 0 if bad == 0 else 1


def probe_4(base):
    # contracts/ was outside the scan until W32, so a citation there could name
    # a file that does not exist while every gate stayed green. Add exactly that
    # to a fixture contract document and demand a red.
    bad = 0
    fx = Fixture()
    fx.write("src/mod.rs", "x\n")
    fx.write("spec.md", "See `src/mod.rs:1`.\n")
    bad += ck("fixture base lock is green", fx.update(), 0)
    fx.write("contracts/contract.md",
             "W32 probe: see `crates/chat-stasher/src/w32-probe-missing.rs:1`.\n")
    rc, text = fx.check()
    bad += ck("a dangling citation inside contracts/ must be red", rc, 1)
    bad += ck_in("the red names the missing contract citation",
                 text, "w32-probe-missing.rs")
    return 0 if bad == 0 else 1


def probe_5(base):
    # Probe 4 proves a red; this one proves what the red is for — the scan set
    # really is DOC_FILES plus every contract document. Read both from the
    # checker in the fixture, never from prose here.
    bad = 0
    fx = Fixture()
    fx.write("src/mod.rs", "x\n")
    fx.write("spec.md", "See `src/mod.rs:1`.\n")
    fx.write("contracts/alpha.md", "See `src/mod.rs:1`.\n")
    fx.write("contracts/beta.md", "See `src/mod.rs:1`.\n")
    scanned = set(fx.mod.doc_files())
    on_disk = set(
        os.path.relpath(p, fx.root)
        for p in glob.glob(os.path.join(fx.root, "contracts", "*.md"))
    )
    if not on_disk:
        print("  %s no fixture contract document found at all — nothing checked" % KO)
        return 1
    missing = sorted(on_disk - scanned)
    if missing:
        print("  %s scan set is missing: %s" % (KO, ", ".join(missing)))
        bad += 1
    else:
        print("  %s scan set covers all %d fixture contracts: %s"
              % (OK, len(on_disk), ", ".join(sorted(on_disk))))
    if "spec.md" in scanned:
        print("  %s spec.md (a DOC_FILES entry) is in the scan set" % OK)
    else:
        print("  %s spec.md (a DOC_FILES entry) is missing from the scan set" % KO)
        bad += 1
    return 0 if bad == 0 else 1


def probe_6(base):
    # An extensionless citation must be attributed to its own file (W35). The
    # fixture sentence cites `src/mod.rs:1` first, so a parser that failed to
    # see `.gitignore` as a path would inherit `src/mod.rs` and mis-anchor the
    # bare `:12` — the exact bug. Green, and --list must own `.gitignore:12`
    # with no inherited `src/mod.rs:12`.
    bad = 0
    fx = Fixture()
    fx.write("src/mod.rs", "".join("mod l%d;\n" % i for i in range(20)))
    fx.write(".gitignore", "\n".join("entry%d" % i for i in range(1, 16)) + "\n")
    fx.write("spec.md", "W35 probe: see `src/mod.rs:1` and `.gitignore:12`.\n")
    bad += ck("fixture builds a green lock including .gitignore:12", fx.update(), 0)
    rc, _ = fx.check()
    bad += ck("an extensionless citation of an unchanged fixture must stay green", rc, 0)
    rc, out = fx.listed()
    bad += ck_in("--list attributes the citation to .gitignore:12", out, ".gitignore:12")
    bad += ck_not_in("no inherited src/mod.rs:12 anchor is invented", out, "src/mod.rs:12")
    return 0 if bad == 0 else 1


def probe_7(base):
    # A continuation behind a token that is *shaped* like a path (it contains a
    # slash) but names no file must fail instead of inheriting the citation
    # before it (W35 / fail-loudly). The appended locked anchor keeps the red
    # attributable to the token, not to a moved anchor.
    bad = 0
    fx = Fixture()
    fx.write("src/mod.rs", "x\n")
    fx.write("spec.md", "See `src/mod.rs:1`.\n")
    bad += ck("fixture base lock is green", fx.update(), 0)
    fx.write("spec.md",
             "See `src/mod.rs:1`.\n\n"
             "W35 probe: see `src/mod.rs:1`, `W35-PROBE-NOT-A-DIR/not-a-file:1`.\n")
    rc, text = fx.check()
    bad += ck("a continuation behind an unresolvable path token must be red", rc, 1)
    bad += ck_in("the red names the token it could not resolve",
                 text, "W35-PROBE-NOT-A-DIR/not-a-file")
    return 0 if bad == 0 else 1


def probe_8(base):
    # code that merely precedes a colon and a number is not a citation (W35b).
    # The appended sentence names a port, a host:port, a Rust path and a clock
    # time — none path-shaped. The run stays green, and --list must not gain an
    # anchor for any of them.
    bad = 0
    fx = Fixture()
    fx.write("src/mod.rs", "x\n")
    fx.write("spec.md", "See `src/mod.rs:1`.\n")
    bad += ck("fixture base lock is green", fx.update(), 0)
    _, before = fx.listed()
    fx.write("spec.md",
             "See `src/mod.rs:1`.\n\n"
             "W35b probe: a port like `http://x:8080`, a host and port like `example.com:8080`,\n"
             "a Rust path like `std::fmt:5` and a time like `HH:23` are plain code.\n")
    rc, text = fx.check()
    bad += ck("code that merely precedes a colon and a number must stay green", rc, 0)
    _, after = fx.listed()
    if before == after:
        print("  %s no anchor was created for any of the four tokens" % OK)
    else:
        print("  %s the citation list changed; a token that names no file became an anchor:"
              % KO)
        for ln in difflib.unified_diff(before.splitlines(), after.splitlines(), n=1):
            print("      " + ln)
        bad += 1
    return 0 if bad == 0 else 1


def probe_9(base):
    # Probe 8 widens what counts as *not* a citation. This pins the other edge:
    # a citation that is a path by the letter of the rule (slash, known
    # extension) of a file that does not exist must still be red, and name the
    # path.
    bad = 0
    fx = Fixture()
    fx.write("src/mod.rs", "x\n")
    fx.write("spec.md", "See `src/mod.rs:1`.\n")
    bad += ck("fixture base lock is green", fx.update(), 0)
    fx.write("spec.md", "See `src/mod.rs:1`.\n\nW35b probe: see `not-a-real/dir.rs:3`.\n")
    rc, text = fx.check()
    bad += ck("a path-shaped citation of a missing file must be red", rc, 1)
    bad += ck_in("the red names the path it could not resolve", text, "not-a-real/dir.rs")
    return 0 if bad == 0 else 1


PROBES = ["1", "2", "3", "4", "5", "6", "7", "8", "9"]


def run(num):
    fn = globals().get("probe_%s" % num)
    if fn is None:
        print("unknown probe %s" % num, file=sys.stderr)
        return 2
    return fn(BASE)
PY

run_one() { # run_one <num>
  python3 - "$REPO" "$TMP" "$1" <<'PY'
import sys
sys.path.insert(0, sys.argv[2])
import common as C
sys.exit(C.run(sys.argv[3]))
PY
  return $?
}

titled() { # titled <num> <title> [desc...]
  echo "=============================================================="
  echo "Probe $1: $2"
  shift 2
  for line in "$@"; do echo "$line"; done
  echo "=============================================================="
}

titled 1 "a continuation moved to an unrelated-but-legal line must be red (W33/W193)"
echo "  Move \`:3\` (a continuation inheriting src/mod.rs) onto \`:1\` — a line that"
echo "  exists and is non-empty but says nothing about the claim. Only hashing the"
echo "  cited line catches it; bounds-and-non-empty would pass it."
run_one 1; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

titled 2 "content edited inside a cited range must be red"
echo "  The range's first line (the snippet; line 271) is left alone and an inner"
echo "  line is edited. Only hashing the whole range detects it."
run_one 2; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

titled 3 "change nothing must stay green"
run_one 3; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

titled 4 "a dangling citation inside contracts/ must be red (W32)"
echo "  contracts/ sat outside the scan until W32; a citation there naming a"
echo "  missing file must go red."
run_one 4; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

titled 5 "every contracts/*.md is in the scan set (W32)"
echo "  Reads the scan set from the checker against the fixture, and compares it"
echo "  with what is actually on disk in the fixture."
run_one 5; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

titled 6 "an extensionless citation is its own anchor (.gitignore, W35)"
echo "  The fixture sentence also cites \`src/mod.rs:1\` first, so a parser that"
echo "  failed to see \`.gitignore\` as a path would inherit src/mod.rs and mis-anchor"
echo "  the bare \`:12\`. Green, and --list must own \`.gitignore:12\` with no inherited"
echo "  \`src/mod.rs:12\`."
run_one 6; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

titled 7 "a continuation behind an unresolvable path token must be red (W35)"
echo "  A token with a slash names no file; it must fail loudly instead of"
echo "  inheriting the citation before it."
run_one 7; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

titled 8 "code that precedes a :N but names no file is not a citation (W35b)"
echo "  A port, a host:port, a Rust path and a clock time stay green, and --list"
echo "  must not gain an anchor for any of them."
run_one 8; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

titled 9 "a path-shaped citation of a missing file must still be red (W35b)"
echo "  The other edge of probe 8: a slash + known-ext token of a missing file"
echo "  is a citation, and it must be red and name the path."
run_one 9; rc=$?
echo
[ "$rc" -ne 0 ] && FAILED=1

echo "=============================================================="
echo "After: the working tree is untouched (all probes ran in \$TMP)"
echo "=============================================================="
remains="$(git status --porcelain)"
if [ -n "$remains" ]; then
  printf '%s\n' "$remains"
  FAILED=1
else
  echo "(empty)"
fi
echo

if [ "$FAILED" -eq 0 ]; then
  echo "SELFTEST PASS: all nine probes asserted what they must, against self-contained fixtures."
  exit 0
fi
echo "SELFTEST FAIL: at least one probe returned the wrong result."
exit 1