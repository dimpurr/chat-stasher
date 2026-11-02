<!-- BEGIN GENERATED -->
# perplexity.ai (web)

| Field | Value |
|---|---|
| Id | `perplexity` |
| Surface | web |
| Origins | https://www.perplexity.ai |
| Channel | experimental |
| Capture credibility | from-source |
| Status | experimental |
| Last verified end to end | not yet verified end to end |

OS availability comes from the browser registration table, not from this row: see the browser grid in [the support tables](../support.md).
<!-- END GENERATED -->

## What it is

perplexity.ai is the Perplexity web answer engine, and this page is about what the browser extension would archive from it. The extension works inside your logged-in browser and reads what the page itself fetches; it never drives the site and never holds an account credential of its own. Perplexity is on the experimental channel: [the extension contract](../../apps/extension/lib/contract.ts) marks the row `experimental`, a stable build omits experimental platforms entirely, and only a development build (`pnpm build:dev`, which sets `CS_RELEASE_CHANNEL=dev`) registers it. [The install page](../install.md) says the same in user terms: the released build covers ChatGPT, Claude, DeepSeek, Gemini and Grok, and Perplexity is in the development build only.

## What we collect

The row registers one route: the `GET` the page makes under `/rest/thread/` to load one thread, accepted only when the response is a 2xx whose JSON body has an `entries` array. An empty `entries` array passes the shape check (it is a measurement, not a missing field); a body with no `entries` is refused and warned about rather than filed as an empty-looking capture. The list request (`POST /rest/thread/list_ask_threads`) and the named sibling routes (mark-viewed, set-title, delete) are `POST`s, so they fall outside the row by method.

The conversation is filed under the thread's slug from the page URL (`/search/<slug>`) first, and from the content URL's own path second; the second pattern excludes the closed set of known sibling route names so a list response is not filed as a thread. If neither names a thread, the capture is refused rather than named from an identifier inside the body. WebSocket frames are not observed (`webSocketCapture: false`).

Read-side, this build has no extractor of its own for Perplexity: an archived Perplexity bundle goes to the generic reader (`normalize.rs`), which looks for a top-level `messages` array or a top-level array of records. The row's envelope is keyed by `entries`, so by that code the generic reader does not find messages in it and counts the body as unrendered rather than showing an invented transcript. This is from reading the code, not from running a real Perplexity capture, and the raw archived bytes are unaffected.

## Coverage and completeness

Nothing on this page is a verified end-to-end claim. The capture row is `from-source`: the route and envelope were read out of four independent reference implementations and the site's own extracted endpoint table (the contract records the sources as checked 2026-08-17, and the content route as read out of them in W28 on 2026-09-14), and, in the contract's own words, nobody has opened a logged-in perplexity.ai page with the capture row. The 2026-09-24 live acceptance of extension 0.1.0.18 covered the five stable-channel platforms only; Perplexity was inert in that build and carries no verification record, so the generated header says `not yet verified end to end`.

A backfill leg is implemented for the platform: it lists threads with `POST /rest/thread/list_ask_threads` (an offset page with no termination field the plan could read, so an empty or short page is a client-inferred end, never recorded as the API saying the list is complete) and fetches each thread with `GET /rest/thread/<slug>` and a pinned five-key query. A 2026-09-23 probe from a logged-in page (three body requests) observed top-level `has_next_page` and `next_cursor` fields, and the plan archives a body only when the response says it holds the whole thread, refuses one that says there is more, and refuses an empty `entries` as unverified. That probe observed one thread with one entry; it is not a completed backfill, and a completed backfill has not been observed in a real browser. Whether the live capture delivers the conversation it names is recorded as unverified in [the extension install notes](../../docs-dev/install.md).

## Quirks

Perplexity identifies a thread by slug on the wire, and the content route accepts either slug or uuid. The live leg takes the slug from the address bar first so the same thread is not filed twice under two names; the stated cost is that a prefetched request for a different thread would be named after the address bar. The contract also records that whether the content envelope carries a session-id candidate key was never measured, so a body is never used to name a capture.

## Known issues

An account switch is not yet guarded on this platform: after switching accounts, the new account's list ids can land in the old run's scope (see issue #4). The route and envelope have not been seen on a real logged-in page by the capture row; if the real shape differs, the shape check rejects the body and the page hook warns instead of guessing. Whether a very long thread arrives complete is covered only by the `has_next_page` rule above, from a probe that did not send a long thread.

## How to check it yourself

Build the development extension (`pnpm build:dev` in `apps/extension`), load it in a Chromium browser, log into perplexity.ai and open one thread. Whether the popup shows the route firing is exactly what has not been recorded for this platform, so treat the result as a first observation: `chat-stasher read` on the staged session shows what the reader made of it, and a count of unrendered lines there is the expected outcome per the reader note above. A stable (released) build will show nothing for this platform by design.

## Changelog

2026-08-17 — the conversation-list route and its request shape were recorded from source (R26 research).
2026-09-14 — the capture row was re-pointed at the conversation-content route (`GET /rest/thread/<id>`), read from four reference implementations and the site's endpoint table (W28); still `from-source`.
2026-09-23 — a logged-in probe observed the thread body's completeness signal (`has_next_page`, `next_cursor`), and the backfill plan's body segment was filled in on it (W84).
2026-09-24 — live acceptance of the stable channel did not include this platform: it was inert in that build.
