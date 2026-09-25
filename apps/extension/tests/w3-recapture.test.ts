/**
 * W3 · Not delivering a conversation that has not changed since it was last
 * acknowledged (lib/recapture.ts, wired into handleCaptured).
 *
 * Measured 2026-09-14: ChatGPT returns the whole conversation on every view, and
 * two copies of one conversation differed **only** in the top-level `safe_urls`
 * field. Every view therefore appended another full copy (4 x 2.1 MB for one
 * conversation in 20 minutes). The fix: fingerprint the response with the known
 * volatile fields removed, remember it **only after a matching ack**, and on the
 * next capture with the same fingerprint answer `saved:true, status:'unchanged'`
 * without touching the outbox.
 *
 * Three things this file is built to catch, in the order they would hurt:
 *
 *  1. A fingerprint that is not actually insensitive to the volatile field ⇒ the
 *     duplicates come back (tested both directly and through handleCaptured).
 *  2. A fingerprint recorded **before** the delivery was acknowledged ⇒ a
 *     conversation that was only queued is skipped forever and silently lost.
 *     🔴 This is the one that breaks the project's first invariant, so it is
 *     tested at the handleCaptured level, not only at the store level.
 *  3. A platform with no registered volatile fields being skipped at all.
 *
 * Everything at the handleCaptured level goes through the **real background
 * entry point** (`runtime.onMessage('chat-captured')`) with only `browser.*` and
 * the native host swapped for stubs, the same shape as tests/w2-live-leg.test.ts.
 * All conversation bodies are synthetic; nothing here touches a real archive.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import { memoryStore } from '../lib/backfill/store';
import {
  LAST_DELIVERED_KEY,
  MAX_REMEMBERED,
  contentFingerprint,
  isUnchangedCapture,
  rememberDelivered,
  type DeliveryDestination,
} from '../lib/recapture';

// ---------------------------------------------------------------------------
// A synthetic ChatGPT conversation.
//
// `mapping` / `current_node` / `conversation_id` are the response's stable part;
// `safe_urls` is the field measured to differ between two copies of the same
// conversation. `buildChatgptBody` is the single place either of them is spelled.
// ---------------------------------------------------------------------------

const CHATGPT_SID = 'b7f1c2d3-1111-4222-8333-9a0b0c0d0e0f';

function buildChatgptBody(safeUrls: string[], nodeText = 'synthetic answer text'): string {
  return JSON.stringify({
    conversation_id: CHATGPT_SID,
    current_node: 'node-1',
    safe_urls: safeUrls,
    mapping: {
      'node-0': { id: 'node-0', message: { content: { parts: ['synthetic question'] } } },
      'node-1': { id: 'node-1', message: { content: { parts: [nodeText] } } },
    },
    account_id: 'acct-synthetic',
  });
}

// ---------------------------------------------------------------------------
// The fake extension surface (see w2-live-leg.test.ts for the same pattern).
// ---------------------------------------------------------------------------

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];

/** 'up' acks every delivery; 'down' makes every host call throw. */
let hostMode: 'up' | 'down' = 'up';
let deliveries: Array<{ name: string; payload: string }> = [];
let lastCapturedAt = 1_700_000_000_000;

