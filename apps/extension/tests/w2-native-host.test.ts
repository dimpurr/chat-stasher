/**
 * W2 · The transport layer: the only success that counts is a matching ack.
 *
 * Spec §1:
 *   「A conversation counts as delivered only when the extension holds an `ack`
 *     whose `request_id` and `sha256` equal the ones it sent.」
 *
 * 🔴 **Every** negative case in this file pins two things at once:
 *      · the transport must report `delivered:false` (not true, and not "probably worked");
 *      · every non-delivery must have a named `reason` (silently swallowing an error is not allowed).
 *
 * Zero real processes and zero real browser throughout: `runtime.sendNativeMessage` is a
 * programmable stub in this file that answers whatever it is told to.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import {
  deliver,
  hello,
  isValidDeliverName,
  NATIVE_HOST_NAME,
  PROTOCOL,
  REQUEST_TIMEOUT_MS,
} from '../lib/native-host';

type Responder = (
  message: Record<string, unknown>,
) => { via: 'callback'; response: unknown; lastError?: string } | { via: 'promise'; response: unknown } | { via: 'throw'; error: string } | { via: 'silent' };

let responder: Responder;
let sent: Array<Record<string, unknown>>;

const fakeBrowser: any = {
  runtime: {
    id: 'w2-native-host-test',
    lastError: undefined as { message?: string } | undefined,
    sendNativeMessage(host: string, message: Record<string, unknown>, callback: (r: unknown) => void) {
      sent.push(message);
      const outcome = responder(message);
      if (outcome.via === 'throw') throw new Error(outcome.error);
      if (outcome.via === 'silent') return undefined;
      if (outcome.via === 'promise') return Promise.resolve(outcome.response);
      // Chrome's shape: lastError is only valid in the callback's own beat.
      fakeBrowser.runtime.lastError = outcome.lastError ? { message: outcome.lastError } : undefined;
      callback(outcome.response);
      fakeBrowser.runtime.lastError = undefined;
      return undefined;
    },
  },
};

beforeEach(() => {
  sent = [];
  responder = () => ({ via: 'callback', response: undefined });
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  fakeBrowser.runtime.lastError = undefined;
});

afterEach(() => {
  vi.useRealTimers();
});

/**
 * Wait until "the request really went out".
 *
 * 🔴 Why it is needed: `deliver` computes the SHA-256 with `crypto.subtle` first, and that is
 *    **really** asynchronous (the libuv thread pool); fake timers cannot reach it. Advancing
 *    time before the digest completes would create the 60-second timeout timer at an instant
 *    already in the past and the test would hang. So: fake timers take over setTimeout only,
 *    and the real step is awaited first, then time is advanced.
 */
