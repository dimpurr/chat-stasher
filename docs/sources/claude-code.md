<!-- BEGIN GENERATED -->
# Claude Code

| Field | Value |
|---|---|
| Id | `claude-code` |
| Surface | cli |
| Status | verified end-to-end (2026-09-25) |
| Last verified end to end | 2026-09-25: real sessions read back and rendered from the real archive on a real machine (git 41db2bf); the founding pass sealed 91 real sessions and matched the read-back hash (git 52c2e00) |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.claude/projects/<sanitized-cwd>/<uuid>.jsonl` | jsonl | source-confirmed | verified end-to-end (2026-09-25) |
| Linux | `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl` | jsonl | official-docs | verified end-to-end (2026-09-25) |
| Windows | `%USERPROFILE%\.claude\projects\<sanitized-cwd>\<session-uuid>.jsonl` | jsonl | official-docs | verified end-to-end (2026-09-25) |
<!-- END GENERATED -->

## What it is

Claude Code is Anthropic's coding agent CLI. It keeps each session as one JSONL file under `~/.claude/projects/`, in a directory named after the session's working directory, plus one journal per workflow run underneath the parent session's directory. This tool scans and archives those files read-only; the CLI's own files are never moved or rewritten.

## What we collect

The session lines that carry conversation are the `user` and `assistant` entries: their text, thinking and code render as messages in the derived reading, with the speaker from the line's own type field and the time from the line's timestamp. A whole turn whose content is only tool traffic — the results the CLI hands back under a user line — renders as tool output rather than as the person speaking; a turn that mixes prose with tool traffic keeps its recorded speaker, so visible text is never mislabeled. Lines marked as metadata or as sidechain (the CLI's own bookkeeping and its subagent chains) still render, as thinking, so the archive keeps them without letting them masquerade as conversation. Tool calls render with their call shape and their results.

Beyond the sessions themselves, Claude Code Workflows writes one `journal.jsonl` per workflow run under the parent session's subagents directory, and each run is keyed by its own run directory rather than the constant file name — without that, every run of the same session would collapse into one id. `CLAUDE_CONFIG_DIR` moves the entire base directory, and the same runs that honor it in the CLI honor it in the scan.

## Coverage and completeness

All three operating systems have established locations, and the whole harness was verified end to end on 2026-09-25: real sessions were read back and rendered from the real archive, on a machine whose founding pass sealed 91 real sessions and matched them against the read-back hash. What can still be incomplete is the source, not the archive: Claude Code cleans up its own local sessions after 30 days by default, so a session deleted client-side before a collection runs is gone from the source when the scanner next looks — the archive keeps everything it ever collected, which is the whole reason to archive often here.

## Quirks

The working-directory segment of a session path is sanitized, and its edge cases are where sessions hide. Short paths keep a readable slug; long ones (over roughly 200 characters) become a fixed path-hash directory instead. On Windows that hash is case-sensitive: the same project reached as `c:\…` and as `C:\…` produces two different directories, so sessions can appear to have disappeared when they have merely moved next door. How the drive-letter colon and the backslash are spelled inside short-path slugs on Windows has not been measured, and the page claims nothing about it.

## Known issues

The registry's standing caveat for this harness is exactly the Windows quirk above: on Windows the long-path directory hash is case-sensitive, so sessions can seem to disappear, and the drive-letter and backslash sanitization of short paths is still unmeasured.

## How to check it yourself

`chat-stasher doctor` reports the store found and its session count on your machine; `chat-stasher status --json` carries the same per-harness count. After a `run-once`, `chat-stasher read` renders a chosen session — visible text with its speakers, thinking as thinking, tool traffic as tool output — and `chat-stasher export --turns user` writes out exactly the person's messages for this harness. `chat-stasher verify` then checks that what was staged is what the destination holds. A safe way to see the scanner work without touching a live store is to `cp -Rp` a session directory to a scratch location and point `[harness_roots]` at the copy — knowing that the collect cursor is keyed by the source path, so a later ordinary collect from the real location re-delivers that source's rows once: duplicate rows that stay, never a loss.

## Changelog

2026-08-16 — the phase-one core scanned, stored and read back this harness's sessions.
2026-09-25 — verified end to end: the founding pass sealed 91 real sessions, matched the read-back hash, and real sessions were read back and rendered from the real archive.
2026-10-11 — workflow journals are keyed by their workflow-run directory, so each run of a session keeps its own identity in the archive.
