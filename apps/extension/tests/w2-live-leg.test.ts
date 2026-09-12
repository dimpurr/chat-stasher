/**
 * W2 · The live leg's four outcomes, none of which may be confused with another.
 *
 *   delivered — a matching ack arrived ⇒ saved:true
 *   queued    — written into the outbox but not yet confirmed ⇒ saved:false (distinguishable; the badge counts it)
 *   rejected  — the host explicitly nacked it and it is not retryable ⇒ kept, visible, never retried automatically
 *   refused   — we did not even take it in (outbox full / unreadable / no name could be produced) ⇒ a named reason
 *
 * 🔴 The most important thing in this file is write-ahead:
 *    at the very moment delivery happens, the payload must already be lying in the outbox.
 *    The way it is asserted: the stub reads the outbox when the delivery request arrives, and it must find that entry.
 *
 * Everything goes through the **real background entry point** (runtime.onMessage('chat-captured')),
 * swapping only browser.* and the host for programmable stubs.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];

/** What the host does at each moment. */
let hostMode: 'up' | 'down' | 'nack-nonretryable' | 'nack-retryable';
/** Whether the entry was in the outbox when the delivery request arrived (the direct evidence for write-ahead). */
let outboxHadPayloadAtDelivery: boolean | null = null;
let deliveries: Array<{ name: string; payload: string }> = [];
let lastCapturedAt = 1_700_000_000_000;

