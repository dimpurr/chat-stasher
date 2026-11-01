<!-- BEGIN GENERATED -->
# Zed

| Field | Value |
|---|---|
| Id | `zed` |
| Surface | ide |
| Status | verified end-to-end (2026-10-03) |
| Last verified end to end | 2026-10-03 (macOS): all locally present macOS sessions collected, pushed, and read back with matching SHA-256; user and agent text, thinking, and tool calls render and index; other recorded event types remain in the raw archive and are counted |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/Library/Application Support/Zed/threads/threads.db` | sqlite | measured-locally | verified end-to-end (2026-10-03) |
| Linux | `$XDG_DATA_HOME/zed/threads/threads.db` | sqlite | source-confirmed | supported |
| Windows | `%LOCALAPPDATA%\Zed\threads\threads.db` | sqlite | source-confirmed | supported |
<!-- END GENERATED -->

## What it is

Zed is the open-source editor from Zed Industries. Its assistant-panel and agent threads persist in one local SQLite store — the `threads` table of `~/Library/Application Support/Zed/threads/threads.db` on macOS (`$XDG_DATA_HOME/zed/threads/threads.db` on Linux, `~/.local/share/zed/threads/threads.db` when the variable is unset; `%LOCALAPPDATA%\Zed\threads\threads.db` on Windows). This tool reads that store read-only through the SQLite probe and archives every thread; the editor's main app database under `db/0-<channel>/db.sqlite` is not a conversation store and is not read.

## What we collect

One thread is one `threads` row — the macOS store was measured locally with `id`, `summary`, `updated_at`, `data_type` and `data` columns, `updated_at` written as ISO 8601 text — and the collector exports the whole row as one `chat-stasher.sqlite.session.v1` JSON line. The database is opened read-only with a short busy timeout, so a running editor is never interrupted and nothing this tool does writes into Zed's own files. The `data` column carries the recorded conversation: in the measured store it arrives as a `zstd`-compressed JSON payload (`data_type` names the codec). The exporter decompresses it before the row enters the archive envelope — a payload that fails to decompress fails that read as a named error rather than entering the archive as if it were content — and plain bytes that are not JSON keep a digest spelling rather than being guessed into content.

The reader renders the `messages` array of the decoded payload: single-key entries tagged `User` and `Agent` become User and Assistant messages, with content blocks rendered as text, thinking, and tool calls. A tool call carries its name and a compact input summary, and its result is matched by the result's own `tool_use_id`, so a matched call also shows its output's byte size — the opaque identifiers make that match internally and never reach the rendered view. An image block becomes a typed attachment reference: the payload bytes stay in the raw archive, and the reference claims the block existed, never an invented filename or a media decode. Everything else — record types the reader does not know, tool results it cannot match, entries that are empty or malformed — is counted as an unrendered record, never dropped silently.

Provenance: the `folder_paths` column states a thread's workspace folders — Zed stores the list newline-separated and sorted, `NULL` for a thread that belongs to no folder — and it exports as a JSON array. Every entry records a project/workspace identity in the archive's index (the container dimension), and the first entry also records the session's `cwd`. A `NULL` or empty value leaves both dimensions unobserved, and a value that is not text stays `NULL` at the export boundary rather than reaching the index as a folder identity the column never held.

## Coverage and completeness

The harness was verified end to end on 2026-10-03 on macOS: all locally present macOS sessions were collected, pushed, and read back with matching SHA-256; user and agent text, thinking, and tool calls render and index; other recorded event types remain in the raw archive and are counted. The macOS location is measured locally; the Linux and Windows locations are confirmed from Zed's own published source (`crates/agent/src/db.rs`, `crates/paths/src/paths.rs` in the `zed-industries/zed` repository) and remain unverified end to end. Per-message times are not rendered: the store records a session-level `updated_at` only, and the reader keeps each message's time unknown rather than borrowing the session's.

## Quirks

The store is one SQLite file, so its seal policy is `no-rename`: the live store's descriptor semantics are unconfirmed, and the tool never renames a database a running editor may hold. The compressed payload is expanded at export, so the archived export line carries the decompressed JSON rather than the `zstd` bytes the column holds.

## Known issues

Image payloads, tool result contents, compaction summaries, per-message timestamps, and unrecognized content remain available in the raw archive and are not rendered. The Linux and Windows locations are source-confirmed from upstream code only — neither operating system has had a measured store or an end-to-end run.

## How to check it yourself

Run `chat-stasher doctor` to confirm the Zed store is found and see its thread count. `chat-stasher run-once` archives the store's threads; `chat-stasher read` on a staged session renders user and agent text, thinking, and tool calls, and reports the records it did not render; `chat-stasher verify` checks that what was staged is what the destination holds; `chat-stasher search --text "<query>"` finds rendered text after `chat-stasher index build`.

## Changelog

2026-10-03 — Zed support first shipped: the `threads` store measured locally and probed read-only, `zstd` `data` payloads decompressed at export, ISO 8601 session times read, and the thread content rendered and indexed.
2026-10-03 — verified end to end on macOS: all locally present sessions collected, pushed and read back with matching SHA-256.
2026-10-06 — a `folder_paths` value that is not text is kept unobserved rather than read as a folder identity.
2026-10-09 — a thread's workspace folders reach the archive's index: every entry as the session's container identity, the first entry as its `cwd`.
