# OpenClaw scanner

The `openclaw` harness scans the configured agents root; by default this is
`$HOME/.openclaw/agents`, from the harness registry. For each direct child it
looks for `agent/openclaw-agent.sqlite` and compressed JSONL files in
`sessions/`. A configured `harness_roots.openclaw` path can select another
agents root.

SQLite connections are opened read-only. Existing WAL sidecars are included in
the database footprint measurement. If a WAL-mode database has no `-shm`
sidecar, the scanner uses SQLite's immutable, main-file-only read path so it
does not create sidecars; that view may omit changes still waiting in an
uncheckpointed WAL. The scanner does not write database records or create
missing WAL sidecars.

When the database can be enumerated and its modification time is readable,
every identifiable `session_windows` row and `session_transcript_archives` row
is a separate source record. Window records export the window columns and
transcript events ordered by `seq`. Each event's
`event_json` is parsed into a JSON value and exported with its `seq` and
`created_at`; assistant message fields such as event identity, model, provider,
usage and cost remain in that raw event value. Archive records include the
archive metadata columns and `archive_blob` as hexadecimal bytes, with its
declared encoding and SHA-256. Collection verifies the blob against that
digest before accepting it. Nullable timestamps and event JSON remain explicit
`null` values. A missing required table or column makes that agent store
unenumerable; unreadable rows are counted, and other stores and cold files
continue to be scanned.

Cold files under `sessions/` are considered when the file name's extension is
`.zst`; a `.jsonl` name component is not required
(`crates/chat-stasher/src/scanner.rs:2313-2322`). The part of the name before
the first `.jsonl` supplies the session ID — the whole file name when the name
contains no `.jsonl` — and a file whose part before `.jsonl` is empty is
skipped. A file is also skipped when an identifiable SQLite archive row for the
same agent and session ID was enumerated. Discovery does not read or verify the
archive blob, so this deduplication is based on row identity; collection later
checks that the blob and digest are present and consistent. The filename has
no generation, so an archive row for one generation can also suppress a cold
file belonging to a sibling generation. A cold file without such an archive
row is kept as its own source record. Its zstd stream is decompressed in full
and the complete JSONL lines — the ones terminated by a newline — are
preserved in the raw archive; a trailing line still missing its final newline
is held as in progress rather than sealed, and the first pass after the source
gains that newline re-decodes and seals that tail in full
(`crates/chat-stasher/src/collect.rs:2239-2251`, `:2373-2387`, `:25-34`). The
OpenClaw normalizer does not interpret that native line format, so those lines
remain unrendered.

The agent directory name is part of each archived session ID, preventing
collisions between agents. Rollover windows remain separate observations,
with their logical session key and parent/fork fields retained in the raw
window record. Each direct child of the configured agents root is checked;
parent and spawn relationships come from source fields rather than inference
from labels. Scanner tests build synthetic SQLite stores and cold files in the
shared test `Sandbox`.
