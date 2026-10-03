# OpenClaw scanner

The `openclaw` harness is a read-only local scanner for the per-agent database
under `~/.openclaw/agents/<agentId>/agent/openclaw-agent.sqlite`. It reads
SQLite through `mode=ro` and the WAL-aware connection path shared with the
other database scanners. It includes `.db`, `-wal` and `-shm` in footprint
measurements. It does not copy the database file or change OpenClaw's state.

Each `session_windows` row becomes a session record. Its `transcript_events`
are exported in sequence order with their raw `event_json`, `seq` and
`created_at`; assistant message model, provider, usage, cost and event identity
therefore stay attached to their original message. `session_transcript_archives`
rows are also retained, including the `archive_blob` bytes and their
`session_id` and `generation`. SQLite cold archives deduplicate matching
`sessions/` files by native session ID. A compressed file with no matching
SQLite archive row is kept as its own source record.

A cold file is skipped only when a SQLite archive row was actually enumerated
for its session. The cold-file name observed upstream —
`<session_id>.jsonl.deleted.<timestamp>...zst` — carries the native session ID
but no generation, so the skip keys on the session ID: when the store holds an
enumerated archive row for that session, the database holds the canonical
bytes (`session_transcript_archives.archive_blob`, digest-verified) and the
disk file is a redundant copy. Two corollaries are deliberate. A row that could
not be identified (a NULL `session_id` or `generation`) never suppresses its
cold file — the readable copy is kept. And a cold file for a generation that
is no longer in the store while a sibling generation of the same session still
is would be skipped by this rule; the name alone cannot distinguish
generations, so the redundancy judgement can only go as far as the session ID.
Evidence from a real store that ties `archive_name` or a digest to the file
name would be needed to narrow the skip to the exact generation.

The nullable upstream columns are read as nullable: a window without
`started_at` or an archive without `created_at` (legacy imports leave both
NULL) stays a fully enumerable candidate, and `read_openclaw_session` exports
the unknown value as an explicit `null` rather than eliding the row or
inventing a timestamp. A NULL in one row never aborts the enumeration: rows
whose identity is unreadable are counted and reported in the probe's unreadable
count (and its note), while every other row of the store — including the
archive rows behind the cold-file dedup — is still enumerated.

The agent ID is part of the archived session ID, so multiple agent roots do not
collide. Rollover windows remain separate source observations, with their
logical session key and parent/fork fields carried in the raw window record.
Sub-agent windows are scanned by default; the source schema provides explicit
parent/spawn relationships, which are retained without inferring lineage from
labels.

The SQLite archive blob is stored in the raw export as hexadecimal bytes,
alongside its declared encoding and SHA-256. Collection does not need to
decompress an archive to preserve it. The normalizer renders ordinary message
text and leaves non-message events counted but available in the raw archive.
Cold files that survive dedup are preserved raw: their native per-line JSONL
is an upstream export format this scanner does not interpret, so their lines
count as unrendered while the bytes stay in the archive. An unknown required
table or column makes the source unenumerable; it is never reported as an
empty store. A NULL or mistyped value in one row is a different thing: the
row is counted and reported, the rest of the store stays enumerable.

The default root follows the documented OpenClaw agent layout. The path is
listed as `official-docs` evidence in the harness registry. No real local
conversation data is used by the scanner tests; they build synthetic SQLite
stores through the shared test `Sandbox`.