async function waitUntilSent(): Promise<void> {
  for (let i = 0; i < 1000 && sent.length === 0; i += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  if (sent.length === 0) throw new Error('the request was never sent');
}

/** A fully conforming §6.2 ack whose fields can be overridden to build the negative cases. */
function ackFor(message: Record<string, unknown>, overrides: Record<string, unknown> = {}): unknown {
  return {
    protocol: 1,
    type: 'ack',
    request_id: message.request_id,
    status: 'stored',
    sha256: message.sha256,
    shard: '0001-chatgpt-abc.json',
    ...overrides,
  };
}

const NAME = 'chatgpt-11111111-2222-3333-4444-555555555555.json';
const PAYLOAD = JSON.stringify({ schema: 'chat-stasher/inbox@2', sessionId: 'x', raw: { text: 'hi' } });

/** Compute the hash with an independent implementation, proving deliver really reports the SHA-256 of the payload bytes. */
async function independentSha256(text: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

// ===========================================================================
// 1 · The request itself
// ===========================================================================
describe('W2 · the request deliver sends', () => {
  it('conforms to §6.2 byte for byte — protocol/type/request_id/name/payload/sha256 — and the sha256 is the SHA-256 of the payload UTF-8 bytes', async () => {
    responder = (message) => ({ via: 'callback', response: ackFor(message) });

    const result = await deliver(NAME, PAYLOAD);
    expect(result.delivered).toBe(true);

    expect(sent).toHaveLength(1);
    const req = sent[0]!;
    expect(Object.keys(req).sort()).toEqual(
      ['name', 'payload', 'protocol', 'request_id', 'sha256', 'type'],
    );
    expect(req.protocol).toBe(PROTOCOL);
    expect(req.type).toBe('deliver');
    expect(req.name).toBe(NAME);
    expect(req.payload).toBe(PAYLOAD);                       // verbatim, not one byte touched
    expect(req.sha256).toBe(await independentSha256(PAYLOAD));
    expect(String(req.request_id)).toMatch(/^[A-Za-z0-9_-]{1,128}$/);
    console.log('[W2-NM] request:', { ...req, payload: `<${PAYLOAD.length} chars>` });
  });

  it('two requests have different request_ids (randomUUID, not a constant)', async () => {
    responder = (message) => ({ via: 'callback', response: ackFor(message) });
    await deliver(NAME, PAYLOAD);
    await deliver(NAME, PAYLOAD);
    expect(sent[0]!.request_id).not.toBe(sent[1]!.request_id);
  });

  it('§6.2 name rule implemented byte for byte (a non-conforming name is stopped by ourselves)', () => {
    expect(isValidDeliverName('chatgpt-abc.json')).toBe(true);
    expect(isValidDeliverName('deepseek-c622b5dd-0000-4000-8000-00000000abcd.json')).toBe(true);
    expect(isValidDeliverName('ChatGPT-abc.json')).toBe(false);   // the platform segment must be lowercase
    expect(isValidDeliverName('chatgpt-a/b.json')).toBe(false);   // no path may appear
    expect(isValidDeliverName('chatgpt-abc.txt')).toBe(false);
    expect(isValidDeliverName('chatgpt-abc')).toBe(false);
  });
});

// ===========================================================================
// 2 · 🔴 only a fully matching ack counts as delivered
// ===========================================================================
describe('W2 · 🔴 regression: without a matching ack it is never delivered', () => {
  it('a fully matching ack ⇒ delivered:true, carrying the host status and shard', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { status: 'duplicate', shard: '0007-x.json' }),
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toEqual({
      delivered: true,
      status: 'duplicate',
      shard: '0007-x.json',
      requestId: expect.any(String),
      sha256: await independentSha256(PAYLOAD),
    });
  });

  it('a timeout (no answer within §2\'s 60 seconds) ⇒ timeout, and **never** delivered', async () => {
    responder = () => ({ via: 'silent' });
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    const pending = deliver(NAME, PAYLOAD);
    await waitUntilSent();
    await vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS + 1);
    const result = await pending;

    expect(result.delivered).toBe(false);
    expect(result).toMatchObject({ reason: 'timeout', retryable: true });
    console.log('[W2-NM] timeout:', result);
  });

  it('runtime.lastError ⇒ send-failed (named, not silent)', async () => {
    responder = () => ({ via: 'callback', response: undefined, lastError: 'Specified native messaging host not found.' });
    const result = await deliver(NAME, PAYLOAD);
    expect(result.delivered).toBe(false);
    expect(result).toMatchObject({
      reason: 'send-failed',
      detail: 'Specified native messaging host not found.',
      retryable: true,
    });
  });

  it('the call throwing outright ⇒ send-failed', async () => {
    responder = () => ({ via: 'throw', error: 'boom' });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toMatchObject({ delivered: false, reason: 'send-failed' });
  });

  it('neither an answer nor lastError ⇒ send-failed (silence is not success)', async () => {
    responder = () => ({ via: 'callback', response: undefined });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toMatchObject({ delivered: false, reason: 'send-failed' });
  });

  it('the ack\'s request_id does not match ⇒ malformed-response, and the entry may not be treated as delivered', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { request_id: 'some-other-request' }),
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result.delivered).toBe(false);
    expect(result).toMatchObject({ reason: 'malformed-response' });
    expect((result as { detail?: string }).detail).toContain('request_id');
  });

  it('the ack\'s sha256 does not match ⇒ malformed-response', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { sha256: 'f'.repeat(64) }),
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result.delivered).toBe(false);
    expect(result).toMatchObject({ reason: 'malformed-response' });
    expect((result as { detail?: string }).detail).toContain('sha256');
  });

  it('the response is missing a field ⇒ malformed-response', async () => {
    responder = (message) => {
      const ack = ackFor(message) as Record<string, unknown>;
      delete ack.shard;
      return { via: 'callback', response: ack };
    };
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'malformed-response',
    });
  });

  it('the response has an extra field ⇒ malformed-response (the schema\'s additionalProperties:false)', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { extra: 'not in the schema' }),
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toMatchObject({ delivered: false, reason: 'malformed-response' });
    expect((result as { detail?: string }).detail).toContain('unexpected field');
  });

  it('a field has the wrong type ⇒ malformed-response', async () => {
    const cases: Array<Record<string, unknown>> = [
      { status: 42 },
      { status: 'ok' },
      { sha256: 12345 },
      { shard: null },
      { request_id: 99 },
    ];
    for (const override of cases) {
      responder = (message) => ({ via: 'callback', response: ackFor(message, override) });
      const result = await deliver(NAME, PAYLOAD);
      expect([override, result.delivered]).toEqual([override, false]);
      expect(result).toMatchObject({ reason: 'malformed-response' });
    }
  });

  it('the response is not an object (string / array / null) ⇒ malformed-response', async () => {
    for (const response of ['an ack, trust me', [1, 2, 3], null, 7]) {
      responder = () => ({ via: 'callback', response });
      const result = await deliver(NAME, PAYLOAD);
      expect([response, result.delivered]).toEqual([response, false]);
      expect(result).toMatchObject({ reason: 'malformed-response' });
    }
  });

  it('type is neither ack nor nack ⇒ malformed-response', async () => {
    responder = (message) => ({
      via: 'callback',
      response: { ...(ackFor(message) as object), type: 'ok' },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'malformed-response',
    });
  });

  it('the protocol version does not match ⇒ malformed-response (this extension speaks protocol 1 only, §9)', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { protocol: 2 }),
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'malformed-response',
    });
  });

  it('the Promise shape (Firefox) is recognised too: a matching ack still ⇒ delivered:true', async () => {
    responder = (message) => ({ via: 'promise', response: ackFor(message) });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({ delivered: true, status: 'stored' });
  });
});

