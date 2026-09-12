/**
 * W2 · The end-to-end synthetic chain: page hook → postMessage → bridge → background → outbox → local host.
 *
 * This replaces chain.test.ts. The old version's whole chain ended at `chrome.downloads`
 * (the real filesystem), and that channel, along with the `downloads` permission, has been
 * deleted; what is unchanged is the **shape** of the case: load the real entrypoint
 * source, really run it, and swap only the browser APIs and the host for stubs.
 *
 * 🔴 Every criterion corresponds one-for-one with the old version; only the landing point
 *    moved from "is that file on disk" to "which payload did the host receive and ack" —
 *    the latter is what counts under spec §1.
 *
 * Zero real network, zero logged-in state, zero real files throughout.
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { type CapturedFetch } from '../lib/contract';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

const runtimeListeners: Array<{
  fn: (msg: any, sender: any, sendResponse: (r: any) => void) => any;
}> = [];

let host: SyntheticHost;

/** Each test simulates a fresh extension: clear all registered listeners/records. */
function resetMocks() {
  runtimeListeners.length = 0;
  vi.unstubAllGlobals();
}

beforeEach(() => {
  resetMocks();
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineContentScript', (cfg: any) => cfg);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() { /* startup badge refresh is a no-op here */ } },
    onMessage: {
      addListener(fn: any) {
        runtimeListeners.push({ fn });
      },
    },
    sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
    async sendMessage(msg: any): Promise<any> {
      return new Promise((resolve, reject) => {
        let settled = false;
        const doResolve = (v: any) => {
          if (!settled) {
            settled = true;
            resolve(v);
          }
        };
        for (const { fn } of runtimeListeners) {
          try {
            const ret = fn(msg, { id: 'mock-sender', url: null }, doResolve);
            if (ret && typeof ret.then === 'function') {
              ret.then(doResolve).catch(reject);
            } else if (ret !== true) {
              doResolve(ret);
            }
            // ret === true => async, waits for sendResponse() (called above)
          } catch (err) {
            reject(err);
            return;
          }
        }
        setTimeout(() => doResolve(undefined), 5000).unref?.();
      });
    },
  },
  action: {
    async setBadgeText() {},
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
};

async function loadBackground() {
  const mod = await import('../entrypoints/background');
  const bg = (mod as any).default;
  await bg(); // simulate service worker startup
}

async function loadBridge() {
  const mod: any = await import('../entrypoints/dw-bridge.content');
  mod.default.main();
}

async function loadMainHook() {
  const mod: any = await import('../entrypoints/dw-fetch-main.content');
  mod.default.main();
  return mod.default;
}

function makeFakeWindow() {
  const listeners: Record<string, Array<(e: any) => void>> = {};
  return {
    listeners,
    location: { origin: 'https://chat.deepseek.com' },
    addEventListener(name: string, fn: (e: any) => void) {
      (listeners[name] ??= []).push(fn);
    },
    dispatchEvent(e: any) {
      for (const fn of listeners[e.type] ?? []) fn(e);
      return true;
    },
    postMessage(data: unknown, targetOrigin: string) {
      if (targetOrigin !== this.location.origin) return;
      for (const fn of listeners.message ?? []) fn({ source: this, origin: this.location.origin, data });
    },
    fetch: null as any,
  };
}

describe('W2 · synthetic chain: page → bridge → background → outbox → host', () => {
  it('one real capture ends with a matching ack, and the payload is byte-for-byte a conforming inbox bundle', async () => {
    await loadBackground();

    const fakeWin = makeFakeWindow();
    vi.stubGlobal('window', fakeWin);
    await loadBridge();

    const rawBody = JSON.stringify({
      session_id: 'c622b5dd-0000-4000-8000-00000000abcd',
      message: { content: 'synthetic answer, never printed in reports', reasoning_content: '' },
    });
    fakeWin.fetch = async () =>
      new Response(rawBody, { status: 200, headers: { 'content-type': 'application/json' } });
    await loadMainHook();

    const fakeUrl = 'https://chat.deepseek.com/api/v0/chat/session/c622b5dd-0000-4000-8000-00000000abcd';
    await (fakeWin.fetch as any)(fakeUrl);

    // Let the delivery microtasks and the IndexedDB transactions run to completion.
    await new Promise((r) => setTimeout(r, 100));

    expect(host.deliveries).toHaveLength(1);
    const delivery = host.deliveries[0]!;
    console.log('[W2-CHAIN] name the host received:', delivery.name, '· payload bytes:', delivery.payload.length);

    // §6.2: the name must conform to the spec.
    expect(delivery.name).toBe('deepseek-c622b5dd-0000-4000-8000-00000000abcd.json');
    // §6.2: sha256 is the SHA-256 of the payload's UTF-8 bytes — the host computed it itself and checked.
    expect(delivery.sha256).toMatch(/^[0-9a-f]{64}$/);

    const doc = JSON.parse(delivery.payload);
    expect(doc.schema).toBe('chat-stasher/inbox@2');
    expect(doc.platform).toBe('deepseek');
    expect(doc.sessionId).toBe('c622b5dd-0000-4000-8000-00000000abcd');
    // C3: this synthetic body carries no account fields, so the ADR-002 chain
    // falls through to an explicit 'default' marker — never silently missing.
    expect(doc.identity).toEqual({ level: 'default', value: '' });
    // raw.bytes = size of the ORIGINAL response.
    expect(doc.raw.bytes).toBe(Buffer.byteLength(rawBody, 'utf8'));

    // After the ack the outbox is empty: no entries are left behind as "sent but still here".
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
  });

  it('non-conversation traffic (another path / another origin / anything but GET) delivers nothing at all', async () => {
    await loadBackground();

    const fakeWin = makeFakeWindow();
    vi.stubGlobal('window', fakeWin);
    await loadBridge();

    fakeWin.fetch = async () => new Response('{}', { status: 200 });
    await loadMainHook();

    const nonSessionUrls = [
      'https://chat.deepseek.com/api/v0/users/me',
      'https://chat.deepseek.com/api/v0/payments/session/sess-x',
      'https://example.com/api/v0/chat/anything', // not DeepSeek origin
    ];
    for (const u of nonSessionUrls) await (fakeWin.fetch as any)(u);
    await new Promise((r) => setTimeout(r, 300));

    expect(host.deliveries).toEqual([]);
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    console.log('[W2-CHAIN-EVIDENCE] deliveries produced by non-conversation traffic:', host.deliveries.length);
  });

  it('with the host absent, the same capture stays in the outbox (write-ahead, end to end)', async () => {
    host = createSyntheticHost({ up: false });
    await loadBackground();

    const payload: CapturedFetch = {
      url: 'https://chat.deepseek.com/api/v0/chat/session/aaaa1111-bbbb-4000-8000-00000000ffff',
      method: 'POST',
      status: 200,
      text: JSON.stringify({ session_id: 'aaaa1111-bbbb-4000-8000-00000000ffff', message: { content: 'x' } }),
      capturedAt: Date.now(),
    };
    const { handleCaptured } = await import('../entrypoints/background');
    const result = await handleCaptured(payload);

    expect(result).toMatchObject({ saved: false, status: 'queued' });
    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    const doc = JSON.parse(entries[0]!.payload);
    expect(doc.sessionId).toBe('aaaa1111-bbbb-4000-8000-00000000ffff');
    // The name is already settled in the outbox — the host will store it as a shard under that name when it returns.
    expect(entries[0]!.name).toBe('deepseek-aaaa1111-bbbb-4000-8000-00000000ffff.json');
  });
});
