<!-- BEGIN GENERATED -->
# Claude Code

| Field | Value |
|---|---|
| Id | `claude-code` |
| Surface | cli, desktop app (Cowork) |
| Status | verified end-to-end (2026-09-25) |
| Last verified end to end | 2026-09-25: real sessions read back and rendered from the real archive on a real machine (git 41db2bf); the founding pass sealed 91 real sessions and matched the read-back hash (git 52c2e00) |

| Source root | OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|---|
| primary | macOS | `~/.claude/projects/<sanitized-cwd>/<uuid>.jsonl` | jsonl | source-confirmed | verified end-to-end (2026-09-25) |
| primary | Linux | `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl` | jsonl | official-docs | verified end-to-end (2026-09-25) |
| primary | Windows | `%USERPROFILE%\.claude\projects\<sanitized-cwd>\<session-uuid>.jsonl` | jsonl | official-docs | verified end-to-end (2026-09-25) |
| claude-desktop-cowork | macOS | `~/Library/Application Support/Claude/local-agent-mode-sessions/` | jsonl | measured-locally | supported |
| claude-desktop-cowork | Linux | - | - | - | not supported |
| claude-desktop-cowork | Windows | `%APPDATA%\Claude\local-agent-mode-sessions\` | jsonl | unascertained | not supported |
<!-- END GENERATED -->

## What it is

Claude Code is Anthropic's coding agent CLI. It keeps each session as one JSONL file under `~/.claude/projects/`, in a directory named after the session's working directory, plus one journal per workflow run underneath the parent session's directory. Claude Desktop's Cowork sessions are a second store of the same harness: the app gives each one its own sandbox home under its application-support tree, and the transcript inside is a `.claude/projects/.../.jsonl` file the same binary writes — the app's metadata json beside the sandbox names that file with its `cliSessionId`, which is the session's id here too, and contributes a title and timestamps rather than identity. This tool scans and archives both stores read-only; the CLI's own files are never moved or rewritten.

## What we collect

The session lines that carry conversation are the `user` and `assistant` entries: their text, thinking and code render as messages in the derived reading, with the speaker from the line's own type field and the time from the line's timestamp. A whole turn whose content is only tool traffic — the results the CLI hands back under a user line — renders as tool output rather than as the person speaking; a turn that mixes prose with tool traffic keeps its recorded speaker, so visible text is never mislabeled. Lines marked as metadata or as sidechain (the CLI's own bookkeeping and its subagent chains) still render, as thinking, so the archive keeps them without letting them masquerade as conversation. Tool calls render with their call shape and their results.

Beyond the sessions themselves, Claude Code Workflows writes one `journal.jsonl` per workflow run under the parent session's subagents directory, and each run is keyed by its own run directory rather than the constant file name — without that, every run of the same session would collapse into one id. `CLAUDE_CONFIG_DIR` moves the entire base directory, and the same runs that honor it in the CLI honor it in the scan.

## Coverage and completeness

All three operating systems have established locations for the CLI store, and the whole harness was verified end to end on 2026-09-25: real sessions were read back and rendered from the real archive, on a machine whose founding pass sealed 91 real sessions and matched them against the read-back hash. The desktop Cowork root is measured on macOS — the app keeps every Cowork session in its own sandbox, and zero of those ids exist in the CLI store — so those conversations join the archive under the same id space as CLI sessions, marked with their surface. Its Windows location is recorded but unmeasured, so the Windows scan refuses to walk it until measured on a real Windows install, and no Linux location is declared because the desktop app has no Linux build. What can still be incomplete is the source, not the archive: Claude Code cleans up its own local CLI sessions after 30 days by default, so a session deleted client-side before a collection runs is gone from the source when the scanner next looks — the archive keeps everything it ever collected, which is the whole reason to archive often here. Desktop and Cowork transcripts are exempt from that default sweep: their retention is bounded by `desktopSessionCleanupPeriodDays` (unset means no ceiling, a number sets one, and a managed `cleanupPeriodDays` overrides the whole mechanism) — so CLI and desktop sessions age on different clocks, and archiving often still covers the CLI half.

## Quirks

The working-directory segment of a session path is sanitized, and its edge cases are where sessions hide. Short paths keep a readable slug; long ones (over roughly 200 characters) become a fixed path-hash directory instead. On Windows that hash is case-sensitive: the same project reached as `c:\…` and as `C:\…` produces two different directories, so sessions can appear to have disappeared when they have merely moved next door. How the drive-letter colon and the backslash are spelled inside short-path slugs on Windows has not been measured, and the page claims nothing about it.

## Known issues

The registry's standing caveat for this harness is exactly the Windows quirk above: on Windows the long-path directory hash is case-sensitive, so sessions can seem to disappear, and the drive-letter and backslash sanitization of short paths is still unmeasured. The desktop root's Windows location is derived rather than measured — the same session-layout constant measured in the app bundle, resolved through Electron's Windows convention — so the scan refuses to walk that path on Windows until a real install confirms it. That unmeasured root does not erase the count already taken: the machine's combined total prints as a lower bound — the measured CLI sessions, rendered `≥N` with the desktop root named as unverified in `status` and `doctor` alike — rather than collapsing into an unknown, because an unverified additional root must not make the measured CLI total disappear. A machine without the desktop app installed does not carry the mark: the desktop root was looked for and found absent, and beside a scanned CLI store that measured absence counts as zero, so doctor keeps the CLI count it measured instead of dimming it. Unknown survives only where nothing was scanned at all — a machine with neither store reports unknown, never a silently smaller zero.

## How to check it yourself

`chat-stasher doctor` reports the store found and its session count on your machine; `chat-stasher status --json` carries the same per-harness count. After a `run-once`, `chat-stasher read` renders a chosen session — visible text with its speakers, thinking as thinking, tool traffic as tool output — and `chat-stasher export --turns user` writes out exactly the person's messages for this harness. `chat-stasher verify` then checks that what was staged is what the destination holds. A safe way to see the scanner work without touching a live store is to `cp -Rp` a session directory to a scratch location and point `[harness_roots]` at the copy — knowing that the collect cursor is keyed by the source path, so a later ordinary collect from the real location re-delivers that source's rows once: duplicate rows that stay, never a loss. A source root — the desktop Cowork store among them — can also be overridden individually with a `[harness_roots]` key of the form `claude-code:claude-desktop-cowork`, while the plain `claude-code` key keeps naming the CLI store.

## Changelog

2026-08-16 — the phase-one core scanned, stored and read back this harness's sessions.
2026-09-25 — verified end to end: the founding pass sealed 91 real sessions, matched the read-back hash, and real sessions were read back and rendered from the real archive.
2026-10-11 — workflow journals are keyed by their workflow-run directory, so each run of a session keeps its own identity in the archive.
2026-10-16 — Claude Desktop's Cowork store is scanned as a second root of the same harness on macOS: each sandbox transcript joins the archive under its own filename id, with the `desktop-cowork` surface recorded; the registry also declares the desktop sweep key `desktopSessionCleanupPeriodDays` and how to read it.
2026-10-17 — a measured primary beside an unverified root — the unmeasured Windows desktop cell — no longer turns the harness's combined total unknown: `status` and `doctor` report the measured count as a marked lower bound (`≥N`, the unverified root named), and unknown survives only where no root of the family was scanned at all.
