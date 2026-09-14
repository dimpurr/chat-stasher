/**
 * W27 · **The tab registry stays alive: the ping has a budget, and the tab keeps
 * saying hello.**
 *
 * ## The two defects this file pins, both seen in a real browser
 *
 *  1. **A hung ping hung the sweep.** `pickLiveTab` awaited `tabs.sendMessage`
 *     with no timeout — the same property W7 fixed on the fetch channel — so one
 *     tab whose content-script context was gone did not merely fail: the loop
 *     never reached the *next* registered tab. `resolveHttpPort` returned nothing
 *     and the tick reported 'no-http-port' while a healthy tab of that origin sat
 *     in the registry.
 *  2. **Only a page load put a tab back.** Reloading the extension tears the
 *     content scripts down; the hello they had sent is not re-sent, so the
 *     registry went stale and stayed stale until the user reloaded that tab by
 *     hand.
 *
 * 🔴 What is asserted, and what is deliberately not:
 *   · The tests below are about **the registry and the message**, not about the
 *     backfill engine's own behaviour (W13/W7 own that).
 *   · Every timeout assertion uses **fake timers**, so "10 seconds" is a number
 *     this file advances rather than waits for.
 *   · The content-script half is driven through the **real entrypoint**
 *     (`entrypoints/dw-bridge.content.ts` → the real background message handler →
 *     the real registry in storage), not by calling `installTabHello` in isolation
 *     — a hello scheduler that the bridge did not call would pass the second kind
 *     of test and fix nothing.
 *   · No network, no real browser profile, no real conversation data anywhere.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import {
  BACKFILL_PING_TIMEOUT_MS,
  BACKFILL_TAB_HELLO_MESSAGE,
  BACKFILL_TAB_REPLY_TIMEOUT_MS,
  MAX_TAB_ENTRIES,
  forgetTab,
  loadTabs,
  pickLiveTab,
  rememberTab,
  TAB_PING_MISSES_BEFORE_FORGET,
} from '../lib/backfill/tab-port';
import {
  TAB_HELLO_MAX_INTERVAL_MS,
  TAB_HELLO_MIN_INTERVAL_MS,
  drawHelloDelayMs,
} from '../lib/backfill/tab-hello';
import { BACKFILL_TICK_DELAY_MIN_MINUTES } from '../lib/backfill/alarm';
import { memoryStore } from '../lib/backfill/store';
import {
  MAIN_PROBE_MESSAGE,
  MAIN_READY_MESSAGE,
  PAGE_HOOK_VERSION,
  type CapturedFetch,
} from '../lib/contract';

const ORIGIN = 'https://chatgpt.com';
const LIST_PATH = '/backend-api/conversations';
const TAB_ID = 77;

/** A ping that never settles: exactly what a torn-down content-script context looks like. */
const hungPing = (): Promise<unknown> => new Promise(() => { /* never settles */ });

/** Let every pending microtask run, so an awaited chain has really started before the clock moves. */
async function drain(): Promise<void> {
  for (let i = 0; i < 10; i += 1) await Promise.resolve();
}

/**
 * Let the real macrotask queue run.
 *
 * 🔴 `setImmediate` is deliberately **not** faked here (see `beforeEach`): the
 *    IndexedDB the backfill leg writes through (`fake-indexeddb`) schedules its
 *    work on `setImmediate`, so faking it would stall every transaction and no
 *    round would ever finish. Only the timers this file reasons about — the ping
 *    budget and the hello interval — are faked.
 */
async function settleIo(rounds = 20): Promise<void> {
  for (let i = 0; i < rounds; i += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

beforeEach(() => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'Date'],
  });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

// ===========================================================================
// W27-A · the liveness ping is bounded, and a bound is a miss, not an eviction
// ===========================================================================