const fakeBrowser: any = {
  runtime: {
    id: 'w3-recapture',
    onStartup: { addListener() { /* startup badge refresh is a no-op here */ } },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    async sendNativeMessage(_host: string, message: Record<string, any>) {
      if (hostMode === 'down') throw new Error('Specified native messaging host not found.');
      if (message.type === 'hello') {
        return { protocol: 1, type: 'hello', ok: true, host_version: '0.3.0', machine: 'm', stage: '/stage' };
      }
      if (message.type === 'deliver') {
        deliveries.push({ name: message.name, payload: message.payload });
        return {
          protocol: 1, type: 'ack', request_id: message.request_id,
          status: 'stored', sha256: message.sha256, shard: `0001-${message.name}`,
        };
      }
      throw new Error(`unexpected ${message.type}`);
    },
  },
  storage: {
    local: {
      async get(query: Record<string, unknown> | null) {
        if (query === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(query)) out[k] = k in store ? store[k] : query[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: {
    badgeText: '' as string,
    async setBadgeText(o: { text: string }) { fakeBrowser.action.badgeText = o.text; },
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
};

function chatgptCapture(text: string): CapturedFetch {
  return {
    url: `https://chatgpt.com/backend-api/conversation/${CHATGPT_SID}`,
    method: 'GET',
    status: 200,
    text,
    pageUrl: `https://chatgpt.com/c/${CHATGPT_SID}`,
    capturedAt: (lastCapturedAt += 1),
  };
}

function deepseekCapture(text: string, sid: string): CapturedFetch {
  return {
    url: `https://chat.deepseek.com/api/v0/chat/session/${sid}`,
    method: 'POST',
    status: 200,
    text,
    capturedAt: (lastCapturedAt += 1),
  };
}

/** Run one capture through the real onMessage listener and await its answer. */
async function dispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (runtimeListeners.length === 0) await mod.default();
  const replied = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    expect(ret).toBe(true);
  });
  await mod.backfillTickSettled();
  return replied;
}

/** The original response texts the host was actually handed, in delivery order. */
function deliveredTexts(): string[] {
  return deliveries.map((d) => JSON.parse(d.payload).raw.text as string);
}

beforeEach(() => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  vi.resetModules();
  (globalThis as any).indexedDB = new IDBFactory();
  hostMode = 'up';
  deliveries = [];
  lastCapturedAt = 1_700_000_000_000;
  fakeBrowser.action.badgeText = '';
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

// ---------------------------------------------------------------------------
// 1 · contentFingerprint
// ---------------------------------------------------------------------------

describe('W3-RECAPTURE · contentFingerprint', () => {
  it('two ChatGPT responses differing only in safe_urls have the same fingerprint', async () => {
    const withNone = buildChatgptBody([]);
    const withOne = buildChatgptBody(['https://example.invalid/link']);

    const a = await contentFingerprint('chatgpt', withNone);
    const b = await contentFingerprint('chatgpt', withOne);

    expect(a).not.toBeNull();
    expect(a).toMatch(/^[0-9a-f]{64}$/);
    // 🔴 The whole point: the one field measured to change on every request must
    //    not move the fingerprint. The bodies really do differ.
    expect(withNone).not.toBe(withOne);
    expect(a).toBe(b);
  });

  it('any other difference — one field of one node in mapping — changes the fingerprint', async () => {
    const base = await contentFingerprint('chatgpt', buildChatgptBody([]));
    const changed = await contentFingerprint(
      'chatgpt',
      buildChatgptBody([], 'synthetic answer text, edited'),
    );

    expect(changed).not.toBe(base);
  });

  it('a platform with no registered volatile fields gets no fingerprint at all', async () => {
    const body = JSON.stringify({ session_id: 'dddddddd-1111-4222-8333-000000000000', message: { content: 'x' } });
    // Not "unchanged by default": a null fingerprint means the caller cannot skip.
    expect(await contentFingerprint('deepseek', body)).toBeNull();
  });

  it('a body that is not a JSON object gets no fingerprint at all', async () => {
    expect(await contentFingerprint('chatgpt', 'not json at all')).toBeNull();
    expect(await contentFingerprint('chatgpt', '')).toBeNull();
    expect(await contentFingerprint('chatgpt', '[1, 2, 3]')).toBeNull();
    expect(await contentFingerprint('chatgpt', '"a string"')).toBeNull();
    expect(await contentFingerprint('chatgpt', 'null')).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// 2 · rememberDelivered / isUnchangedCapture
//
// The store is the repository's own in-memory implementation (the two methods
// the port asks for), not a second copy of it.
//
// 🔴 W50b · Every call now carries a destination, because "unchanged" is only ever
//    true of *where* the copy was stored. `DEST_A` is the fixture host this section
//    acknowledges against. The two failure cases that need the real delivery path
//    (a host that has changed, and an entry written before W50b) are exercised
//    through `runtime.onMessage` in tests/w50b-delivery-destination.test.ts; what is
//    asserted here is the store's own bookkeeping.
// ---------------------------------------------------------------------------

const DEST_A: DeliveryDestination = { machine: 'fixture-machine', stage: '/stage/a' };
const DEST_B: DeliveryDestination = { machine: 'fixture-machine', stage: '/stage/b' };

/** The guard as a delivery leg calls it, with the host's answer fixed for the test. */
function unchanged(
  s: ReturnType<typeof memoryStore> | null,
  name: string,
  fingerprint: string,
  destination: DeliveryDestination | null,
): Promise<boolean> {
  return isUnchangedCapture(s, name, { platform: 'chatgpt', fingerprint }, async () => destination);
}

describe('W3-RECAPTURE · the remembered fingerprint', () => {
  it('is unchanged only after that exact fingerprint was recorded for that name at that destination', async () => {
    const s = memoryStore();
    const name = `chatgpt-${CHATGPT_SID}.json`;

    expect(await unchanged(s, name, 'fp-1', DEST_A)).toBe(false);

    await rememberDelivered(s, name, 'fp-1', DEST_A);
    expect(await unchanged(s, name, 'fp-1', DEST_A)).toBe(true);
    // A different fingerprint for the same conversation is a change.
    expect(await unchanged(s, name, 'fp-2', DEST_A)).toBe(false);
    // The same fingerprint under another name says nothing about this one.
    expect(await unchanged(s, 'chatgpt-other.json', 'fp-1', DEST_A)).toBe(false);
    // 🔴 W50b · The same body acknowledged at another destination is not a match:
    //    this copy is not on record as stored *there*, whatever it says about here.
    expect(await unchanged(s, name, 'fp-1', DEST_B)).toBe(false);
    // 🔴 And a destination the host would not confirm is not a match either — the
    //    unknown is resolved toward delivering, never toward skipping.
    expect(await unchanged(s, name, 'fp-1', null)).toBe(false);
  });

  it('an unavailable store answers "not known to be unchanged", never "unchanged"', async () => {
    // 🔴 "Cannot read it" must not collapse into "it did not change": the caller
    //    has to keep delivering.
    expect(await unchanged(null, 'anything.json', 'fp-1', DEST_A)).toBe(false);
    // And recording into it is a no-op rather than a throw.
    await expect(rememberDelivered(null, 'anything.json', 'fp-1', DEST_A)).resolves.toBeUndefined();
  });

  it('keeps the most recent MAX_REMEMBERED, dropping the oldest first', async () => {
    const seed: Record<string, unknown> = {};
    for (let i = 0; i < MAX_REMEMBERED; i += 1) {
      seed[`conv-${i}`] = { fingerprint: `fp-${i}`, machine: DEST_A.machine, stage: DEST_A.stage };
    }
    const s = memoryStore({ [LAST_DELIVERED_KEY]: seed });

    await rememberDelivered(s, 'conv-new', 'fp-new', DEST_A);

    const oldest = 'conv-0';
    const newest = `conv-${MAX_REMEMBERED - 1}`;
    // 🔴 The oldest really is gone, and the newest really is still there — both
    //    halves asserted, because either alone would pass on a broken cap.
    expect(await unchanged(s, oldest, 'fp-0', DEST_A)).toBe(false);
    expect(await unchanged(s, newest, `fp-${MAX_REMEMBERED - 1}`, DEST_A)).toBe(true);
    expect(await unchanged(s, 'conv-new', 'fp-new', DEST_A)).toBe(true);

    const saved = s.data[LAST_DELIVERED_KEY] as Record<string, unknown>;
    expect(Object.keys(saved)).toHaveLength(MAX_REMEMBERED);
    expect(oldest in saved).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 3 · through the real entry point
// ---------------------------------------------------------------------------

describe('W3-RECAPTURE · handleCaptured', () => {
  it('🔴 the same conversation, differing only in safe_urls: the second capture is not delivered again', async () => {
    const firstBody = buildChatgptBody([]);
    const secondBody = buildChatgptBody(['https://example.invalid/link']);

    const first = await dispatch(chatgptCapture(firstBody));
    expect(first).toMatchObject({ ok: true, saved: true, status: 'delivered' });
    expect(first.finalName).toBe(`chatgpt-${CHATGPT_SID}.json`);
    expect(deliveries).toHaveLength(1);
    expect(deliveredTexts()).toEqual([firstBody]);

    const second = await dispatch(chatgptCapture(secondBody));
    expect(second.saved).toBe(true);
    expect(second.status).toBe('unchanged');
    expect(second.finalName).toBe(`chatgpt-${CHATGPT_SID}.json`);

    // 🔴 Not sent, and nothing new entered the outbox.
    expect(deliveries).toHaveLength(1);
    expect(deliveredTexts()).toEqual([firstBody]);
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    // Nothing is waiting, so the badge stays empty (an unchanged capture is not a
    // pending item).
    expect(fakeBrowser.action.badgeText).toBe('');
  });

  it('a real change after that is delivered again', async () => {
    await dispatch(chatgptCapture(buildChatgptBody([])));
    expect(deliveries).toHaveLength(1);

    const edited = buildChatgptBody([], 'synthetic answer text, edited');
    const result = await dispatch(chatgptCapture(edited));

    expect(result).toMatchObject({ saved: true, status: 'delivered' });
    expect(deliveries).toHaveLength(2);
    expect(deliveredTexts()).toContain(edited);
  });

  it('🔴 an unconfirmed delivery must not make the next capture look unchanged', async () => {
    const firstBody = buildChatgptBody([]);
    const secondBody = buildChatgptBody(['https://example.invalid/link']);

    // The host is not there: the payload is written into the outbox, and no ack
    // ever arrives. Nothing about it was confirmed, so nothing may be remembered.
    hostMode = 'down';
    const first = await dispatch(chatgptCapture(firstBody));
    expect(first).toMatchObject({ saved: false, status: 'queued' });
    expect(deliveries).toHaveLength(0);

    hostMode = 'up';
    const second = await dispatch(chatgptCapture(secondBody));

    // 🔴 It must really go out. Answering 'unchanged' here would mean a
    //    conversation that was never stored is now skipped forever.
    expect(second.status).not.toBe('unchanged');
    expect(second).toMatchObject({ saved: true, status: 'delivered' });
    expect(deliveredTexts()).toContain(secondBody);

    // And the unconfirmed first copy is still waiting in the outbox, not lost and
    // not struck off: it is on the outbox's own retry backoff, which is a state of
    // its own — "not delivered yet", never "done".
    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    expect(JSON.parse(entries[0]!.payload).raw.text).toBe(firstBody);
    expect(entries[0]!.state).toBe('pending');
  });

  it('a platform with no registered volatile fields is always delivered', async () => {
    const sid = 'dddddddd-1111-4222-8333-000000000000';
    const body = JSON.stringify({ session_id: sid, message: { content: 'synthetic answer' } });

    const first = await dispatch(deepseekCapture(body, sid));
    const second = await dispatch(deepseekCapture(body, sid));

    // Byte-for-byte identical bodies, and still delivered twice: for deepseek we
    // have no measured notion of "volatile", so we do not get to skip anything.
    expect(first).toMatchObject({ saved: true, status: 'delivered' });
    expect(second).toMatchObject({ saved: true, status: 'delivered' });
    expect(deliveries).toHaveLength(2);
  });
});
