# Changelog

Version numbers here are the CLI's, and they match the `vX.Y.Z` git tags. The
browser extension has its own version and ships on its own schedule; see
[`RELEASING.md`](RELEASING.md) for what a release contains.

## 0.5.0-rc.1 — 2026-09-25

A release candidate for `0.5.0`, cut so that these exact artifacts can be
installed and run before the version becomes stable. It is not what an
unqualified install gets: `scripts/install.sh` pins the newest stable release,
so reaching this one means naming it (`CHAT_STASHER_VERSION=0.5.0-rc.1`).

Compared against `v0.4.0`, tagged 2026-09-24.

### CLI

#### Added

- **Linux and Windows binaries, not only macOS ones.** A Release now carries
  `chat-stasher-linux-x86_64` and `chat-stasher-linux-arm64` — static musl, so
  one binary covers every distribution — and `chat-stasher-windows-x86_64.exe`
  beside the two macOS binaries. `install.sh` installs the Linux artifacts: it
  reads the Release's `SHA256SUMS` **before** downloading anything, and starts
  the downloaded binary once before moving it into place, so a binary that
  matches its checksum but cannot start is refused and whatever was installed
  there before is left alone. Windows is not installed by that script — it is a
  POSIX `sh` script — so that branch prints the `.exe` asset to download instead.
  The version that command installs without being asked stays the newest stable
  release, so a Linux install of *this* release is the one that names it.
- **`npm install chat-stasher`.** An npm launcher package resolves a
  per-platform package carrying the released binary; the platform packages are
  assembled from a Release's own assets after their checksums are verified.
- **`cargo install chat-stasher`** installs the CLI from crates.io, and
  `cargo binstall chat-stasher` finds a prebuilt binary for every released
  target.
- **`chat-stasher setup`** walks through the first run. It scans local sources,
  then performs the local first save: two `run-once` passes (the first creates
  the encrypted local repository and its masterkey, the second proves that a pass
  with nothing new to archive adds nothing), a read of one session back out of
  the repository through the same store and query `search` uses, and the
  masterkey path with the instruction to copy it elsewhere. Copying it is
  confirmed by typing a sentence, which the walkthrough records as a declaration
  it cannot verify: nothing checks that a copy exists. A non-TTY run does the
  same work from named flags and prints one JSON object, including any missing
  named parameters. The destination and scheduler steps are still descriptive
  stubs.
- **`chat-stasher cache`** reports this machine's conversation-body cache: where
  it lives, the quota it must stay under (`[cache] max_bytes`, 2 GiB by default)
  and how much of it is in use; `cache clear` deletes every cached block. The
  cache holds the destination's own **ciphertext**, is disposable — losing it
  costs only speed — and is never consulted by `verify`, `export`, `dest-init`,
  `push` or `read --all-machines`, which read the destination itself because
  proving the destination intact is the point.
- **A read-only `has` request in the Native Messaging protocol.** The extension
  names a bundle's platform, session id and content fingerprint, and the host
  answers whether the stage already holds that exact content, looking only in the
  directory a `deliver` of that bundle would write to and taking no lock.
  `held: true`, `held: false` and a `nack` stay three separate outcomes: the
  middle one is an answer, the last one is a question that could not be asked,
  and "already archived" is never concluded from the extension's own memory.
- **A label per session in the activity index.** Each session's row now records
  what a list should call it: the harness's own title when it wrote one, else the
  head of the session's first user line, capped at 100 characters and flagged
  when the cap cut anything. `search --json` and the dashboard resolve the label
  from that row, and the honesty rules are the ones the tool already applies to
  time: content read with nothing label-able in it records an explicit "no label
  recorded", and an index written before labels existed reads as "label unknown"
  at machine level. Neither is an empty string, and neither is a guess.
- **`ui` picks the destination itself when the choice is unambiguous.** With
  exactly one `[destinations.<name>]` declared it opens that one; with several it
  opens the one the native host names, or lists the declared names and exits `2`
  when no default is recorded. `search`, `export` and `overview` still require
  the copy to be named, always.
- **`schedule install` and `schedule uninstall`** write and remove macOS launchd
  agents idempotently, one unit per configured destination (`--destination`).
  `schedule` without a subcommand still only renders.