describe('W27-A · a ping that never answers is bounded by its own budget', () => {
  it('🔴 the production budget is 10 s, and a full registry of hung tabs fits inside one tick gap', () => {
    // The ping is not the fetch: it carries no payload, so it must not inherit the
    // 90 s that exists to cover a 16 MiB body.
    expect(BACKFILL_PING_TIMEOUT_MS).toBe(10_000);
    expect(BACKFILL_PING_TIMEOUT_MS).toBeLessThan(BACKFILL_TAB_REPLY_TIMEOUT_MS);

    // The reason the number is safe: the worst case is *every* registered tab hung,
    // and that sweep still finishes before the next alarm can arrive.
    const worstCaseMs = MAX_TAB_ENTRIES * BACKFILL_PING_TIMEOUT_MS;
    const shortestTickGapMs = BACKFILL_TICK_DELAY_MIN_MINUTES * 60_000;
    console.log('[W27-A] worst-case sweep', worstCaseMs, 'ms vs shortest tick gap', shortestTickGapMs, 'ms');
    expect(worstCaseMs).toBeLessThan(shortestTickGapMs);
  });

  it('🔴 a tab that never answers is abandoned after the budget, and the next tab of that origin is used', async () => {
    const store = memoryStore();
    // Registry order is oldest-first in the array as written, newest first in
    // `rememberTab`'s output — so remember the healthy one first to make the hung
    // one the first candidate. The order is the point: the hung tab is tried first.
    await rememberTab(store, { tabId: 2, origin: ORIGIN, at: 2 });
    await rememberTab(store, { tabId: 1, origin: ORIGIN, at: 1 });
    expect((await loadTabs(store)).map((t) => t.tabId)).toEqual([1, 2]);

    const asked: number[] = [];
    const ping = (id: number): Promise<unknown> => {
      asked.push(id);
      return id === 1 ? hungPing() : Promise.resolve({ ok: true });
    };

    // 🔴 No fourth argument: this is the production call shape, so the budget being
    //    asserted is the shipped one.
    const picked = pickLiveTab(store, ORIGIN, ping);
    await drain();
    expect(asked, 'the hung tab is the first candidate').toEqual([1]);

    // One millisecond short of the budget it is still waiting — the timeout is what
    // ends this, not some other path.
    await vi.advanceTimersByTimeAsync(BACKFILL_PING_TIMEOUT_MS - 1);
    expect(await Promise.race([picked, Promise.resolve('still-pending')])).toBe('still-pending');

    await vi.advanceTimersByTimeAsync(1);
    const live = await picked;
    console.log('[W27-A] abandoned tab 1 after', BACKFILL_PING_TIMEOUT_MS, 'ms and picked', live?.tabId, '· pings asked:', asked);
    expect(live?.tabId, 'the sweep moved on to the next tab instead of hanging').toBe(2);
    expect(asked).toEqual([1, 2]);

    // And the sweep left no timer behind.
    expect(vi.getTimerCount()).toBe(0);
  });

  it('🔴 a timeout counts toward the existing 2-strike rule: two of them forget the tab', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: 1, origin: ORIGIN, at: 1 });

    const first = pickLiveTab(store, ORIGIN, hungPing);
    await drain();
    await vi.advanceTimersByTimeAsync(BACKFILL_PING_TIMEOUT_MS);
    expect(await first, 'a single timeout is not an eviction').toBeNull();

    const afterOne = await loadTabs(store);
    expect(afterOne.map((t) => t.tabId)).toEqual([1]);
    expect(afterOne[0]!.misses).toBe(1);
    expect(TAB_PING_MISSES_BEFORE_FORGET).toBe(2);

    const second = pickLiveTab(store, ORIGIN, hungPing);
    await drain();
    await vi.advanceTimersByTimeAsync(BACKFILL_PING_TIMEOUT_MS);
    expect(await second).toBeNull();
    expect(await loadTabs(store), 'two silent pings in a row do leave the list').toEqual([]);
  });
});

// ===========================================================================
// W27-B · the hello interval is jittered, and the jitter never shortens it
// ===========================================================================

