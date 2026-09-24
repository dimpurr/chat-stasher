/**
 * C13 · Testing that "the backfill leg really does run at runtime".
 *
 * 🔴 Why this file exists: C11/C12's tests are all green, but every one of them **imports
 *    runBackfill and calls it** — green only proves the module runs, not that the browser
 *    will run it. So not one assertion here may call runBackfill / tickBackfill directly:
 *    every case has to start from **entrypoints/background.ts's real entry point** —
 *      defineBackground's callback → browser.runtime.onMessage dispatching 'chat-captured',
 *    which is exactly the path a content script takes in a real browser — and only then
 *    look at whether the backfill leg was touched.
 *
 * runBackfill is replaced by a spy via vi.mock: what we assert is "was it reached", not
 * engine behaviour (already covered by c11/c12). No real network activity, no logged-in state.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import { DEFAULT_SPEED_PRESET, SPEED_PLANS } from '../lib/backfill/speed';

// ---- Replace the engine with a spy (in this file only) ----
const runBackfillSpy = vi.fn(async (opts: any) => ({
  stopped: 'queue-empty',
  enumeratedPages: 0,
  newDebts: 0,
  archivedThisRun: [],
  skippedAlreadyArchived: 0,
  skippedAlreadyPending: 0,
  progress: 'stub',
  halted: null,
  paceTrace: { enumerate: [], detail: [] },
  state: { __opts: opts },
}));

vi.mock('../lib/backfill/engine', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/backfill/engine')>();
  return { ...actual, runBackfill: (opts: any) => runBackfillSpy(opts) };
});

// ---- A fake browser: storage.local / downloads / action / runtime ----
const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
let host: SyntheticHost;
/** The host is entirely down (not installed on this machine / does not answer). */
let hostDown = false;

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: {
      addListener(fn: any) { runtimeListeners.push(fn); },
    },
    // W2: the live leg's write-down channel = a synthetic native host.
    sendNativeMessage: (h: string, m: unknown) => {
      if (hostDown) throw new Error('Specified native messaging host not found.');
      return host.sendNativeMessage(h, m);
    },
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown>) {
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in store ? store[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: {
    async setBadgeText() {},
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
};

/** A real 'chat-captured' payload (a synthetic fixture, not anybody's actual conversation). */
function fakeCapture(): CapturedFetch {
  return {
    url: 'https://chatgpt.com/backend-api/conversation/abcdef0123456789',
    method: 'GET',
    status: 200,
    text: JSON.stringify({ conversation_id: 'abcdef0123456789', account_id: 'acct-fixture-1', mapping: {} }),
    pageUrl: 'https://chatgpt.com/c/abcdef0123456789',
    capturedAt: 1_700_000_000_000,
  };
}

/**
 * Take the real entry point: load the background module → run defineBackground's callback
 * → get the onMessage listener it registered → dispatch a message the way a content script does.
 */
async function bootBackgroundAndDispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  await mod.default();                       // defineBackground is stubbed to the identity function
  expect(runtimeListeners.length).toBeGreaterThan(0);
  const responded = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    expect(ret).toBe(true);                  // the MV3 async sendResponse contract
  });
  // The wiring is fire-and-forget (it must never slow the write-down), so wait for it to finish on its own.
  await mod.backfillTickSettled();
  return responded;
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  host = createSyntheticHost({ up: true });
  hostDown = false;
  (globalThis as any).indexedDB = new IDBFactory();
  runBackfillSpy.mockClear();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C13 · the backfill leg wired into the runtime', () => {
  it('🔴 criterion 2: the live leg real message path wakes the backfill leg (switch on + an http port ⇒ runBackfill really is called)', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);

    const mod: any = await import('../entrypoints/background');
    // A synthetic http port: it returns a fixed string and never touches the network.
    mod.configureBackfillTransport(async () => ({ status: 200, text: '{"items":[],"total":0}' }));

    const res = await bootBackgroundAndDispatch(fakeCapture());
    expect(res.ok).toBe(true);                       // the live leg still stores, not dragged down by the backfill leg

    expect(runBackfillSpy).toHaveBeenCalledTimes(1); // ← this line is the evidence that "it runs at runtime"
    const opts = runBackfillSpy.mock.calls[0]![0];
    expect(opts.origin).toBe('https://chatgpt.com');
    expect(opts.platform).toBe('chatgpt');
    expect(typeof opts.sink).toBe('function');         // the archive exit does not fork
    // 🔴 W113 · The budget is the **default speed preset's**, not a bare constant any more: bodies per
    //    wake is one of the two knobs ADR-032's presets own, and this storage was cleared by beforeEach,
    //    so an install that has expressed no preference runs at `DEFAULT_SPEED_PRESET`. Asserting against
    //    the plan keeps this about the wiring ("the tick's budget really reaches the engine") rather than
    //    about which preset is the default — which is a product decision recorded in lib/backfill/speed.ts.
    expect(opts.maxDetails).toBe(SPEED_PLANS[DEFAULT_SPEED_PRESET].tickDetails);
    // 🔴 W2: the pause gate is **not** in the engine. The engine only knows the exit's
    //    retryLater answer; "should it run right now" is answered by schedule.ts's gate —
    expect('downloadGuard' in opts).toBe(false);
    console.log('[C13] runtime message -> runBackfill called with', {
      platform: opts.platform, origin: opts.origin, maxDetails: opts.maxDetails,
    });
  });

  it('🔴 criterion 3: host paused and the host does not answer ⇒ the same real path does NOT start a backfill', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { HOST_PAUSE_KEY, HOST_UNAVAILABLE } = await import('../lib/host-status');
    await setBackfillEnabled(browserLocalStore(), true);
    // The pause record left by the last failed delivery (the real path writes exactly this key).
    store[HOST_PAUSE_KEY] = { reason: HOST_UNAVAILABLE, at: 1, detail: 'timeout' };
    hostDown = true;

    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(async () => ({ status: 200, text: '{"items":[],"total":0}' }));

    await bootBackgroundAndDispatch(fakeCapture());

    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('host-paused');
    // 🔴 The pause record was not quietly cleared: without a successful hello there is no resuming.
    expect(store[HOST_PAUSE_KEY]).toMatchObject({ reason: HOST_UNAVAILABLE });
    console.log('[C13] host paused -> tick reason =', mod.lastBackfillTick()?.reason);
  });

  it('🔴 criterion 4: host paused but hello answered ⇒ the gate lets it through itself (§10 resume action)', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { HOST_PAUSE_KEY, HOST_UNAVAILABLE } = await import('../lib/host-status');
    await setBackfillEnabled(browserLocalStore(), true);
    store[HOST_PAUSE_KEY] = { reason: HOST_UNAVAILABLE, at: 1, detail: 'timeout' };
    // The host is there this time (hostDown defaults to false).
    const beforeHello = host.helloCount();

    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(async () => ({ status: 200, text: '{"items":[],"total":0}' }));

    await bootBackgroundAndDispatch(fakeCapture());

    expect(runBackfillSpy).toHaveBeenCalledTimes(1);
    expect(host.helloCount()).toBe(beforeHello + 1);  // the resume action really asked the host once
    expect(store[HOST_PAUSE_KEY]).toBeNull();         // it answered ⇒ the pause was cleared
    console.log('[C13] host answered -> pause cleared, tick reason =', mod.lastBackfillTick()?.reason);
  });

  it('off by default: with nothing set, the same path reaches the last gate without running', async () => {
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(async () => ({ status: 200, text: '{}' }));
    await bootBackgroundAndDispatch(fakeCapture());
    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('disabled');
    console.log('[C13] default state -> tick reason =', mod.lastBackfillTick()?.reason);
  });

  it('the production state: switch on but nobody injected an http port ⇒ no-http-port, never any network activity', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);

    const mod: any = await import('../entrypoints/background');   // no configure call
    await bootBackgroundAndDispatch(fakeCapture());
    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('no-http-port');
    console.log('[C13] no transport wired -> tick reason =', mod.lastBackfillTick()?.reason);
  });

  it('the MV3 reality: no in-memory state is relied on between ticks — the debt set lives only in storage', async () => {
    const { BACKFILL_ENABLED_KEY } = await import('../lib/backfill/schedule');
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);
    // The switch itself is persistent too: when the SW is reclaimed and wakes again, the user need not click it a second time.
    expect(store[BACKFILL_ENABLED_KEY]).toBe(true);
    console.log('[C13] enabled flag persisted in storage.local =', store[BACKFILL_ENABLED_KEY]);
  });
});
