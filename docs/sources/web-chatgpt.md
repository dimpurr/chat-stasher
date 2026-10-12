<!-- BEGIN GENERATED -->
# chatgpt.com (web)

| Field | Value |
|---|---|
| Id | `chatgpt` |
| Surface | web |
| Origins | https://chatgpt.com, https://chat.openai.com |
| Channel | stable |
| Capture credibility | from-source |
| Status | verified end-to-end (2026-09-24) |
| Last verified end to end | 2026-09-24 (extension 0.1.0.18): live acceptance: real conversations archived end to end on a real machine (main e71eb33) |

OS availability comes from the browser registration table, not from this row: see the browser grid in [the support tables](../support.md).
<!-- END GENERATED -->

## What it is

ChatGPT is OpenAI's web conversational interface, accessible at `https://chatgpt.com` and `https://chat.openai.com`. This page documents what the browser extension archives from it. The extension runs inside your logged-in browser session and observes the data traffic exchanged when you interact with the site; it never asks for or stores account credentials, and it does not automate actions on the page. Capture runs in the stable release channel, enabled by default in production builds.

## What we collect

The extension captures the conversation content route: HTTP GET requests to `/backend-api/conversation/<uuid>` that return a 2xx response. The response must satisfy the JSON shape gate by containing both the `mapping` dictionary (the conversation message tree) and the `current_node` pointer (the leaf of the active thread). The conversation UUID is parsed directly from the request path. WebSocket frames are explicitly ignored (`webSocketCapture: false`) because complete message state is delivered over HTTP GET. When you open a conversation, the page hook requests the complete message graph. The delivered capture bundle preserves the full node tree along with conversation list metadata (title, create and update timestamps).

## Coverage and completeness

The live capture leg observes conversations whenever they are loaded in an active browser tab while the extension is enabled. What it observes, it captures completely: the full branching message tree is archived. Conversations that were never visited while the extension was running are not captured by the live leg.

A historical backfill leg is implemented to systematically retrieve conversation history across your account: it enumerates main conversations, archived conversations, and per-project conversations using separate resumable cursors, requesting the conversation text for each. However, a completed historical backfill has not yet been observed in a live browser. Historical conversations can also be imported using official data exports once supported (see [the ChatGPT export page](import-chatgpt.md)). The live capture leg was verified end to end on 2026-09-24 using extension build 0.1.0.18.

## Quirks

ChatGPT stores conversations as an acyclic node graph (`mapping`) rather than a simple sequential list of turns. The normalizer derives the active conversation thread by walking from the root node to `current_node`.

Workspaces and accounts are isolated through request headers: outgoing API calls carry an account identifier in `request-header-chatgpt-account-id`, which the extension hashes using an irreversible HMAC fingerprint (`chatgpt:fp1:<hmac>`) to partition backfill ledgers without ever recording raw account strings. If the active workspace cannot be established unambiguously, backfill operations pause with a named refusal rather than risking cross-workspace data leakage.

If a response fails the shape check (for example, if `mapping` or `current_node` is missing), the capture is rejected and logged as a shape mismatch rather than recorded as an empty conversation.

## Known issues

A workspace switch in the browser is not yet dynamically pinned to the active run scope: switching workspaces during an active browser session can cause new workspace list IDs to be scheduled under the previous workspace's run scope (tracked in issue #4).

## How to check it yourself

Load the extension in a Chromium browser, log into `https://chatgpt.com`, and open a conversation. The extension popup indicator confirms that the content route fired. Run `chat-stasher read` on the staged session to inspect the rendered conversation turns, speakers (User and Assistant), and timestamps. Use `chat-stasher verify` to confirm checksum integrity between staged files and destination storage, and run `chat-stasher search --text "<query>"` after `chat-stasher index build` to verify full-text searchability.

## Changelog

2026-08-17 — conversation detail route (`/backend-api/conversation/`) and tree response shape (`mapping`, `current_node`) specified from source.
2026-09-24 — live acceptance: real conversations captured and archived end to end on a real machine by extension build 0.1.0.18 (commit e71eb33).
2026-10-02 — workspace-scoped backfill ledgers introduced with fingerprinted HMAC account leases and migration support.
2026-10-12 — delivered capture bundles now include conversation list metadata alongside the node mapping tree.
