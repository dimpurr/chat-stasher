<!-- BEGIN GENERATED -->
# CodeBuddy

| Field | Value |
|---|---|
| Id | `codebuddy` |
| Surface | cli |
| Status | supported |
| Last verified end to end | not yet verified end to end |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.codebuddy/projects/` | jsonl | measured-locally | supported |
| Linux | `$HOME/.codebuddy/projects/` | jsonl | unascertained | not supported |
| Windows | `%USERPROFILE%\.codebuddy\projects\` | jsonl | unascertained | not supported |
<!-- END GENERATED -->

## What it is

CodeBuddy is the CLI of the CodeBuddy family — the `cbc` core as a command-line tool, where WorkBuddy (its sibling source in this archive) is the same core wrapped in the desktop app. A CLI installation keeps each conversation as one JSONL transcript under `~/.codebuddy/projects/`, in either of the two layouts measured on one installation: flat `~/.codebuddy/projects/<session-id>.jsonl`, or under a project directory as `~/.codebuddy/projects/<project-slug>/<session-id>.jsonl`. There is no sqlite session store beside them: the transcript file's own name is the whole record that a session exists. This tool reads the transcripts read-only and never writes into the CLI's own files.

## What we collect

The scanner walks the projects root with only one layout rule per file: a `*.jsonl` name is a transcript, and everything the core writes beside them is excluded — `*.file-rollback.ndjson` rollback journals by name, any subdirectory inside a project slug (the `tool-results/` cache and the render sidecars like `Read_*-render.txt`), and any deeper nesting, which was never measured. A file named `history.jsonl` anywhere inside projects — flat or under a project slug, either level the walk accepts — is excluded by stem: that name belongs to the per-machine input log the entry declares as a machine log (below), never to a session.

Each transcript archives on its own file-name stem through the ordinary append-only JSONL reader, so a running session's new lines archive as they arrive. The same session id seen in more than one location — flat and under a slug, or under two slugs — is one archive slot: the newest-mtime copy archives and the stale copies are named in the scan note, byte-for-byte what the WorkBuddy reader does with the same rule.

Because the file name is the only record that a session exists, a `*.jsonl` name the walk accepted whose metadata could not be read is a session the source recognizes and this build cannot hand out. The scan counts it, emits no record for it, and the difference becomes a named archive gap, so collect reports PARTIAL and exits 3 — never 0. That is the transcripts-only counterpart of the rule the desktop store's rows give that source's gap arm.

The reader is the one shared cbc-core reader, with no codebuddy-specific branch: `message` (a person's text is the last `<user_query>` span inside the content; unwrapped user-role records are the harness's own injections and never a person speaking; an assistant reply is its `output_text` blocks concatenated), `reasoning` (rendered as thinking), `function_call` and `function_call_result` (paired by `callId`, an unclosed call never synthesized a reply), and `session-meta`, `file-history-snapshot`, `ai-title`, `summary`, `turn-metrics` as recognized bookkeeping that renders nothing. `summary` and `turn-metrics` were measured on this very source, and the desktop reader already counted them, so neither product's read can drift silently. An unknown top-level type is counted as census drift and the session keeps rendering around it.

The input log `~/.codebuddy/history.jsonl` — one line per prompt the person typed, with `display`, `pastedContents`, `project` and `timestamp` fields — is collected as a machine log (W1047 captured it; W1047f held the capture for its owner's ruling; the ruling approved it 2026-10-31, W1047f2, after the file was re-verified as the same kind of source the claude-code history is: the same one-object-per-typed-prompt JSONL shape and the same field names, one key — `sessionId` — short): body-tier prose, sealed whole — raw and append-only through the same machinery that handles the claude-code history, under `machine-logs/<machine>/codebuddy/history/`, outside session lists, session counts and coverage, and never under a session id. It is what the CLI keeps for its own input recall; it is not a conversation and never became one at any point of the pipeline.

## Coverage and completeness

The recognized set is exactly the transcripts the walk could read plus the accepted names it could not, so a clean walk's count is a measurement: a readable empty root is the one place a zero is earned. When the walk cannot read entries, the count carries the shape of what was missed — an accepted name without metadata is recognized-not-archived (the gap arm above), while a directory entry that could not be typed or listed is an entry-level unknown that rides along in the note and the unreadable tallies, the same three-state rule every scanner walk keeps.

There is no tombstone arm and no row-join, because the CLI keeps no store half to observe: a session's deletion is, to this source, its transcript's disappearance, and the archive records that honestly — the bytes that were archived stay archived.

Recorded times are file times; event timestamps inside a transcript can repeat or step backwards (measured on the same core), so the reader walks in file order and the collector keeps byte cursors. Nothing ever sorts events by time.

The macOS cell and both layouts were measured locally (2026-10-30, one installation, 41 transcript files across the two layouts, counted read-only without reading content; the input log's structure was read the same day, keys only). Linux and Windows remain unmeasured, and the scanner refuses to guess a path it has not measured.

## Quirks

A projects root that exists but cannot be listed keeps the session count unknown, never zero — nothing was recognized and nothing is claimed as archivable. The registry root is the projects directory; the input log lives beside it as its sibling, and the same shape holds when you state the root yourself ([`harness_roots`](../config.md#harness_roots)), which is also the way to read a copied `~/.codebuddy` tree without touching the live one.

## Known issues

There is no official transcript schema, only third-party reverse-engineering — the same caveat the WorkBuddy source carries, because the event core is shared. Every read censuses the top-level event types and counts a new or vanished type as drift rather than treating it as failure, and version drift between the CLI's and the desktop app's builds surfaces as census drift in each source's counts, never folded away. The input log's rotation and truncation behaviour is likewise unverified; rewrites of its head keep every previously sealed generation in the archive (ADR-053 D3).

## How to check it yourself

`chat-stasher doctor` reports the transcript count with the layout split — flat files, files under project directories — plus any recognized-but-unreadable names and unreadable entries in the note. `chat-stasher run-once` collects the sessions, exiting 3 with a PARTIAL report when a name it recognized could not be read; `chat-stasher read` renders one conversation per transcript through the shared cbc reader; `chat-stasher search --text` finds transcript text after `chat-stasher index build`. The typed-prompts input log is reported by doctor as its own machine-log row, never joins the session counts, and its content is sealed separately under `machine-logs/<machine>/codebuddy/history/` — never into a session body. To try the read without touching the live root, copy the tree (`cp -Rp ~/.codebuddy /somewhere`) and point the `codebuddy` row of your `[harness_roots]` at the copy's `projects/` directory; every read this tool makes is read-only.

## Changelog

2026-10-30 - first shipped (W1047): the registry entry, the both-layouts transcript scanner (flat and project-directory files, no sqlite half), the recognized-by-name gap arm for accepted names whose metadata could not be read, the `history` input log collected as a machine log, and the shared-reader dispatch with no codebuddy-specific branch.

2026-10-31 - fix round (W1047f): the `history` stem is excluded at both levels inside projects (a slug-level `history.jsonl` was previously read as a session); the input log's body capture is withheld pending review — still declared, measured and reported, but no content is sealed into the archive until that review is settled; entry-level unknowns are pinned as prose-plus-exit-0, and a store row without any readable time (the WorkBuddy probe, fixed the same round) is named in the note instead of mislabeled.

2026-10-31 - ruling round (W1047f2): the input log's body capture, held by the fix round pending the owner's ruling, is approved and turned back on — a same-kind re-verification confirmed the file against the claude-code `history.jsonl` (the same one-object-per-typed-prompt line shape and field names, one key fewer), so the log is sealed whole as body-tier machine-log content like its claude-code counterpart; the privacy page now states the two harnesses' prompt-history files side by side as the complete set. The slug-level `history` stem exclusion from the fix round stays as it landed.
