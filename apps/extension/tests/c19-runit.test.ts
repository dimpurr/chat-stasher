/**
 * C19 · Making the backfill leg really run on its own.
 *
 * Three things, none of them vague:
 *  1. the alarm (chrome.alarms): switch on ⇒ created; off ⇒ cleared. Still off by default.
 *  2. the http port is injected **in production code** — and fetching happens inside the user's
 *     logged-in page context (a same-origin fetch from the content script), with no new host permission.
 *  3. 🔴 BUG-3: the body-fetch minimum interval takes effect **across ticks** too.
 *
 * Zero real network and zero logged-in state throughout: fetch is a pure function in this file.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { runBackfill } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { DEFAULT_DETAIL_PACE, type Clock } from '../lib/backfill/pace';
import { handleBackfillMessage } from '../lib/backfill/tab-port';
import type { CapturedFetch } from '../lib/contract';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

const ORIGIN = 'https://chatgpt.com';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
  'c3333333-0000-4000-8000-000000000003',
];

/** A synthetic "server": a pure function that never touches the network. */
function syntheticPort() {
  const calls: string[] = [];
  return {
    calls,
    port: async (url: string) => {
      calls.push(url);
      const u = new URL(url);
      if (u.pathname === '/backend-api/conversations') {
        const offset = Number(u.searchParams.get('offset') ?? '0');
        return {
          status: 200,
          text: JSON.stringify({
            items: IDS.slice(offset).map((id) => ({ id })),
            total: IDS.length,
          }),
        };
      }
      const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
      return {
        status: 200,
        text: JSON.stringify({
          mapping: { n1: { id: 'n1', message: { content: { parts: [`synthetic ${id}`] } } } },
          current_node: 'n1',
          account_id: 'acct-fixture-1',
        }),
      };
    },
  };
}

/** A fake clock: sleep does not really wait, it only advances now — so "how long it waited" is an assertable number. */
function fakeClock(startMs = 1_700_000_000_000): Clock & { readonly at: () => number } {
  let now = startMs;
  return {
    now: () => now,
    sleep: async (ms: number) => { now += ms; },
    at: () => now,
  };
}

describe('C19 task 3 · BUG-3: the body-fetch minimum interval must take effect **across ticks**', () => {
  it('🔴 the second tick (a new runBackfill, the same storage) must make up the full 20 seconds', async () => {
    const store = memoryStore();
    const server = syntheticPort();
    const clock = fakeClock();

    const tick = () => runBackfill({
      platform: 'chatgpt',
      origin: ORIGIN,
      scope: 'acct-fixture-1',
      store,
      http: server.port,
      clock,
      maxDetails: 1,   // consistent with runtime: one tick clears exactly 1 debt
    });

    const r1 = await tick();
    const r2 = await tick();
    const r3 = await tick();

    console.log('[C19-3] tick1 detail wait sequence (ms) =', JSON.stringify(r1.paceTrace.detail));
    console.log('[C19-3] tick2 detail wait sequence (ms) =', JSON.stringify(r2.paceTrace.detail));
    console.log('[C19-3] tick3 detail wait sequence (ms) =', JSON.stringify(r3.paceTrace.detail));

    // The first has no "previous one" to speak of ⇒ it waits 0, which is correct.
    expect(r1.paceTrace.detail).toEqual([0]);
    // 🔴 The second and third are **new runBackfills**, but the moment of the last fetch is already persisted ⇒ the interval must be made up.
    expect(r2.paceTrace.detail).toEqual([DEFAULT_DETAIL_PACE.minIntervalMs]);
    expect(r3.paceTrace.detail).toEqual([DEFAULT_DETAIL_PACE.minIntervalMs]);

    // All three debts were cleared: 3 body fetches in total, 20 seconds apart.
    expect(store.data['cs_backfill_v1:chatgpt:acct-fixture-1']).toMatchObject({
      archived: IDS,
      pending: [],
    });
  });

  it('an alarm waking every 5 minutes is not held up by the interval (elapsed is far past 20 seconds)', async () => {
    const store = memoryStore();
    const server = syntheticPort();
    const clock = fakeClock();

    const tick = () => runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1',
      store, http: server.port, clock, maxDetails: 1,
    });

    await tick();
    await clock.sleep(5 * 60_000);   // the alarm period
    const r2 = await tick();
    console.log('[C19-3] tick2 detail wait sequence after 5 minutes (ms) =', JSON.stringify(r2.paceTrace.detail));
    expect(r2.paceTrace.detail).toEqual([0]);
  });

  it('the daily cap still holds (making the interval work must not break the daily cap that already worked)', async () => {
    const store = memoryStore();
    const server = syntheticPort();
    const clock = fakeClock();

    const r = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1',
      store, http: server.port, clock,
      pace: { enumerate: { minIntervalMs: 0, maxPerDay: null }, detail: { minIntervalMs: 20_000, maxPerDay: 2 } },
    });
    console.log('[C19-3] stop reason with a daily cap of 2 =', r.stopped, 'cleared =', r.archivedThisRun.length);
    expect(r.stopped).toBe('daily-cap');
    expect(r.archivedThisRun.length).toBe(2);
  });
});

