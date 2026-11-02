<!-- BEGIN GENERATED -->
# DeepSeek (official export)

| Field | Value |
|---|---|
| Id | `deepseek` |
| Surface | export |
| Export file | `conversations.json` |
| Parser | parser written, not yet offered by the command |
| Last verified end to end | not yet verified end to end |
<!-- END GENERATED -->

## What it is

This page is about DeepSeek's official export: the takeout whose conversation file is `conversations.json`. The live counterpart is [the chat.deepseek.com web page](web-deepseek.md). Not to be confused with the local [DeepSeek Harness](deepseek-harness.md), which is a different source.

## What we collect

In this build, nothing: the import command names the platform (its spelling on the command line is `deep-seek`) but refuses it. On 2026-11-01 a run of `chat-stasher import deep-seek <file> --inbox <dir> --stage <dir>` on a synthetic file exited 2 with "this build has no deepseek export parser", saying the export was not importable and that nothing was sealed. The cause is in `crates/chat-stasher/src/import.rs`: it has no parser wired for it there. The generated header above says the same (`parser written, not yet offered by the command`).

A parser exists in the source (`crates/chat-stasher/src/import/deepseek.rs`). Its notes describe a pure function from the export's bytes to conversation-level records, with no disk, network or archive writes. The shape it reads was measured on one export dated 2026-07-01 (13,074,881 bytes):

- a top-level JSON array of 234 conversations, each with `id`, `inserted_at`, `updated_at`, `title` and `mapping` (234 of 234);
- the conversation `id` equals the id the live web capture already files DeepSeek sessions under (234 of 234), so the two join on it without a mapping of its own;
- `mapping` is an object keyed by node id (2,166 nodes in all) with exactly one root per conversation; 59 of the 234 conversations are trees with more than one leaf;
- a message carries `files`, `model`, `inserted_at` and `fragments`; the fragment kinds measured are `REQUEST`, `RESPONSE`, `THINK`, `SEARCH`, `TOOL_SEARCH`, `TOOL_OPEN` and `FILE`;
- every timestamp measured was an RFC 3339 string with an offset; any other spelling is reported as unknown with its reason.

Three things the parser deliberately does not do: it does not pick a branch (the export names no current branch, so which leaf is "the" conversation is recorded as not named by the source), it does not turn fragment kinds into speakers, and it does not convert a time spelling it has not measured. An absent field, an explicit `null` and an unreadable shape stay three different states, and a conversation whose `mapping` is missing or not an object is reported as a failure with its id rather than emitted as an empty conversation.

## Coverage and completeness

There is no coverage to state, because no import of a DeepSeek export can run in this build, and the parser has no end-to-end run recorded. Its measurements come from a single export; the notes do not claim that other accounts or later exports have the same shape.

## Quirks

The import module's notes describe a development gate for this platform: the takeout is meant to validate the extension's capture before it is poured into the archive being measured. The extension side, for its part, was verified end to end on 2026-09-24 for live capture; see [the web page](web-deepseek.md).

## Known issues

The command does not accept this platform yet. Fragment kinds are not mapped to speakers by the parser, so a reader built on it has to decide that.

## How to check it yourself

Run `chat-stasher import deep-seek <export> --inbox <dir> --stage <dir>`: the expected result today is the named refusal (exit 2), and nothing is sealed. The parser's tests sit beside it in `crates/chat-stasher/src/import/deepseek.rs`.

## Changelog

The changelog starts when the command offers the platform; the parser's history is in the repository log.
