/**
 * C29 · Reproducing what a real machine showed: the channel connected, the alarm ticking, the SW
 * waking — but **enumeration** never started.
 *
 * Reproducing the root cause:
 * 1. the user turns on "Backfill past conversations automatically" in the popup;
 * 2. the user opens https://chatgpt.com/, the content script checks in (cs-backfill-tab-hello),
 *    and rememberTab records the tab into cs_backfill_tabs_v1;
 * 3. but the user has not yet triggered any live conversation capture on the page (no
 *    chat-captured message), so rememberTarget was never called and cs_backfill_targets_v1 stays
 *    empty;
 * 4. at this point the popup finds transportWired true and renders "archiving — one debt is
 *    cleared automatically every 5 minutes";
 *    ⚠️ fixed in C30: the popup now also reads the target registry, so this step says "NOT
 *    running — there are no backfill targets at all";
 * 5. the cs-backfill-tick alarm fires, the SW wakes and runs runAlarmTick();
 * 6. runAlarmTick() reads loadTargets() as empty, takes the targets.length === 0 branch,
 *    returns { ran: false, reason: ... , report: null }, and no enumeration request is ever
 *    sent, so a debt set is never created in storage.
 *    ⚠️ C30 fixed two things: the outcome is named 'no-targets' (the port was never broken;
 *    there were no targets), and every tick writes "did nothing, and why" into
 *    cs_backfill_lasttick_v1.
 *    🔴 The behaviour (skip, do not enumerate) is unchanged character for character — the
 *    assertions in the lower half of this file remain valid as written.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { handleBackfillMessage } from '../lib/backfill/tab-port';
import type { BackfillRuntimeStatus } from '../lib/popup-view';

const ORIGIN = 'https://chatgpt.com';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
];

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
const alarmBook = new Map<string, { periodInMinutes?: number }>();
const liveTabs = new Map<number, string>();
const contentFetches: string[] = [];

function syntheticPageFetch(url: string) {
  contentFetches.push(url);
  const u = new URL(url);
  if (u.pathname === '/backend-api/conversations') {
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({
        items: IDS.map((id) => ({ id })),
        total: IDS.length,
      }),
    });
  }
  return Promise.resolve({
    status: 200,
    text: async () => JSON.stringify({
      mapping: { n1: { id: 'n1', message: { content: { parts: ['synthetic'] } } } },
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
    create(name: string, info: any) { alarmBook.set(name, info); },
    async clear(name: string) { return alarmBook.delete(name); },
    async get(name: string) { return alarmBook.get(name) ?? undefined; },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    async sendMessage(tabId: number, message: unknown) {
      const origin = liveTabs.get(tabId);
      if (!origin) throw new Error('Could not establish connection. Receiving end does not exist.');
      const pending = handleBackfillMessage(message, origin, syntheticPageFetch as any);
      if (!pending) return undefined;
      return await pending;
    },
  },
};

let runtimeNow = 1_700_000_000_000;
const runtimeClock = {
  now: () => runtimeNow,
  sleep: async (ms: number) => { runtimeNow += ms; },
};

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

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
  alarmListeners.length = 0;
  contentFetches.length = 0;
  alarmBook.clear();
  liveTabs.clear();
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C29-NOENUM · reproducing the real machine: opening a tab but never having a live conversation leaves the alarm waking without enumerating', () => {
  it('reproduces the full 12:55–13:12 record: the popup showed archiving while every alarm tick silently skipped because targets was empty', async () => {
    const { setBackfillEnabled, tickBlockReason } = await import('../lib/backfill/schedule');
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const { renderPopup, pickBackfillState, collectFailures } = await import('../lib/popup-view');

    // 1. ~12:55: the backfill switch is turned on in the popup
    await setBackfillEnabled(browserLocalStore(), true);

    // 2. 12:56: a ChatGPT page is opened
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();

    // The content script checks in (cs-backfill-tab-hello)
    const helloReply = await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);
    expect(helloReply).toEqual({ ok: true });

    // At this point storage holds only the switch and the tabs registry (matching the real machine's 298-byte snapshot)
    expect(store).toHaveProperty('cs_backfill_enabled_v1', true);
    expect(store).toHaveProperty('cs_backfill_tabs_v1');
    expect(store).not.toHaveProperty('cs_backfill_targets_v1');

    // 3. The popup queries the status: transportWired is true
    const statusReply: BackfillRuntimeStatus = await dispatch({ type: 'cs-backfill-status' });
    expect(statusReply.transportWired).toBe(true);

    // The popup computes the gates and renders.
    // 🔴 C30 fixed this: it **used to** pass only hasHttp — which is half the defect itself:
    //    answering "is there work to do" with "is the channel up". The popup now
    //    (entrypoints/popup/main.ts) reads the backfill target registry as well, so this
    //    section is written along the production path.
    const { loadTargets } = await import('../lib/backfill/alarm');
    const { browserLocalStore: storeFn } = await import('../lib/backfill/store');
    const targets = await loadTargets(storeFn());
    expect(targets).toEqual([]);      // the sequencing is unchanged: the registry really is empty

    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => true,
      isHostPaused: () => false,
      hasHttp: statusReply.transportWired,
      hasTargets: targets.length > 0,
    });
    // 🔴 Before C30 this was toBeNull() (all gates passed ⇒ the popup claimed it was archiving).
    //    Not one character of the sequencing changed; what changed is that it can now **say**
    expect(block).toBe('no-targets');

    const snapshot = await browserLocalSnapshot();
    const view = renderPopup({
      enabled: true,
      block,
      state: pickBackfillState(snapshot),
      target: null,
      failures: collectFailures(snapshot),
    });

    // 🔴 After C30: progress still says the same true thing (not one conversation enumerated),
    //    and running no longer contradicts it — it now says "NOT running" and spells out what
    expect(view.running).not.toContain('archiving');
    expect(view.running).toContain('NOT running');
    expect(view.progress).toBe(
      'Progress: not started yet — storage holds no debt set for this platform, which means nothing has been enumerated at all.',
    );

    // 4. 12:59:39 & 13:09:39: the alarm fires (alarmListeners)
    expect(alarmListeners.length).toBeGreaterThan(0);
    alarmListeners[0]!({ name: 'cs-backfill-tick' });
    await mod.backfillTickSettled();

    // 🔴 The core assertion: runAlarmTick skips because targets.length === 0.
    //    Before C30 this reported 'no-http-port' — the port was never broken, and that
    expect(mod.lastBackfillTick()).toEqual({
      ran: false,
      reason: 'no-targets',
      report: null,
    });
    // 🔴 C30 · this tick did nothing, but it **left a trace** (no longer a silent skip).
    const { loadLastTick } = await import('../lib/backfill/alarm');
    expect(await loadLastTick(storeFn())).toMatchObject({
      ran: false, reason: 'no-targets', targets: 0,
    });

    // 🔴 no enumeration request was sent
    expect(contentFetches).toEqual([]);

    // 🔴 no platform's debt ledger was created in storage
    const allKeys = Object.keys(store);
    expect(allKeys.some((k) => k.startsWith('cs_backfill_v1:'))).toBe(false);
  });
});
