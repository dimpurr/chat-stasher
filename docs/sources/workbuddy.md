<!-- BEGIN GENERATED -->
# WorkBuddy

| Field | Value |
|---|---|
| Id | `workbuddy` |
| Surface | desktop app |
| Status | supported |
| Last verified end to end | not yet verified end to end |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.workbuddy/projects/` | jsonl | measured-locally | supported |
| Linux | `$HOME/.workbuddy/projects/` | jsonl | unascertained | not supported |
| Windows | `%USERPROFILE%\.workbuddy\projects\` | jsonl | unascertained | not supported |
<!-- END GENERATED -->

## What it is

WorkBuddy is the desktop agent application of the CodeBuddy family. One installation keeps everything under a single `~/.workbuddy` root: session metadata in the sqlite store `~/.workbuddy/workbuddy.db`, and each conversation as a JSONL transcript at `~/.workbuddy/projects/<project-slug>/<session-id>.jsonl`. The sibling CodeBuddy CLI writes the same transcript event core; this tool reads the store first and the transcripts second, both read-only, and never writes anything into the app's own files.

## What we collect

The scanner enumerates the store's `sessions` table and walks the transcript directory, one session joining its two halves: a store row and its transcript file share one session id (measured: the transcript file name is the session id, on every installation this entry was measured from). The row is exported as one `chat-stasher.workbuddy.session.v1` metadata line whenever the row's own bytes change; the transcript file is collected as ordinary append-only JSONL through a byte cursor, so a running session's new lines archive as they arrive.

The store is opened strictly `mode=ro`, never `immutable=1`. A WAL-mode store whose `-shm` sidecar is absent is read through a copied `db`, `-wal` and `-shm` trio in a temporary directory that is deleted after the read, so uncheckpointed WAL content still reads and no sidecar is ever created beside the user's own store. The schema is validated before anything is claimed: the `sessions` table must carry `id`, `transport`, `deleted_at`, `created_at` and `last_activity_at`, and a store without them is refused by name, never counted as zero.

The transcript walk recognizes only `*.jsonl` files; the database-shaped neighbours the app writes beside them are excluded by name wherever they appear (`*.file-rollback.ndjson` rollback journals and the `tool-results/` derivative cache are never sessions). `~/.workbuddy/models.json` holds credentials and this tool never opens or reads it.

The reader handles the nine top-level transcript event types the two products share: `message` (a person's text is the last `<user_query>` span inside the content; unwrapped user-role records are the harness's own injected context and never count as a person speaking; an assistant reply is the concatenation of its `output_text` blocks), `reasoning` (rendered as thinking), `function_call` and `function_call_result` (paired by `callId`; an unclosed call keeps its result unclaimed and is never synthesized one), and `session-meta`, `file-history-snapshot`, `ai-title`, `summary`, `turn-metrics` as recognized bookkeeping that renders nothing. The final two have only been measured on the sibling CLI, and the reader already counts them so the shared core cannot drift silently. A session whose transcripts exist under more than one project slug archives the newest-mtime copy and names the stale ones in the scan note.

## Coverage and completeness

A row whose `deleted_at` is not null (any non-null value; the store's own cloud-sort index implies `-1` is a sentinel too, so the rule is written as non-null, never as matching one value) is a tombstone: the collect exports a bare tombstone line carrying the session's identity and the deletion marker and no metadata, while a still-present transcript keeps being archived beside it. A session the source deleted is recorded as deleted; its earlier archived bytes stay.

A live row without a matching transcript on this machine - a transport the store holds no local transcript for, or the rows whose project directory was moved with the transcript left behind - is a recognized session the archive cannot write. The scan names the difference as a gap, and `run-once` reports PARTIAL and exits 3, never 0. A transcript whose id has no store row archives on its own file-name id.

Recorded times are per-row `last_activity_at` milliseconds for the store half and file times for the transcript half. Event timestamps inside a transcript can repeat or step backwards (measured), so the reader walks in file order and the collector keeps byte cursors; nothing ever sorts events by time.

The macOS cell, the store schema and the join rule were measured locally (2026-10-30, three real sessions, read-only); the projects path is also confirmed by the shipped app's own bundled `WORKBUDDY_PLATFORM_OWNED_PROJECT_FILE_PATTERNS` constant. Linux and Windows remain unmeasured, and the scanner refuses to guess a path it has not measured.

## Quirks

A store that exists but cannot be enumerated (a schema this build does not recognize, a database this process cannot open) keeps its transcripts archiving while its session count prints as unknown, not 0. When the store file is not present at all, the transcripts below `projects/` still archive on their own file-name ids and the store's count is unknown. The registry root is the `projects/` directory, and the store is expected as its sibling `workbuddy.db`; the same shape holds when you state the root yourself ([`harness_roots`](../config.md#harness_roots)), which is also the way to read a copied `~/.workbuddy` tree without touching the live one.

## Known issues

There is no official transcript schema - only third-party reverse-engineering - so every read censuses the top-level event types and counts a new or vanished type as drift rather than treating it as failure. A machine whose store holds rows with no local transcripts stays PARTIAL on every collect for exactly those sessions; that unarchived remainder is the honest state of sources this tool refuses to over-claim, not a repairable error, and the per-session gap lines name it each run.

## How to check it yourself

`chat-stasher doctor` reports the store found and its session count, with tombstoned and transcript-less rows called out in the note. `chat-stasher run-once` collects the sessions - exiting 3 with a PARTIAL report if the store recognizes sessions this machine holds no transcript bytes for - and `chat-stasher read` renders one conversation per session with the metadata line as bookkeeping and the transcript as speech. `chat-stasher verify` checks what was staged, and `chat-stasher search --text` finds transcript text after `chat-stasher index build`. To try the read without touching the live root, copy the tree (`cp -Rp ~/.workbuddy /somewhere`) and point the `workbuddy` row of your `[harness_roots]` at the copy's `projects/` directory; every read this tool makes is read-only.

## Changelog

2026-10-30 - first shipped (W1046): the registry entry, the dual-store scanner (sqlite metadata plus JSONL transcripts, one id per session), bare-tombstone exports for `deleted_at` rows, transcript-only fallbacks, and the reader for the nine shared event types.
