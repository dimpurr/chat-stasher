# W293: parallel FTS builds on Windows

## Root cause

The first build of a destination creates its cache directory and publishes
`.chat-stasher-fts` by renaming a fully written staging file into place. The
directory ownership check and the marker publication were individually safe,
but the check sequence was not atomic:

1. Builder A observes that the marker does not exist.
2. Builder B publishes the complete marker.
3. Builder A reads the directory, sees the marker, and refuses the now-owned
   directory as a non-empty unmarked directory.

The error therefore did not mean that an unrelated directory was being
adopted. It was a stale absence observation racing a sibling's completed
publication. The test run that failed was on Windows, but this ordering is
possible on any filesystem. The sibling's marker is whole because the rename
is atomic; the defect was failing to recheck that marker before refusing.
Windows adds a related publication case: renaming onto an existing marker
fails there, so a losing publisher must verify and adopt the sibling's complete
marker.

## Change

When the directory inspection finds entries after an absent-marker probe, the
builder now verifies whether the marker has since been published. It proceeds
only when the marker contents identify this tool as owner; any other non-empty
unmarked directory is still refused. Marker publication also treats a sibling
winning the rename race as success after verifying the final marker, including
Windows where rename does not replace an existing destination. The refusal
rule remains unchanged for foreign contents.

The unit test `missing_marker_probe_adopts_a_sibling_marker` forces the exact
three-step interleaving on every OS and separately checks that an unrelated
file is refused. With the ownership recheck removed, the test fails with the
same refusal seen in CI; with the fix, it passes. The existing CLI test still
checks that all parallel builds finish and that the resulting index covers the
archive.

Windows was not available for local execution, so the Windows behavior is
reasoned from the platform's rename semantics and covered by the deterministic
cross-platform test. No archive data or privacy boundary changes; only the
disposable local index startup behavior changes.
