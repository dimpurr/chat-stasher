<!-- BEGIN GENERATED -->
# claude.ai (web)

| Field | Value |
|---|---|
| Id | `claude` |
| Surface | web |
| Origins | https://claude.ai |
| Channel | stable |
| Capture credibility | from-source |
| Status | verified end-to-end (2026-09-24) |
| Last verified end to end | 2026-09-24 (extension 0.1.0.18): live acceptance: real conversations archived end to end on a real machine (main e71eb33) |

OS availability comes from the browser registration table, not from this row: see the browser grid in [the support tables](../support.md).
<!-- END GENERATED -->

## What it is

claude.ai is Anthropic's web conversational AI platform. This page describes what the browser extension archives from it. The extension runs inside your logged-in browser, monitoring network traffic read-only as the application interacts with Anthropic's backend. It requires no API tokens, stores no login credentials, and performs no unauthorized automation. Capture is enabled on the stable release channel.

## What we collect

The extension intercepts the conversation detail route: HTTP GET requests matching `/chat_conversations/<uuid>` (under the `/api/organizations/<org>/` endpoint hierarchy) that succeed with a 2xx status. The response is validated against a strict JSON schema requiring the `chat_messages` array. The trailing slash on `/chat_conversations/` is deliberate: it captures individual conversation details while silently ignoring the `/chat_conversations` list endpoint (which returns only summary metadata, not transcript text). Org-level settings and project routes outside this hint are also ignored. Session UUIDs are extracted either from the API path or the browser address bar (`/chat/<uuid>`). WebSocket connections are ignored (`webSocketCapture: false`). The delivered bundle stores the full response body, so the message tree travels with the name and create/update timestamps the detail body itself carries.

## Coverage and completeness

Live capture operates passively: whenever you view or load a conversation on claude.ai, the extension captures the complete message tree. Unopened conversations remain unrecorded by the live leg.

A backfill mechanism is implemented to enumerate historical chats across an organization and request conversation bodies sequentially. An organization resolution step runs when neither request headers nor cookies supply the active organization UUID. However, a complete backfill pass has not yet been observed completing in a real browser.

To archive historical conversations comprehensively without manually clicking every chat, users can import an official data takeout with `chat-stasher import claude <export> --inbox <dir> --stage <dir>` (see [the official export page](import-claude.md)). The live leg was verified end to end on 2026-09-24 using extension build 0.1.0.18.

## Quirks

The capture row is source-backed: the endpoint path was read out of public open-source reference implementations, not from a logged-in claude.ai session — the contract row records that nobody on that change ever opened claude.ai — and was only then live-verified by the recorded end-to-end acceptance (2026-09-24, extension 0.1.0.18). The query string tree parameter varies between platform implementations (`?tree=true` vs `?tree=True`), so the interceptor matches the path prefix without query string constraints.

The body validator mandates the presence of `chat_messages`. If the backend alters the response structure, the extension rejects the payload and emits a shape mismatch warning rather than creating a deceptively empty archive record.

## Known issues

Two accounts belonging to the same organization are not yet distinguished by the organization verification check, which can lead to shared scope assignment across accounts within one org (tracked in issue #4).

## How to check it yourself

Install the extension, log into claude.ai, and open an existing chat. The extension popup indicator confirms successful payload capture. In the CLI, execute `chat-stasher read` to inspect the normalized turns, timestamps, and speaker roles. Execute `chat-stasher verify` to check repository hash seals, and `chat-stasher search --text "<query>"` to confirm searchability.

## Changelog

2026-08-17 — interceptor route (`/chat_conversations/`) and body gate (`chat_messages`) defined from reference implementations.
2026-09-24 — live acceptance: real conversations archived end to end on a real machine by extension build 0.1.0.18 (commit e71eb33).
