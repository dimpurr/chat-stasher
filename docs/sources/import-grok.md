<!-- BEGIN GENERATED -->
# Grok (official export)

| Field | Value |
|---|---|
| Id | `grok` |
| Surface | export |
| Export file | `prod-grok-backend.json` |
| Parser | shipped in this build (`chat-stasher import grok <export> --inbox <dir> --stage <dir>` parses it) |
| Last verified end to end | not yet verified end to end |
<!-- END GENERATED -->

## What it is

This page is about grok.com's official export: the takeout the platform's own export flow delivers, whose payload file is `prod-grok-backend.json` — one JSON object, not a flat array, carrying the account's conversations under its `conversations` key. `chat-stasher import grok <export> --inbox <dir> --stage <dir>` parses it in this build. The live counterpart is [the grok.com web page](web-grok.md), and the measured relationship between the two is on record: the recall oracle compared one archive's web-captured sessions against this export (312 conversations) and found 61 matches — every matched pair opened during the capture window, every missing conversation predating it — so importing this file is the measured way to close the historical gap the live leg leaves ([the capture-gap note](../../docs-dev/grok-capture-gap.md)).

## What we collect

The exported shape was measured on a real account's export (the 2026-09-24 census: 312 conversations, 2,397 responses): each conversation is one wrapper — a `conversation` object (`id`, `create_time`, `modify_time`) with a `responses` list of response wrappers — and `projects`, `tasks` and `media_posts` sit beside `conversations` empty in the measured file. The parser carries each wrapper whole, field for field, so every field the census marked not interpreted survives exactly as the platform wrote it, absences included. Four of the census's rules are the parser's own:

- The raw `responses` list is the complete node set and the only measurement axis. `conversation.leaf_response_id` (present on 65 of the 312 measured) names the active branch tip, not necessarily the deepest branch, so the parser keeps the whole list and walks nothing.
- Times are carried, never derived and never rebuilt. A conversation's `create_time`/`modify_time` are ISO-8601 strings; a response's `create_time` is a MongoDB extended-JSON `$date` record in epoch **milliseconds** (divided, never read as seconds), and a value that arrives in some other shape is kept as written. The export's own `conversation.create_time` is the created time — the minimum response time equalled it on 0 of 38 measured conversations, so approximating it from the responses would be wrong every time.
- A missing parent is a root, never an error: 62 measured responses carry a `parent_response_id` naming no response within their own conversation's set.
- Attachments are references, not bytes: `file_attachments` entries are asset-reference strings, and the record promises nothing about any payload they name.

The seam's shared rules apply here as on [the Claude export page](import-claude.md): an id outside the identifier grammar is refused by name rather than sanitized, a repeated id within one export is refused, a manifest is refused before anything is archived (its download links are one-time-use credentials), each conversation becomes one `chat-stasher/inbox@3` `web-capture` bundle that a later `chat-stasher ingest --inbox` seals, the run never seals or pushes anything itself, both `--stage` and `--inbox` are named with no defaults, the whole export file is stored byte-exact under `<stage>/import-raw/grok/<sha256>` and never moved or rewritten, and the bundle claims no capture time, no capture fingerprint and no account.

## Coverage and completeness

The parser ships in this build and was written from the measured census of a real export, pinned by tests against that shape — but no end-to-end run of an import has been recorded yet, so the "Last verified" row above says so rather than carrying an invented date. The measured payload runs to 65 MB and is read whole into memory, which fits comfortably and is why this parser needs no streaming. What the export covers is the account's own statement of itself, which is what makes it the closing path for conversations the live capture never saw.

## Quirks

A Grok export is an object with a `conversations` key; a Claude `conversations.json` is a flat array. Each parser refuses the other's shape by name, so a file passed to the wrong platform is a named refusal, never a misread. Because one zip holds the payload, the payload must be unzipped out first; passing the manifest is the expected mistake and leads the refusals.

## Known issues

The archive probe is not wired — the importer cannot yet suppress a body whose normalized form already equals the archive's latest, so every conversation gets a body and the run prints the probe's own answer. The `import-raw/` namespace is local for now, not yet wired into `push` or archive readback. And no account is claimed for imported conversations: the takeout names conversations, not the account that requested it.

## How to check it yourself

Request the export from grok.com, unzip it, and run [`chat-stasher import grok`](../cli.md#import) with the `prod-grok-backend.json` payload, an `--inbox` and a `--stage` you name: the command prints the raw copy's digest, the conversation and bundle counts, and every rejection by name. Then `chat-stasher ingest --inbox <dir>` seals the bundles, `chat-stasher read` renders a conversation, and `chat-stasher verify` checks the stage against the destination. To see the gap this import closes, compare the imported count against the web-captured sessions on the web page — the note linked above records how the two populations were measured against each other.

## Changelog

2026-09-24 — the export's shape measured in the census of a real takeout: 312 conversations, 2,397 responses; the whole-list, carry-the-times, missing-parent-is-root and references-not-bytes rules recorded.
2026-10-07 — the recall oracle measured the extension's historical gap against this export (61 of 312 conversations archived, the 251 missing all predating the capture window) and named this import the closing path; recorded in the capture-gap note.
2026-10-08 — the Grok parser shipped behind the seam (`chat-stasher import grok`), carrying each wrapper whole and classifying a fully read malformed export the same way the Claude leg does.
