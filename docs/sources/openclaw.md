<!-- BEGIN GENERATED -->
# OpenClaw

| Field | Value |
|---|---|
| Id | `openclaw` |
| Surface | cli |
| Status | supported |
| Last verified end to end | not yet verified end to end |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `$HOME/.openclaw/agents` | sqlite | official-docs | supported |
| Linux | `$HOME/.openclaw/agents` | sqlite | official-docs | supported |
| Windows | `$HOME/.openclaw/agents` | sqlite | official-docs | supported |
<!-- END GENERATED -->

## What it is

OpenClaw is an open-source agent platform (upstream project `openclaw/openclaw`, MIT) that runs agents from the command line. This page is about the sessions it keeps on your machine. Each agent has its own directory under `~/.openclaw/agents`; the registry's source for that layout is the upstream session documentation (`docs.openclaw.ai/concepts/session`, confidence `official-docs`). The tool reads those files read-only and never writes into OpenClaw's own files.

## What we collect

For each direct child of the agents root the scanner looks for `agent/openclaw-agent.sqlite` and for compressed JSONL files under `sessions/` ([scanner notes](../../docs-dev/openclaw-scanner.md)). From the SQLite store it reads the `session_windows`, `transcript_events` and `session_transcript_archives` tables; every identifiable window row and archive row becomes its own source record. A window record exports the window's columns plus its transcript events ordered by `seq`, each event's `event_json` parsed and kept with its `seq` and `created_at`, so model, provider, usage and cost fields stay in the raw event. An archive record keeps the archive metadata and the `archive_blob` bytes with their declared encoding and SHA-256, and collection checks the blob against that digest before accepting it.

Cold files under `sessions/` are considered when the extension is `.zst`; they are skipped when an identifiable archive row for the same agent and session id was enumerated. A kept cold file is decompressed in full and its complete newline-terminated JSONL lines are preserved in the raw archive; a trailing line still missing its newline is held as in progress and sealed in full on the first pass after the newline arrives. The agent directory name is part of each archived session id, so two agents cannot collide.

Reading: OpenClaw has an extractor of its own that renders the message text of the SQLite window events (`message.role` and `message.content` of each event) and counts every other event record as unrendered. The notes state that the extractor does not interpret the native line format of cold files, so those lines are archived but remain unrendered.

## Coverage and completeness

The generated header says `not yet verified end to end`: no real OpenClaw session has a recorded end-to-end run. The scanner's tests build synthetic SQLite stores and cold files, and the claims above come from those tests and the scanner notes, not from a live installation. The macOS, Linux and Windows cells all share the template `$HOME/.openclaw/agents`, each at confidence `official-docs`; no per-OS measurement is recorded.

When a database cannot be enumerated (a required table or column is missing) that agent's store is unenumerable and the other stores and cold files continue to be scanned; unreadable rows are counted rather than dropped.

## Quirks

SQLite connections are opened read-only and existing WAL sidecars are included in the footprint measurement. If a WAL-mode database has no `-shm` sidecar, the scanner uses SQLite's immutable main-file-only read path so it does not create sidecars; that view may omit changes still waiting in an uncheckpointed WAL. The cold-file name carries no generation, so an archive row for one generation can suppress a cold file belonging to a sibling generation. Archived ids can grow long (hex-encoding doubles the byte count): past 255 bytes the id keeps a UTF-8 prefix plus a SHA-256-derived suffix, identical on every machine ([scanner notes](../../docs-dev/openclaw-scanner.md#bounded-ids)). Rollover windows stay separate observations, with the logical session key and parent/fork fields retained in the raw window record. `[harness_roots]` can point `openclaw` at another agents root (the scanner notes state this; see [config](../config.md#harness_roots) for the table form).

## Known issues

The registry's known-issue line for this source is that the cold transcript matching rules are documented in the scanner notes (above): the filename-only dedupe against archive rows and the unrendered cold-file lines are the two behaviours that matter to a reader.

## How to check it yourself

`chat-stasher doctor` reports the source and its session count; `chat-stasher run-once` collects; `chat-stasher read` renders a window's message text; `chat-stasher search --text` finds it after `chat-stasher index build`. To try it without touching the live tree, copy `~/.openclaw/agents` elsewhere and point `[harness_roots]` `openclaw` at the copy.

## Changelog

2026-10-03 — read-only OpenClaw session scanning was added (git history of the registry entry). No end-to-end verification has been recorded since.