- **An S3-compatible destination is documented end to end**, and a backend
  option value may be spelled `env:NAME`: it is read from the process
  environment at run time, so an access key and a secret need not be written into
  the config file.
- **A "no conversation content" state**, kept apart from "time unknown"
  everywhere. A session archived with no conversation content — an empty shard,
  or metadata lines such as a summary with no user or assistant message — is
  counted and listed separately, is never placed in a time bucket, and does not
  make an answer incomplete, because there is no conversation whose time could be
  missing. `overview` gains `summary.no_conversation_content_sessions` and its
  own section; `search` and `export` gain a distinct unplaced reason for it; the
  dashboard and the view JSON separate it too.
- **A partly-read range is no longer reported as a complete span.** A session
  whose records were read but not all of them placed in time keeps the bounds it
  did measure and records them as inner bounds: a window those bounds do not
  reach is answered "could not be placed" rather than "not selected", because the
  unplaced record may be stamped in that very window. A window the bounds do
  overlap is still a real match.
- **kimi-code conversation time**, read from the records the archive holds; and
  the harnesses whose conversation time was wrong — grok, gemini-cli, perplexity
  — now report the bounds they actually measured.

#### Changed

- `ui` now starts its server even when the archive read matched nothing: the
  destination is served as an honest empty page (`(this destination holds no
  sessions)` on a complete read; a floor sentence when parts were unreadable)
  instead of exiting `1` before any socket was bound. `ui` no longer exits
  `1`; a served read that could not complete still exits `3`.
- `chat-stasher ui` names machines that hold sessions but have no activity
  index on the HTML overview: a banner carrying exactly the machine list of
  the `machines_without_activity_index` API field, and their heatmap rows read
  as unknown — `?` cells with the reason and the `chat-stasher activity-index`
  repair command on the page — instead of a row of empty cells.
- **A config file that exists but cannot be used is no longer replaced by the
  built-in defaults.** A config that does not parse, a value of the wrong type,
  or a path that cannot be resolved stops every command that reads it with exit
  `3`, printing the file, the position and the reason. Continuing on the defaults
  would run a scheduled `push` exactly as if no destination had ever been
  declared. `doctor` is the one command that keeps going, and it lists the checks
  it therefore could not perform; an **absent** config file stays what it always
  was — the normal first run, which does use the defaults.
- **An unreadable `[cache]` section no longer takes the whole config with it**,
  and a section whose values cannot be read leaves the body cache **off**: a
  mistyped quota must not activate a cache nobody asked for.

#### Fixed

- `ui`'s `[ui] sessions : N in view / M in the archive` narration quoted the
  launch-filtered count as if it were the archive count; it now quotes the
  archive.
- **`verify` distinguishes failing to read from finishing a read.** An observed
  integrity mismatch still exits `1`; a verification that could not finish
  reading exits `3`, so its silence about everything else proves nothing.
- **The reader no longer renders a message without a bound or invents what it
  was not given.** A message renders at most 64 KiB, with the dropped byte count
  printed where the text stops and the raw shards one link away; a timestamp the
  reader had to interpret is labelled as interpreted rather than shown beside a
  recorded one as though the two were the same claim; a mixed turn keeps its
  recorded role; and the empty and window states say what they are.
- **codex and gemini-cli bodies render in the shape the archive actually holds**,
  and a message node that carried no content is no longer counted as one the
  reader failed to render.
- **`install.sh` is a POSIX script now.** It is documented as `curl -fsSL … | sh`,
  and on Debian and Ubuntu `sh` is dash, which rejects the bash-only constructs
  the script used — so the installer was broken for exactly the users its Linux
  artifacts exist for.
- **The activity index is repaired by a pass that pushes nothing**, instead of
  staying stale until something else changed.
- **A Windows config path is repaired as a path**, control escapes and all,
  rather than as a string.
- **The string scanner knows a character literal is not a string**, so an
  apostrophe in a comment no longer derails what it reads.
- **Claude Code's own metadata timestamps are ignored** when conversation time
  is derived.

#### Security

