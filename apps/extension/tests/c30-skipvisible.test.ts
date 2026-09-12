/**
 * C30 · A "skip" has to be observable.
 *
 * C29 reproduced the sequencing precisely: transport present, target absent ⇒ the alarm silently
 * skipped on every wake.
 * But C29 only **pinned the illusion** (asserting that the popup said "archiving" and that the
 * reason was 'no-http-port'), writing the wrong current state down as the expectation. This file
 * does the opposite:
 *
 *  1. 🔴 with transport but no target, the popup must **not** claim it is archiving;
 *  2. 🔴 why this tick did nothing must be **written into storage**, not left in memory;
 *  3. 🔴 the outcome must be named precisely enough to distinguish: no target ≠ no channel;
 *  4. 🔴 a healthy-path guard: a target really exists + the channel is live ⇒ it still says "archiving".
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
const changeListeners: Array<(c: any) => void> = [];
const contentFetches: string[] = [];

function syntheticPageFetch(url: string) {
  contentFetches.push(url);
  const u = new URL(url);
  if (u.pathname === '/backend-api/conversations') {
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({ items: IDS.map((id) => ({ id })), total: IDS.length }),
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

/** The equivalent of the popup's production path: it reads the same facts entrypoints/popup/main.ts does. */
async function popupNow(transportWired: boolean) {
  const { tickBlockReason } = await import('../lib/backfill/schedule');
  const { loadTargets, loadLastTick } = await import('../lib/backfill/alarm');
  const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
  const { renderPopup, pickBackfillState, collectFailures, popupText } =
    await import('../lib/popup-view');
  const s = browserLocalStore();
  const targets = await loadTargets(s);
  const block = await tickBlockReason({
    hasStore: s !== null,
    isEnabled: () => true,
    isHostPaused: () => false,
    hasHttp: transportWired,
    hasTargets: targets.length > 0,
  });
  const snapshot = await browserLocalSnapshot();
  const view = renderPopup({
    enabled: true,
    block,
    state: pickBackfillState(snapshot),
    target: null,
    failures: collectFailures(snapshot),
    lastTick: await loadLastTick(s),
  });
  return { block, view, text: popupText(view) };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  contentFetches.length = 0;
  alarmBook.clear();
  liveTabs.clear();
  changeListeners.length = 0;
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C30-SKIPVISIBLE · a skip must be observable and its reason must be named', () => {
  it('🔴 the counter-proof: with transport but no backfill target at all, the popup may not claim it is archiving', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');

    await setBackfillEnabled(browserLocalStore(), true);
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    // The real machine's snapshot: a switch, tabs, no targets.
    expect(store).toHaveProperty('cs_backfill_tabs_v1');
    expect(store).not.toHaveProperty('cs_backfill_targets_v1');

    const statusReply: BackfillRuntimeStatus = await dispatch({ type: 'cs-backfill-status' });
    expect(statusReply.transportWired).toBe(true);

    const { block, view, text } = await popupNow(statusReply.transportWired);
    console.log('[C30-EVIDENCE no targets]\n' + text);

    // 🔴 The core counter-proof: transport alone cannot carry the sentence "archiving".
    expect(view.running).not.toContain('Running: archiving');
    expect(text).not.toContain('Running: archiving');
    expect(view.running).toContain('NOT running');
    // 🔴 After reading it the user must know what they have to do to make it start.
    expect(view.missing).not.toBeNull();
    expect(view.missing!).toContain('archived LIVE');
    expect(block).toBe('no-targets');
  });

  it('🔴 every alarm skip must be written into storage, with the outcome named no-targets (not no-http-port)', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { BACKFILL_LAST_TICK_KEY, loadLastTick } = await import('../lib/backfill/alarm');

    await setBackfillEnabled(browserLocalStore(), true);
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    alarmListeners[0]!({ name: 'cs-backfill-tick' });
    await mod.backfillTickSettled();

    expect(mod.lastBackfillTick()).toEqual({ ran: false, reason: 'no-targets', report: null });
    expect(contentFetches).toEqual([]);            // the behaviour is unchanged: still not one enumerated

    // 🔴 (a): that this tick did nothing, and why, must stay in storage.
    expect(store).toHaveProperty(BACKFILL_LAST_TICK_KEY);
    const rec = await loadLastTick(browserLocalStore());
    expect(rec).toMatchObject({ ran: false, reason: 'no-targets', targets: 0 });
    expect(typeof rec!.at).toBe('number');

    // The popup has to say this record out loud ("the alarm really did wake and really did nothing").
    const { text } = await popupNow(true);
    expect(text).toContain('Alarm');
    expect(text).not.toContain('Running: archiving');
  });

  it('🔴 the two outcomes must be worded differently: no targets vs no channel', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);

    const noTargets = await popupNow(true);
    store['cs_backfill_targets_v1'] = [
      { platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1', at: 1 },
    ];
    const noPort = await popupNow(false);

    expect(noTargets.block).toBe('no-targets');
    expect(noPort.block).toBe('no-http-port');
    expect(noTargets.view.running).not.toBe(noPort.view.running);
    expect(noTargets.view.missing).not.toBe(noPort.view.missing);
    expect(noPort.view.missing!).toContain('page');
  });

  it('🟢 healthy-path guard: with a target really present and the channel live, the popup still says "archiving"', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);
    store['cs_backfill_targets_v1'] = [
      { platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1', at: 1 },
    ];
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    const statusReply: BackfillRuntimeStatus = await dispatch({ type: 'cs-backfill-status' });
    expect(statusReply.transportWired).toBe(true);

    const { block, view, text } = await popupNow(statusReply.transportWired);
    console.log('[C30-EVIDENCE healthy path]\n' + text);
    expect(block).toBeNull();
    expect(view.running).toContain('archiving');
    expect(view.missing).toBeNull();

    // The alarm tick really ran — so the record in storage must say ran too.
    alarmListeners[0]!({ name: 'cs-backfill-tick' });
    await mod.backfillTickSettled();
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    const { loadLastTick } = await import('../lib/backfill/alarm');
    expect(await loadLastTick(browserLocalStore())).toMatchObject({ ran: true, reason: 'ran' });
  });
});
