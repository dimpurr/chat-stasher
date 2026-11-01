<!-- BEGIN GENERATED -->
# GitHub Copilot CLI

| Field | Value |
|---|---|
| Id | `github-copilot-cli` |
| Surface | cli |
| Status | verified end-to-end (2026-10-06) |
| Last verified end to end | 2026-10-06 (macOS): macOS: the one real local session was collected into an isolated stage, pushed to a throwaway local destination, and read back; five independent SHA-256 digests (export manifest, exported file, staged shards, archive read-back, expected source) agreed. Linux and Windows remain unverified. |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.copilot/` | sqlite + jsonl | source-confirmed | verified end-to-end (2026-10-06) |
| Linux | `~/.copilot/` | sqlite + jsonl | source-confirmed | supported |
| Windows | `%USERPROFILE%\.copilot\` | sqlite + jsonl | source-confirmed | supported |
<!-- END GENERATED -->

## What it is

GitHub Copilot CLI is GitHub's coding agent CLI. It persists sessions — non-ephemeral by design — under `~/.copilot/` (`%USERPROFILE%\.copilot\` on Windows, with its cache over in `%LOCALAPPDATA%\copilot\`): a `session-store.db` SQLite store holds the sessions, per-session `events.jsonl` files live below `session-state/`, and further databases (`data.db`, `session.db`, `embedding-cache.db`) sit beside them. The whole home can be moved with `COPILOT_HOME`. On Linux, when `XDG_CONFIG_HOME` and `XDG_STATE_HOME` are set, the CLI performs a one-time migration that moves state — including `session-store.db` — to `$XDG_STATE_HOME/copilot`; that migration does not apply on Windows. This tool reads the store read-only and never modifies or interrupts a running CLI.

## What we collect

The store is the enumerable source, and the store's shape is the CLI's own shipped documentation of itself: the v1.0.80 build artifacts' embedded SQLite DDL describes a `sessions` table with one row per session (`id`, `cwd`, `repository`, `branch`, `summary`, `created_at`, `updated_at`) and a `turns` table with one row per turn (`session_id`, `turn_index`, `user_message`, `assistant_response`, `timestamp`). The scanner probes `session-store.db` read-only and emits one session per `sessions` row; the collector reads that row together with all its turns inside one WAL-aware read-only SQLite transaction and exports them as a single `chat-stasher.github-copilot-cli.session.v1` JSON line. The reader renders each turn's two sides as their own messages — `user_message` as User, `assistant_response` as Assistant, in turn order — because that is what a turn is: two speakers, one exchange. A side that is missing, null, empty or not text renders nothing and the turn is counted instead; missing is never read as a blank speaker. The session's working directory and repository are read as provenance from the row (`cwd`, and `repository` as the container dimension); a missing or non-text value observes nothing rather than an empty one.

No time is claimed. The `created_at`/`updated_at`/`timestamp` columns travel through the raw export untouched, but the schema was read from build artifacts and no real `session-store.db` has ever been compared against it, so no layer renders a message or session time from them, and the collector's cursor is a content hash of the exported line rather than anything time-based. The per-session `events.jsonl` below `session-state/` is deliberately not read: its directory layout has never been measured, and a file-name-based walk would collapse every session onto the same `events` stem.

## Coverage and completeness

The harness was verified end to end on 2026-10-06 on macOS: the one real local session was collected into an isolated stage, pushed to a throwaway destination and read back, with five independent SHA-256 digests agreeing. That run predates the derived reader above and proved the store's bytes reach the archive intact; it does not, on its own, prove that the build-artifact schema matches the store on your machine. When the store is absent, the scan never reports zero: it counts `events.jsonl` files by their exact, writer-confirmed name, and finding any is reported as sessions this build cannot enumerate. All three operating systems' locations are source-confirmed from the same v1.0.80 artifacts; Linux and Windows remain unverified end to end.

## Quirks

This harness's schema is read out of the product's own build artifacts rather than out of a live store: the reader ships ahead of any observation of a real `session-store.db`, pinned by tests against synthetic stores built from that schema, with every unmeasured boundary written down rather than papered over. The Linux XDG migration means the same store may live under `$XDG_STATE_HOME/copilot` on machines that set both variables — a different root, the same session store.

## Known issues

The reader has not been exercised against a real store — its schema comes from the v1.0.80 build artifacts, and the 2026-10-06 end-to-end run predates it — so conversation time is not claimed, and the per-session `events.jsonl` below `session-state/` (turn-level cwd, event timestamps) is not read because its on-disk layout is unmeasured.

## How to check it yourself

`chat-stasher doctor` reports the Copilot home found and the session count from the store's `sessions` table; a store that is absent while `events.jsonl` files exist is reported as unknown, never as zero. `chat-stasher run-once` collects the sessions, `chat-stasher read` renders each turn's two sides in order, `chat-stasher verify` checks what was staged against the destination, and `chat-stasher search --text` finds turn text after `chat-stasher index build`. `COPILOT_HOME` is honoured for a moved home.

## Changelog

2026-08-16 — registered in the first harness path registry: `~/.copilot/` on all three operating systems, source-confirmed from the CLI's layout.
2026-10-06 — verified end to end on macOS: the one real local session collected, pushed and read back, with five SHA-256 digests agreeing.
2026-10-09 — the store became the enumerable source: sessions and turns are exported per session and rendered turn by turn, `COPILOT_HOME` is declared and honoured, and time is deliberately not claimed.
