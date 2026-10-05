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

## Bounded IDs

The native form is `oc-<hex agent>-<hex session>`; cold archives append
`~cold-<hex file name>`. Hex encoding doubles the UTF-8 byte count, so a valid
source filename can produce an ID longer than a filesystem component permits.
Cold and live native IDs preserve that exact form through 255 bytes. Longer
values use a UTF-8 prefix followed by `~` and the first 32 lowercase hexadecimal
digits (128 bits) of SHA-256 of the **full unbounded native value**. The prefix
fills the remaining byte budget. This rule depends only on the source values,
so the native ID is identical across machines and repeated scans.

The canonical `<source>.<machine>.<native>` ID applies the same rule to its
whole value with a 255-byte budget. Existing canonical IDs within the budget
remain byte-for-byte unchanged, including native IDs above 176 bytes. If both
bounds apply, the canonical digest covers the composed value containing the
bounded native ID. Machine partitions continue to distinguish machines.

The shared bound uses bytes, cutting only at a UTF-8 character boundary. This
matches APFS and common POSIX component limits and is conservative for NTFS,
which counts UTF-16 units. The hash distinguishes values whose readable prefixes
match; it is collision-resistant rather than mathematically collision-free.

Other ID-derived paths were checked too: extension stage directories apply the
bound to `<platform>.<sanitized session>`, shared by `has` and delivery. Export
bounds its session filename stem at 249 bytes before appending `.jsonl`, while
the manifest keeps the canonical session ID and records the derived path.
Store writes and readback join the canonical ID directly, without a second
transformation. Machine partition names are retained under their existing
normalization/identity rules; native-host install IDs are validated UUIDs or
at most 128 ASCII bytes, so their `.json` filenames fit. Raw inbox filename fallbacks use the same
shared bound.

The synthetic regression in
`crates/chat-stasher/tests/w680_cold_id_name_max_test.rs` attempts the same
shard-directory creation as collect. Against the original code, a rotated cold
filename triggers `File name too long` (macOS error 63). It also checks short-ID
compatibility and distinct archive generations.

Implementation: `crates/chat-stasher/src/sqlite_probe.rs:342-399`,
`crates/chat-stasher/src/id.rs:232-287`,
`crates/chat-stasher/src/inbox.rs:594-623`,
`crates/chat-stasher/src/export.rs:962-979`, and
`crates/chat-stasher/src/store.rs:1217-1220`.
