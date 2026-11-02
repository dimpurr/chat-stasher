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

By declaration, one markdown file per project directory, archived whole and read-only. Read this together with the next section, because on a 2026-11-01 check the scanner of this build did not recognise that file. Because the path is relative to a working directory the tool cannot know in advance, the template has no fixed root: with no `[harness_roots]` entry for `aider`, `chat-stasher doctor` reports the source as skipped with the session count unknown ("no root to scan: template not statically resolvable"), never zero (the scanner's module notes in `crates/chat-stasher/src/scanner.rs` say the same of every CWD-relative store). You name a root yourself ([`harness_roots`](../config.md#harness_roots)).

## Coverage and completeness

The generated header says `not yet verified end to end`, and no real aider session has a recorded run. The 2026-11-01 check below is the only dated observation of this build's behaviour for this source.

On 2026-11-01, with a debug build of this branch (base commit `2c8f0faf`) on one machine, an isolated home and synthetic files only, `chat-stasher doctor --json` gave:

- `[harness_roots] aider` set to a single file: state `file_target`, session count unknown ("store not enumerable"), and an archive-gap entry whose recognised-session count is unknown.
- `[harness_roots] aider` set to a directory holding a file named `.aider.chat.history.md`: scanned, session count 0.
- The same directory with an additional file named `visible.markdown` or `probe.txt`: session count 1 (and a file named `visible.md` was not counted).

The reason is in the scanner: a file is a session only when its name ends with one of the suffixes the registry's `format` declares, and `markdown / txt / jsonl` yields `.markdown`, `.txt` and `.jsonl`, not `.md` (`format_suffixes` and `build_record` in `crates/chat-stasher/src/scanner.rs`). aider's real file name ends in `.md`. So, as of that check, an aider session written by aider itself is not counted by doctor or recognised by the scan in this build, whatever its status cell below says. The `run-once` collection path was not run in that check.

## Quirks

Because the history lives beside the project, one machine can hold many aider histories, one per project directory, and a single `[harness_roots]` path names one location. The seal policy is `no-rename`: how the tool holds the file open was not measured, so nothing is renamed or moved.

## Known issues

From the registry: there is no reader extractor yet, so archived aider sessions render as the raw view only (the full-text index is the exception that does read them: it treats an archived aider shard as a markdown transcript and indexes its non-blank lines). And, from the check above, the `.md` history file is not matched by the scan in this build. This page does not claim collection works until a run records it.

## How to check it yourself

Create a throwaway directory containing a copy of an aider history named `.aider.chat.history.md`, point `[harness_roots]` `aider` at the directory, and run `chat-stasher doctor`: a count of 0 reproduces the finding above; renaming the copy to end in `.txt` changes the count to 1. Do it on a copy, not on your live project.

## Changelog

2026-08-26 — the registry entry first appears in commit `12387a19`.
2026-11-01 — the scan check above was recorded for this page; no change to the scanner was made.