describe('W27-B · the hello interval is jittered inside its documented band', () => {
  it('the floor and the ceiling are exact, and the floor is inside the shortest tick gap', () => {
    expect(TAB_HELLO_MIN_INTERVAL_MS).toBe(4 * 60_000);
    expect(TAB_HELLO_MAX_INTERVAL_MS).toBe(6 * 60_000);
    // The floor is what makes the registry heal *before* the tick that reads it:
    // the shortest gap between two backfill ticks is 5 minutes.
    expect(TAB_HELLO_MIN_INTERVAL_MS).toBeLessThan(BACKFILL_TICK_DELAY_MIN_MINUTES * 60_000);
    expect(TAB_HELLO_MAX_INTERVAL_MS).toBeGreaterThan(BACKFILL_TICK_DELAY_MIN_MINUTES * 60_000);
  });

  it('the two boundaries are pinned, and no hostile draw can go below the floor', () => {
    expect(drawHelloDelayMs(() => 0)).toBe(TAB_HELLO_MIN_INTERVAL_MS);
    expect(drawHelloDelayMs(() => 1)).toBe(TAB_HELLO_MAX_INTERVAL_MS);
    for (const bad of [-5, 1.5, Number.NaN, Number.POSITIVE_INFINITY]) {
      const drawn = drawHelloDelayMs(() => bad);
      expect(drawn, `a draw of ${bad} must stay in the band`).toBeGreaterThanOrEqual(TAB_HELLO_MIN_INTERVAL_MS);
      expect(drawn).toBeLessThanOrEqual(TAB_HELLO_MAX_INTERVAL_MS);
    }
  });

  it('a run of draws stays inside the band and is not a metronome', () => {
    let s = 20260914;
    const stream = (): number => {
      s = (s * 1103515245 + 12345) % 2147483648;
      return s / 2147483648;
    };
    const drawn: number[] = [];
    for (let i = 0; i < 200; i += 1) drawn.push(drawHelloDelayMs(stream));
    console.log('[W27-B] draws: min', Math.min(...drawn), '· max', Math.max(...drawn), '· first three', drawn.slice(0, 3));
    expect(Math.min(...drawn)).toBeGreaterThanOrEqual(TAB_HELLO_MIN_INTERVAL_MS);
    expect(Math.max(...drawn)).toBeLessThanOrEqual(TAB_HELLO_MAX_INTERVAL_MS);
    expect(new Set(drawn).size, 'consecutive intervals are not all the same').toBeGreaterThan(150);
  });
});

// ===========================================================================
// W27-C · the real content script keeps the registry alive
//
// 🔴 Loaded and run for real: `entrypoints/dw-bridge.content.ts` → the same
//    `browser.runtime.sendMessage` a page uses → background's registered
//    listener → the registry in `storage.local`. Only the browser APIs are fake.
// ===========================================================================

const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
/** Every hello the content script actually put on the wire. */
const hellos: any[] = [];
const store: Record<string, unknown> = {};
let host: SyntheticHost;

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() { /* no badge refresh in this file */ } },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    /**
     * The page's own sendMessage. It reaches the listeners the way the browser
     * does — the first one that returns `true` owns the reply via sendResponse —
     * and background's listener (registered first) is the one that answers a hello.
     */
    sendMessage(msg: any): Promise<any> {
      if (msg?.type === BACKFILL_TAB_HELLO_MESSAGE) hellos.push(msg);
      return new Promise((resolve) => {
        for (const fn of runtimeListeners) {
          const ret = fn(msg, { tab: { id: TAB_ID } }, resolve);
          if (ret === true) return;              // async sendResponse owns the reply
          if (ret !== undefined) { resolve(ret); return; }
        }
        resolve(undefined);
      });
    },
    sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in store ? store[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
  alarms: {
    create() {},
    async clear() { return false; },
    async get() { return undefined; },
    onAlarm: { addListener() {} },
  },
  tabs: {
    // The bridge's periodic hello never fetches anything; nothing in this describe
    // goes near the fetch channel.
    async sendMessage() { throw new Error('the fetch channel is not exercised in W27-C'); },
  },
};

