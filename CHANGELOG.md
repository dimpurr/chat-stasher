# Changelog

Version numbers here are the CLI's, and they match the `vX.Y.Z` git tags. The
browser extension has its own version and ships on its own schedule; see
[`RELEASING.md`](RELEASING.md) for what a release contains.

The comparison point for the entry below is `v0.2.0`, tagged 2026-09-12.

## Unreleased — 0.3.0

### Added

- **`export --out <dir>`** writes the sessions a query selects to files:
  `<out>/<machine>/<harness>/<session-id>.jsonl` holds each session's archived
  lines in their native format, byte-identical to what `read` returns for it,
  and `<out>/manifest.json` records per session its machine, harness, id, first
  and last message time, shard count, bytes written, sha256 of the file on disk
  and the filters that were applied — plus the sessions no filter could place,
  the machines whose activity index could not be read, the sessions that could
  not be written, and the run's exit status. Selection is `search`'s own code
  path, so the written set equals what `search` reports for the same flags.
  `--dry-run` prints the cost and writes nothing. `--turns user` keeps only the
  user's own lines and only for harnesses whose format makes that certain;
  elsewhere every line is written and the session records
  `turns_filter: "not-supported"`. Exit codes match `search`'s. From issue #1.
- **The native messaging host now works.** At `v0.2.0`, `native-host` without
  `--self-test` printed "the stdio message loop is not implemented in this
  build" and exited 2. It now reads one length-prefixed request frame and writes
  one response frame, answering `hello`, `deliver`, and two read-only requests:
  `summary` (session counts in the stage, in the last 24 hours, and by harness,
  plus the last successful push — counts and timestamps, never text, titles or
  session ids) and `open_dashboard` (starts `chat-stasher ui` and returns its
  per-launch URL, including the access token, to the calling extension only;
  never logged, never written to disk). A request carrying any field beyond
  `protocol` and `type` is refused with `bad-request`.
- **`search` filters on conversation time.** `--day <local date>`, or
  `--since`/`--until` for a range, matches a session when its own activity
  interval intersects the window. A session whose time is unknown is listed
  separately with the reason rather than dropped, and while a window is active
  the exit code is `3` rather than `1`, because "0 matched" is then unproven.
  `search` is also the dry run of `export`, and both build their filter from one
  shared set of flags.
- **Kimi Code** is a known harness. Kimi Code names a session by its directory
  and always calls the transcript `wire.jsonl`, so the registry cell declares
  that shape: the native id comes from the session directory, and only
  `agents/main/wire.jsonl` under it is read. Everything beside the transcript —
  `state.json`, `logs/`, the home-level index — stays out.

### Changed

- **`ui` replaces `view`.** `ui` opens the archive dashboard (totals, a
  per-machine health row, a machine × source matrix, a weekly activity
  heatmap) on `127.0.0.1`; a click through to a session fetches and decrypts its
  text, and prints the byte cost first. `view` remains as a deprecated alias for
  one release: it prints a one-line notice on stderr and behaves identically.
- The Windows discovery root is derived from the home directory instead of being
  read from the process environment. `nativehost::default_root` is now pure —
  the same `(platform, home)` gives the same answer on every machine — and the
  single place that reads `%LOCALAPPDATA%` is `machine_root`, which
  `install-native-host` uses for its default root, so no manifest lands anywhere
  new.

### Fixed

- `export` refuses to write through a symlink below `--out`. With `--force` on
  an `--out` holding a pre-existing component that pointed outside `--out`,
  archived content landed outside the directory the user named. A component that
  cannot be inspected also refuses the write: "unreadable" is not "absent".
- The dashboard's accepted connection no longer inherits the listener's
  non-blocking mode, and a narration failure is no longer fatal to a dashboard
  whose output is a socket.
- `schedule`'s launchd template and the `reload` dev cycle: repeatable unpacked
  reloads with normalized build numbers, a recoverable swap window, and cleanup
  failures reported instead of swallowed.

### Repository and release tooling

Nothing here is in the shipped binary.

- Commit messages, pull requests, issues, comments and docs must be English, and
  a `scripts/hooks/commit-msg` hook plus a CI job enforce it. The one exception
  is `apps/extension/locales/zh_CN.yml`.
- `scripts/relocate-citations.py` moves `file:line` citations across a merge
  mechanically, and refuses any citation whose new position is not forced, so a
  citation is never quietly pointed at the wrong line.
- `scripts/check-citation-drift.py` now also scans `contracts/`, and the drift
  self-test was repaired.
- `scripts/dev/reload-extension.sh` and its test were added to the gates.
