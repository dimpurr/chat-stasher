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
An unknown required table or column makes the source unenumerable; it is never
reported as an empty store.

The default root follows the documented OpenClaw agent layout. The path is
listed as `official-docs` evidence in the harness registry. No real local
conversation data is used by the scanner tests; they build synthetic SQLite
stores through the shared test `Sandbox`.