interface FakePage {
  win: any;
  doc: any;
  /** Flip the tab's visibility and fire the event, exactly as a tab switch would. */
  setVisibility(state: 'visible' | 'hidden'): void;
  injected: string[];
}

/**
 * The page the bridge thinks it is in. It answers the MAIN-world readiness probe
 * (so the fallback path is never entered and this file stays about the hello), and
 * it records inline scripts so an accidental fallback injection would be visible.
 */
function makeFakePage(): FakePage {
  const messageListeners: Array<(e: any) => void> = [];
  const visibilityListeners: Array<() => void> = [];
  const injected: string[] = [];
  const win: any = {
    location: { origin: ORIGIN, href: `${ORIGIN}/` },
    addEventListener(type: string, fn: any) {
      if (type === 'message') messageListeners.push(fn);
    },
    postMessage(data: any, targetOrigin: string) {
      if (targetOrigin !== ORIGIN) return;
      if (data?.type !== MAIN_PROBE_MESSAGE) return;
      // What lib/page-hook.ts answers from the page world: the token, echoed.
      for (const fn of [...messageListeners]) {
        fn({
          source: win,
          origin: ORIGIN,
          data: { type: MAIN_READY_MESSAGE, version: PAGE_HOOK_VERSION, token: data.token },
        });
      }
    },
  };
  const doc: any = {
    visibilityState: 'visible',
    addEventListener(type: string, fn: any) {
      if (type === 'visibilitychange') visibilityListeners.push(fn);
    },
    documentElement: {
      appendChild(script: { textContent: string }) { injected.push(script.textContent); },
    },
    createElement() { return { textContent: '', remove() { /* detached again */ } }; },
  };
  return {
    win,
    doc,
    injected,
    setVisibility(state) {
      doc.visibilityState = state;
      for (const fn of [...visibilityListeners]) fn();
    },
  };
}

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

async function loadBridge(page: FakePage): Promise<void> {
  vi.stubGlobal('window', page.win);
  vi.stubGlobal('document', page.doc);
  vi.stubGlobal('defineContentScript', (cfg: any) => cfg);
  const mod: any = await import('../entrypoints/dw-bridge.content');
  mod.default.main();
}

