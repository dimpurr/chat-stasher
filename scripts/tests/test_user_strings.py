"""Regression tests for the Rust user-string scanner's malformed-input rules.

The scanner is the shared front end of output-inventory.py and
check-terminology.py, and the one contract it must hold on input it cannot
parse is that every method makes progress: a source file the scanner
misreads may cost it the rest of that file, never the rest of the run. The
regression here is the one fixed alongside W932: an unterminated raw-string
lookalike (``r`` + ``#``* + ``"`` with no closing delimiter ever again in
the file) made parse_string report the position it started at, and the main
scan, which trusts a reported end to move (``i = end; continue``), spun on
it forever — so a regeneration step that had to cross such a file never
finished at all.

Committed sources parse cleanly, so the fixture is synthetic: a file that
ends in the middle of a raw string, as a file stopped mid-write or a
conflicted hunk leaves one. That is the shape that reaches the branch in
real life. Comments are not such a shape and cannot become one: the walk
consumes every comment at its own slash before the characters inside it
are examined, so an ``r#"`` inside a comment is skipped as comment content
— worth stating here because "what does not trigger it" is the half of the
contract easiest to break by refactoring the branch order away.
"""

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "_user_strings.py")
SPEC = importlib.util.spec_from_file_location("user_strings", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
user_strings = importlib.util.module_from_spec(SPEC)
# Registered before exec_module: _user_strings uses frozen dataclasses, and
# their annotation processing resolves the module through sys.modules by
# its __module__ name - unregistered, that lookup returns None on Python
# 3.14 and the module fails to load at all.
sys.modules["user_strings"] = user_strings
SPEC.loader.exec_module(user_strings)

# The file ends inside the raw string: no closing '"#' anywhere after the
# opening, so everything after the lookalike is unreachable while the
# scanner believes it is inside a raw string. Synthetic and opaque, per the
# fixture rules: a shape, not a real source.
UNTERMINATED_AT_EOF = '''\
fn main() {
    println!("synthetic user-visible string");
    let fixture = r#"synthetic truncated fixture
'''

# A scan driver the subprocess case runs: prints one line per hit, so the
# caller can assert both that the scan finished and that the string before
# the malformed region was still extracted.
DRIVER = """
import importlib.util
import os
import sys

spec = importlib.util.spec_from_file_location("user_strings", sys.argv[1])
mod = importlib.util.module_from_spec(spec)
sys.modules["user_strings"] = mod
spec.loader.exec_module(mod)
root = os.path.dirname(os.path.abspath(sys.argv[1]))
for hit in mod.extract_file(sys.argv[2], root):
    print(f"{hit.kind}\\t{hit.text}")
"""


class ParseStringContract(unittest.TestCase):
    """parse_string is the unit the main scan loops on, so its failure return
    is the whole contract: no reported end may equal the start position."""

    def test_unterminated_raw_string_reports_no_end(self):
        # The truncated-file shape, and the same lookalike as it would sit
        # inside comment text (the walk skips comments, per the header, but
        # the contract is the unit's wherever the text comes from).
        texts = [UNTERMINATED_AT_EOF,
                 '// opens as r#"synthetic quoted opens and never closes here\n']
        for text in texts:
            with self.subTest(comment=text.startswith("//")):
                scanner = user_strings._Scanner(text)
                pos = text.index('r#"')
                end, display, value = scanner.parse_string(pos)
                self.assertIsNone(
                    end, "an unterminated raw string must not report a position")
                self.assertIsNone(display)
                self.assertIsNone(value)

    def test_terminated_raw_string_still_parses(self):
        # The fix must not tighten the happy path: a well-formed raw string
        # keeps yielding a display form and a value, including the byte-string
        # spelling, which reaches the same branch because the scan sees the
        # 'r' of 'br' before it sees anything that would own the 'b'.
        cases = [
            ('let s = r#"synthetic raw value"#;', "synthetic raw value"),
            ('let b = br#"synthetic byte value"#;', "synthetic byte value"),
        ]
        for text, expected in cases:
            with self.subTest(text=text):
                scanner = user_strings._Scanner(text)
                end, display, value = scanner.parse_string(text.index('r'))
                self.assertIsNotNone(end)
                self.assertEqual(value, expected)
                self.assertEqual(display, f'r#"{expected}"#')


class ScanTerminates(unittest.TestCase):
    """The end-to-end property: a file truncated inside a raw string is
    scanned to its end, and the string before the malformed region is still
    extracted.

    The case runs the scan in a subprocess under a timeout: the regression
    this module guards is an unbounded spin, and a suite that hangs the
    runner is not a red suite — the timeout turns the regression back into a
    bounded failure with a message that names it.
    """

    def test_scan_finishes_and_extracts_before_the_lookalike(self):
        with tempfile.NamedTemporaryFile("w", suffix=".rs", delete=False,
                                         encoding="utf-8") as fh:
            fh.write(UNTERMINATED_AT_EOF)
            fixture = fh.name
        try:
            try:
                proc = subprocess.run(
                    [sys.executable, "-c", DRIVER, SCRIPT, fixture],
                    capture_output=True, text=True, timeout=30, check=False,
                )
            except subprocess.TimeoutExpired:
                self.fail("the scan of a file truncated inside a raw string "
                          "did not finish in 30 s: parse_string is reporting "
                          "a position it started at again, and the main scan "
                          "is spinning on it")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            hit_lines = [line for line in proc.stdout.splitlines() if line]
            self.assertTrue(any("synthetic user-visible string" in line for line in hit_lines),
                            f"expected the macro string to be extracted; got: {hit_lines}")
        finally:
            os.unlink(fixture)


if __name__ == "__main__":
    unittest.main()
