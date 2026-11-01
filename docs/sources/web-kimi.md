<!-- BEGIN GENERATED -->
# kimi.com (web)

| Field | Value |
|---|---|
| Id | `kimi` |
| Surface | web |
| Origins | https://www.kimi.com |
| Channel | experimental |
| Capture credibility | from-source |
| Status | experimental |
| Last verified end to end | not yet verified end to end |

OS availability comes from the browser registration table, not from this row: see the browser grid in [the support tables](../support.md).
<!-- END GENERATED -->

## What it is

kimi.com is the Kimi web chat, and this page is about what the browser extension would archive from it. The extension works inside your logged-in browser and reads what the page itself fetches; it never drives the site and never holds an account credential of its own. Kimi is on the experimental channel: [the extension contract](../../apps/extension/lib/contract.ts) marks the row `experimental`, a stable build omits experimental platforms entirely, and only a development build (`pnpm build:dev`) registers it. [The install page](../install.md) states it in user terms: Perplexity and Kimi are in the development build only. The only origin is `https://www.kimi.com`; the legacy `kimi.moonshot.cn` origin is deliberately not registered because there is no source-backed evidence the current route is served from it.

## What we collect

The row registers one route: the `POST` the page makes to `ChatService/ListMessages` on the `/apiv2/` gateway to load one conversation's messages, accepted only when the response is a 2xx whose JSON body has a top-level `messages` array. A body whose `messages` is absent or not an array fails the shape check and is warned about rather than filed as an empty-looking capture. The conversation index (`FeedService/ListFeeds`) is outside the row: its items carry a name and a `messageContent` preview, not messages, and a preview is not the conversation. The chat id travels in the request body, not the URL, so the live leg names the capture from the page URL (`/chat/<id>`), with a deliberately wide character class and no minimum length. WebSocket frames are not observed (`webSocketCapture: false`).

Read-side, this build has no extractor of its own for Kimi web: an archived bundle goes to the generic reader (`normalize.rs`), which reads a top-level `messages` array and counts any item it cannot turn into a message as unrendered rather than inventing text. How much of a real Kimi `messages` item (`id, parentId, role, status, blocks, scenario, createTime, isGoal` were the keys measured) that reader turns into text has not been checked against a real capture; this is from reading the code, not from running it.

## Coverage and completeness

The route was measured in a logged-in www.kimi.com session on 2026-09-14: the list call and the detail call, with five conversations sampled, each 2 to 3 messages. That is a measurement of the request and response shape, not a verified end-to-end archive: the 2026-09-24 live acceptance of extension 0.1.0.18 covered the five stable-channel platforms only, and Kimi was inert in that build, so the generated header says `not yet verified end to end`.

A backfill leg is implemented: it lists conversations with `FeedService/ListFeeds` (page size and page token in the POST body; the measured run returned 3 items then 2 with no overlap and the token absent on the last page) and fetches each conversation's messages. Its requests carry the page's own login token, read from the page's local storage at request time and held in memory only; a cookie-only list request was measured to answer HTTP 401, which the leg treats as an auth halt, never as "no conversations". Whether a long conversation's detail response pages was not measured: every sampled conversation was short and carried no page-token field. If a response ever does carry one, the plan refuses to archive that conversation and records a failure instead of storing a truncated one as whole. A completed Kimi backfill has not been observed in a real browser ([the extension install notes](../../docs-dev/install.md)).

## Quirks

Kimi uses connect-style unary RPC: every call is a `POST` with a JSON body. The service package is spelled `kimi.gateway.chat.v1.ChatService` on the measured page and `kimi.chat.v1.ChatService` in one outside implementation, so the route hint stops at `ChatService/ListMessages` and the shape check decides what is conversation data. Kimi also runs front-end signing and WAF challenges according to the contract; that affects the page's own requests, and the extension only reads what the page has already fetched. Whether the gateway actually requires the two extra headers the page sends (`x-msh-platform`, `x-language`) is recorded as untested.

## Known issues

An account switch is not yet guarded on this platform: after switching accounts, the new account's list ids can land in the old run's scope (see issue #4). Long-conversation paging is unmeasured and handled by refusing, as above. The row is `from-source` credibility.

## How to check it yourself

Build the development extension (`pnpm build:dev` in `apps/extension`), load it in a Chromium browser, log into www.kimi.com and open one conversation. No end-to-end run has been recorded for this platform, so treat the result as a first observation: `chat-stasher read` on the staged session shows what the reader made of it. A stable (released) build will show nothing for this platform by design.

## Changelog

2026-08-17 — the first row was written from four outside implementations that agree on the `ListMessages` route (checked as source code on that date).
2026-09-14 — the list and detail routes were measured in a logged-in www.kimi.com session (five short conversations), and the backfill plan was written from that measurement (W22).
2026-09-24 — live acceptance of the stable channel did not include this platform: it was inert in that build.
