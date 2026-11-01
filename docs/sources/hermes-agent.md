<!-- BEGIN GENERATED -->
# Hermes Agent

| Field | Value |
|---|---|
| Id | `hermes-agent` |
| Surface | cli |
| Status | verified end-to-end (2026-10-06) |
| Last verified end to end | 2026-10-06 (macOS): macOS: all 420 real sessions in the sqlite state store were collected into an isolated stage, pushed to a throwaway local destination, and read back; five independent SHA-256 digests (export manifest, exported file, staged shards, archive read-back, expected source) agreed for every session. Linux and Windows remain unverified. |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.hermes/state.db` | sqlite | official-docs | verified end-to-end (2026-10-06) |
| Linux | `~/.hermes/state.db` | sqlite | official-docs | supported |
| Windows | `~/.hermes/state.db` | sqlite | official-docs | supported |
<!-- END GENERATED -->

## What it is

Hermes Agent is [NousResearch's open-source agent](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/session-storage-recovery.md). Its durable state sits in one SQLite file — `~/.hermes/state.db`, per the upstream's own session-storage documentation — with an earlier layout's session files under `~/.hermes/sessions` kept beside it. This tool reads both, read-only: the store is scanned through the SQLite reader, and the older JSON/JSONL files are scanned as compatibility sources so a pre-store session still lands in the archive.

## What we collect

The scanner enumerates the store's `sessions` table — one session row per session, its `id` the native id — and the collector reads, inside one WAL-aware read-only SQLite transaction, that row, its `messages` rows and its `session_model_usage` rows, exporting them as a single `chat-stasher.hermes-agent.session.v1` JSON line. The three tables' schemas are validated before anything is read (`sessions` needs `id`; `messages` needs `id`, `session_id`, `role`; `session_model_usage` needs `session_id`, `model`, `billing_provider`), and a mismatch is a named refusal, never an archive that looks empty. The reader renders a message row whose `role` names a speaker it knows and whose `content` is non-empty text, timestamped from the row's own `timestamp`; a row with neither is counted, not guessed into prose. Model, provider and token/cost aggregates — the `session_model_usage` rows — stay session-level metadata: the source records no per-message attribution, and none is invented onto messages. The collect cursor is a content hash of the exported line. Legacy `~/.hermes/sessions` files (`.json` and `.jsonl`) are deduplicated by their file-stem session id against the store's records, so the same session is never archived twice — and when you state a harness root yourself (`[harness_roots]`), that root is the whole source and the legacy scan is not run behind it.

## Coverage and completeness

The location is documented by upstream for all three operating systems (`~/.hermes/state.db`), and the harness was verified end to end on 2026-10-06 on macOS: all 420 real sessions in the sqlite state store were collected into an isolated stage, pushed to a throwaway destination and read back, with five independent SHA-256 digests agreeing for every session. Linux and Windows remain unverified end to end.

## Quirks

A SQLite store in WAL mode can be live-written by the agent while it is scanned, so the open is read-only (`mode=ro`, which still reads committed WAL content) inside a transaction, with a short busy timeout; no database or sidecar is ever opened for writing. The seal policy is not-applicable for the same reason as other SQLite sources: a live store holds locks that make a file-rename seal meaningless.

## Known issues

Session-level model/provider/token-cost aggregates are not assigned to messages — the state store's shape gives session totals, not per-message attribution, and the archive keeps that boundary instead of inventing one. No caveat is recorded for the read itself.

## How to check it yourself

`chat-stasher doctor` reports the store found and its session count. `chat-stasher run-once` collects the sessions, `chat-stasher read` renders one turn per message row with its recorded time, `chat-stasher verify` checks what was staged against the destination, and `chat-stasher search --text` finds message text after `chat-stasher index build`.

## Changelog

2026-10-03 — first shipped: the scanner reads `~/.hermes/state.db` read-only and the legacy `~/.hermes/sessions` JSON/JSONL as deduplicated compatibility sources.
2026-10-06 — verified end to end on macOS: all 420 real sessions collected, pushed and read back, with five SHA-256 digests agreeing for every session.
