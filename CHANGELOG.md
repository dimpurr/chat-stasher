# Changelog

Version numbers here are the CLI's, and they match the `vX.Y.Z` git tags. The
browser extension has its own version and ships on its own schedule; see
[`RELEASING.md`](RELEASING.md) for what a release contains.

## Unreleased — 0.4.0

### CLI

#### Added

- `activity-index --rebuild --destination … --machine …` rebuilds that machine's
  activity index from every archived snapshot. It is safe to rerun but starts
  over rather than resuming, and restarts if the same machine pushes during the
  rebuild. From issue #2.
- `search --json` and export manifests report per-machine recall with
  `located`, `time_unknown` and `index_trusted`; daily manifest entries also
  report `unknown_anywhere`. A `WARN` goes to stderr when at least half of a
  machine's candidate sessions have unknown time. From issue #2.
- `overview` and `status --destination …` show each machine's last writer
  version and flag versions behind the newest writer.

#### Changed

- Conversation time is now derived from message timestamps for ChatGPT, Claude,
  Gemini, DeepSeek, Grok, Perplexity and Kimi web captures. Export manifests
  mark `time_source` as `messages` or `list-updated`, and numeric epochs are
  interpreted as absolute timestamps. From issue #3.

#### Fixed

- `install-native-host --stage …` appends with the file's dominant line ending
  and refuses a dotted or implicit `stage` table.

### Browser extension (first stable release, 0.2.0)

#### Added

- Stable releases now ship the extension for ChatGPT, Claude, DeepSeek, Gemini
  and Grok; Perplexity and Kimi are available in development builds.
- Perplexity conversation bodies can be archived in development builds when the
  response proves the body is complete; incomplete or unproven bodies are
  refused.

#### Changed

- Claude history backfill now archives verified active branches, re-lists once
  to recover conversations an earlier walk dropped, and works from a newly
  opened `claude.ai/new` page by resolving its organization from that page.
- Gemini history listing now continues beyond the former request-body size cap.
- Backfill rotates fairly across platforms and accounts by least-recent service.
  Turning it off is honored during an active run, and the popup switch sends
  alarm changes through the worker's single synchronization queue.

#### Fixed

- An empty conversation no longer stops platform backfill: its id stays owed
  until a real body is archived, while three consecutive empty bodies halt the
  run as a signal that the endpoint may have changed.

## 0.3.0 — 2026-09-24

Compared against `v0.2.0`, tagged 2026-09-12.

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
  `turns_filter: "not-supported"`. `--trim-to-window` also drops individual
  lines whose own timestamp falls outside the window; a line whose time cannot
  be read is kept and counted in the manifest's `untimed_lines`, and the flag is
  refused outright if no window was given. Exit codes match `search`'s. From
  issue #1.
- **The native messaging host now works.** At `v0.2.0`, `native-host` without
  `--self-test` printed "the stdio message loop is not implemented in this
  build" and exited 2. It now reads one length-prefixed request frame and writes
  one response frame, answering `hello`, `deliver`, and two read-only requests:
  `summary` (session counts in the stage, in the last 24 hours, and by harness,
  plus the last successful push — counts and timestamps, never text, titles or
  session ids) and `open_dashboard` (starts `chat-stasher ui` and returns its
  per-launch URL, including the access token, to the calling extension only;
  never logged, never written to disk). A `summary` or `open_dashboard` request
  carrying any field beyond `protocol` and `type` is refused with `bad-request`;
  `hello` and `deliver` ignore fields the protocol does not define, because
  refusing one would reject a real conversation for a reason no document states.
- **`install-native-host --stage <dir>`** records the stage a browser-spawned
  host is allowed to write to, as `[native_host] stage` in the config. The
  directory must already exist and be a directory — the host never creates a
  stage — the edit preserves the config file's comments and everything else in
  it, and the flag cannot be combined with `--uninstall`.
- **`doctor` reports whether the Native Messaging host is usable**, as a new
  `D8` section: the browser-side registration on this machine and the stage
  `[native_host]` points at. Read-only, like the rest of `doctor`.
- **`search` filters on conversation time.** `--day <local date>`, or
  `--since`/`--until` for a range, matches a session when its own activity
  interval intersects the window. A session whose time is unknown is listed
  separately with the reason rather than dropped, and while a window is active
  the exit code is `3` rather than `1`, because "0 matched" is then unproven.
  `--harness <id>[,<id>…]` matches the leading `.`/`~` segment of an archived
  session id — the same shared filter `export` takes — and a session whose id
  carries no harness prefix cannot answer that question, so it is listed as
  unplaced rather than counted as a non-match. `--json` prints one object on
  stdout in place of the human report, keeping matched, not-matched and
  could-not-be-placed as separate fields so a consumer cannot read an unknown as
  an absence. `search` is also the dry run of `export`, and both build their
  filter from one shared set of flags.
- **Kimi Code** is a known harness. Kimi Code names a session by its directory
  and always calls the transcript `wire.jsonl`, so the registry cell declares
  that shape: the native id comes from the session directory, and only
  `agents/main/wire.jsonl` under it is read. Everything beside the transcript —
  `state.json`, `logs/`, the home-level index — stays out.

### Changed

- **BREAKING — `search --since-unix` / `--until-unix` no longer bound the same
  thing.** The two flags still exist and still take unix seconds, but at `v0.2.0`
  they bounded a session's *rustic snapshot time*; they now bound that session's
  own conversation interval, and they are deprecated in favour of `--day`,
  `--since` and `--until` (local calendar days), which mean the same thing. A
  saved command that passed a snapshot time therefore selects a different set of
  sessions, and it can both gain and lose sessions: the old test asked whether
  the snapshot time fell inside the window, the new one whether the
  conversation's own interval overlaps it, so a conversation written before a
  later snapshot can now match a window the snapshot missed, and the reverse. A
  session whose time cannot be read is no longer compared at all but listed as
  unplaced, which makes the exit code `3` rather than `0`/`1`.
  **What to do:** re-run the command, read the hits, and move the bounds to
  `--day`/`--since`/`--until`. The flags print a deprecation notice on stderr and
  are removed in the next release.
- **`ui` replaces `view`.** `ui` opens the archive dashboard (totals, a
  per-machine health row, a machine × source matrix, a weekly activity
  heatmap) on `127.0.0.1`; a click through to a session fetches and decrypts its
  text, and prints the byte cost first. `view` remains as a deprecated alias for
  one release: it prints a one-line notice on stderr and behaves identically.
- The Windows discovery root is derived from the home directory instead of being
  read from the process environment. `nativehost::default_root` is now pure —
  the same `(platform, home)` gives the same answer on every machine, so a test
  with a temporary home, or a `--target-root` probe, gets an answer about *that*
  home — and the environment is consulted in exactly one place, `machine_root`,
  which `install-native-host` uses for its default root, so no manifest lands
  anywhere new. That is the native-host path only: the scanner's Windows *cache*
  directory still reads `%LOCALAPPDATA%`, because that cache is a known folder
  rather than a child of `$HOME`.

### Fixed

- `export` refuses to write through a symlink below `--out`. With `--force` on
  an `--out` holding a pre-existing component that pointed outside `--out`,
  archived content landed outside the directory the user named. A component that
  cannot be inspected also refuses the write: "unreadable" is not "absent".
- The dashboard's accepted connection no longer inherits the listener's
  non-blocking mode, and a narration failure is no longer fatal to a dashboard
  whose output is a socket.
- The `reload` dev cycle: repeatable unpacked reloads with normalized build
  numbers, a recoverable swap window, and cleanup failures reported instead of
  swallowed.

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
