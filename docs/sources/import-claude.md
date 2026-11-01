<!-- BEGIN GENERATED -->
# Claude (official export)

| Field | Value |
|---|---|
| Id | `claude` |
| Surface | export |
| Export file | `conversations.json` |
| Parser | shipped in this build (`chat-stasher import claude <export> --inbox <dir> --stage <dir>` parses it) |
| Last verified end to end | not yet verified end to end |
<!-- END GENERATED -->

## What it is

This page is about claude.ai's official export: the takeout the platform's own export flow delivers, requested by the account holder. The parser reads the `conversations` category's `conversations.json` — the export arrives as one zip per category, and this file is the single flat JSON array of conversation objects that category holds. `chat-stasher import claude <export> --inbox <dir> --stage <dir>` parses it in this build and is the archive's whole-account path for this platform; the live counterpart is [the claude.ai web page](web-claude.md), whose capture only reaches conversations somebody opened.

## What we collect

Each element of the array is one conversation object, and the parser carries it whole — its `uuid` becomes the conversation's id, and every field the object holds travels into the record field for field, with nothing projected away. Two rules a payload cannot argue its way past: an id outside the identifier grammar (a leading ASCII alphanumeric, at most 255 characters, then alphanumerics, `-` and `_`) is refused by name rather than silently sanitized into a session path, and an id that repeats within one export is refused, because two entries under one id would be two bodies for one session. A conversation inside a readable export that cannot be carried stays a named rejection reported beside the ones that were imported.

The file is refused, in words, in each direction it can be wrong. A manifest (a file whose `data_files` key lists the categories) is refused because its download links are one-time-use credentials that must never be echoed — and the refusal happens before anything is archived, so a run that ends `2` or `3` has written nothing anywhere, the export file included. JSON that stops part-way means the read never finished and nothing about the contents is proven (exit `3`); JSON read to the end that is not this shape is a completed read of the wrong input (exit `2`); a conversation refusal or a failed write after the read is its own answer (exit `1`).

Every carried conversation becomes one ordinary `chat-stasher/inbox@3` `web-capture` bundle in the named inbox — the same bundle kind an extension capture is — which a later `chat-stasher ingest --inbox` seals into the archive. The import itself never seals, never pushes, never reads the config, a destination or a machine identity; `--stage` and `--inbox` are named on the command line and neither has a default, a stage that is not there is refused rather than created, and the inbox folder is created because it is the drop folder you pointed at. The whole export file is also stored byte-exact under `<stage>/import-raw/claude/<sha256>` — its own namespace beside `sessions/`, entering no session count — and it is never moved or rewritten, so importing from the same file again is safe.

Each bundle is deliberately sparse about what it claims: no capture time (the run's own moment travels in the raw-export provenance record, and an unchanged conversation is byte-identical across takeouts, so the archive's content-addressed dedup keeps one body no matter how many takeouts carry it), no capture fingerprint (that derivation belongs to the extension, and the importer never re-derives a different value under the same name), and no account claim (a conversations file does not prove an account: until the export's users file is read, the bundle states identity level `default`, the unreliability itself).

## Coverage and completeness

The parser ships in this build and no end-to-end run of the import has been recorded yet, so the "Last verified" row above says so rather than carrying an invented date. A measured conversations file runs to tens of megabytes and is read whole into memory — it fits comfortably; the seam's one larger format, a different platform's export, is the one that will need streaming. What the export covers is the account's own statement of itself: every conversation the platform still holds at export time, including ones the extension never saw.

## Quirks

The one zip-per-category layout means the conversations file must be unzipped out first; passing the manifest instead of the payload is the expected mistake, so it is the named refusal the command leads with. An export refused by the parser is not archived byte-exact either — keeping a refused file token-for-token would write a credential delivery into the stage, which is why the raw copy is made only after the parse succeeds.

## Known issues

The archive probe is not wired: the importer cannot yet read back the archive to learn that a conversation's normalized body already equals its latest, so every conversation gets a body and the run prints the probe's own answer. The `import-raw/` namespace is local for now — not yet wired into `push` or archive readback — so a raw export staged by an import stays with the stage it was named against. And no account is bound to the imported conversations until the users-file slice lands.

## How to check it yourself

Request the export from claude.ai, unzip the conversations category, and run [`chat-stasher import claude`](../cli.md#import) with the payload file, an `--inbox` and a `--stage` you name: the command prints the raw copy's digest, the conversation and bundle counts, and every rejection by name. Then run `chat-stasher ingest --inbox <dir>` to seal the bundles, `chat-stasher read` on one of the imported conversations, and `chat-stasher verify` to check the stage against the destination. A conversation the web page never captured shows up here only through this import.

## Changelog

2026-10-07 — the import seam first shipped with this parser: the flat-array `conversations.json` read whole, the manifest refused, one inbox bundle per conversation, and the export file kept byte-exact beside `sessions/`.
2026-10-08 — a malformed export that was read to the end is classified as wrong input (exit `2`) rather than an incomplete read, keeping the three kinds of refusal apart by exit code.
