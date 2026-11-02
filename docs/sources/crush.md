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

crush is Charm's terminal coding agent (upstream project `charmbracelet/crush`, MIT). It keeps its chats in a project-level SQLite database, `.crush/crush.db`, in the working directory; the data directory defaults to `.crush` and follows the project. The registry notes that the global directories hold only configuration, not chat history (config under `~/.config/crush/`, data under `~/.local/share/crush/` on Linux, and `%APPDATA%\crush\` and `%LOCALAPPDATA%\crush\` on Windows). The path is source-confirmed against upstream (`internal/db/connect.go`, `internal/config/load.go`), and that same source is where the database is opened in WAL journal mode. The same template is declared for macOS, Linux and Windows.

## What we collect

Nothing, and this build does not try. All three cells are held at confidence `unascertained`, which the scanner reads as "do not look": with no `[harness_roots]` entry for `crush`, `chat-stasher doctor` reports the state as `skip_unascertained` and the session count as unknown — not as 0, because nothing was enumerated to be absent.

The reason is not the file name. `crush.db` is a live WAL-mode SQLite database, and this build has no reader for crush's schema, so the only thing the collect path could do with it is copy the file whole while crush is writing to it. That is not a consistent snapshot, and a shard nobody can prove the contents of is worse than no shard: the source is held back instead. The registry's own `known_issue` carries the same reason.

One residual, stated because it is easy to misread: a root you write yourself is still looked at, because an explicit root is you stating where the store is, and it outranks the confidence gate (`crates/chat-stasher/src/scanner.rs`, which resolves `explicit_root` before consulting confidence). But the cell's `format` is `sqlite`, so only a name ending `.sqlite` can qualify, and `crush.db` never does. Pointing `[harness_roots] crush` at a `.crush` directory therefore reads nothing. [`harness_roots`](../config.md#harness_roots) lists `crush` for that reason and says the same.

## Coverage and completeness

The generated header says `not yet verified end to end`, and no crush session has a recorded run — nor can one, while nothing is collected. The only measured observation of this source is below, and it is of a build that behaved differently.

On 2026-11-01, with a debug build of this branch (base commit `2c8f0faf`) on one machine, an isolated home and a synthetic SQLite file only, `chat-stasher doctor --json` gave:

- `[harness_roots] crush` set to the database file itself: state `file_target`, session count unknown ("store not enumerable"), with an archive-gap entry whose recognised-session count is unknown.
- `[harness_roots] crush` set to the `.crush` directory holding a file named `crush.db`: scanned, session count 0.
- The same directory with a copy named `probe.sqlite`: session count 1.

The cells then were `source-confirmed` and their status said supported, so the scan walked the tree and matched names on `format` = `sqlite`, whose only suffix is `.sqlite` — not `.db` (`format_suffixes` and `build_record` in `crates/chat-stasher/src/scanner.rs`). A count of 0 there meant "looked, and no name matched", which is a different claim from the one the page makes now, and the honest fix was the second one, not the first.

What changed (commit `a3880fd8`, merged into `main` on 2026-11-02) is the cells, not the name rule: they are `unascertained` and the status is `not supported`, so the guessed `$CWD/.crush` location is never walked at all. That is a declaration, not a measurement — no real crush store was read before or after. Both halves are pinned by `crates/chat-stasher/tests/w1064_registry_names_test.rs`, which drives the shipped registry with synthetic files: the probe refuses with `SkipUnascertained`, and a `crush.db` with its `-wal` sidecar under an explicit root yields no record.

## Quirks

The store is per project, so one machine can hold many crush databases, each with its WAL and shm sidecars, and a single `[harness_roots]` path names one location. The seal policy is `no-rename`. The full-text index treats an archived crush shard as a database and reports it as "not indexable: sqlite" instead of guessing at text in it (`crates/chat-stasher/src/fts.rs`); that path is unreachable while nothing is collected.

## Known issues

From the registry: not collected — `crush.db` is a WAL-mode SQLite database and this build has no reader for its schema, so a raw file copy would not be a consistent snapshot. Because nothing is archived, there is no reader extractor to be missing yet, and the archived-shape notes above describe what would happen if a shard existed. A reader for crush's schema is the way to restore support; until then the status cell says not supported and this page says the same.

## How to check it yourself

Point `[harness_roots] crush` at a throwaway directory holding a copy of a crush database named `crush.db`, and run `chat-stasher doctor`: the directory is looked at, and the name does not match the cell's `sqlite` format, so no session is read. With no root set, the same command reports the state `skip_unascertained` and the count unknown. Do it on a copy, not on your live project.

## Changelog

2026-08-26 — the registry entry first appears in commit `12387a19`.
2026-11-01 — the scan check above was recorded for this page; no change to the scanner was made.
2026-11-02 — the three cells were set to `unascertained` and the source's status to not supported, with the reason recorded in the registry's `known_issue` (commit `a3880fd8`, merged into `main` on 2026-11-02); this page is re-cut around that, and the 2026-11-01 counts are kept as the record of the build they were taken on.
