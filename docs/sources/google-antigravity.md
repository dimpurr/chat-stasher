<!-- BEGIN GENERATED -->
# Google Antigravity

| Field | Value |
|---|---|
| Id | `google-antigravity` |
| Surface | cli, ide, desktop app |
| Status | verified end-to-end (2026-10-03) |
| Last verified end to end | 2026-10-03 (macOS): macOS local roots: 72 transcript.jsonl files across CLI, IDE, and app collected and indexed. Archive push/readback verified full SHA-256 for every session; archive search and 72-document FTS check passed; archive-only activity rebuild preserved SHA-bound surface observations. Linux and Windows remain unverified. |

| Source root | OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|---|
| antigravity-cli | macOS | `~/.gemini/antigravity-cli/brain/` | jsonl | measured-locally | verified end-to-end (2026-10-03) |
| antigravity-cli | Linux | `$HOME/.gemini/antigravity-cli/brain/` | jsonl | unascertained | not supported |
| antigravity-cli | Windows | `%USERPROFILE%\.gemini\antigravity-cli\brain\` | jsonl | unascertained | not supported |
| antigravity-ide | macOS | `~/.gemini/antigravity-ide/brain/` | jsonl | measured-locally | verified end-to-end (2026-10-03) |
| antigravity-ide | Linux | `$HOME/.gemini/antigravity-ide/brain/` | jsonl | unascertained | not supported |
| antigravity-ide | Windows | `%USERPROFILE%\.gemini\antigravity-ide\brain\` | jsonl | unascertained | not supported |
| antigravity | macOS | `~/.gemini/antigravity/brain/` | jsonl | measured-locally | verified end-to-end (2026-10-03) |
| antigravity | Linux | `$HOME/.gemini/antigravity/brain/` | jsonl | unascertained | not supported |
| antigravity | Windows | `%USERPROFILE%\.gemini\antigravity\brain\` | jsonl | unascertained | not supported |
<!-- END GENERATED -->

## What it is

Google Antigravity is Google's agent platform spanning CLI, IDE, and desktop application surfaces. Across all three surfaces, it records structured event logs in session directories under `~/.gemini/`. This tool scans all three roots and archives session transcripts read-only without modifying source files.

## What we collect

The platform uses three distinct source roots under `~/.gemini/`, one per surface:

- `antigravity-cli`: `~/.gemini/antigravity-cli/brain/` (surface: `cli`)
- `antigravity-ide`: `~/.gemini/antigravity-ide/brain/` (surface: `ide`)
- `antigravity`: `~/.gemini/antigravity/brain/` (surface: `app`)

The registry holds each root as its session path template; inside each root, a session is one directory, and its transcript sits at `.system_generated/logs/transcript.jsonl` within that session directory.

The log is an append-only JSONL event stream. The normalizer processes event types:
- `USER_INPUT` records map to User messages.
- `PLANNER_RESPONSE` records map to Assistant messages, extracting conversational content, `thinking` text, and structured `tool_calls` (name and input arguments).
- Other event types (such as `CHECKPOINT` or internal runtime signals) are counted as unrendered records until their semantics are verified.

The surface provenance (`cli`, `ide`, `app`) is recorded for each session based on the originating root. Sessions are sealed using a copy rather than renaming (`seal_policy: "no-rename"`).

## Coverage and completeness

Verified end to end on macOS on 2026-10-03: 72 `transcript.jsonl` files across CLI, IDE, and desktop app roots were collected, sealed, and indexed. Full SHA-256 integrity was confirmed on readback, full-text search indexed all 72 documents, and activity rebuilds preserved surface observations.

Linux and Windows locations are defined in the registry but remain unascertained and unverified.

## Quirks

A single logical harness identifier (`google-antigravity`) aggregates three distinct source roots corresponding to different user-facing surfaces.

If configured with manual `[harness_roots]`, pointing the harness to a single parent directory that conflates multiple roots is rejected as ambiguous by the scanner.

## Known issues

Only macOS source locations have been measured and verified locally; Linux and Windows paths remain unascertained.

Only surface provenance (`cli`, `ide`, `app`) is recorded; no project container, tenant, or session lifecycle status is inferred.

## How to check it yourself

On macOS, run `chat-stasher doctor` to see detected sessions across the CLI, IDE, and app roots. Run `chat-stasher run-once` to archive sessions without modifying the original log files. Use `chat-stasher read` to view rendered User turns and Planner responses (including reasoning thoughts and tool invocations). Confirm archive consistency with `chat-stasher verify` and search transcripts with `chat-stasher search --text "<query>"`.

## Changelog

2026-10-01 — multi-surface scanner supporting CLI, IDE, and desktop app roots implemented on macOS.
2026-10-03 — verified end to end on macOS: 72 sessions across all three surfaces archived, read back, and indexed with full SHA-256 verification.
2026-10-11 — normalizer updated to extract `USER_INPUT`, `PLANNER_RESPONSE`, thinking blocks, and tool calls.
2026-10-12 — ambiguous manual root configuration guard verified by unit tests.
