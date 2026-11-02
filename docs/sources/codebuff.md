<!-- BEGIN GENERATED -->
# Codebuff / Freebuff

| Field | Value |
|---|---|
| Id | `codebuff` |
| Surface | cli |
| Status | supported |
| Last verified end to end | not yet verified end to end |

| Source root | OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|---|
| codebuff | macOS | `~/.config/manicode/` | json | source-confirmed | supported |
| codebuff | Linux | `$HOME/.config/manicode/` | json | unascertained | not supported |
| codebuff | Windows | `%USERPROFILE%\.config\manicode\` | json | unascertained | not supported |
| codebuff-dev | macOS | `~/.config/manicode-dev/` | json | source-confirmed | supported |
| codebuff-dev | Linux | `$HOME/.config/manicode-dev/` | json | unascertained | not supported |
| codebuff-dev | Windows | `%USERPROFILE%\.config\manicode-dev\` | json | unascertained | not supported |
| codebuff-staging | macOS | `~/.config/manicode-staging/` | json | source-confirmed | supported |
| codebuff-staging | Linux | `$HOME/.config/manicode-staging/` | json | unascertained | not supported |
| codebuff-staging | Windows | `%USERPROFILE%\.config\manicode-staging\` | json | unascertained | not supported |
<!-- END GENERATED -->

## What it is

Codebuff is a terminal coding agent, and Freebuff is its free, sponsored flavour: one product under two npm names. On disk the harness still uses its legacy name, `manicode`. A chat lives at `~/.config/manicode/projects/<project-basename>/chats/<chat-id>/`, and the `-dev` and `-staging` build channels use sibling roots (`~/.config/manicode-dev/`, `~/.config/manicode-staging/`) that are builds of the same tool, not separate surfaces (registry entry and `crates/chat-stasher/src/codebuff.rs`). The chat id is the session start time in ISO-8601 with `:` replaced by `-`, not a UUID. The tool reads these files read-only and never writes into the harness's own directories.

## What we collect

Exactly two files per chat: `chat-messages.json` (the transcript, a JSON array) and `chat-meta.json` (the sidecar, whose `firstPrompt` is a truncated copy of your typed text, so it is body-tier data like the transcript). The run state, the debug log and half-written temp files are excluded by this two-file whitelist, and `message-history.json` (the per-machine input log) is not part of this capture ([privacy notes](../../docs-dev/privacy.md)). The transcript is sealed whole, byte for byte, as raw bytes, one full generation per pass that saw the file change. The native session id is `<project-basename>.<chat-id>`; the basename is identity, never a container, and it is not parsed as a working directory.

The session's start time is read from the chat directory's own name and recorded at capture with its source named (`chat-directory-name`); it is never re-read from the archive id, because a long project basename gets the id bounded at the tail and the timestamp goes with it. The recorded time is cross-checked against the directory's mtime: a directory that precedes the recorded time is written as an observation, and the recorded time still stands.

`FREEBUFF_CONFIG_DIR` replaces the whole base directory for every build: when it is set, the primary root reads the override and the dev and staging roots are not scanned; when it is unset, all three roots scan.

Reading: the reader takes the latest generation of the transcript. `variant: "user"` renders as the user; `"ai"` and `"agent"` render as the assistant; `"error"` is not a turn and is counted unrendered. Text blocks render (as thinking where the source marked the text as reasoning), tool blocks render as tool calls, and unknown block types are counted unrendered rather than guessed (`crates/chat-stasher/src/normalize/codebuff.rs`).

## Coverage and completeness

The generated header says `not yet verified end to end`: no real Codebuff session has a recorded end-to-end run. The macOS cells are `source-confirmed` from the upstream source (`cli/src/utils/config-dir.ts`, `cli/src/project-files.ts`, read 2026-10-08 on an unpinned branch); the same layout was probed read-only (counts and field names only) on one machine on 2026-10-06, and no install was present on the machine the entry was written on (checked 2026-10-13). The Linux and Windows cells are `unascertained`, so the scanner refuses to guess those paths.

A transcript that cannot be read whole and validated as a JSON array that pass is skipped: nothing is sealed for it, not even partial bytes, and the run reports exit code 3 so an incomplete scan cannot look complete. The skip leaves one bounded record with a registry root id, the session's directory-name identity, a byte size and a closed-enum reason (`torn-json`, `not-an-array`, `unreadable`, `io-error`), and no content. Because the harness writes the transcript temp-then-rename in its own directory, a copy observes a complete generation or a torn file that gets skipped.

## Quirks

Per-message `timestamp` values in the transcript are a display clock face with no date, so a message has no known time; the reader never parses or rebuilds one. System-injected messages are written as `"ai"` by the harness itself, so they read as assistant text; that is a declared limitation of the source. Earlier sealed generations are not re-rendered by the reader, but stay in the raw archive.

## Known issues

From the registry: attachment bytes are not archived and per-message times stay unknown; sessions render through the reader, while attachments stay in the raw transcript only. Windows and Linux are unmeasured.

## How to check it yourself

`chat-stasher doctor` reports the sessions found under each root; `chat-stasher run-once` collects them and exits 3 with a PARTIAL report when a transcript was skipped; `chat-stasher read` renders a chat from its latest generation; `chat-stasher search --text` finds its text after `chat-stasher index build`. To try it on a copy, copy `~/.config/manicode` elsewhere and set `FREEBUFF_CONFIG_DIR` to the copy.

## Changelog

2026-10-13 — Codebuff / Freebuff session archiving added under the ADR-054 rules (the whitelist, the whole-file sealing, the skip observation and the start-time cross-check).
