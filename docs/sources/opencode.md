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

Each session is exported as a single JSON line under schema `chat-stasher.opencode.session.v1`. The export embeds the session row, each message row, and all child parts belonging to that message. The reader maps part types to conversation blocks: `text` becomes a Text block, `reasoning` becomes a Thinking block, and `tool` (with its input/output states) becomes a ToolCall block. Non-conversational parts (`step-start`, `step-finish`, `patch`) are counted as unrendered lines.

If message rows have been purged (e.g. due to context compaction) but their parts remain in the database, these are exported under `orphan_parts` and counted rather than arbitrarily reordered into the conversation. Provenance dimensions are also extracted: the working directory from `session.directory`, the project container from `session.project_id`, and archival timestamps from `session.time_archived`.

## Coverage and completeness

All sessions present in `opencode.db` are collected in full across macOS, Linux, and Windows. The harness was verified end to end on 2026-09-25: the complete real database was exported into shards in a measured pass (commits 1595419, a908a00) and real sessions were read back and rendered from the archive (commit 41db2bf).

The database file path can be customized via the `OPENCODE_DB` environment variable, which the scanner detects and honors.

## Quirks

Because opencode stores all sessions inside a shared SQLite file rather than isolated JSONL files, the scanner uses a durable logical cursor rather than file byte offsets.

The seal policy is marked not-applicable because SQLite databases in WAL mode hold file locks and cannot be sealed via file renaming without breaking the running CLI.

## Known issues

A one-line modification in an active session currently causes the collector to re-export the entire session as a new full snapshot shard rather than an incremental delta (commit a908a00).

## How to check it yourself

Run `chat-stasher doctor` to see whether the opencode SQLite database is detected, along with its total session count. Execute `chat-stasher run-once` to collect the sessions into the staging area. Run `chat-stasher read` on an opencode session to see messages, reasoning blocks, and tool executions. Run `chat-stasher verify` to check cryptographic shard digests, and `chat-stasher search --text "<query>"` to perform full-text search across all sessions.

## Changelog

2026-08-16 — multi-table SQLite scanner (`session`, `message`, `part`) implemented for opencode.
2026-09-25 — verified end to end: entire real database exported into shards and read back successfully (commits 1595419, a908a00, 41db2bf).
2026-10-11 — provenance dimensions (`cwd`, `project_id`, `time_archived`) extracted and integrated into search indexing.
2026-10-12 — orphan part handling for compacted messages pinned by test fixtures.
