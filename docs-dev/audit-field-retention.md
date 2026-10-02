# Audit-field retention

This list records the raw fields available after the supported harness sources
are collected, sealed into stage shards, pushed, and read back. The encrypted
raw body is the exact recovery source; the activity/search index is not a
message-audit source. A regression test uses synthetic data to check that the
fields below remain present and unchanged.

The shared retention target is source/session identity and stable within-session
message or event key, harness, timestamp, model or provider identity, source
generation/body digest and record order, every usage field including unknown
future subfields, error indicator and structured error class/status, and cwd.
The raw body carries the source fields and order exactly; the archive's
append-only snapshots preserve source generations, and SHA-256 is computed for
the body on readback. Cwd is retained as its raw value; the destination-scoped
hash described in the approved recommendation belongs to a future audit
sidecar. The raw body preserves full error details.

## Claude Code

| Field | Retained value |
|---|---|
| Shared fields | Per-record timestamp; source/session identity and stable message/event key; harness; model/provider; source generation/body digest and record order; all usage fields, including unknown subfields; error indicator and structured status/class; cwd (raw value in body) |
| Model and message identity | `message.model`; message ID class and raw ID for exact recovery / keyed join in a future sidecar |
| Usage | Every `message.usage` subfield, including unknown future subfields |
| Error | `isApiErrorMessage` and structured error facts; complete error detail remains in raw body |
| Record classification | `type`, `userType`, `isSidechain`, `entrypoint`; tool-result records and their raw probe/error text |
| Session context | `timestamp`, `cwd`, `sessionId`, parent session reference |
| Worker provenance | Whether the source path was under `subagents/` is **not yet retained**. The regression test has an ignored expected-failure assertion pending Step 2 provenance work. |

## Codex

| Field | Retained value |
|---|---|
| Shared fields | Per-event timestamp; source/session identity and stable event key; harness; model/provider; source generation/body digest and event order; all usage fields including unknown subfields; error indicator and structured status/class; cwd when present |
| Model | `turn_context.model` |
| Usage | Every `token_count.info.last_token_usage` subfield, including unknown future subfields |
| Rate limits | Every `rate_limits` object, including `limit_id`, both windows, `window_minutes`, `used_percent`, and `resets_at` |
| Event order | Original event order and timestamp |

## OpenCode

| Field | Retained value |
|---|---|
| Shared fields | Per-message timestamp; source/session identity and stable message ID; harness; model/provider; source generation/body digest and record order; all usage fields including unknown subfields; error indicator and structured status/class; `path.cwd` |
| Provider and model | `message.data.providerID`, `message.data.modelID` |
| Usage | Every `message.data.tokens` subfield, including nested cache read/write values |
| Error | `message.data.error`, including non-null structured error values |
| Path and identity | `message.data.path.cwd`, message ID, and message timestamp |

## Grok

| Field | Retention status |
|---|---|
| `usage.json` | **Not yet collected.** `session.modelUsage`, keyed by model, including `cachedReadTokens` and the remaining token counters, plus session/update time, requires a usage source adapter. The current SQLite collector does not provide these records. |

## Hermes

| Field | Retention status |
|---|---|
| `state.db` | **Not yet collected.** `sessions.billing_provider`, model, session time/identity, and all input/output/cache token columns require a new source adapter. |

## Test scope

The synthetic regression test covers Claude Code, Codex, and OpenCode through
collect (which seals each collected body), push to a temporary local repository,
and archive readback. Claude and Codex bodies must match the collected source
bytes and SHA-256 exactly; OpenCode's SQLite rows are represented as an archive
session envelope and each listed message field is compared semantically. It
also includes an API error, unknown usage fields, a Claude parent and worker,
two Codex rate-limit windows, and an OpenCode row with tokens, error, and
`path.cwd`. The `subagents/` source-path provenance assertion is marked as a
TODO / expected failure until Step 2 adds provenance.