// ===========================================================================
// Tasks 1 + 2 · the runtime: the alarm wakes, and the port is injected in production code
//
// 🔴 Not one assertion in this section may call runBackfill / tickBackfill directly.
//    Every case starts from a **real entry point**:
//      · the live leg: browser.runtime.onMessage receiving 'chat-captured'
//      · the alarm: browser.alarms.onAlarm firing
//    and only then does it look at whether the backfill leg really fetched anything.
//
// 🔴 The fetching also goes through the **real content-script handler**, handleBackfillMessage —
//    the fake tabs.sendMessage hands it the message, and it then calls a synthetic fetch.
//    That is, background → tabs.sendMessage → content script → same-origin fetch
//    is really walked in the test; only the final fetch is synthetic.
// ===========================================================================

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
let host: SyntheticHost;

/** The alarm register: "on ⇒ created / off ⇒ cleared" is asserted against it. */
const alarmBook = new Map<string, { periodInMinutes?: number }>();
const alarmLog: string[] = [];

/** The platform tabs that are "open" right now. Delete one and its ping naturally fails. */
const liveTabs = new Map<number, string>();
/** The URLs the content script sent on our behalf — the evidence that fetching really happens in the page context. */
const contentFetches: string[] = [];

function syntheticPageFetch(url: string) {
  contentFetches.push(url);
  const u = new URL(url);
  if (u.pathname === '/backend-api/conversations') {
    const offset = Number(u.searchParams.get('offset') ?? '0');
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({
        items: IDS.slice(offset).map((id) => ({ id })),
        total: IDS.length,
      }),
    });
  }
  const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
  return Promise.resolve({
    status: 200,
    text: async () => JSON.stringify({
      mapping: { n1: { id: 'n1', message: { content: { parts: [`synthetic ${id}`] } } } },
      current_node: 'n1',
      account_id: 'acct-fixture-1',
    }),
  });
}

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    // W2: the write-down channel = a synthetic native host.
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
    create(name: string, info: any) { alarmBook.set(name, info); alarmLog.push(`create ${name} ${JSON.stringify(info)}`); },
    async clear(name: string) { const had = alarmBook.delete(name); alarmLog.push(`clear ${name}`); return had; },
    async get(name: string) { return alarmBook.get(name) ?? undefined; },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    /**
     * 🔴 This is the "background → logged-in page" hop.
     * A tab not in liveTabs ⇒ throw, matching how "the tab is closed" behaves in a real browser.
     */
    async sendMessage(tabId: number, message: unknown) {
      const origin = liveTabs.get(tabId);
      if (!origin) throw new Error('Could not establish connection. Receiving end does not exist.');
      const pending = handleBackfillMessage(message, origin, syntheticPageFetch as any);
      if (!pending) return undefined;
      return await pending;
    },
  },
};

/** A fake clock through background's test seam, so the test does not really sleep 20 seconds. */
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

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

/** Dispatch a message the way a content script does; sender.tab.id is filled in by the "browser". */
async function dispatch(message: unknown, tabId?: number): Promise<any> {
  const sender = tabId === undefined ? { id: 's' } : { id: 's', tab: { id: tabId } };
  return await new Promise((resolve) => {
    const ret = runtimeListeners[0]!(message, sender, resolve);
    if (ret !== true) resolve(undefined);
  });
}

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  contentFetches.length = 0;
  alarmBook.clear();
  alarmLog.length = 0;
  liveTabs.clear();
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C19 task 1 · the alarm: on ⇒ created, off ⇒ cleared', () => {
  it('🔴 with the default (off), an SW start **does not** create an alarm; it only makes sure none exists', async () => {
    const { BACKFILL_DEFAULT_ENABLED } = await import('../lib/backfill/schedule');
    expect(BACKFILL_DEFAULT_ENABLED).toBe(false);   // 🔴 the default did not change a character

    await bootBackground();
    console.log('[C19-1] alarm operations after an SW start in the default state:', alarmLog, 'alarms now:', [...alarmBook.keys()]);
    expect([...alarmBook.keys()]).toEqual([]);
    expect(alarmLog.every((l) => l.startsWith('clear'))).toBe(true);
  });

  it('switch on ⇒ the alarm is created; off ⇒ it is cleared', async () => {
    const { syncBackfillAlarm, BACKFILL_ALARM_NAME, BACKFILL_ALARM_PERIOD_MINUTES } =
      await import('../lib/backfill/alarm');

    const created = await syncBackfillAlarm(fakeBrowser.alarms, true);
    console.log('[C19-1] switch on ->', created, 'alarms now:', [...alarmBook.entries()]);
    expect(created).toBe('created');
    expect(alarmBook.get(BACKFILL_ALARM_NAME)).toEqual({
      periodInMinutes: BACKFILL_ALARM_PERIOD_MINUTES,
    });

    // Syncing again must not restart the period from zero (every SW wake goes down this path).
    expect(await syncBackfillAlarm(fakeBrowser.alarms, true)).toBe('kept');

    const cleared = await syncBackfillAlarm(fakeBrowser.alarms, false);
    console.log('[C19-1] switch off ->', cleared, 'alarms now:', [...alarmBook.keys()]);
    expect(cleared).toBe('cleared');
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
  });

  it('when the switch is already on, an SW start restores the alarm (no second click after a restart)', async () => {
    await enableBackfill();
    await bootBackground();
    const mod = await import('../entrypoints/background');
    await (mod as any).syncAlarmWithSwitch();
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');
    console.log('[C19-1] switch persisted on + SW start -> alarms now:', [...alarmBook.keys()]);
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
  });
});

