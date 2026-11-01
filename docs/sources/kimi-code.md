<!-- BEGIN GENERATED -->
# Kimi Code

| Field | Value |
|---|---|
| Id | `kimi-code` |
| Surface | cli |
| Status | verified end-to-end (2026-10-03) |
| Last verified end to end | 2026-10-03 (macOS): Read-only copies of all 8 real sessions were collected, staged, pushed and read back with matching SHA-256; all 8 were indexed and rendered by the normalized reader (tested code git f636b57). |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.kimi-code/sessions/<workspaceId>/<sessionId>/agents/main/wire.jsonl` | jsonl | source-confirmed | verified end-to-end (2026-10-03) |
| Linux | `$HOME/.kimi-code/sessions/<workspaceId>/<sessionId>/agents/main/wire.jsonl` | jsonl | unascertained | not supported |
| Windows | `%USERPROFILE%\.kimi-code\sessions\<workspaceId>\<sessionId>\agents\main\wire.jsonl` | jsonl | unascertained | not supported |
<!-- END GENERATED -->

## What it is

Kimi Code is Moonshot AI's terminal coding agent. Its sessions live under `~/.kimi-code` — the whole home moves with `KIMI_CODE_HOME` when set — one directory per session below `sessions/`: `<home>/sessions/<workspaceId>/<sessionId>/`, each session directory named by the session id itself (`session_<uuid>`). A session's transcript is the main agent's journal, `agents/main/wire.jsonl`, inside that directory. This tool scans and archives those journals read-only and never touches the agent's other per-session state.

## What we collect

The scanner names sessions the way the harness does: `wire.jsonl` is a constant filename, so the id comes from the session directory, not the file. The registry cell declares that shape — a `session_*` directory matched at any depth under a declared root, and only the declared relative transcript `agents/main/wire.jsonl` beneath it is read — so everything else in a session directory (`state.json`, `logs/`) stays out. The home-level index `<home>/session_index.jsonl` (one line per session) is read for its `sessionId`, `sessionDir` and `workDir` columns: the recorded `workDir` is the session's observed working directory. The `workspaces.json` workspace map is not read. Both the working directory and the workspace segment above a session are read relative to the sessions root, so a configured harness root that points at the home itself, or directly at a session directory, leaves both dimensions unobserved.

The transcript is an event journal, not a message log: one `{type, time, …}` record per line, its shapes read off the installed implementation (Kimi Code 0.39.1) and corroborated against 41 real journals by an independent importer's census. `time` is the writer's `Date.now()` in epoch milliseconds, so every rendered message time is labelled inferred, never exact. The record types this build renders:

- `context.append_message` — the user side. The harness marks its own user-role records with `origin.kind`: a human record (kind `user`, or no origin at all — every record the harness writes itself carries one, and its compaction path keeps origin-less ones) renders as User, as do skill and plugin records a slash invocation typed; the machine origins (`injection`, `shell_command`, `system_trigger`, `background_task`, `skill_activation`, `plugin_command`, `compaction_summary`, `cron_job`, `cron_missed`, `hook_result`, `retry`) render as System, folded beside the human's words rather than dropped. An origin kind this build does not know is counted, not guessed into either speaker.
- `context.append_loop_event` — the assistant side, grouped the way the harness's own replay groups one step's parts into one message: a `content.part` appends to its step's message (`text`; `think`, with an encrypted reasoning record without readable text counted, not decrypted; `image_url`/`audio_url`/`video_url`, which become attachment references claiming only the recorded type — the media bytes were offloaded into the harness's own blob store and none of them is fetched); a `tool.call` renders in its step's message; a `tool.result` renders as its own Tool message beside it.
- `turn.prompt` / `turn.steer` — the turn lifecycle, which mirrors the input of the `context.append_message` that follows it; counted, not rendered, or every human turn would appear twice.
- everything else — config, tool discovery, usage, and any operation type a later release adds — one record, one count: an unknown op stays visible as a gap, never as "nothing here".

## Coverage and completeness

The harness was verified end to end on 2026-10-03 on macOS: read-only copies of all 8 real sessions were collected, staged, pushed and read back with matching SHA-256, and all 8 were indexed and rendered by the normalized reader. The macOS location is source-confirmed: the installed implementation was read and the layout measured live (three session directories under one workspace). The Linux and Windows cells are unascertained — no install was measured and no platform-specific path branch was read; the path code that was read is platform-independent (`os.homedir()`), so those shapes are expected rather than confirmed, and an unascertained cell is not scanned. On-disk retention and cleanup are not established: no retention or sweep constant was found in the implementation that was read, so the answer stays unknown rather than "none".

## Quirks

The journal writer appends by re-opening the path for every record (mkdir → open → write → sync → close, holding no descriptor between calls), so a session file can be safely sealed by the rename policy. Because the transcript's name never varies, the archive's session id is the `session_<uuid>` directory name itself — a layout rule the registry declares as a directory shape rather than a filename glob.

## Known issues

Activity bounds are inferred and partial when unrecognized wire operation types occur: the times that place a session in a window are read from the journal's epoch-millisecond stamps (labelled inferred), and a record whose operation type this build cannot read is counted rather than placed. Media parts (image, audio, video) are archived as typed references only — the bytes live in the harness's own blob store and a scan archives none of them. Retention and cleanup are not established for the harness itself. The Linux and Windows cells are unascertained and not scanned.

## How to check it yourself

Run `chat-stasher doctor` — it honours `KIMI_CODE_HOME` for a moved home — to confirm the harness is found and see its session count. `chat-stasher run-once` archives the journals; `chat-stasher read` on a staged session renders one assistant message per step with its thinking, text and tool traffic, machine injections as System records beside the human's turns, and tool results as their own messages. `chat-stasher verify` checks that what was staged is what the destination holds, and `chat-stasher search --text "<query>"` finds rendered text after `chat-stasher index build`.

## Changelog

2026-09-19 — registered with its directory-shaped rule: the session id comes from the `session_<uuid>` directory and only `agents/main/wire.jsonl` is read.
2026-09-25 — the journal reader first rendered: step-grouped assistant messages, origin-classified user records (machine origins as System), Tool messages for results, and one count per record nothing renders.
2026-10-03 — the session-directory rule learned recursive matching, so a workspace level between the sessions root and a session directory is walked.
2026-10-03 — verified end to end on macOS: read-only copies of all 8 real sessions collected, pushed and read back with matching SHA-256; all 8 rendered.
