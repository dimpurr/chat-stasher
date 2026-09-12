/**
 * Native Messaging transport — question/answer, one request per call.
 *
 * Normative text: `contracts/nativehost-protocol.md` §1, §2, §6 and the
 * machine-checkable shapes in `contracts/nativehost-message.schema.json`.
 *
 * 🔴 THE ONE RULE (§1): a conversation counts as delivered **only** when this
 *    module holds an `ack` whose `request_id` and `sha256` equal the ones it
 *    sent. A `nack`, a timeout, a disconnect / `runtime.lastError`, a response
 *    that does not parse, a response with a missing field, an extra field or a
 *    wrong-typed field — all of those are *not delivered*. There is no
 *    "probably delivered" branch in this file, and none may be added.
 *
 * Transport (§2): `runtime.sendNativeMessage`, NOT `connectNative`. The browser
 * starts one host process per request. The old `connectNative` port had a
 * fatal shape: it resolved `ok: true` when the port stayed open for N ms
 * without an error, which turned every silent host into a false success.
 *
 * The response validator below is written by hand against the schema on
 * purpose: pulling in a JSON-schema library would add a runtime dependency to
 * the one code path that decides whether data is safe to forget. The schema's
 * `additionalProperties: false` is enforced too — an `ack` with an unexpected
 * field is not the `ack` this contract describes.
 */

/**
 * Pinned host name registered by `chat-stasher install-native-host`.
 * Outsource: `crates/chat-stasher/src/nativehost.rs` (HOST_NAME = "com.chat_stasher.host").
 */
export const NATIVE_HOST_NAME = 'com.chat_stasher.host';

/** The only protocol version this extension implements. §9. */
export const PROTOCOL = 1;

/** §2: "The extension's timeout per request is 60 seconds." */
export const REQUEST_TIMEOUT_MS = 60_000;

/**
 * Timeout used **only** by the popup's "am I connected?" question.
 *
 * `hello` reads config and stats the stage directory, so a healthy host answers
 * in milliseconds. The popup is a UI surface: it must not hang for a minute
 * behind a wedged host. The delivery path (§2) always uses REQUEST_TIMEOUT_MS.
 * A probe that runs out of time is reported as `timeout` — the literal fact —
 * and never as "not connected" or "connected".
 */
export const HELLO_PROBE_TIMEOUT_MS = 3_000;

/** §6.3 — the full `kind` closed set for `nack`. */
export const NACK_KINDS = [
  'protocol-version',
  'bad-request',
  'too-large',
  'integrity',
  'invalid-bundle',
  'config',
  'stage-unavailable',
  'io',
] as const;
export type NackKind = (typeof NACK_KINDS)[number];

/** Every way a request can end without a matching `ack`. Named, never silent. */
export type UndeliveredReason =
  /** No `runtime.sendNativeMessage` on this build — nothing was even attempted. */
  | 'no-runtime-api'
  /** The call threw, or came back with `runtime.lastError`. */
  | 'send-failed'
  /** No response inside the 60 s window (§2). The host may still have sealed it. */
  | 'timeout'
  /** The host answered `nack` (§6.3). `kind` carries its reason. */
  | 'nack'
  /** A response arrived but is not a valid message per the schema. */
  | 'malformed-response'
  /** This environment has no usable WebCrypto, so no request can be built. */
  | 'crypto-unavailable';

export interface HelloOk {
  ok: true;
  machine: string;
  stage: string;
  hostVersion: string;
}

export interface HelloFailed {
  ok: false;
  reason: UndeliveredReason;
  /** Present when the host answered `nack`. */
  kind?: NackKind;
  detail?: string;
  retryable: boolean;
}

export type HelloResult = HelloOk | HelloFailed;

export interface Delivered {
  delivered: true;
  /** §6.2: `stored` = a new shard; `duplicate` = identical bytes already sealed. */
  status: 'stored' | 'duplicate';
  /** Shard file name the host answered with. */
  shard: string;
  requestId: string;
  sha256: string;
}

export interface NotDelivered {
  delivered: false;
  reason: UndeliveredReason;
  kind?: NackKind;
  detail?: string;
  /**
   * May this exact request be sent again later? Comes from the host's `nack`
   * when there was one; otherwise from this module's own reading of the
   * situation. 🔴 It never upgrades a failure into a success.
   */
  retryable: boolean;
  requestId: string;
  sha256: string;
}