describe('C19 task 2 · the http port: really injected in production code', () => {
  it('🔴 the live leg\'s real message path: with no configureBackfillTransport call, it really does fetch bodies', async () => {
    await enableBackfill();
    liveTabs.set(42, ORIGIN);                 // the user has a logged-in chatgpt page open
    const mod = await bootBackground();
    // 🔴 mod.configureBackfillTransport is deliberately **not** called — this is the production state.

    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 42);
    await mod.backfillTickSettled();

    console.log('[C19-2] tick conclusion:', mod.lastBackfillTick()?.reason);
    console.log('[C19-2] URLs the content script sent from the page context:', contentFetches);
    console.log('[C19-2] names the host acked:', host.names());

    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    // One page enumerated + one body fetched, all through the content script's same-origin fetch.
    expect(contentFetches.some((u) => u.includes('/backend-api/conversations'))).toBe(true);
    expect(contentFetches.filter((u) => u.includes('/backend-api/conversation/')).length).toBe(1);
    const s: any = store['cs_backfill_v1:chatgpt:acct-fixture-1'];
    expect(s.archived.length).toBe(1);
    expect(s.pending.length).toBe(IDS.length - 1);
  });

  it('🔴 the alarm path: with no live capture at all, the alarm waking on its own still clears a debt', async () => {
    await enableBackfill();
    liveTabs.set(7, ORIGIN);
    const mod = await bootBackground();

    // First let the content script check in — when the alarm wakes, the SW has only this registry to work from.
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 7);
    // Target registration: left by one real capture (the minimum premise of "the user used it at least once").
    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 7);
    await mod.backfillTickSettled();
    const before = (store['cs_backfill_v1:chatgpt:acct-fixture-1'] as any).archived.length;

    // Now send no capture at all, and only let the alarm fire.
    contentFetches.length = 0;
    runtimeNow += 5 * 60_000;
    expect(alarmListeners.length).toBeGreaterThan(0);
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');
    alarmListeners[0]!({ name: BACKFILL_ALARM_NAME });
    await mod.backfillTickSettled();

    const after = (store['cs_backfill_v1:chatgpt:acct-fixture-1'] as any).archived.length;
    console.log('[C19-2] one alarm wake:', { reason: mod.lastBackfillTick()?.reason, before, after });
    console.log('[C19-2] URLs this alarm tick sent on our behalf:', contentFetches);
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    expect(after).toBe(before + 1);           // 🔴 with no live capture at all, one debt was still cleared
  });

  it('🔴 with no open platform page ⇒ it still returns no-http-port faithfully (never pretending to run)', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    // Check in first, then "close" the tab.
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 9);
    liveTabs.set(9, ORIGIN);
    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 9);
    await mod.backfillTickSettled();
    liveTabs.delete(9);                        // the user closed the page

    contentFetches.length = 0;
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');
    alarmListeners[0]!({ name: BACKFILL_ALARM_NAME });
    await mod.backfillTickSettled();

    console.log('[C19-2] the alarm\'s conclusion after the page closed:', mod.lastBackfillTick()?.reason, 'URLs sent:', contentFetches);
    expect(mod.lastBackfillTick()?.reason).toBe('no-http-port');
    expect(contentFetches).toEqual([]);        // not one request went out
    expect(await mod.backfillRuntimeStatus()).toEqual({
      transportWired: false,
      lastTickReason: 'no-http-port',
      // 🔴 A field added by C33: no live channel ⇒ it cannot answer "which platform" either ⇒ null.
      //    The channel decision itself did not change a character (it is still the result of the ping above).
      liveTarget: null,
    });
  });

  it('🔴 the content script sends only URLs that are **same-origin + in the platform table + one of the backfill leg\'s two paths**', async () => {
    const { isAllowedBackfillUrl, serveBackfillFetch } = await import('../lib/backfill/tab-port');
    const cases: Array<[string, boolean]> = [
      [`${ORIGIN}/backend-api/conversations?offset=0&limit=100`, true],
      [`${ORIGIN}/backend-api/conversation/abc`, true],
      [`${ORIGIN}/backend-api/accounts/check`, false],        // in the platform, but not one of the backfill leg\'s paths
      ['https://evil.example.com/steal', false],              // cross-origin
      ['https://claude.ai/backend-api/conversations', false], // another platform (cross-origin from this page)
      ['not a url', false],
    ];
    for (const [url, allowed] of cases) {
      expect([url, isAllowedBackfillUrl(url, ORIGIN)]).toEqual([url, allowed]);
    }
    const refused = await serveBackfillFetch('https://evil.example.com/steal', ORIGIN, syntheticPageFetch as any);
    console.log('[C19-2] replies for out-of-scope URLs:', refused);
    expect(refused).toEqual({ ok: false, error: 'refused: url is not a same-origin backfill endpoint' });
    expect(contentFetches).toEqual([]);        // 🔴 fetch was not even called
  });

  it('🔴 with the switch off, an open page still fetches nothing at all', async () => {
    liveTabs.set(42, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 42);
    await mod.backfillTickSettled();
    console.log('[C19-2] default (off) + a page open ->', mod.lastBackfillTick()?.reason, 'URLs sent:', contentFetches);
    expect(mod.lastBackfillTick()?.reason).toBe('disabled');
    expect(contentFetches).toEqual([]);
  });
});