- **The reader refuses a link target it cannot call local.** The predicate tested
  the first byte, so a target beginning `//` — a protocol-relative URL, which a
  browser resolves against the page's own scheme — and the `/\` and `\\` forms
  it maps to were emitted as links. A fragment is local, and a path is local only
  when the byte after its leading `/` is neither `/` nor `\`; anything else is
  printed as text, which is what the page's own footer claims happens.
- **The body cache refuses to write or delete outside a directory it created.**
  Clearing the cache, or evicting to stay under its quota, cannot be aimed
  elsewhere by a path in the config; a component that cannot be inspected refuses
  the operation rather than being read as absent.
- **Backend credentials can come from the environment** (`env:NAME`), so they
  need not be stored in the config file, and the documented S3 configuration sets
  both switches that stop the backend from looking for credentials anywhere the
  user did not name — the ambient AWS environment and profiles, and the instance
  metadata service. A missing or mistyped credential cannot then quietly
  authenticate as the machine's instance role.

### Browser extension

The extension has its own version, and this candidate does not move it: the zip
attached to this Release is built on the stable channel and named
`chat-stasher-extension-0.2.0.zip`, the same version the previous Release
carried. Everything below is in that zip.

#### Added

- **A coverage page** — a standalone extension page, plus a summary card in the
  popup — that shows what the extension knows about its own backfill, per
  platform and account scope: how much is owed, what has been settled, what was
  skipped and why. Its wording is held to the state the model actually reported,
  a total says where it came from, a truncated list's count is marked as a lower
  bound, and a scope that is unregistered is named as such even while it still
  owes work.
- **Speed presets** — gentle (the default), standard and faster — wired to the
  backfill leg's caps, and a doubled rate for platforms on the stable channel.
- **Target-registry cap evictions are recorded durably**, shown in the popup and
  disclosed in the privacy document, instead of being a silent eviction.
- **`Retry-After` is honoured on 429/503 responses**, including a response whose
  body cannot be read, and list-only pages are paced accordingly. A value that is
  not an HTTP date is rejected before it can be parsed into something else.

#### Changed

- **Backfill no longer trusts its own record of what it delivered.** A capture is
  treated as already archived only when the host answers `has` with `held: true`
  for that capture's own fingerprint; a remembered "we stored this" survives the
  archive being replaced or restored at the same path, and the conversation would
  then be settled as archived without a byte reaching the new archive. A
  delivery is also keyed by the destination that acknowledged it, so two
  destinations do not settle each other's work.
- **A repeated later page halts the leg**, not only a repeated first page, and a
  re-enumeration that legitimately repeats is no longer read as a changed
  response shape.
- **Each conversation's own list time is recorded**, and it is kept when its debt
  is settled, so a long backfill does not restamp conversations with the time it
  finished.

#### Fixed

- The retry bucket is worded from the reasons it actually holds, and the popup
  card and chart geometry are grounded on real surfaces — month labels stay
  inside the chart, and the duplicate-skip sentence the view could never reach is
  gone.

### Repository and release tooling

Nothing here is in the shipped binary.

- The release workflow reconciles a re-run's assets instead of growing the set:
  every asset the run does not stage is deleted first, the staged files are
  uploaded over them, and the run fails unless the Release's asset set then
  equals the staged set exactly.
- It assembles the npm packages from the Release's own assets and publishes them
  (`next` for an rc, `latest` for a stable tag), then publishes the crate to
  crates.io, skipping an exact version that is already published — and it refuses
  a run whose ref is a branch rather than a tag, or whose tag disagrees with
  `Cargo.toml`.
- The Homebrew tap update is automated behind a token: it opens a draft pull
  request against the tap and never merges, so the tap cannot make a release
  fail and a release cannot reach the tap unreviewed.
- The committed support tables (`scripts/support-matrix/short-table.md` and
  `full-table.md`) are generated from the harness registry and the extension's
  platform table, and CI fails when they go stale against either input.
- A Linux end-to-end smoke test drives `doctor` → `init` → `run-once` → `read` →
  `overview` → `schedule` against synthetic histories, and the installer's own
  self-test now runs in CI, including its `dash` cases.
- Several documents stopped promising Linux binaries that no Release carried, and
  the Homebrew surface's claims about Linux were corrected.

## 0.4.0 — 2026-09-24

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