// ===========================================================================
// 3 · nack: never a delivery, but its kind / retryable must come through as-is
// ===========================================================================
describe('W2 · classifying a nack', () => {
  it('retryable nack ⇒ reason:nack + kind + retryable:true', async () => {
    responder = (message) => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: message.request_id,
        kind: 'io', retryable: true, detail: 'sealing failed',
      },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'nack', kind: 'io', retryable: true, detail: 'sealing failed',
    });
  });

  it('a non-retryable nack ⇒ retryable:false (the entry is judged dead but still kept)', async () => {
    responder = (message) => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: message.request_id,
        kind: 'invalid-bundle', retryable: false, detail: 'payload is not an inbox bundle',
      },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'nack', kind: 'invalid-bundle', retryable: false,
    });
  });

  it('a nack with a null request_id still counts (§6.3: unreadable means null)', async () => {
    responder = () => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: null,
        kind: 'protocol-version', retryable: false, detail: 'unsupported', supported: [1],
      },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'nack', kind: 'protocol-version', retryable: false,
    });
  });

  it('🔴 a nack whose request_id belongs to **someone else** ⇒ malformed-response (it must not be used to judge the user\'s data dead)', async () => {
    responder = () => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: 'not-ours',
        kind: 'invalid-bundle', retryable: false, detail: 'x',
      },
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toMatchObject({ delivered: false, reason: 'malformed-response', retryable: true });
    expect(result).not.toMatchObject({ kind: 'invalid-bundle' });
  });

  it('a malformed nack ⇒ malformed-response (its claimed kind is not believed)', async () => {
    responder = (message) => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: message.request_id,
        kind: 'something-new', retryable: 'yes', detail: 'x',
      },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'malformed-response',
    });
  });
});