// ===========================================================================
// Task 4 · the popup's three states must agree with reality
// ===========================================================================
describe('C19 task 4 · what the popup says agrees with the real state', () => {
  async function viewFor(opts: { enabled: boolean; transportWired: boolean }) {
    const { tickBlockReason, setBackfillEnabled, isBackfillEnabled } =
      await import('../lib/backfill/schedule');
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const { renderPopup, popupText, pickBackfillState, collectFailures } = await import('../lib/popup-view');
    await setBackfillEnabled(browserLocalStore(), opts.enabled);
    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => isBackfillEnabled(browserLocalStore()),
      isHostPaused: () => false,
      hasHttp: opts.transportWired,
    });
    const snapshot = await browserLocalSnapshot();
    const state = pickBackfillState(snapshot);
    const view = renderPopup({
      enabled: opts.enabled, block, state,
      target: state ? { platform: state.platform, scope: state.scope } : null,
      failures: collectFailures(snapshot),
    });
    return { view, text: popupText(view) };
  }

  it('state one · off: it says "the switch is off" and never mentions a port', async () => {
    const { view, text } = await viewFor({ enabled: false, transportWired: false });
    console.log('[C19-4 · off]\n' + text + '\n');
    expect(view.running).toContain('NOT running');
    expect(view.running).toContain('the switch is off');
    expect(text).not.toContain('Running: archiving');
  });

  it('🔴 state two · on but no usable page: it must still say "NOT running + what is missing"', async () => {
    const { view, text } = await viewFor({ enabled: true, transportWired: false });
    console.log('[C19-4 · on but no port]\n' + text + '\n');
    expect(view.running).toContain('NOT running');
    expect(view.missing).toContain('fetch channel');
    // 🔴 C18's honesty must not be broken.
    for (const forbidden of ['Running: archiving', 'backfilling now', 'estimated remaining']) {
      expect(text).not.toContain(forbidden);
    }
  });

  it('🔴 state three · really running: "Running: archiving" may appear only with every gate passed', async () => {
    const { view, text } = await viewFor({ enabled: true, transportWired: true });
    console.log('[C19-4 · really running]\n' + text + '\n');
    expect(view.running).toContain('Running: archiving');
    expect(view.missing).toBeNull();
    expect(view.running).toContain('5 minutes');
    expect(view.running).toContain('200');
    expect(text).not.toContain('estimated remaining');
  });

  it('🔴 the transportWired background reports to the popup is **pinged on the spot**, not a static flag', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 5);

    liveTabs.set(5, ORIGIN);
    const open = await mod.backfillRuntimeStatus();
    liveTabs.delete(5);
    const closed = await mod.backfillRuntimeStatus();
    console.log('[C19-4] page open ->', open, ' page closed ->', closed);
    expect(open.transportWired).toBe(true);
    expect(closed.transportWired).toBe(false);
  });
});
