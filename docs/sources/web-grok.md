<!-- BEGIN GENERATED -->
# grok.com (web)

| Field | Value |
|---|---|
| Id | `grok` |
| Surface | web |
| Origins | https://grok.com |
| Channel | stable |
| Capture credibility | from-source |
| Status | verified end-to-end (2026-09-24) |
| Last verified end to end | 2026-09-24 (extension 0.1.0.18): live acceptance: real conversations archived end to end on a real machine (main e71eb33) |

OS availability comes from the browser registration table, not from this row: see the browser grid in [the support tables](../support.md).
<!-- END GENERATED -->

## What it is

grok.com is the Grok web chat, and this page is about what the browser extension archives from it. The extension works inside your logged-in browser and reads what the page itself fetches; it never drives the site and never holds an account credential of its own. Capture runs in the stable channel, so a release build of the extension has it on.

## What we collect

Exactly one route is captured: the content request the page makes to load one conversation's messages — a POST of the response ids to the conversations endpoint's load-responses path — and the capture is accepted only when the response is a 2xx carrying the `responses` array the platform's own ecosystem agrees on, with each entry's message, sender, creation time and parent response id. The conversation id comes from that request's own path first, so a body is filed under the id the backfill list would give the same conversation; the page URL is the fallback for a capture that arrives while the address bar has not caught up. Three things are deliberately not captured: the conversation-list request (an index of titles, not conversation data), the message-tree skeleton request (ids without content), and the streaming transport a reply plays back over while it is being written — the last is a separate transport the row does not claim to read, and a conversation's completeness never depends on it. Since one archive change the delivered bundle also carries the conversation's list metadata, so what the platform's own index says about a conversation (its title, timestamps) travels with the capture.

## Coverage and completeness

The live leg captures a conversation only when somebody opens it on grok.com while the extension is active. What it captures, it captures whole: in the one published measurement, every conversation the archive held from this platform was complete on the content axes against the platform's own official export, and the missing ones were exactly the conversations never opened during the capture window — every archived conversation postdated the window's start, and every missing one predated it. That measurement, [the capture-gap note](../../docs-dev/grok-capture-gap.md), put the archive's recall against the official export at 61 of 312 conversations (19.6%) on that account, a red number that is the live leg's signature rather than a defect in it: the live leg cannot see a conversation nobody opened.

The designed path for the un-opened history is the backfill leg: enable backfill in the popup and it enumerates the conversation list and fetches each conversation through your open, logged-in tab, gently and capped by day. A completed backfill has not yet been observed in a real browser — the threat model records it as implemented but unobserved — so treat the historical gap as closed by importing the official export (the Grok takeout parser ships in the same build; see [the official-export page](import-grok.md)), by a backfill run you confirm completed, or by opening the conversations you care about while capture is on.

## Quirks

The capture row is source-backed: the route, the response shape and the id grammar were read out of public reference implementations and corroborated by a closed-source store build, and were only then live-verified (2026-09-24, extension 0.1.0.18). A body whose shape drifts is refused and warned about, never guessed into an empty-looking capture: a response without the `responses` array is the platform changing, not a conversation with no messages, and the page hook reports the mismatch instead of filing it.

## Known issues

Whether a very long conversation arrives complete from the body endpoint is unverified: the endpoint is not known to page, and no source documents a truncation guarantee, so the row records that gap openly ([the threat model](../../docs-dev/threat-model.md) keeps it). An account switch is also not yet guarded on this platform: after switching accounts, the new account's list ids can land in the old run's scope (see issue #4).

## How to check it yourself

Load the extension, open grok.com, and open one conversation you want archived: the popup's capture indicator confirms the route fired, and `chat-stasher read` then renders the conversation — speakers, times and tool-adjacent fields as captured — while `chat-stasher search --text` finds text from it after `chat-stasher index build`. Every request the page makes that is not the content route is left where the page made it; nothing is replayed against the site. To see the completeness picture for yourself, contrast a conversation you just opened against one you know is old: the old one will be absent until backfill or an official-export import covers it, and the pages above explain the difference.

## Changelog

2026-09-14 — the capture row was written from source: the route, response shape and id grammar were read out of public reference implementations and corroborated against a store build.
2026-09-24 — live acceptance: real conversations archived end to end on a real machine by extension build 0.1.0.18.
2026-10-07 — a recall oracle measured the live leg's historical gap against the official export; recorded in the capture-gap note.
2026-10-12 — the delivered capture bundle now carries the conversation's list metadata alongside the responses.