async function sha256Of(text: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

const fakeBrowser: any = {
  runtime: {
    id: 'w2-live-leg',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    async sendNativeMessage(host: string, message: Record<string, any>) {
      if (message.type === 'deliver') {
        // 🔴 The write-ahead checkpoint: delivery happens **right now** — the host being alive or dead makes no difference.
        const { getEntry } = await import('../lib/outbox');
        const lookup = await getEntry(message.sha256);
        outboxHadPayloadAtDelivery = lookup.ok && lookup.entry !== null;
      }
      if (hostMode === 'down') throw new Error('Specified native messaging host not found.');
      if (message.type === 'hello') {
        return { protocol: 1, type: 'hello', ok: true, host_version: '0.3.0', machine: 'm', stage: '/stage' };
      }
      if (message.type === 'deliver') {
        if (hostMode === 'nack-nonretryable' || hostMode === 'nack-retryable') {
          const retryable = hostMode === 'nack-retryable';
          return {
            protocol: 1, type: 'nack', request_id: message.request_id,
            kind: retryable ? 'integrity' : 'invalid-bundle', retryable,
            detail: retryable ? 'sha256 does not match payload' : 'payload is not an inbox bundle',
          };
        }
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

const SID = 'aaaaaaaa-1111-2222-3333-444444444444';

function capture(overrides: Partial<CapturedFetch> = {}): CapturedFetch {
  return {
    url: `https://chatgpt.com/backend-api/conversation/${SID}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-1' }),
    pageUrl: `https://chatgpt.com/c/${SID}`,
    capturedAt: (lastCapturedAt += 1),
    ...overrides,
  };
}

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

/**
 * Nudge the outbox's "bytes used" to one byte short of full.
 *
 * It uses the module's own exported database name / store name / counter key — not a second
 * implementation, just pushing that counter to the limit so the real branch
 * (`used + bytes > capacity`) really runs. Really writing 256 MiB would also work, but that is the same assertion, slower and more fragile.
 */
async function fillCounterToCapacity(): Promise<void> {
  const { OUTBOX_DB_NAME, OUTBOX_DB_VERSION, OUTBOX_META_STORE, OUTBOX_META_BYTES_KEY, OUTBOX_CAPACITY_BYTES } =
    await import('../lib/outbox');
  const db: IDBDatabase = await new Promise((resolve, reject) => {
    const req = (globalThis as any).indexedDB.open(OUTBOX_DB_NAME, OUTBOX_DB_VERSION);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(OUTBOX_META_STORE, 'readwrite');
    tx.objectStore(OUTBOX_META_STORE).put(OUTBOX_CAPACITY_BYTES - 1, OUTBOX_META_BYTES_KEY);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
  db.close();
}

beforeEach(() => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  vi.resetModules();
  (globalThis as any).indexedDB = new IDBFactory();
  hostMode = 'up';
  outboxHadPayloadAtDelivery = null;
  deliveries = [];
  lastCapturedAt = 1_700_000_000_000;
  fakeBrowser.action.badgeText = '';
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

describe('W2-LIVE · delivered', () => {
  it('🔴 a matching ack ⇒ saved:true, status:delivered; and **the payload was already in the outbox at delivery time**', async () => {
    const result = await dispatch(capture());

    expect(result.ok).toBe(true);
    expect(result.saved).toBe(true);
    expect(result.status).toBe('delivered');
    expect(result.channel).toBe('native-messaging');
    expect(result.finalName).toBe(`chatgpt-${SID}.json`);
    expect(deliveries).toHaveLength(1);

    // 🔴 write-ahead: the stub reads the outbox at the moment of delivery, and the entry must already be there.
    expect(outboxHadPayloadAtDelivery).toBe(true);

    // After the ack the entry is deleted and the badge goes back to empty.
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    expect(fakeBrowser.action.badgeText).toBe('');
  });
});

describe('W2-LIVE · queued (enqueued, not confirmed)', () => {
  it('🔴 the host is absent ⇒ saved:false, status:queued (neither success nor failure), the entry stays in the outbox and the badge shows 1', async () => {
    hostMode = 'down';
    const result = await dispatch(capture());

    expect(result.ok).toBe(false);          // ok === saved, and neither may lie
    expect(result.saved).toBe(false);
    expect(result.status).toBe('queued');
    expect(result.reason).toBe('send-failed');
    expect(outboxHadPayloadAtDelivery).toBe(true);   // written down before the attempt here too

    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ name: `chatgpt-${SID}.json`, state: 'pending', attempts: 1 });
    expect(fakeBrowser.action.badgeText).toBe('1');
  });

  it('once the host is back, the next heartbeat sends it and clears the ledger (badge emptied)', async () => {
    hostMode = 'down';
    await dispatch(capture());
    const { listEntries } = await import('../lib/outbox');
    expect((await listEntries())!).toHaveLength(1);

    hostMode = 'up';
    const { drainOutbox } = await import('../lib/outbox');
    // The next heartbeat: the backoff window is long past (a real alarm ticks every 5 minutes).
    const report = await drainOutbox({ now: () => Date.now() + 10 * 60_000 });
    expect(report).toMatchObject({ delivered: 1, stoppedBy: 'drained' });
    expect(await listEntries()).toEqual([]);
    expect(deliveries).toHaveLength(1);
  });
});

describe('W2-LIVE · rejected (the host says outright that this one will not do)', () => {
  it('🔴 a non-retryable nack ⇒ saved:false, status:rejected, carrying the kind, and the entry is **kept**', async () => {
    hostMode = 'nack-nonretryable';
    const result = await dispatch(capture());

    expect(result.saved).toBe(false);
    expect(result.status).toBe('rejected');
    expect(result.kind).toBe('invalid-bundle');
    expect(result.reason).toContain('invalid-bundle');

    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ state: 'rejected', rejectKind: 'invalid-bundle' });
    expect(fakeBrowser.action.badgeText).toBe('!');   // the alert state, not a number
  });

  it('a retryable nack ⇒ still only queued (the host said "try again", not "this one will not do")', async () => {
    hostMode = 'nack-retryable';
    const result = await dispatch(capture());
    expect(result).toMatchObject({ saved: false, status: 'queued' });
    // The outcome is "not delivered yet", with the reason the host gave (kind included), not a death sentence.
    expect(result.reason).toContain('nack');
    expect(result.reason).toContain('integrity');
    expect(result.kind).toBeUndefined();
    const entries = (await (await import('../lib/outbox')).listEntries())!;
    expect(entries[0]!.state).toBe('pending');
  });
});

describe('W2-LIVE · refused (we did not even take it in)', () => {
  it('🔴 the outbox is full ⇒ this capture is refused with a visible reason; the queued entries are all there, not one byte changed', async () => {
    const ob = await import('../lib/outbox');
    // Really put one in first (so "the old ones were not touched" has something to point at). The host goes offline ⇒ it stays in the queue.
    hostMode = 'down';
    const firstResult = await dispatch(capture());
    expect(firstResult).toMatchObject({ saved: false, status: 'queued' });
    const before = await ob.listEntries();
    expect(before).toHaveLength(1);
    hostMode = 'up';

    // Push "bytes used" straight to the limit: the counter in meta is the only ledger there is.
    // (Faster and steadier than really writing 256 MiB, and it takes the same real branch.)
    await fillCounterToCapacity();

    const result = await dispatch(capture());
    expect(result.saved).toBe(false);
    expect(result.status).toBe('refused');
    expect(result.reason).toBe('outbox-full');

    const after = await ob.listEntries();
    expect(after).toEqual(before);   // 🔴 all there, not one byte changed
    console.log('[W2-LIVE] the refusal returned after it filled up:', result);
  });

  it('🔴 no conversation identity can be produced ⇒ refused, with not one byte entering the outbox', async () => {
    const result = await dispatch(capture({
      url: 'https://chatgpt.com/backend-api/conversation/shortid',
      pageUrl: undefined,
    }));
    expect(result).toMatchObject({ saved: false, status: 'refused' });
    expect(result.reason).toContain('no-session-id');
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    expect(deliveries).toEqual([]);
  });

  it('🔴 IndexedDB cannot be read ⇒ refused/queued naming outbox-unavailable, never passed off as success', async () => {
    delete (globalThis as any).indexedDB;
    vi.resetModules();
    const result = await dispatch(capture());
    expect(result.saved).toBe(false);
    expect(result.reason).toBe('outbox-unavailable');
    expect(deliveries).toEqual([]);            // nothing goes into the outbox, so nothing is delivered
  });
});