export type DeliverResult = Delivered | NotDelivered;

// ---------------------------------------------------------------------------
// WebCrypto (hash + request id)
// ---------------------------------------------------------------------------

function webcrypto(): Crypto | null {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  if (!c || typeof c.subtle?.digest !== 'function' || typeof c.randomUUID !== 'function') return null;
  return c;
}

/**
 * SHA-256 of the UTF-8 bytes of `payload`, lowercase hex (§6.2).
 * Returns null when WebCrypto is unusable — the caller must then refuse rather
 * than send a request it cannot prove anything about.
 */
export async function sha256Hex(payload: string): Promise<string | null> {
  const c = webcrypto();
  if (!c) return null;
  const digest = await c.subtle.digest('SHA-256', new TextEncoder().encode(payload));
  const bytes = new Uint8Array(digest);
  let out = '';
  for (const b of bytes) out += b.toString(16).padStart(2, '0');
  return out;
}

/** §6.2 request_id: 1–128 chars of [A-Za-z0-9_-]. A UUID fits. */
export function newRequestId(): string | null {
  const c = webcrypto();
  return c ? c.randomUUID() : null;
}

// ---------------------------------------------------------------------------
// Hand-written validation against nativehost-message.schema.json
// ---------------------------------------------------------------------------

const SHA256_RE = /^[0-9a-f]{64}$/;
const REQUEST_ID_RE = /^[A-Za-z0-9_-]{1,128}$/;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * Exact key check: every required key present, and — because the schema says
 * `additionalProperties: false` — no key outside `required ∪ optional`.
 */
function keysOk(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[] = [],
): string | null {
  for (const key of required) {
    if (!(key in value)) return `missing field '${key}'`;
  }
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) return `unexpected field '${key}'`;
  }
  return null;
}

interface Ack {
  request_id: string;
  status: 'stored' | 'duplicate';
  sha256: string;
  shard: string;
}

function validateAck(value: Record<string, unknown>): Ack | string {
  if (value.protocol !== PROTOCOL) return `protocol is not ${PROTOCOL}`;
  if (value.type !== 'ack') return "type is not 'ack'";
  const bad = keysOk(value, ['protocol', 'type', 'request_id', 'status', 'sha256', 'shard']);
  if (bad) return bad;
  if (typeof value.request_id !== 'string' || !REQUEST_ID_RE.test(value.request_id)) {
    return 'request_id is not 1-128 chars of [A-Za-z0-9_-]';
  }
  if (value.status !== 'stored' && value.status !== 'duplicate') {
    return "status is neither 'stored' nor 'duplicate'";
  }
  if (typeof value.sha256 !== 'string' || !SHA256_RE.test(value.sha256)) {
    return 'sha256 is not 64 lowercase hex chars';
  }
  if (typeof value.shard !== 'string') return 'shard is not a string';
  return {
    request_id: value.request_id,
    status: value.status,
    sha256: value.sha256,
    shard: value.shard,
  };
}

interface Nack {
  request_id: string | null;
  kind: NackKind;
  retryable: boolean;
  detail: string;
}

function validateNack(value: Record<string, unknown>): Nack | string {
  if (value.protocol !== PROTOCOL) return `protocol is not ${PROTOCOL}`;
  if (value.type !== 'nack') return "type is not 'nack'";
  const bad = keysOk(
    value,
    ['protocol', 'type', 'request_id', 'kind', 'retryable', 'detail'],
    ['supported'],
  );
  if (bad) return bad;
  const id = value.request_id;
  if (id !== null && (typeof id !== 'string' || !REQUEST_ID_RE.test(id))) {
    return 'request_id is neither null nor 1-128 chars of [A-Za-z0-9_-]';
  }
  if (typeof value.kind !== 'string' || !(NACK_KINDS as readonly string[]).includes(value.kind)) {
    return `kind '${String(value.kind)}' is not in the nack kind set`;
  }
  if (typeof value.retryable !== 'boolean') return 'retryable is not a boolean';
  if (typeof value.detail !== 'string') return 'detail is not a string';
  if ('supported' in value) {
    const supported = value.supported;
    if (!Array.isArray(supported) || supported.some((v) => !Number.isInteger(v))) {
      return "'supported' is not an array of integers";
    }
  }
  return {
    request_id: id,
    kind: value.kind as NackKind,
    retryable: value.retryable,
    detail: value.detail,
  };
}