describe('W27-C · the real bridge re-announces its tab', () => {
  beforeEach(async () => {
    for (const k of Object.keys(store)) delete store[k];
    runtimeListeners.length = 0;
    hellos.length = 0;
    host = createSyntheticHost({ up: true });
    (globalThis as any).indexedDB = new IDBFactory();
    vi.stubGlobal('browser', withI18n(fakeBrowser));
    vi.stubGlobal('chrome', fakeBrowser);
    vi.stubGlobal('defineBackground', (cb: any) => cb);
    vi.stubGlobal('defineContentScript', (cfg: any) => cfg);
    vi.resetModules();
  });

  it('🔴 a periodic hello puts a tab back that the registry lost, and repeats do not duplicate it', async () => {
    await bootBackground();
    const page = makeFakePage();
    await loadBridge(page);
    await drain();

    const { browserLocalStore } = await import('../lib/backfill/store');
    const registry = browserLocalStore();
    if (!registry) throw new Error('this suite runs against a fake browser with storage.local; it must not be null');

    // The load-time hello, unchanged: one message, and the tab is registered.
    console.log('[W27-C] hellos after load:', hellos.length, '· message:', JSON.stringify(hellos[0]));
    expect(hellos).toHaveLength(1);
    expect(hellos[0]).toEqual({ type: BACKFILL_TAB_HELLO_MESSAGE, origin: ORIGIN });
    expect((await loadTabs(registry)).map((t) => t.tabId)).toEqual([TAB_ID]);
    expect(page.injected, 'the readiness probe was answered, so nothing was injected').toEqual([]);

    // 🔴 The defect: the extension (or its service worker) restarted, the content
    //    script's registration is gone, and the page is still open. Nothing the
    //    user does brings it back — only this timer.
    const atOfFirstHello = (await loadTabs(registry))[0]!.at;
    await forgetTab(registry, TAB_ID);
    expect(await loadTabs(registry)).toEqual([]);

    await vi.advanceTimersByTimeAsync(TAB_HELLO_MAX_INTERVAL_MS);
    await drain();
    const afterTick = await loadTabs(registry);
    console.log('[W27-C] registry after one interval:', afterTick.length, 'row(s)');
    expect(hellos.length, 'a second hello went out on its own').toBe(2);
    expect(afterTick.map((t) => t.tabId), 'the tab is registered again').toEqual([TAB_ID]);
    // The row is fresh, not the old timestamp resurrected: a hello is evidence of
    // life *now*.
    expect(afterTick[0]!.at).toBeGreaterThan(atOfFirstHello);

    // 🔴 And repeats are idempotent: background dedups by tab id, so a long-lived
    //    page adds no rows however many times it checks in.
    const counts = new Set<number>();
    for (let i = 0; i < 5; i += 1) {
      await vi.advanceTimersByTimeAsync(TAB_HELLO_MAX_INTERVAL_MS);
      await drain();
      counts.add((await loadTabs(registry)).length);
    }
    console.log('[W27-C] hellos sent in total:', hellos.length, '· registry sizes seen:', [...counts]);
    expect(hellos.length).toBeGreaterThanOrEqual(7);
    expect([...counts]).toEqual([1]);
  });

  it('a tab that becomes visible checks in at once, and going hidden does not', async () => {
    await bootBackground();
    const page = makeFakePage();
    await loadBridge(page);
    await drain();
    expect(hellos).toHaveLength(1);

    const { browserLocalStore } = await import('../lib/backfill/store');
    const registry = browserLocalStore()!;

    // Hidden: nothing changes and nothing is sent. Hiding is not news this registry
    // has any use for, and a background tab must not be made busier by it.
    page.setVisibility('hidden');
    await drain();
    expect(hellos).toHaveLength(1);

    // Visible again: one hello, right now — this is what heals a registry that went
    // stale while the tab sat in the background.
    await forgetTab(registry, TAB_ID);
    page.setVisibility('visible');
    await drain();
    console.log('[W27-C] hellos after becoming visible:', hellos.length);
    expect(hellos).toHaveLength(2);
    expect((await loadTabs(registry)).map((t) => t.tabId)).toEqual([TAB_ID]);
  });
});

// ===========================================================================
// W27-D · the real entry point: a hung tab does not cost the tick its channel
//
// 🔴 Nothing below calls pickLiveTab or resolveHttpPort. A real `chat-captured`
//    message arrives at background's registered listener **with no sender tab**
//    (the case where the registry is what decides), and the round runs.
// ===========================================================================

