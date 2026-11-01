<!-- BEGIN GENERATED -->
# opencode

| Field | Value |
|---|---|
| Id | `opencode` |
| Surface | cli |
| Status | verified end-to-end (2026-09-25) |
| Last verified end to end | 2026-09-25: the whole real store was exported into shards in one measured pass (git 1595419, git a908a00) and real sessions read back and rendered from the real archive (git 41db2bf) |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | verified end-to-end (2026-09-25) |
| Linux | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | verified end-to-end (2026-09-25) |
| Windows | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | verified end-to-end (2026-09-25) |
<!-- END GENERATED -->

## What it is

opencode (maintained by anomalyco, formerly sst/opencode) is an open-source TypeScript coding agent CLI. It stores its local sessions, messages, and execution parts in a SQLite database located at `$XDG_DATA_HOME/opencode/opencode.db` (defaulting to `~/.local/share/opencode/opencode.db` on macOS, Linux, and Windows). This tool scans and archives opencode sessions read-only; it never modifies the database file or interferes with running processes.

## What we collect

opencode persists conversation state across three relational tables: `session`, `message`, and `part`. The collector executes a read-only transaction (`BEGIN`) to read all three tables in a single atomic snapshot, preventing inconsistencies caused by concurrent writes from the CLI.

Each session's first observation is exported as a single JSON line under schema `chat-stasher.opencode.session.v1`. The export embeds the session row, each message row, and all child parts belonging to that message. The reader maps part types to conversation blocks: `text` becomes a Text block, `reasoning` becomes a Thinking block, and `tool` (with its input/output states) becomes a ToolCall block. Non-conversational parts (`step-start`, `step-finish`, `patch`) are counted as unrendered lines.

After the first observation, changed passes write **records** instead of new whole-session copies (schema `chat-stasher.opencode.session.v2`): a message record carries the message row without its parts inlined, each part is a record of its own under its own `part.id`, a session record fires when the session row itself changed, and a deletion the source performed (revert, compaction) is written down as an explicit tombstone record. Unchanged records are never re-written, so an hour that grows a session one to five percent writes about that one to five percent instead of a full copy. Readback assembles the session from every stored line together — whole-file observations and increment records mixed — taking the newest version of each record, so old sessions archived before this change are read the same way, with no conversion of anything already stored; a session whose shard set cannot be assembled is a loud read failure, never a short session.

If message rows have been purged (e.g. due to context compaction) but their parts remain in the database, these are exported under `orphan_parts` and counted rather than arbitrarily reordered into the conversation. Provenance dimensions are also extracted: the working directory from `session.directory`, the project container from `session.project_id`, and archival timestamps from `session.time_archived`.

The default view of an archived session is the source's final state — byte for byte what a fresh export of the same database rows would produce. `read --include-deleted` and `export --include-deleted` emit the records the source deleted at their original positions, each carrying `"deleted": true`; nothing is ever erased to match a deletion, so a session the tool reverted keeps the reverted turns archived and reachable.

## Coverage and completeness

All sessions present in `opencode.db` are collected in full across macOS, Linux, and Windows. The harness was verified end to end on 2026-09-25: the complete real database was exported into shards in a measured pass (commits 1595419, a908a00) and real sessions were read back and rendered from the archive (commit 41db2bf).

The database file path can be customized via the `OPENCODE_DB` environment variable, which the scanner detects and honors.

## Quirks

Because opencode stores all sessions inside a shared SQLite file rather than isolated JSONL files, the scanner uses a durable logical cursor rather than file byte offsets.

The seal policy is marked not-applicable because SQLite databases in WAL mode hold file locks and cannot be sealed via file renaming without breaking the running CLI.

## Known issues

Writes to a snapshot session are incremental per record since 2026-10-23, so an active session no longer re-stores a full copy per change; the remaining caveat is measurement, not behaviour: the one-day real-session amplification gate (written ≤ 1.2 × true growth per hour, assembled readback byte-identical to the source) has not yet been run on a live deputy session, so no absolute weekly write number is pinned yet.

## How to check it yourself

Run `chat-stasher doctor` to see whether the opencode SQLite database is detected, along with its total session count. Execute `chat-stasher run-once` to collect the sessions into the staging area. Run `chat-stasher read` on an opencode session to see its shard set and the digest of its assembled body, and `chat-stasher read --include-deleted` (or `export --include-deleted`) to reach the records a revert or compaction removed. Run `chat-stasher verify` to check cryptographic shard digests, and `chat-stasher search --text "<query>"` to perform full-text search across all sessions.

## Changelog

2026-08-16 — multi-table SQLite scanner (`session`, `message`, `part`) implemented for opencode.
2026-09-25 — verified end to end: entire real database exported into shards and read back successfully (commits 1595419, a908a00, 41db2bf).
2026-10-11 — provenance dimensions (`cwd`, `project_id`, `time_archived`) extracted and integrated into search indexing.
2026-10-12 — orphan part handling for compacted messages pinned by test fixtures.
2026-10-23 — snapshot sources go increment per record (ADR-061): changed sessions write message-row, part and tombstone records under `chat-stasher.opencode.session.v2`; readback assembles whole-file and increment lines by last-version-per-record, the default view equals the source's final state, and `read`/`export --include-deleted` reach deleted records at their original positions. The one-day live-session amplification gate is still pending.
2026-10-27 — post-reclaim summary baseline (SG-2b): when stage shard bodies are deleted by reclaim, for a destination that holds a cursor the retained per-session manifest summary (ADR-020 role C) and the record-digest list under meta act as the baseline so unchanged sessions write zero bytes and changed sessions write only the increment.
