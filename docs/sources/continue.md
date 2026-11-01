<!-- BEGIN GENERATED -->
# Continue

| Field | Value |
|---|---|
| Id | `continue` |
| Surface | ide extension |
| Status | supported |
| Last verified end to end | not yet verified end to end |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.continue/sessions/<id>.json + sessions.json` | json | source-confirmed | supported |
| Linux | `~/.continue/sessions/<id>.json + sessions.json` | json | source-confirmed | supported |
| Windows | `%USERPROFILE%\.continue\sessions\<id>.json + sessions.json` | json | source-confirmed | supported |
<!-- END GENERATED -->

## What it is

Continue is an open-source AI coding assistant for IDEs (upstream project `continuedev/continue`, Apache-2.0). It keeps one JSON file per chat session under `~/.continue/sessions/<id>.json`, with a `sessions.json` index beside them. The registry entry is source-confirmed against upstream (`core/util/paths.ts`, `core/util/history.ts`): on Windows the home is `%USERPROFILE%`, with no `%APPDATA%` or XDG variants unless `CONTINUE_GLOBAL_DIR` overrides the whole directory. The registry also notes that Continue's SQLite store under `~/.continue/dev_data/` is analytics, not chat history.

## What we collect

By declaration, the per-session JSON files, archived whole with a `no-rename` seal policy (overwrite and create semantics, file-descriptor behaviour not confirmed). Read this together with the next section, because on a 2026-11-01 check the scanner of this build did not count any such file.

## Coverage and completeness

The generated header says `not yet verified end to end`, and no real Continue session has a recorded run. The only dated observation of this build's behaviour for this source is below.

On 2026-11-01, with a debug build of this branch (base commit `2c8f0faf`) on one machine, an isolated home and synthetic files only, two files `~/.continue/sessions/s1.json` and `sessions.json` (later also `probe.json`) were present and `chat-stasher doctor --json` reported the Continue root as scanned with a session count of 0 and zero unreadable entries.

The reason is in the scanner: a registry cell whose format is JSON and which declares no `session_pattern` never yields a session, by design, to keep settings and state JSON out of the count (`build_record` in `crates/chat-stasher/src/scanner.rs`: "a generic JSON cell without a pattern is rejected"). Continue's registry cell has no pattern, because its session files have no distinguishing name. So, as of that check, Continue sessions are not counted by doctor or recognised by the scan in this build, whatever the status cell above says. The `run-once` collection path was not run in that check.

## Quirks

The `sessions.json` index and the per-session files share one directory and one extension. Whether `CONTINUE_GLOBAL_DIR` is honored by this build was not checked; the registry records the override as an upstream fact only. `[harness_roots]` can name another root ([config](../config.md#harness_roots)).

## Known issues

From the registry: there is no reader extractor yet, so archived Continue sessions render as the raw view only. And, from the check above, the scan does not recognise the session files in this build. This page does not claim collection works until a run records it.

## How to check it yourself

Put a copy of a Continue `sessions` directory in a throwaway home, or point `[harness_roots]` `continue` at the copy, and run `chat-stasher doctor`: a count of 0 with `.json` files present reproduces the finding above. Do it on a copy, not on your live `~/.continue`.

## Changelog

2026-08-26 — the registry entry first appears in commit `12387a19`.
2026-11-01 — the scan check above was recorded for this page; no change to the scanner was made.
