<!-- BEGIN GENERATED -->
# OpenAI Codex CLI

| Field | Value |
|---|---|
| Id | `codex` |
| Surface | cli |
| Status | verified end-to-end (2026-09-25) |
| Last verified end to end | 2026-09-25: real sessions read back and rendered from the real archive on a real machine (git 41db2bf); the founding pass sealed 91 real claude-code and codex sessions and matched the read-back hash (git 52c2e00) |

| Source root | OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|---|
| primary | macOS | `~/.codex/sessions/` | jsonl / jsonl.zst | source-confirmed | verified end-to-end (2026-09-25) |
| primary | Linux | `$HOME/.codex/sessions/` | jsonl / jsonl.zst | source-confirmed | verified end-to-end (2026-09-25) |
| primary | Windows | `%USERPROFILE%\.codex\sessions\` | jsonl / jsonl.zst | source-confirmed | verified end-to-end (2026-09-25) |
| codex-archived | macOS | `~/.codex/archived_sessions/` | jsonl / jsonl.zst | measured-locally | supported |
| codex-archived | Linux | `$HOME/.codex/archived_sessions/` | jsonl / jsonl.zst | source-confirmed | supported |
| codex-archived | Windows | `%USERPROFILE%\.codex\archived_sessions\` | jsonl / jsonl.zst | source-confirmed | supported |
<!-- END GENERATED -->

## What it is

Codex is the OpenAI coding agent CLI. It writes each session as a rollout file under `~/.codex/sessions/`, one JSONL stream of records per session, compressing the file with zstandard once it goes idle. The whole tree can be relocated with `CODEX_HOME`, which replaces the `.codex` layer entirely; the CLI does not follow the XDG variables. The desktop app drives the same CLI through its app-server and moves a thread's rollout into `~/.codex/archived_sessions/` when you archive it (and back when you unarchive), so that directory is scanned as a second root of the same store. This tool scans and archives both roots read-only, decompressing the idle `.jsonl.zst` files as it goes.

## What we collect

Every archived line is one record with a timestamp and a typed payload, and the payload's own type name decides what it is. The `response_item` records are the transcript the model saw, and they are what renders: `message` entries become turns with their speaker from the record's own role field; `reasoning` entries render as thinking, read from their summary — the reasoning content beside it is archived as the CLI wrote it, encrypted by the CLI itself, and the reader does not pretend to have read what it cannot; and the call records (`function_call` and its tool-shaped siblings) render as tool calls with the function name and arguments, their outputs as results. The `event_msg` records carry the same turns a second time at the application level — rendering both would show every turn twice — so they are recognized, counted and read through the raw route, not rendered. Session and turn-context metadata are counted the same way: recognized bookkeeping, not conversation.

## Coverage and completeness

The location is source-confirmed for every operating system from the CLI's own code: the session root is the home directory's `.codex/sessions`, with `CODEX_HOME` moving the whole home. The archived-threads root sits beside it under the same home and was measured locally on macOS (the desktop's archive and unarchive actions move a rollout between the two; they never copy it, so no session exists twice). The harness was verified end to end on 2026-09-25: real sessions were read back and rendered from the real archive, and the founding pass that sealed 91 real sessions across this harness and Claude Code matched against the read-back hash. What is recorded about the source's own retention is that idle rollouts are compressed rather than removed; the registry records no cleanup window for this harness either way, so archive on your own schedule rather than assuming one.

## Quirks

The double record is the double turn: the same moment exists once as the model-level `response_item` and once as the application-level `event_msg`, and the derived reading keeps exactly one rendering of it. Encrypted reasoning is the other deliberate boundary — the raw archive holds the bytes exactly as written, and the derived view shows only the summary the CLI itself made readable; nothing is decrypted, nothing is guessed. A thread that moves between `sessions/` and `archived_sessions/` keeps its session identity wherever it sits, because a rollout's name is its identity — the archive and unarchive round trip cannot fork a conversation into two. The round trip also never duplicates content: when the collector re-meets bytes the archive already holds at another path, it recognizes them and reads only what is genuinely new, and a sighting under the archived root is recorded as an observation bound to the one session's body rather than any second session.

## Known issues

No caveat is recorded for this harness in the registry or the reader. The two boundaries above — the application-level duplicate records, and reasoning text the CLI encrypts — are shown by the reader itself rather than hidden as issues, so a session that looks thinner than you expected is telling the truth about what was readable at the source. One counting rule is worth naming: a rollout that physically sits in both roots at once (the store moves files rather than copying them, but a manual copy left behind looks the same to the scan) is one session in every table — both doctor tables count by the rollout's uuid, so the same conversation seen twice is never two. Doctor reports that rule at two scopes: the family row counts the thread once, and the per-root row beside it (`primary`, `codex-archived`) counts what each root's own directory holds — a copy left in the archived root shows in the archived row, and neither number claims to be the other.

## How to check it yourself

`chat-stasher doctor` reports the store found and its session count — a thread counts once wherever the desktop app last moved it — and `chat-stasher status --json` carries the same count. After a `run-once`, `chat-stasher read` on a codex session shows speakers, thinking summaries and tool calls — one message per turn, not two — and the raw route shows the full record the way the CLI wrote it. `chat-stasher verify` checks what was staged is what the destination holds, and `chat-stasher search --text` finds text from the readable layers. As anywhere, a scratch copy under `[harness_roots]` lets you watch the scanner work without touching a live store, with the same path-keyed note: collecting through a copy, then switching back, re-delivers that source's rows once — duplicates that stay, never a loss.

## Changelog

2026-08-16 — the phase-one core archived, read back and rendered this harness's sessions, compressed idle rollouts included.
2026-09-25 — verified end to end: real sessions read back and rendered from the real archive; the founding pass sealed 91 real sessions across Claude Code and Codex and matched the read-back hash.
2026-10-16 — the desktop app's archived-threads root (`~/.codex/archived_sessions/`) is scanned as a second root of the same store; archiving and unarchiving a thread moves it between the roots without forking its session identity.
2026-10-17 — the uuid dedup between the two roots is applied end to end: a rollout sitting in both roots at once counts once in every table, archive → unarchive → re-archive round trips over a growing thread never re-seal bytes the archive already holds, and a discovery under the archived root is recorded as an `archived` status observation bound to the one session's body.
2026-10-17 — doctor's coverage table reports the store one row per source root (`primary`, `codex-archived`) beside the family row, each row carrying its own root's count and bytes; the family row is where the dedup by rollout uuid happens, so a thread counts once there while a physical copy still shows in the archived root's own row.