interface HelloResponse {
  machine: string;
  stage: string;
  host_version: string;
}

function validateHello(value: Record<string, unknown>): HelloResponse | string {
  if (value.protocol !== PROTOCOL) return `protocol is not ${PROTOCOL}`;
  if (value.type !== 'hello') return "type is not 'hello'";
  const bad = keysOk(value, ['protocol', 'type', 'ok', 'host_version', 'machine', 'stage']);
  if (bad) return bad;
  if (value.ok !== true) return 'ok is not true';
  if (typeof value.host_version !== 'string') return 'host_version is not a string';
  if (typeof value.machine !== 'string') return 'machine is not a string';
  if (typeof value.stage !== 'string') return 'stage is not a string';
  return { machine: value.machine, stage: value.stage, host_version: value.host_version };
}

// ---------------------------------------------------------------------------
// The call itself
// ---------------------------------------------------------------------------

type SendOutcome =
  | { kind: 'response'; response: unknown }
  | { kind: 'error'; detail: string }
  | { kind: 'timeout' }
  | { kind: 'no-api' };

function getRuntime(): { sendNativeMessage?: (...args: unknown[]) => unknown; lastError?: { message?: string } } | null {
  const g = globalThis as { browser?: any; chrome?: any };
  const runtime = g.browser?.runtime?.id ? g.browser.runtime : (g.chrome?.runtime?.id ? g.chrome.runtime : null);
  return runtime ?? null;
}

/**
 * One `sendNativeMessage` round trip.
 *
 * Both calling conventions are supported: Chrome's callback form (with
 * `runtime.lastError`) and Firefox's Promise form. Whichever settles first
 * wins; the other is ignored.
 */
function sendOnce(
  runtime: ReturnType<typeof getRuntime>,
  message: unknown,
  timeoutMs: number,
): Promise<SendOutcome> {
  return new Promise<SendOutcome>((resolve) => {
    if (!runtime || typeof runtime.sendNativeMessage !== 'function') {
      resolve({ kind: 'no-api' });
      return;
    }
    let settled = false;
    const settle = (outcome: SendOutcome): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(outcome);
    };
    const timer = setTimeout(() => settle({ kind: 'timeout' }), timeoutMs);

    const onResponse = (response: unknown): void => {
      // 🔴 lastError must be read synchronously inside the callback, before any
      // await: Chrome only guarantees it for the duration of the turn.
      let lastError: string | null = null;
      try {
        lastError = runtime.lastError?.message ?? null;
      } catch { /* Firefox throws when read outside a callback scope */ }
      if (lastError) {
        settle({ kind: 'error', detail: lastError });
        return;
      }
      if (response === undefined) {
        // No lastError and no response: this is the browser telling us nothing.
        settle({ kind: 'error', detail: 'no response and no runtime.lastError' });
        return;
      }
      settle({ kind: 'response', response });
    };

    try {
      const maybePromise = runtime.sendNativeMessage(NATIVE_HOST_NAME, message, onResponse);
      if (maybePromise && typeof (maybePromise as Promise<unknown>).then === 'function') {
        (maybePromise as Promise<unknown>).then(onResponse, (err: unknown) =>
          settle({ kind: 'error', detail: (err as Error)?.message ?? String(err) }));
      }
    } catch (err) {
      settle({ kind: 'error', detail: (err as Error)?.message ?? String(err) });
    }
  });
}

/**
 * Turn a send outcome into either a validated message or a named failure.
 * Used by both `hello` and `deliver`; the validators differ, the classification
 * does not.
 */
