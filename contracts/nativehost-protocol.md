# Native messaging protocol — version 1

This is the contract between the browser extension (`apps/extension`) and the
`chat-stasher` CLI acting as a native messaging host. Both sides implement
exactly what is written here. If an implementation needs something this
document does not say, change this document first.

Machine-checkable message shapes: [`nativehost-message.schema.json`](nativehost-message.schema.json).
Bundle payload shape: [`inbox.schema.json`](inbox.schema.json).

## 1. The one rule

**A conversation counts as delivered only when the extension holds an `ack`
whose `request_id` and `sha256` equal the ones it sent.**

Everything else is *not delivered*: a `nack`, a timeout, a disconnect or
`runtime.lastError`, a response that does not parse or does not match the
schema. There is no "probably delivered". Silence is never success.

The host sends `ack` only after the shard holding the payload is durable on
disk — the same fsync proof `ingest` already requires — or after it has found
the identical bytes already sealed (`status: "duplicate"`).

## 2. Transport

- The extension uses `runtime.sendNativeMessage` — one request per call. It
  does not use `connectNative`.
- The browser starts one host process per request. The host reads exactly one
  request frame from stdin, writes exactly one response frame to stdout, and
  exits with status 0. On a frame it cannot read at all (EOF inside the header
  or body) it writes nothing and exits non-zero.
- Framing is the browser's: a 32-bit unsigned length in native byte order,
  followed by that many bytes of UTF-8 JSON.
- **Request cap: 64 MiB.** The host checks the length prefix before
  allocating. A larger prefix gets `nack` `too-large` without reading the body.
  (Chrome's own limit toward the host is 64 MiB; Firefox allows 4 GB, so the
  host must enforce the cap itself.)
