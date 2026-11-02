<!-- BEGIN GENERATED -->
# crush

| Field | Value |
|---|---|
| Id | `crush` |
| Surface | cli |
| Status | not supported |
| Last verified end to end | not yet verified end to end |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `$CWD/.crush/crush.db` | sqlite | unascertained | not supported |
| Linux | `$CWD/.crush/crush.db` | sqlite | unascertained | not supported |
| Windows | `$CWD/.crush/crush.db` | sqlite | unascertained | not supported |
<!-- END GENERATED -->

## What it is

crush is Charm's terminal coding agent (upstream project `charmbracelet/crush`, MIT). It keeps its chats in a project-level SQLite database, `.crush/crush.db`, in the working directory; the data directory defaults to `.crush` and follows the project. The registry notes that the global directories hold only configuration, not chat history (config under `~/.config/crush/`, data under `~/.local/share/crush/` on Linux, and `%APPDATA%\crush\` and `%LOCALAPPDATA%\crush\` on Windows). The entry is source-confirmed against upstream (`internal/db/connect.go`, `internal/config/load.go`). The same template is declared for macOS, Linux and Windows.

## What we collect

By declaration, the database file, archived whole with a `no-rename` seal policy (a single-file SQLite store whose file-descriptor behaviour was not confirmed). Read this together with the next section, because on a 2026-11-01 check the scanner of this build did not recognise that file. The template is relative to a working directory, so it has no fixed root: with no `[harness_roots]` entry for `crush`, `chat-stasher doctor` reports the source as skipped with the session count unknown, never zero. You name a root yourself ([`harness_roots`](../config.md#harness_roots)).

## Coverage and completeness

The generated header says `not yet verified end to end`, and no real crush session has a recorded run. The only dated observation of this build's behaviour for this source is below.

On 2026-11-01, with a debug build of this branch (base commit `2c8f0faf`) on one machine, an isolated home and a synthetic SQLite file only, `chat-stasher doctor --json` gave:

- `[harness_roots] crush` set to the database file itself: state `file_target`, session count unknown ("store not enumerable"), with an archive-gap entry whose recognised-session count is unknown.
- `[harness_roots] crush` set to the `.crush` directory holding a file named `crush.db`: scanned, session count 0.
- The same directory with a copy named `probe.sqlite`: session count 1.

The reason is in the scanner: a file is a session only when its name ends with a suffix the registry's `format` declares, and `sqlite` yields `.sqlite`, not `.db` (`format_suffixes` and `build_record` in `crates/chat-stasher/src/scanner.rs`). crush's real file is `crush.db`. So, as of that check, a crush database written by crush itself is not counted by doctor or recognised by the scan in this build, whatever the status cell above says. The `run-once` collection path was not run in that check.

## Quirks

The store is per project, so one machine can hold many crush databases and a single `[harness_roots]` path names one location. The full-text index treats an archived crush shard as a database and reports it as "not indexable: sqlite" instead of guessing at text in it (`crates/chat-stasher/src/fts.rs`).

## Known issues

From the registry: there is no reader extractor yet, so archived crush sessions render as the raw view only. And, from the check above, the `crush.db` file name is not matched by the scan in this build. This page does not claim collection works until a run records it.

## How to check it yourself

Create a throwaway directory containing a copy of a crush database named `crush.db`, point `[harness_roots]` `crush` at the directory, and run `chat-stasher doctor`: a count of 0 reproduces the finding above; renaming the copy to end in `.sqlite` changes the count to 1. Do it on a copy, not on your live project.

## Changelog

2026-08-26 — the registry entry first appears in commit `12387a19`.
2026-11-01 — the scan check above was recorded for this page; no change to the scanner was made.