function classify(
  outcome: SendOutcome,
  validate: (value: Record<string, unknown>) => unknown | string,
  expectedRequestId?: string,
): { ok: true; value: unknown } | { ok: false; reason: UndeliveredReason; kind?: NackKind; detail?: string; retryable: boolean } {
  if (outcome.kind === 'no-api') {
    return { ok: false, reason: 'no-runtime-api', retryable: false };
  }
  if (outcome.kind === 'timeout') {
    // §7: the host may have sealed it anyway; the retry is safe and answers
    // `duplicate`. So a timeout is retryable — it is still NOT delivered.
    return { ok: false, reason: 'timeout', retryable: true };
  }
  if (outcome.kind === 'error') {
    return { ok: false, reason: 'send-failed', detail: outcome.detail, retryable: true };
  }
  const response = outcome.response;
  if (!isRecord(response)) {
    return { ok: false, reason: 'malformed-response', detail: 'response is not an object', retryable: true };
  }
  // A nack is never a delivery, but its `kind` and `retryable` are the host's
  // own statement about why (§6.3) — carry them through instead of flattening.
  if (response.type === 'nack' && response.protocol === PROTOCOL) {
    const nack = validateNack(response);
    if (typeof nack === 'string') {
      return { ok: false, reason: 'malformed-response', detail: nack, retryable: true };
    }
    // §6.3: request_id is the one we sent, or null when the host could not read
    // it. Anything else is a nack we cannot attribute to this request, so we
    // refuse to act on its `kind` / `retryable` and call the response malformed.
    if (
      expectedRequestId !== undefined
      && nack.request_id !== null
      && nack.request_id !== expectedRequestId
    ) {
      return {
        ok: false,
        reason: 'malformed-response',
        detail: 'nack request_id matches neither the request nor null',
        retryable: true,
      };
    }
    return { ok: false, reason: 'nack', kind: nack.kind, detail: nack.detail, retryable: nack.retryable };
  }
  const validated = validate(response);
  if (typeof validated === 'string') {
    return { ok: false, reason: 'malformed-response', detail: validated, retryable: true };
  }
  return { ok: true, value: validated };
}

/**
 * §6.1 — "is the host there, and where does it write?".
 * A `hello` that returns `ok` means a `deliver` can succeed.
 */
export async function hello(options: { timeoutMs?: number } = {}): Promise<HelloResult> {
  const outcome = await sendOnce(
    getRuntime(),
    { protocol: PROTOCOL, type: 'hello' },
    options.timeoutMs ?? REQUEST_TIMEOUT_MS,
  );
  const classified = classify(outcome, (value) => validateHello(value));
  if (!classified.ok) {
    return {
      ok: false,
      reason: classified.reason,
      kind: classified.kind,
      detail: classified.detail,
      retryable: classified.retryable,
    };
  }
  const res = classified.value as HelloResponse;
  return { ok: true, machine: res.machine, stage: res.stage, hostVersion: res.host_version };
}

/**
 * §6.2 — archive one bundle. `payload` is the bundle serialised with
 * `JSON.stringify`; the sha256 is computed here over its UTF-8 bytes and is the
 * same content key every other channel uses.
 *
 * The only success is a matching `ack`. Everything else — including a response
 * that merely *looks* like an ack — comes back as `{delivered: false}`.
 */
export async function deliver(name: string, payload: string): Promise<DeliverResult> {
  const sha256 = await sha256Hex(payload);
  const requestId = newRequestId();
  if (sha256 === null || requestId === null) {
    // Cannot even build a request that the host could verify. Refuse, loudly.
    return {
      delivered: false,
      reason: 'crypto-unavailable',
      retryable: false,
      detail: 'crypto.subtle.digest / crypto.randomUUID unavailable in this context',
      requestId: '',
      sha256: '',
    };
  }

  const outcome = await sendOnce(
    getRuntime(),
    { protocol: PROTOCOL, type: 'deliver', request_id: requestId, name, payload, sha256 },
    REQUEST_TIMEOUT_MS,
  );
  const classified = classify(outcome, (value) => {
    if (value.type !== 'ack') return "type is neither 'ack' nor 'nack'";
    const ack = validateAck(value);
    if (typeof ack === 'string') return ack;
    // 🔴 The two comparisons that define delivery (§1). They live here and
    // nowhere else; no caller re-derives them.
    if (ack.request_id !== requestId) return 'ack request_id does not match the request';
    if (ack.sha256 !== sha256) return 'ack sha256 does not match the payload';
    return ack;
  }, requestId);

  if (!classified.ok) {
    return {
      delivered: false,
      reason: classified.reason,
      kind: classified.kind,
      detail: classified.detail,
      retryable: classified.retryable,
      requestId,
      sha256,
    };
  }
  const ack = classified.value as Ack;
  return {
    delivered: true,
    status: ack.status,
    shard: ack.shard,
    requestId,
    sha256,
  };
}

/** The `name` field rule from §6.2, exposed so callers can fail early. */
export const NAME_RE = /^[a-z0-9]+-[^/\\]+\.json$/;

export function isValidDeliverName(name: string): boolean {
  return NAME_RE.test(name);
}
