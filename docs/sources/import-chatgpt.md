<!-- BEGIN GENERATED -->
# ChatGPT (official export)

| Field | Value |
|---|---|
| Id | `chatgpt` |
| Surface | export |
| Parser | parser written, not yet offered by the command |
| Last verified end to end | not yet verified end to end |
<!-- END GENERATED -->

## What it is

This page is about ChatGPT's official export: the data takeout the platform delivers on request, whose conversation file is `conversations.json`. The live counterpart is [the chatgpt.com web page](web-chatgpt.md). An official export is the platform's own statement of what an account holds, which is why it is the measuring stick for the live capture rather than something to pour into the same archive without thought.

## What we collect

In this build, nothing: the import command names the platform (its spelling on the command line is `chat-gpt`) but refuses it. On 2026-11-01 a run of `chat-stasher import chat-gpt <file> --inbox <dir> --stage <dir>` on a synthetic file exited 2 with "this build has no chatgpt export parser", saying the export was not importable and that nothing was sealed. The cause is in `crates/chat-stasher/src/import.rs`: the platform registry in `crates/chat-stasher/src/import.rs` has no parser wired for it. The generated header above says the same (`parser written, not yet offered by the command`).

A parser for the export does exist in the source (`crates/chat-stasher/src/import/chatgpt.rs`). Its module notes say it takes the bytes of one export and returns conversation-level records, writes nothing, reads no disk and reaches no network. Its reading of the shape, measured on two single-conversation copies (35 and 154 mapping nodes) with no value, id or title quoted:

- The file is a top-level JSON array of conversation objects; a single conversation object is also accepted.
- A conversation's tree is the `mapping` object keyed by node id; exactly one node has no parent. The parser reads `conversation_id` (or `id`), `create_time`, `update_time`, `title`, `current_node` and `mapping`, and reports the other keys by name instead of dropping them silently.
- `current_node` is carried as a statement the export makes about itself, not as the conversation: the notes record that it is not the deepest branch, so the reading published is the longest branch, with the leaf count beside it so the choice is visible.
- Timestamps are float epochs in seconds on every value measured; the parser also reads milliseconds and names the unit it read.
- Absent, `null` and unreadable-shape values are kept as three different states, and a conversation that cannot become a record is reported with a reason rather than skipped.

## Coverage and completeness

There is no coverage to state, because no import of a ChatGPT export can run in this build. The parser's measurements come from two single-conversation copies, not from a whole account export; the notes record that the full takeout package is 3.53 GiB, that it would have to be streamed, and that the parser takes one export in memory, so it does not handle that file. Nothing here has an end-to-end run, which is why the header carries no verification date.

## Quirks

The import module's notes describe a development gate: the export is meant to validate the extension's own ChatGPT backfill, and importing it into the archive under measurement would fill the extension's gaps from the very corpus that is supposed to expose them. That is the stated reason the importer is deferred. The extension's historical backfill for ChatGPT is implemented but a completed run has not been observed in a real browser ([web-chatgpt](web-chatgpt.md)).

## Known issues

The import command does not accept this platform yet, and the parser does not stream, so it cannot read a full-size takeout. Both are open work, not defects of a running feature.

## How to check it yourself

Run `chat-stasher import chat-gpt <export> --inbox <dir> --stage <dir>`: the expected result today is the named refusal (exit 2), and nothing is sealed. The parser's tests sit beside it in `crates/chat-stasher/src/import/chatgpt.rs`.

## Changelog

The changelog starts when the command offers the platform; the parser's history is in the repository log.