- **Response cap: 1 MiB** (the browser's limit from the host). `detail` strings
  are truncated to 4 KiB.
- **stdout carries the response frame and nothing else.** Every diagnostic goes
  to stderr. One stray byte on stdout corrupts the frame.
- The extension's timeout per request is **60 seconds**.

## 3. How the host is started

The host is the ordinary `chat-stasher` binary. The browser cannot pass custom
arguments, so the binary recognises a browser launch *before* normal argument
parsing:

| Browser | argv after the program name | Host mode when |
|---|---|---|
| Chrome / Chromium (macOS, Linux) | `chrome-extension://<id>/` | `<id>` equals the pinned Chrome extension id |
| Chrome / Chromium (Windows) | `chrome-extension://<id>/ --parent-window=<n>` | same; the extra argument is ignored |
| Firefox | `<path to host manifest>.json <add-on id>` | the add-on id equals the pinned Firefox id |

A `chrome-extension://` origin with any other id is refused: nothing on
stdout, a line on stderr, non-zero exit. Anything that is not a browser launch
is parsed as a normal command line.

`chat-stasher native-host` runs the same one-request loop on stdin/stdout for
manual testing. `chat-stasher native-host --self-test` is unchanged.

## 4. Where the host writes

- The stage comes from the config key `[native_host] stage = "<absolute path>"`,
  written by `chat-stasher install-native-host --stage <path>`.
- The host **never creates a stage.** A missing key is `nack` `config`; a path
  that does not exist or is not a directory is `nack` `stage-unavailable`. A
  freshly created stage is one that nothing pushes — that is worse than an
  error.
- The machine id is resolved exactly as `ingest` resolves it.
- Payloads go through the same code path as `ingest` (parse → duplicate check
  → seal). One code path, one set of guarantees.

## 5. Concurrency

The host and `ingest` both hold an exclusive lock on `<stage>/.ingest.lock`
while they allocate a shard sequence number and seal the shard. Two browsers,
two profiles or a host racing a manual `ingest` therefore cannot pick the same
sequence number. The wait is bounded (10 seconds); on timeout the answer is
`nack` `stage-unavailable` with `retryable: true`.

## 6. Messages

Every message carries `"protocol": 1` and a `"type"`.

### 6.1 `hello` — is the host there, and where does it write?

Request:

```json
{"protocol": 1, "type": "hello"}
```

Response (or a `nack`):

```json
{"protocol": 1, "type": "hello", "ok": true,
 "host_version": "0.3.0", "machine": "<machine id>", "stage": "<absolute path>"}
```

`hello` checks the config and the stage exactly as `deliver` would, so a
`hello` that returns `ok` means a `deliver` can succeed.

### 6.2 `deliver` — archive one bundle

Request:

```json
{"protocol": 1, "type": "deliver",
 "request_id": "<1–128 chars of [A-Za-z0-9_-]>",
 "name": "<platform>-<path-safe session id>.json",
 "payload": "<the bundle, serialised with JSON.stringify>",
 "sha256": "<64 lowercase hex chars: SHA-256 of the UTF-8 bytes of payload>"}
```

- `payload` is a **string**, byte-for-byte what a bundle file would contain.
  That is what makes the content hash identical across every channel
  (native messaging, the export file, a bundle file in an inbox).
- `name` must match `^[a-z0-9]+-[^/\\]+\.json$`. It is recorded as the shard's
  `source_file` and is the fallback id source, as a file name is for `ingest`.
- The host recomputes SHA-256 over the UTF-8 bytes of `payload`. A mismatch is
  `nack` `integrity` and nothing is written.

Response on success:

```json
{"protocol": 1, "type": "ack", "request_id": "<same>",
 "status": "stored", "sha256": "<same>", "shard": "<shard file name>"}
```

`status` is `"stored"` (a new shard was sealed) or `"duplicate"` (identical
bytes were already sealed; `shard` names the existing one).

### 6.3 `nack` — not delivered

```json
{"protocol": 1, "type": "nack", "request_id": "<same, or null if unreadable>",
 "kind": "<see below>", "retryable": false, "detail": "<human-readable, ≤ 4 KiB>"}
```

| `kind` | Scope | `retryable` | Meaning |
|---|---|---|---|
| `protocol-version` | host | false | `protocol` missing or not supported. The response adds `"supported": [1]`. |
| `bad-request` | item | false | Not JSON, unknown `type`, missing or malformed field (`request_id`, `name`, `sha256`). |
| `too-large` | item | false | Length prefix above 64 MiB. |
| `integrity` | item | true | `sha256` does not match `payload`. |
| `invalid-bundle` | item | false | `payload` is not a valid inbox bundle. |
| `config` | host | false | No `[native_host] stage`, config unreadable, or machine id unresolvable. `detail` names the fix command. |
| `stage-unavailable` | host | true | Stage missing, not a directory, not writable, or lock wait timed out. |
| `io` | host | true | Sealing failed. Nothing was acknowledged; the retry is safe. |

**Scope decides what happens to the item; `retryable` only says whether the
same request can succeed without a human.** A *host*-scope `nack` says nothing
about the item: the item stays pending and delivery pauses until a `hello`
succeeds — even when `retryable` is false (a missing config needs a person,
but once it is fixed, every waiting item must go through). Only an *item*-scope
`nack` with `retryable: false` marks that one item rejected. The scope is fixed
by `kind` and is not sent on the wire.

## 7. Idempotency

The duplicate key is the SHA-256 of the payload bytes — the same `fileSha256`
that `ingest` records. A lost `ack` is harmless: the extension retries, the
host answers `duplicate`, and exactly one shard exists.

## 8. Export file (manual escape hatch)

When the host cannot be reached for a long time, the extension can export
everything it has not delivered as one file:

- Name: `chat-stasher-export-<UTC yyyymmddThhmmssZ>.jsonl`
- One line per bundle. Each line is the exact `payload` string (a
  `JSON.stringify` result contains no raw newline), followed by `\n`.
- `chat-stasher ingest --inbox <dir>` accepts `*.jsonl` files next to
  `*.json` bundle files. Each line is ingested as one bundle whose `fileSha256`
  is the SHA-256 of the line without its trailing newline — the same key as
  `deliver`. A synthetic `source_file` of `<export file name>#<line number>`
  is recorded.
- The export file is retired to `consumed/` only when every line was sealed or
  found to be a duplicate. Otherwise it stays; a re-run is safe because every
  line is content-addressed.

## 9. Versioning

`protocol` is an integer. A host lists the versions it supports; this host
supports `[1]`. The extension sends the highest version it implements. A new
version is a new document section, never an edit to an existing one.

## 10. Extension-side obligations (normative summary)

- **Live captures are written to a persistent outbox before any delivery
  attempt** (write-ahead), and removed only on a matching `ack`.
- **Backfill does not queue payloads.** The conversation still exists on the
  platform, so an undelivered backfill item keeps its debt open and backfill
  pauses with a visible reason until a `hello` succeeds again.
- The outbox never drops an item on its own. When it is full the extension
  refuses new live captures visibly instead.
- Only an item-scope, non-retryable `nack` (§6.3) moves the item to a visible
  "rejected" state; it is kept and is included in an export. A host-scope
  `nack`, a timeout or a send failure keeps the item pending.
- Retries must not depend on the backfill switch: while the outbox holds a
  pending item, the extension keeps its own low-frequency retry timer, and
  clears it once the outbox is empty.
- There is no automatic file download anywhere.
