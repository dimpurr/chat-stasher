<!-- BEGIN GENERATED -->
# Grok (xAI CLI)

| Field | Value |
|---|---|
| Id | `grok` |
| Surface | cli |
| Status | verified end-to-end (2026-10-03) |
| Last verified end to end | 2026-10-03 (macOS): real sessions read back and rendered from the real archive on a real machine; 9 session_docs rows matched read-back SHA-256s, FTS indexed all 9, activity matched all 9 updated_at values and 8 titles, and the remaining empty title stayed unlabeled (reader/render implementation commit 7e7ef29) |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.grok/sessions/session_search.sqlite` | sqlite | measured-locally | verified end-to-end (2026-10-03) |
| Linux | `$HOME/.grok/sessions/session_search.sqlite` | sqlite | unascertained | not supported |
| Windows | `%USERPROFILE%\.grok\sessions\session_search.sqlite` | sqlite | unascertained | not supported |
<!-- END GENERATED -->

## What it is

The Grok CLI keeps each session in its own directory, shaped much like Claude Code's: one directory per session under a sanitized working-directory path, holding the session's own JSONL transcripts and a search database. This tool reads that store read-only. Only the macOS location is measured; the Linux and Windows cells exist in the registry but are unascertained, so on those systems the scanner does not act on them.

## What we collect

One session can be observed by three different producers, and this tool archives all three under a single session id, so one conversation stays one conversation. First, the search database `session_search.sqlite` holds one `session_docs` row per session: the session id, a title, an update time in Unix seconds, and the conversation body as plain text. The row preserves no speaker roles, no turn boundaries and no per-message timestamps — it is a summary of the conversation, not its transcript. Second, each session directory can hold a `usage.json` file, a usage manifest this tool archives whole as a raw file beside the row. Third, the directory holds the CLI's own native transcript, `chat_history.jsonl`, whose lines are typed records — `system`, `user`, `reasoning`, `assistant` and `tool_result` — and since one archive change this is collected too, for every session directory including sessions that have no `session_docs` row at all. The `events.jsonl` sibling that the CLI also writes is not archived in this build.

The derived reading prefers the native transcript: when a body holds native records that render at least one message, the reading is built from those typed turns, with their speakers and their tool traffic, and the summary line is not appended again as if it were another turn. When a body holds only the summary — an older archive, a session the CLI never wrote a native transcript for — that summary stays the reading, rendered as one text block with the speaker explicitly unknown and the session's update time as its time. The raw route keeps both observations either way. Native records carry no timestamps at all, so a native reading trades the summary's one coarse time for no per-message times; that is the source's own shape, and nothing is invented to fill it.

## Coverage and completeness

What is archived per session: the `session_docs` row (always, when the row exists), the linked usage file (when its session has a row), and the native transcript (always, when the file exists). The count that `status` and `doctor` report for this harness is the store's `session_docs` row count only — the scanner's own probe note, and the collect summary, name all three populations — so the archive can legitimately hold many more units than the reported number: in one measured installation the store held 174 rows while the archive held 529 units, the extra ones being 158 usage files and 197 native transcripts, 23 of which belonged to sessions with no row at all. That is three honest counts of three honest things, not a discrepancy to resolve.

The search database is opened read-only, always. When a WAL-mode database has its shared-memory sidecar, committed-but-uncheckpointed rows are part of the read; when it does not (SQLite read-only WAL cannot create one, and Grok's own store has been observed in exactly that state), the read falls back to the main file alone, and rows committed only to the write-ahead log stay invisible until the CLI checkpoints. Either way the probe never creates or touches the `-wal` or `-shm` siblings, so the live store stays byte-identical to before the scan.

## Quirks

The session id is shared deliberately between the row export and the native transcript, and that is why one body can hold both: a separate id per producer would read as two sessions and duplicate the conversation. The summary suppression is per body, not global — a body whose native records all render nothing (recognized but empty) keeps its summary, because that summary is the only readable text it has.

Sessions whose native transcripts predate native collection, or whose `chat_history.jsonl` the CLI never wrote, read as unknown-speaker summaries — the shape the row itself provides. If you archive on Linux or Windows today by pointing `[harness_roots]` at the store yourself, you are collecting ahead of the registry's evidence: the cells are unascertained, not refused, but nothing there has been verified.

## Known issues

The registry's standing caveat for this harness is the summary's lossiness: the `session_docs` row preserves plain text and title but not speaker roles, turn boundaries, or per-message timestamps, and the reader labels the speaker unknown and uses the session update time. Sessions with a native transcript in the archive stopped paying that cost when readable native records land; sessions without one still pay it.

## How to check it yourself

`chat-stasher doctor` reports the store found and its session count; `chat-stasher status --json` carries the same count per harness, and the collect output prints the three populations in one line — rows, usage files and native transcripts — so you can see the units behind the count. `chat-stasher read` on a `grok` session shows the difference directly: a session with native records renders typed turns with speakers and tool calls, while a row-only session renders one unknown-speaker summary. `chat-stasher verify --level l3` then proves what was staged is what the destination holds, and a text search finds text from either shape. Everything reads the store read-only; the CLI's own files are never moved or rewritten.

## Changelog

2026-08-17 — the macOS store was located and measured: the search database opened read-only with its `session_docs` table pinned beside the per-session directories.
2026-10-03 — verified end to end: real row-archived sessions were read back and rendered from the real archive, with read-back hashes, full-text search and activity all matching the store's rows.
2026-10-11 — the native `chat_history.jsonl` transcripts are archived for every session directory, and the derived reading prefers their typed turns over the summary when they render.
2026-10-12 — the native collect's interleavings and its per-session id census were pinned by tests.
