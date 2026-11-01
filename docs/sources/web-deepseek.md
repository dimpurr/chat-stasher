<!-- BEGIN GENERATED -->
# chat.deepseek.com (web)

| Field | Value |
|---|---|
| Id | `deepseek` |
| Surface | web |
| Origins | https://chat.deepseek.com |
| Channel | stable |
| Capture credibility | from-source |
| Status | verified end-to-end (2026-09-24) |
| Last verified end to end | 2026-09-24 (extension 0.1.0.18): live acceptance: real conversations archived end to end on a real machine (main e71eb33) |

OS availability comes from the browser registration table, not from this row: see the browser grid in [the support tables](../support.md).
<!-- END GENERATED -->

## What it is

chat.deepseek.com is the DeepSeek web chat, and this page is about what the browser extension archives from it. The extension works inside your logged-in browser and reads what the page itself fetches; it never drives the site and never holds an account credential of its own. Capture runs in the stable channel, so a release build of the extension has it on. On this platform the extension also adds no request of its own — unlike the ChatGPT and Gemini rows, which ask the page for more — it only observes the one request the page makes when you open a conversation.

## What we collect

Exactly one route is captured: the content request the page makes to load a conversation's history — a GET to `/api/v0/chat/history_messages` with the session named in its `chat_session_id` query — accepted only when the answer is a 2xx JSON envelope whose nested `data.biz_data.chat_session.id` names a session and whose `data.biz_data.chat_messages` is an array. An empty `chat_messages` array passes as a measured zero-message conversation; a non-array value there is a changed response, never an empty chat. The body's nested session id must equal the id the request named, so a body for one conversation cannot be filed under another request's id — a page-supplied payload that fails that binding is silently refused, because a page payload is untrusted. The conversation id is read from that same query. Three things sit deliberately outside the row: the neighbouring `chat_session/fetch_page` route (a conversation index, not a conversation), the POSTs under the old `/api/v0/chat` prefix (the generate endpoint), and WebSocket frames, which no shipped row observes on this platform.

## Coverage and completeness

The live leg captures a conversation when somebody opens it on chat.deepseek.com while the extension is active, and what it observes it archives whole. The route and the response shape were measured in a live logged-in session (2026-09-13): the page loads a past conversation over exactly this request. The dated acceptance on the generated block above — 2026-09-24, extension 0.1.0.18 — ran against that build's then-broader match (any GET or POST under `/api/v0/chat` or `/chat/session`), so it is a true fact about that build and not a verification of the narrowed route above, which postdates it (2026-10-06) and has not been live-verified since.

The designed path for history nobody opened is the backfill leg: it lists conversations from `/api/v0/chat_session/fetch_page` (pages of `count=`, cursor `before_seq_id`, continuation decided only by the envelope's `has_more`), then fetches each conversation's body through your open, logged-in tab — one request per conversation. Before a body is stored, its own tree is walked: `chat_session.current_message_id` back along each message's `parent_id` must close at a root, and a walk that leaves the messages the response carries refuses that whole conversation (`detail-tree-incomplete`) rather than archiving a partial one. A completed backfill has not yet been observed in a real browser ([the threat model](../../docs-dev/threat-model.md) records the list-and-body legs as implemented but unobserved on all five platforms that have them). Historical conversations can also be imported from a DeepSeek offline export once the parser is offered by the CLI (see [the DeepSeek export page](import-deepseek.md)).

## Quirks

The platform refuses in-band: a refused list or body request answers HTTP 200 with the failure named inside the envelope (`code: 40002` "Missing Token" on the list, `code: 40003` "INVALID_TOKEN" on the body — measured 2026-09-23 from the page's own context). The extension reads that as a named auth refusal, never as zero conversations. The completeness walk above proves the response is closed over the visible branch — every ancestor of the newest message present, the chain ending at a root — and not that the server sent every discarded sibling branch or refrained from truncating a long conversation: no implementation reached while measuring this route pages the body, and whether the endpoint can truncate remains unmeasured. That is also why a body whose tree pointers are unreadable halts `shape-changed` rather than being called incomplete — "this conversation is partial" is not something this code may say about a conversation it never checked.

## Known issues

An account switch is not yet guarded on this platform: after switching accounts, the new account's list ids can land in the old run's scope (see issue #4). Whether a very long conversation arrives complete from the history endpoint is unverified — the endpoint is not known to page — so the backfill's tree walk is the only check that stands between a short answer and a stored one, and its residual gap (a truncation that also rewrote the boundary message's `parent_id` to look like a root) is written down rather than closed.

## How to check it yourself

Load the extension in a Chromium browser, log into `https://chat.deepseek.com`, and open a conversation: the popup's capture indicator confirms the content route fired, and the page's own request is the only traffic involved. `chat-stasher read` on the staged session renders speakers, times and text; `chat-stasher verify` confirms what was staged is what the destination holds; and `chat-stasher search --text` finds text from it after `chat-stasher index build`. An old conversation you have not opened will be absent until backfill or an official-export import covers it — the pages linked above explain the difference.

## Changelog

2026-08-14 — the route family and envelope keys first read from open-source reference implementations.
2026-08-17 — the conversation-list endpoint, its paging parameters and the envelope cross-checked across four independent implementations.
2026-09-13 — the conversation route measured live in a logged-in session: the page loads a past conversation as a GET to `/api/v0/chat/history_messages` with a `chat_session_id` query.
2026-09-14 — the backfill leg's body segment filled from that measurement; the live and backfill legs stand on the same measured route.
2026-09-19 — the backfill body gained a completeness gate: the message tree is walked before storing, and an open walk refuses that conversation instead of archiving part of it.
2026-09-23 — the platform's in-band refusals (HTTP 200 with an envelope code) recognized and named as auth refusals, never read as zero conversations.
2026-09-24 — live acceptance: real conversations archived end to end on a real machine by extension build 0.1.0.18 (main e71eb33), against that build's broader match.
2026-10-06 — the capture row narrowed to the one measured route: GET-only, the body's id bound to the query's id, and the old broad `/api/v0/chat` and `/chat/session` shapes no longer matching.
