<!-- BEGIN GENERATED -->
# DeepSeek Harness

| Field | Value |
|---|---|
| Id | `deepseek-harness` |
| Surface | cli, desktop app, web |
| Status | verified end-to-end (2026-10-06) |
| Last verified end to end | 2026-10-06 (macOS): macOS: all 26 real session.v4.jsonl.zstd sessions were collected into an isolated stage, pushed to a throwaway local destination, and read back; five independent SHA-256 digests (export manifest, exported file, staged shards, archive read-back, expected source) agreed for every session. Linux and Windows remain unverified. |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.dsh/sessions/` | jsonl.zstd | measured-locally | verified end-to-end (2026-10-06) |
| Linux | `$HOME/.dsh/sessions/` | jsonl.zstd | unascertained | not supported |
| Windows | `%USERPROFILE%\.dsh\sessions\` | jsonl.zstd | unascertained | not supported |
<!-- END GENERATED -->

## What it is

DeepSeek Harness (`dsh`) is the open-source agent harness behind DeepSeek's agent surfaces: the desktop app, the web client, headless mode, the sdk and the acp integration. Every surface writes into one home — sessions live under `$DSH_HOME/sessions/` (default `~/.dsh/sessions/`), one directory per session — so a session cannot be attributed back to the surface it came from, and the registry declares no supported dimension for it. Each session directory holds `session.v4.jsonl.zstd`: a concatenation of Zstandard frames whose first frame is the session header, beside a 0-byte `session.lock` that says nothing about whether the session is still running. `DSH_HOME` replaces the whole home; the scanner appends the `sessions/` suffix itself. This tool reads the transcripts read-only and never touches the live writer.

## What we collect

The session id comes from the directory name — `session-<uuid>`, or a bare `<uuid>` for a subagent child — because the file name is the constant `session.v4.jsonl.zstd`. The `<slug-of-cwd>` directory component above it is lossy and is never parsed; the header frame's own `cwd` is the authoritative working directory. The archive stores the decoded lines, so the reader works on a plain JSONL stream of `{type, seq, time, data}` records:

- `session` — the header frame (`version`, `id`, `createdAt`, `cwd`, `isSeeded`, `delegationDepth`, `agentPreset`): metadata, nothing to render.
- `user/message` — the person's text, unless `data.source.kind` says otherwise. Measured across every `user/message` record on the measured machine, only 11 of 41 were the person's words; the other 30 — kinds `agent-instructions`, `runtime-context`, `skill-catalog`, `subagent-settled`, `agent-message` — are `dsh` injecting its own context into the transcript, and they render as `System` rather than as if the human had typed them. An unknown kind is counted rather than guessed either way.
- `assistant/message` and `system/message` — `data.message.content[]` renders; `tool_use` parts inside it render as tool calls, and the `usage` and `stream` fields are telemetry, not conversation.
- `tool/result` — the harness handing output back, rendered as `Tool` role output; `data.message.isError` marks a failed call.
- `tool/call` — the same call the assistant message already carries as a `tool_use` part, matched by `callId`; it is recognized and not rendered, so one call is never shown twice.
- packed chunk rows (`text-chunks`, `reasoning-chunks`, `tool-call-chunks`, written by upstream's `packChunks`, documented default on) were observed zero times on the measured machine, so their handling is conservative: text and reasoning chunks render only when their `texts` member is an array of strings, and `tool-call-chunks` is counted, never rendered.
- lifecycle records (`turn/start`, `step/end`, `command/run`, `subagent/catalog`, and the rest of the measured set) carry no message text and are not counted; a type this build does not know is counted, because a `dsh` preview that changes its records should show up as a visible gap rather than a session that reads complete.

Every message time comes from the record's envelope `time` — the writer's clock, since `data.message` carries no timestamp of its own.

## Coverage and completeness

The layout is measured on macOS (2026-10-03, against the installed developer preview app 0.2.0-rc.2, session format 4): seven sessions, the largest a still-live one at 372 frames, were decoded frame by frame, recording field names, counts and digests only, never message text. The harness was then verified end to end on 2026-10-06: all 26 real sessions were collected into an isolated stage, pushed to a throwaway destination and read back, with five independent SHA-256 digests agreeing per session. Linux and Windows carry the same `$DSH_HOME` default in upstream's persistence documentation, but no install has been measured, so both cells stay `unascertained`. Attachments live outside the log at `$DSH_HOME/attachments/v1` and are not archived.

## Quirks

The file is frames, not one stream: `.jsonl.zstd` is a concatenation of Zstandard frames, and a reader that decodes only the first frame would read one record out of hundreds from the largest session. The seal policy is `no-rename`: upstream's JSONL provider materialises a session atomically, truncates and re-encodes a torn tail before its first new append, and one live writer owns a session at a time, so a rename-based seal would race the owner — the archive keeps a read-only sealed copy and leaves the source in place. The 0-byte `session.lock` proves nothing about quiescence and is not consulted.

## Known issues

The measurement stands against a developer preview (app 0.2.0-rc.2, session format 4) that states compatibility-breaking changes are expected, so a changed record type lands as a counted unknown; attachments are not archived yet; and the Linux and Windows cells remain unascertained.

## How to check it yourself

`chat-stasher doctor` reports the `~/.dsh` root found and its session count. `chat-stasher run-once` collects the sessions, and `chat-stasher read` on one shows your turns, agent messages, injected context as `System` and tool results as `Tool`, with a count of anything it did not render. `chat-stasher verify` checks the staged shards against the destination, and `chat-stasher search --text` finds text from them after `chat-stasher index build`. Point `DSH_HOME` (or a `[harness_roots]` entry) at a moved home to exercise the scan against a copy.

## Changelog

2026-10-03 — the home layout, frame structure and record kinds measured on a real install: field names, counts and digests only, never message text.
2026-10-05 — first shipped: the registry entry, the scan and the reader — decoded frames render as conversations, injected context is labelled `System` rather than the human, packed chunk rows are handled conservatively, and `DSH_HOME` actually resolves.
2026-10-06 — verified end to end on macOS: all 26 real sessions collected, pushed and read back, with five SHA-256 digests agreeing per session.
