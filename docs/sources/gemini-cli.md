<!-- BEGIN GENERATED -->
# Gemini CLI

| Field | Value |
|---|---|
| Id | `gemini-cli` |
| Surface | cli |
| Status | verified end-to-end (2026-09-25) |
| Last verified end to end | 2026-09-25: the archived bytes of every real session were read back through the real binary and the on-disk document shape pinned (git 58233aa, git 41db2bf) |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.gemini/tmp/<projectId>/chats/` | json / jsonl | source-confirmed | verified end-to-end (2026-09-25) |
| Linux | `$HOME/.gemini/tmp/<projectId>/chats/` | json / jsonl | source-confirmed | verified end-to-end (2026-09-25) |
| Windows | `%USERPROFILE%\.gemini\tmp\<projectId>\chats\` | json / jsonl | source-confirmed | verified end-to-end (2026-09-25) |
<!-- END GENERATED -->

## What it is

Gemini CLI (`google-gemini/gemini-cli`) is Google's terminal AI assistant. It stores session data on disk under `~/.gemini/tmp/<projectId>/chats/`. This tool scans and archives these session files read-only; no CLI state or configuration is altered.

## What we collect

Each session is stored in files matching the `session-*` pattern. Unlike most CLI tools that use newline-delimited JSON (JSONL), Gemini CLI writes sessions as pretty-printed JSON documents. A single archived file can contain multiple pretty-printed JSON documents concatenated together (such as session documents and export mirrors). The reader uses a dedicated document framing parser that splits and decodes each JSON document.

Within each document, turns in the `messages` array are processed: entries with `type: "user"` become User messages, while `type: "gemini"` or `"assistant"` become Assistant messages.

Provenance metadata is extracted from the top-level `projectHash` field, which records the hash of the working tree where the session executed, mapped as the session's `container` dimension in the archive index.

Because the CLI uses `fs.appendFileSync` (opening, appending, and immediately closing file descriptors on each turn), session files can be safely sealed using the rename policy (`seal_policy: "rename"`).

## Coverage and completeness

Verified end to end on 2026-09-25: the archived bytes of real sessions were read back and rendered through the binary, and the on-disk document structure was pinned by unit tests (commits 58233aa, 41db2bf).

File locations are confirmed from upstream source code across macOS, Linux, and Windows. The `GEMINI_CLI_HOME` environment variable overrides the base directory on all platforms.

## Quirks

Gemini CLI writes multi-line, pretty-printed JSON objects rather than single-line JSONL records. Standard line-based readers would report unrecognized lines; the normalizer re-frames the stream into complete JSON documents.

Although files reside in a directory named `tmp/<projectId>/chats/`, the contents represent persistent conversation history rather than ephemeral cache data.

## Known issues

Gemini CLI includes a built-in 30-day cleanup sweep that purges older contents from its global temporary directory. Sessions deleted by upstream before an archive run cannot be recovered from the source; archiving frequently prevents data loss.

## How to check it yourself

Run `chat-stasher doctor` to check for detected Gemini CLI session files and count them. Execute `chat-stasher run-once` to archive active sessions. Run `chat-stasher read` on a staged session to verify that multi-line JSON structures render correctly into User and Assistant turns with timestamps. Verify hash seals with `chat-stasher verify` and query content with `chat-stasher search --text "<query>"`.

## Changelog

2026-08-16 — file paths confirmed from upstream source and scanner implemented.
2026-09-25 — verified end to end: real sessions read back, rendered, and on-disk shape pinned (commits 58233aa, 41db2bf).
2026-10-11 — document framing parser added to normalizer to decode concatenated pretty-printed JSON documents.
2026-10-12 — `projectHash` dimension extraction added to provenance indexing.