describe('W27-D · a tick whose first registered tab hangs still fetches — through the next one', () => {
  /** Which tab was asked what, in order. */
  const calls: Array<{ tabId: number; type: string }> = [];

  const browser: any = {
    runtime: {
      id: 'mock-extension-id',
      onStartup: { addListener() {} },
      onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
      sendMessage: (m: unknown) => fakeBrowser.runtime.sendMessage(m),
      sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
    },
    storage: fakeBrowser.storage,
    action: fakeBrowser.action,
    alarms: fakeBrowser.alarms,
    tabs: {
      sendMessage(tabId: number, message: any): Promise<unknown> {
        calls.push({ tabId, type: message?.type });
        if (message?.type === 'cs-backfill-ping') {
          // Tab 1 is the wedged one: the message channel never settles.
          return tabId === 1 ? hungPing() : Promise.resolve({ ok: true, origin: ORIGIN });
        }
        if (message?.type === 'cs-backfill-fetch') {
          const path = new URL((message as { url: string }).url).pathname;
          calls.push({ tabId, type: `fetch:${path}` });
          return Promise.resolve({ ok: true, status: 200, text: JSON.stringify({ items: [], total: 0 }) });
        }
        return Promise.resolve(undefined);
      },
    },
  };

  let runtimeNow = 1_700_000_000_000;
  const runtimeClock = {
    now: () => runtimeNow,
    sleep: async (ms: number) => { runtimeNow += ms; },
  };

  function liveCapture(): CapturedFetch {
    const sid = 'aaaaaaaa-1111-2222-3333-444444444444';
    return {
      url: `${ORIGIN}/backend-api/conversation/${sid}`,
      method: 'GET',
      status: 200,
      text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-fixture-1' }),
      pageUrl: `${ORIGIN}/c/${sid}`,
      capturedAt: 1_700_000_000_000,
    };
  }

  /** Deliver a message to background exactly as the runtime does. `tabId` omitted ⇒ no sender tab. */
  async function dispatch(message: unknown, tabId?: number): Promise<any> {
    const sender = tabId === undefined ? { id: 's' } : { id: 's', tab: { id: tabId } };
    return await new Promise((resolve) => {
      const ret = runtimeListeners[0]!(message, sender, resolve);
      if (ret !== true) resolve(undefined);
    });
  }

  beforeEach(async () => {
    for (const k of Object.keys(store)) delete store[k];
    runtimeListeners.length = 0;
    calls.length = 0;
    host = createSyntheticHost({ up: true });
    (globalThis as any).indexedDB = new IDBFactory();
    runtimeNow = 1_700_000_000_000;
    vi.stubGlobal('browser', withI18n(browser));
    vi.stubGlobal('chrome', browser);
    vi.stubGlobal('defineBackground', (cb: any) => cb);
    vi.resetModules();
    const { resetTickLockForTest } = await import('../lib/backfill/schedule');
    resetTickLockForTest();
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);
  });

  it('🔴 the hung tab is given up on, the next tab is pinged, and the list fetch goes through it', async () => {
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillPace({ clock: runtimeClock });
    await mod.default();

    const { browserLocalStore } = await import('../lib/backfill/store');
    const registry = browserLocalStore()!;
    await rememberTab(registry, { tabId: 2, origin: ORIGIN, at: 2 });
    await rememberTab(registry, { tabId: 1, origin: ORIGIN, at: 1 });

    // No sender tab: the registry is the only thing that can answer "who fetches".
    const dispatched = dispatch({ type: 'chat-captured', payload: liveCapture() });
    await settleIo();
    await drain();
    await dispatched;
    await drain();
    await settleIo();
    console.log('[W27-D] after the capture, calls so far:', JSON.stringify(calls));
    // The hung tab has been asked and has not answered; nothing else has happened
    // yet — the round is sitting inside the ping, which is where it used to stay.
    expect(calls, 'the round is waiting on the hung tab').toEqual([{ tabId: 1, type: 'cs-backfill-ping' }]);

    // The round is now waiting on tab 1's ping. Before W27 this is where it stayed
    // forever: the message channel never settles without a timeout.
    await vi.advanceTimersByTimeAsync(BACKFILL_PING_TIMEOUT_MS + 1);
    await drain();
    await settleIo();
    await vi.advanceTimersByTimeAsync(1);
    await drain();
    await settleIo();
    await mod.backfillTickSettled();

    const tick = mod.lastBackfillTick();
    console.log('[W27-D] tick reason:', tick?.reason, '· stopped:', tick?.report?.stopped, '· calls:', JSON.stringify(calls));

    expect(tick?.reason).not.toBe('no-http-port');
    expect(tick?.report?.stopped).not.toBe('halted');
    // Tab 1 was pinged and abandoned; tab 2 was pinged and then really used.
    expect(calls[0]).toEqual({ tabId: 1, type: 'cs-backfill-ping' });
    expect(calls[1]).toEqual({ tabId: 2, type: 'cs-backfill-ping' });
    expect(calls).toContainEqual({ tabId: 2, type: `fetch:${LIST_PATH}` });
    expect(calls.some((c) => c.tabId === 1 && c.type.startsWith('fetch:'))).toBe(false);

    // The silence cost tab 1 one strike, not its place.
    expect((await loadTabs(registry)).map((t) => t.tabId)).toEqual([1, 2]);
    expect((await loadTabs(registry))[0]!.misses).toBe(1);
  });
});