// ===========================================================================
// 4 · Not even the runtime API is there
// ===========================================================================
describe('W2 · an environment with no API', () => {
  it('no runtime.sendNativeMessage ⇒ no-runtime-api (nothing was attempted, and no faking success)', async () => {
    const saved = fakeBrowser.runtime.sendNativeMessage;
    delete (fakeBrowser.runtime as any).sendNativeMessage;
    try {
      const result = await deliver(NAME, PAYLOAD);
      expect(result).toMatchObject({ delivered: false, reason: 'no-runtime-api' });
      expect(sent).toEqual([]);            // not one byte went out
      expect(await hello()).toMatchObject({ ok: false, reason: 'no-runtime-api' });
    } finally {
      fakeBrowser.runtime.sendNativeMessage = saved;
    }
  });
});

// ===========================================================================
// 5 · hello（§6.1）
// ===========================================================================
describe('W2 · hello', () => {
  it('a conforming answer ⇒ {ok:true, machine, stage, hostVersion}', async () => {
    responder = () => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'hello', ok: true,
        host_version: '0.3.0', machine: 'm-1', stage: '/Users/x/stage',
      },
    });
    const result = await hello();
    expect(result).toEqual({ ok: true, machine: 'm-1', stage: '/Users/x/stage', hostVersion: '0.3.0' });
    // The request is §6.1's one, byte for byte.
    expect(sent[0]).toEqual({ protocol: 1, type: 'hello' });
  });

  it('a nack ⇒ {ok:false, reason:nack, kind, detail} (carrying the repair hint)', async () => {
    responder = () => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: null,
        kind: 'config', retryable: false,
        detail: 'run `chat-stasher install-native-host --stage <path>`',
      },
    });
    const result = await hello();
    expect(result.ok).toBe(false);
    expect(result).toMatchObject({ reason: 'nack', kind: 'config', retryable: false });
    expect((result as { detail?: string }).detail).toContain('install-native-host');
  });

  it('a malformed hello answer ⇒ malformed-response', async () => {
    responder = () => ({
      via: 'callback',
      response: { protocol: 1, type: 'hello', ok: true, host_version: '0.3.0', machine: 'm', stage: 42 },
    });
    expect(await hello()).toMatchObject({ ok: false, reason: 'malformed-response' });
  });

  it('anything but ok:true (a pseudo-hello answering ok:false) ⇒ malformed-response, never treated as connected', async () => {
    responder = () => ({
      via: 'callback',
      response: { protocol: 1, type: 'hello', ok: false, host_version: '0', machine: 'm', stage: '/s' },
    });
    expect(await hello()).toMatchObject({ ok: false, reason: 'malformed-response' });
  });

  it('the hello probe window can be shortened by the caller (the popup uses 3 seconds; delivery is always 60)', async () => {
    responder = () => ({ via: 'silent' });
    vi.useFakeTimers();
    const pending = hello({ timeoutMs: 3_000 });
    await vi.advanceTimersByTimeAsync(3_001);
    expect(await pending).toMatchObject({ ok: false, reason: 'timeout' });
  });
});

// ===========================================================================
// 6 · The host name
// ===========================================================================
describe('W2 · the host name', () => {
  it('it uses exactly the name pinned in the contract', async () => {
    let host = '';
    fakeBrowser.runtime.sendNativeMessage = (h: string, message: Record<string, unknown>, cb: (r: unknown) => void) => {
      host = h;
      sent.push(message);
      cb(ackFor(message));
      return undefined;
    };
    await deliver(NAME, PAYLOAD);
    expect(host).toBe(NATIVE_HOST_NAME);
    expect(NATIVE_HOST_NAME).toBe('com.chat_stasher.host');
  });
});
