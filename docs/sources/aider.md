<!-- BEGIN GENERATED -->
# aider

| Field | Value |
|---|---|
| Id | `aider` |
| Surface | cli |
| Status | supported |
| Last verified end to end | not yet verified end to end |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `$CWD/.aider.chat.history.md` | md / markdown | source-confirmed | supported |
| Linux | `$CWD/.aider.chat.history.md` | md / markdown | source-confirmed | supported |
| Windows | `$CWD/.aider.chat.history.md` | md / markdown | source-confirmed | supported |
<!-- END GENERATED -->

## What it is

aider is an open-source terminal pair-programming tool (upstream project `Aider-AI/aider`, Apache-2.0). It keeps its chat history as a markdown file, `.aider.chat.history.md`, in the working directory you ran it from, not in your home directory. The registry entry is source-confirmed against upstream (`aider/args.py`, `aider/io.py`), and records that `~/.aider/` holds only configuration and state, that the older `.aider.history.jsonl` no longer exists, that the tool does not honor XDG or APPDATA, and that two sidecar files (`.input.history`, `.llm.history`) sit beside the chat history. The same path template is declared for macOS, Linux and Windows.

## What we collect

One file per project directory: `.aider.chat.history.md`, archived whole and read-only. The cell declares that name exactly — its `session_pattern` is `.aider.chat.history.md` and its `format` is `md / markdown` — so a file with that name below a root the scan looks at is a session, and nothing else in that tree is: a `README.md`, a `notes.md`, a `.markdown` file and the two sidecars `.aider.input.history` and `.aider.llm.history` are not read.

The path is relative to a working directory the tool cannot know in advance, so the template still has no fixed root: with no `[harness_roots]` entry for `aider`, `chat-stasher doctor` reports the source as skipped with the session count unknown ("no root to scan: template not statically resolvable"), never zero (the scanner's module notes in `crates/chat-stasher/src/scanner.rs` say the same of every CWD-relative store). You name a root yourself ([`harness_roots`](../config.md#harness_roots)), and the tree below it is walked, so one entry can cover several projects.

Because that file name is the same in every project, the name cannot tell two projects apart, so a session's id comes from its directory instead: the cell sets `id_from_parent_dir`, and `dir_scoped_native_id` (`crates/chat-stasher/src/id.rs`) is the first 16 hex digits of the SHA-256 of the absolute parent directory. The directory path is not recoverable from the id, and these ids are new — nothing already archived is renamed by this.

## Coverage and completeness

The generated header says `not yet verified end to end`, and no real aider session has a recorded run. What this page records is one measured check of an earlier build and, for the current declaration, the shipped registry cell together with the tests that read it.

On 2026-11-01, with a debug build of this branch (base commit `2c8f0faf`) on one machine, an isolated home and synthetic files only, `chat-stasher doctor --json` gave:

- `[harness_roots] aider` set to a single file: state `file_target`, session count unknown ("store not enumerable"), and an archive-gap entry whose recognised-session count is unknown.
- `[harness_roots] aider` set to a directory holding a file named `.aider.chat.history.md`: scanned, session count 0.
- The same directory with an additional file named `visible.markdown` or `probe.txt`: session count 1 (and a file named `visible.md` was not counted).

The cell then declared `format` = `markdown / txt / jsonl` and no `session_pattern`, so a session had to end `.markdown`, `.txt` or `.jsonl` — none of which is the `.md` the tool writes (`format_suffixes` and `build_record` in `crates/chat-stasher/src/scanner.rs`). On that build, an aider session written by aider itself was not counted by doctor or recognised by the scan, whatever its status cell said.

Later on 2026-11-01 the cell was changed to name the file aider actually writes (commit `a3880fd8`, merged into `main` on 2026-11-02), and the header above is that cell. That is a change of declaration and of what the scan accepts, not a new measurement: no aider file on a real machine was read after it, and the `run-once` collection path was not run against it. The acceptance and the negatives are pinned by `crates/chat-stasher/tests/w1064_registry_names_test.rs`, which plants the real file names in a synthetic tree and drives the shipped registry through the scan and through `collect` — including two projects under one root coming back as two distinct ids with both bodies archived.

## Quirks

One machine can hold many aider histories, one per project directory, and the root you name is walked, so a single `[harness_roots]` path can cover several of them. `markdown` stands in the declared format although no aider file ends that way, because the support registry pins the word; the exact `session_pattern` is what decides, and it leaves `.markdown` unreachable. The seal policy is `no-rename`: how the tool holds the file open was not measured, so nothing is renamed or moved.

## Known issues

From the registry: there is no reader extractor yet, so archived aider sessions render as the raw view only (the full-text index is the exception that does read them: it treats an archived aider shard as a markdown transcript and indexes its non-blank lines). The name-matching gap recorded in the check above is closed at the registry level; whether it closes against a real aider store has not been observed, so this page still does not claim collection works end to end.

## How to check it yourself

Create a throwaway directory containing a copy of an aider history named `.aider.chat.history.md`, point `[harness_roots]` `aider` at the directory, and run `chat-stasher doctor`. The cell now declares that exact name, so the reading to expect from it is one session; a count of 0 means the name did not match — check first whether the copy kept a different name. Put a `README.md` beside it and the count should not change. Do it on a copy, not on your live project.

## Changelog

2026-08-26 — the registry entry first appears in commit `12387a19`.
2026-11-01 — the scan check above was recorded for this page; no change to the scanner was made.
2026-11-02 — the cell now declares the file name aider really writes, and ids a session by the directory it sits in (registry change `a3880fd8`, merged into `main` on 2026-11-02); this page is re-cut around that, and the 2026-11-01 counts are kept as the record of the build they were taken on.
