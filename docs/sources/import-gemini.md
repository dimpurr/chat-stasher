<!-- BEGIN GENERATED -->
# Gemini (official export)

| Field | Value |
|---|---|
| Id | `gemini` |
| Surface | export |
| Export file | `My Activity/Gemini Apps/MyActivity.json` |
| Parser | parser written, not yet offered by the command |
| Last verified end to end | not yet verified end to end |
<!-- END GENERATED -->

## What it is

This page is about Gemini's data from Google Takeout. It is not a set of conversations: Takeout delivers Gemini as an activity log, `My Activity/Gemini Apps/MyActivity.json`, a JSON array with one row per thing the account did with Gemini. A row is evidence of a prompt, not of a conversation. The live counterpart is [the gemini.google.com web page](web-gemini.md).

## What we collect

In this build, nothing: the import command names the platform (its spelling on the command line is `gemini`) but refuses it. On 2026-11-01 a run of `chat-stasher import gemini <file> --inbox <dir> --stage <dir>` on a synthetic file exited 2 with "this build has no gemini export parser", saying the export was not importable and that nothing was sealed. The cause is in `crates/chat-stasher/src/import.rs`: it has no parser wired for it there. The generated header above says the same (`parser written, not yet offered by the command`).

A parser exists in the source (`crates/chat-stasher/src/import/gemini.rs`). It returns observations, not conversations, and builds no bundle. The shape it reads was measured on the 2026-09-24 Takeout payload (26,508,335 bytes):

- a top-level array of 1,980 rows, each with `header`, `title`, `time`, `products` and `activityControls`;
- `time` is an RFC 3339 string (1,977 rows with milliseconds, 3 without);
- the conversation id is not a top-level field: it is nested in `details[].url`, spelled `https://gemini.google.com/app/<id>`, and the live capture files the same conversation as `c_<id>`;
- 1,842 rows name exactly one conversation id, 91 name more than one, and 47 name none (those are `Created`, `Used`, `Cleared`, `Gave` or `Selected` activity rather than prompts).

The identity rule is decided from `details[].url` and never invented. One distinct id means the row is that conversation; none means a single fixed "unknown session" stub is used and the join falls back to prompt text, marked weak; more than one means the row is marked ambiguous under the same stub, because picking the first would invent a session boundary the source did not state. The parser does not merge rows into conversations. The row's `title` and `safeHtmlItem` are user content: they are carried in the returned structures, and nothing in the parser logs or prints them.

## Coverage and completeness

There is no coverage to state, because no import of a Gemini export can run in this build, and the parser has no end-to-end run recorded. The measurements come from one payload. Because Takeout holds prompts as activity, even a future import would describe what the account asked, with the limits above, and would not by itself reconstruct full conversations.

## Quirks

An earlier inventory that read a top-level `id` on each row counted zero conversation ids; the id lives one level down in `details[].url` (stated in the parser's notes). Rows without a conversation id are a real share of the file (47 of 1,980 measured).

## Known issues

The command does not accept this platform yet, and the grouping of observations into conversations is not implemented in the parser.

## How to check it yourself

Run `chat-stasher import gemini <export> --inbox <dir> --stage <dir>`: the expected result today is the named refusal (exit 2), and nothing is sealed. The parser's tests sit beside it in `crates/chat-stasher/src/import/gemini.rs`.

## Changelog

The changelog starts when the command offers the platform; the parser's history is in the repository log.
