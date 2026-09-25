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

**Unknown fields.** The host ignores request fields this document does not
define. Rejecting them would be `bad-request`, an item-scope refusal, and would
reject a real conversation for a reason no document states. Responses are the
other way round: they carry exactly the fields listed here, and the extension
treats any extra or missing field as a malformed response.

The one exception is the two **parameterless** messages added in §6.4 and §6.5:
they take no request fields at all, so an extra field is a request this document
does not define and is refused with `bad-request`. That rule costs nothing —
neither message carries an item — and it means a future version that wants to
give one of them a parameter has to be a protocol change rather than a field an
older host silently ignores.

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

### 6.4 `summary` — how much is in the stage, in counts only

Request:

```json
{"protocol": 1, "type": "summary"}
```

This message takes **no parameters** (§6): any field other than `protocol` and
`type` is `nack` `bad-request`.

Response on success:

```json
{"protocol": 1, "type": "summary", "ok": true,
 "window_hours": 24,
 "complete": true,
 "sessions": {
   "total": {"kind": "known", "count": 41},
   "last_24h": {"kind": "known", "count": 12},
   "by_harness": [
     {"harness": "claude-code", "total": {"kind": "known", "count": 9},
      "last_24h": {"kind": "known", "count": 3}}
   ]
 },
 "last_push": {"kind": "known", "unix": 1757800000}}
```

**What it reads.** Directory entries under `<stage>/sessions/`, the names of the
sealed shards in each session directory, each shard's own mtime, and
`run-state.json` in the CLI's state directory. It never opens a shard, never
decrypts the repository, never touches the network, and reads no configuration
beyond the stage and the machine id `hello` already resolves. The response
carries counts, one word per harness, the window, and the run-state timestamp —
no conversation content, no titles, no session ids, no paths beyond what
`hello` already returns.

**What a number means.**

- A *session* is a directory under `<stage>/sessions/<machine>/` holding at
  least one sealed shard. A directory with no shard is not a session (and must
  not make an empty stage look populated).
- `total` counts sessions, `last_24h` counts those whose **newest sealed shard
  was written** within the last `window_hours`. That is file mtime, not the
  conversation's own time, because reading the conversation's time would mean
  opening the shard.
- `by_harness` splits both counts by the leading dot-segment of the session
  directory name (`deepseek.abc` → `deepseek`), which is the same rule
  `sidecar::infer_harness` applies to archived ids. `harness` is `null` for a
  directory name with no usable prefix; that bucket is still counted, so the
  per-harness totals always add up to `total`. `by_harness` is empty — and only
  empty — when `total` is `unknown`.

**Unknown is never zero.** Every count is one of `{"kind": "known", "count": n}`
or `{"kind": "unknown", "why": "..."}`, and `last_push` is either
`{"kind": "known", "unix": n}` or `{"kind": "unknown", "why": "..."}`. A part
that could not be read is `unknown` with a reason; a measured zero is
`{"kind": "known", "count": 0}`. `complete` is `true` exactly when every count
and `last_push` is `known` — it is the same statement as "no `unknown` appears
anywhere in this response", and the two are asserted equal by the test
`complete_is_true_exactly_when_nothing_is_unknown` in
`crates/chat-stasher/src/nativehost.rs` rather than left to a reader.

**Cost.** `summary` lists directories and reads file metadata only; it never
opens a shard, decrypts anything or touches the network. Its cost grows with
the total number of sessions and shards in the stage, not with the 24-hour
window, and it has no timeout of its own: on a very large or slow (for example
network-mounted) stage the popup's own 3-second limit can expire first, and the
popup then says the host did not answer.

`last_push` is the `finished_at_unix` of the most recent `run-once` pass whose
recorded outcome was `completed` (a snapshot was created). When the record is
missing, unreadable, describes a failed pass, or describes a pass that had
nothing to archive, the answer is `unknown` with that reason — the file holds
only the most recent pass, and none of those cases establish when the last
successful push was.

### 6.5 `open_dashboard` — start the dashboard and hand back its URL

Request:

```json
{"protocol": 1, "type": "open_dashboard"}
```

This message takes **no parameters** (§6); in particular the extension cannot
name a destination, a repository or a key.

Response on success:

```json
{"protocol": 1, "type": "open_dashboard", "ok": true,
 "url": "http://127.0.0.1:51234/?token=<64 lowercase hex chars>"}
```

**What it does.** The host starts *its own binary* — `std::env::current_exe()`,
so the same build and the same config file — as
`chat-stasher ui --no-open --destination <name>`, where `<name>` is
`[native_host] destination` from the config. It then reads the child's stdout
until the dashboard prints its URL. That line is emitted only after the socket
is bound and listening, so its arrival is the listen event; no second probe is
attempted. The child's stdin is closed and its stdout is a pipe to the host, so
the child can never write on the host's own stdout (§2).

**The token goes to the extension and nowhere else.** The host does not log the
URL, does not write it to disk, and does not print it: the only copy of it
outside the child's own memory is the `url` field of this response.

**Destination.** There is no default destination (ADR-013) and this message
does not invent one: `[native_host] destination` must name a declared
destination. Missing or empty is `nack` `config`, and `detail` names the key
and the file to put it in.

**A second dashboard.** The host cannot tell whether one is already running, and
does not try. The browser starts one host process per request, so there is no
session in which a previous launch could be remembered; and the token is
per-launch and never persisted, so the URL of an earlier dashboard does not
exist anywhere the host could look. Searching the process table was rejected as
unreliable across the platforms this ships on (and a process table names the
*command*, not the token). Every `open_dashboard` therefore starts a new
dashboard; each one exits on its own after its idle timeout (see `chat-stasher ui
--help`).
This is a statement about a limit, not a promise that a second one would be
harmless — an old tab keeps working until it idles out.

**Failure.** A dashboard that does not come up is a `nack`; the extension opens
nothing.

| Situation | `nack` | `retryable` | `detail` says |
|---|---|---|---|
| `[native_host] destination` missing or empty | `config` | false | which key to set, and where |
| the child exited before it printed a URL | `io` | true | the exit status, mapped to the meaning `chat-stasher ui` documents for it (usage error / could not read the archive — a destination that holds nothing is still served as its honest empty page, so there is no "nothing to show" exit any more) |
| no URL within the host's start timeout | `io` | true | that the child was killed, and that no browser tab was opened |
| the URL line could not be read at all | `io` | true | that the child's output could not be read |

The child's own stderr is deliberately **not** relayed: it can carry a
repository URL, which is a real hostname, and this response goes to an extension
that has no business holding one. The exit status is enough to say which of the
CLI's documented outcomes happened; the detail points at running `chat-stasher
ui` by hand for the full text.

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
- The popup asks `summary` **once, when it opens**. It does not poll, and it has
  no timer of its own.
- The popup opens a browser tab **only** with the `url` of a successful
  `open_dashboard` response, and only after checking that it is
  `http://127.0.0.1:<port>/?token=<hex>`. A `nack`, a timeout, a malformed
  response or a URL that is not loopback opens nothing.
