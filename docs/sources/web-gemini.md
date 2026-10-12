<!-- BEGIN GENERATED -->
# gemini.google.com (web)

| Field | Value |
|---|---|
| Id | `gemini` |
| Surface | web |
| Origins | https://gemini.google.com |
| Channel | stable |
| Capture credibility | from-source |
| Status | verified end-to-end (2026-09-24) |
| Last verified end to end | 2026-09-24 (extension 0.1.0.18): live acceptance: real conversations archived end to end on a real machine (main e71eb33) |

OS availability comes from the browser registration table, not from this row: see the browser grid in [the support tables](../support.md).
<!-- END GENERATED -->

## What it is

gemini.google.com is Google's web chat interface for Gemini. This page covers what the browser extension archives from it. The extension runs inside your browser session, passively capturing the Google batchexecute RPC payloads that the web client exchanges with the server. It stores no credentials and requires no external API keys. Capture is active in the stable channel.

## What we collect

The extension intercepts POST requests to `/_/BardChatUi/data/batchexecute`. Because Google packages multiple RPC methods into this single multiplexed endpoint, requests are distinguished by RPC IDs within URL-encoded form data. The response is encoded in Google's batchexecute text format and verified by checking for required markers (`wrb.fr` and `hNvQHb`).

The extension extracts session IDs from the turn contents using the canonical `c_`-prefixed identifier format (`c_<id>`), which is the authoritative identity used by Google's backend. The bare identifier in the page URL (`/app/<id>`) is retained only as a fallback.

To handle conversations spanning multiple pages, the live capture hook inspects continuation tokens and requests successive pages until all turns are fetched, attaching the necessary XSRF tokens from the page's `WIZ_global_data`. Delivered bundles include both the message transcript and conversation list metadata.

## Coverage and completeness

Live capture covers any conversation opened in the browser. When an opened conversation contains more messages than fit in a single response, the capture handler follows continuation tokens page by page.

However, a safety ceiling is enforced: if a conversation requires more than 20 detail pages, the capture is rejected and logged as a failure rather than stored partially.

Historical backfill is implemented via batchexecute RPC enumeration, but an end-to-end backfill completion has not yet been observed in a live browser. An official export parser exists in the codebase (see [the Gemini export page](import-gemini.md)), though it is not yet offered by the CLI. The live leg was verified end to end on 2026-09-24 with extension build 0.1.0.18.

## Quirks

Google's batchexecute protocol wraps JSON arrays in escaped string chunks inside array envelopes. The normalizer and activity modules parse these nested frames to extract message timestamps and text.

Requests to the batchexecute endpoint require an XSRF token (the `at` field) and session identifiers retrieved dynamically from `WIZ_global_data`. If the token is absent or invalid, the backend returns HTTP 400 with a structured error; the extension recognizes this as an authentication refusal rather than an empty conversation list.

## Known issues

Conversations exceeding 20 detail pages are rejected and not archived in part (documented in the project privacy note).

Account switches are not yet guarded: switching Google accounts in the same browser profile can cause conversation list IDs from the new account to land in the previous account's run scope (tracked in issue #4).

## How to check it yourself

Open `https://gemini.google.com` in a browser with the extension installed, then click into a conversation. The extension popup will reflect the captured request. Run `chat-stasher read` to verify that the nested batchexecute frames are decoded into distinct User and Assistant messages. Check `chat-stasher verify` to validate data integrity and run `chat-stasher search --text "<query>"` to verify indexing.

## Changelog

2026-08-17 — batchexecute route and XSRF extraction from `WIZ_global_data` established.
2026-09-14 — canonical `c_` session identity handling and continuation token pagination implemented.
2026-09-24 — live acceptance: real conversations archived end to end on a real machine by extension build 0.1.0.18 (commit e71eb33).
2026-10-12 — delivered capture bundle enhanced to store conversation list metadata alongside parsed turns.
