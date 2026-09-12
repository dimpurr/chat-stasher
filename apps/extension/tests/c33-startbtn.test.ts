/**
 * C33-STARTBTN · "Start backfilling this platform" — the registration entry point for **explicit informed consent**.
 *
 * ## The defect as seen on a real machine
 * A new user turns the switch on and has a supported platform's page open (the fetch channel is
 * up), but backfill **never starts**: the only place a backfill target is registered is
 * kickBackfill, and that requires **one live conversation to have been captured first**.
 *
 * 🔴 That limitation is **deliberate design** and this file does not change a character of it
 *    (lib/backfill/alarm.ts:80-87: when the alarm wakes the SW is brand new with no tab and no
 *      account, and the only information that does not have to be invented is the one the live
 *      leg is holding; docs/privacy.md — the extension has no host permissions).
 * ⇒ What this change adds is **one more registration entry point**: the user clicks once in the
 *   popup and says outright "backfill this platform". The "we do not guess" principle is
 *   unchanged — what changed is that there is now a "the user said so" source as well.
 *
 * ## Four things are pinned here
 *  1. 🔴 the counter-proof: with a channel and no target, clicking the button puts a target into cs_backfill_targets_v1;
 *  2. with a target already present the button **does not appear** (otherwise it is permanent noise);
 *  3. with no channel the button **does not appear** (clicking it would do nothing);
 *  4. after registration the alarm tick **no longer reports no-targets** — walked through with the real runAlarmTick.
 *
 * Zero real network and zero logged-in state throughout: only a fake browser.* and a synthetic fetch.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { readFileSync } from 'node:fs';
import { handleBackfillMessage } from '../lib/backfill/tab-port';

const ORIGIN = 'https://chatgpt.com';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
];

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const downloadCalls: Array<{ id: number; filename: string }> = [];
const changeListeners: Array<(d: any) => void> = [];
const alarmBook = new Map<string, unknown>();
/** The platform tabs that are "open" right now. Delete one and its ping naturally fails. */
const liveTabs = new Map<number, string>();
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
    onAlarm: { addListener() {} },
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

/** A fake clock through background's test seam, so the test does not really sleep 20 seconds. */
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

/** Dispatch a message the way the popup does (the popup has no sender.tab). */
async function dispatch(message: unknown): Promise<any> {
  return await new Promise((resolve) => {
    const ret = runtimeListeners[0]!(message, { id: 'popup' }, resolve);
    if (ret !== true) resolve(undefined);
  });
}

/** A content script checking in: this is what "there is an open platform page" looks like in production. */
async function tabHello(tabId: number): Promise<void> {
  const { BACKFILL_TAB_HELLO_MESSAGE } = await import('../lib/backfill/tab-port');
  liveTabs.set(tabId, ORIGIN);
  await new Promise((resolve) => {
    const ret = runtimeListeners[0]!(
      { type: BACKFILL_TAB_HELLO_MESSAGE, origin: ORIGIN },
      { id: 'cs', tab: { id: tabId } },
      resolve,
    );
    if (ret !== true) resolve(undefined);
  });
}

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

/**
 * The equivalent of the popup's production path: it reads the same facts,
 * the same tickBlockReason and the same renderPopup entrypoints/popup/main.ts does. No second decision is written in the test.
 */
async function popupNow(mod: any) {
  const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
  const { tickBlockReason, isBackfillEnabled } = await import('../lib/backfill/schedule');
  const { loadTargets, loadLastTick } = await import('../lib/backfill/alarm');
  const { renderPopup, popupText, collectFailures, pickBackfillState } =
    await import('../lib/popup-view');

  const s = browserLocalStore();
  const runtime = await mod.backfillRuntimeStatus();
  const enabled = await isBackfillEnabled(s);
  const targets = await loadTargets(s);
  const snapshot = await browserLocalSnapshot();
  const block = await tickBlockReason({
    hasStore: s !== null,
    isEnabled: () => enabled,
    isHostPaused: () => false,
    hasHttp: runtime.transportWired,
    hasTargets: targets.length > 0,
  });
  const view = renderPopup({
    enabled,
    block,
    state: pickBackfillState(snapshot),
    target: null,
    failures: collectFailures(snapshot),
    lastTick: await loadLastTick(s),
    liveTarget: runtime.liveTarget ?? null,
    targetCount: targets.length,
  });
  return { runtime, targets, block, view, text: popupText(view) };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  downloadCalls.length = 0;
  changeListeners.length = 0;
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

describe('C33-STARTBTN · the registration entry point for explicit informed consent', () => {
  it('🔴 the counter-proof: channel, no target ⇒ after clicking the button a target appears in cs_backfill_targets_v1', async () => {
    const { BACKFILL_TARGETS_KEY } = await import('../lib/backfill/alarm');
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');

    await enableBackfill();
    const mod = await bootBackground();
    await tabHello(7);                       // the user has a logged-in chatgpt page open

    const before = await popupNow(mod);
    console.log('[C33-EVIDENCE before the click]\n' + before.text);
    // The premise: the channel is up and the targets are empty — exactly the deadlock seen on a real machine.
    expect(before.runtime.transportWired).toBe(true);
    expect(before.targets).toEqual([]);
    expect(before.block).toBe('no-targets');
    expect(store[BACKFILL_TARGETS_KEY]).toBeUndefined();
    // 🔴 The button **must appear**, or the user can never break that deadlock.
    expect(before.view.startBackfill.visible).toBe(true);
    console.log('[C33-EVIDENCE button wording]', before.view.startBackfill.label);

    // === The click === (through the real runtime.onMessage entry point, not by calling an internal function)
    const reply = await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });
    console.log('[C33-EVIDENCE click reply]', JSON.stringify(reply));
    expect(reply?.ok).toBe(true);

    const written = store[BACKFILL_TARGETS_KEY] as any[];
    console.log('[C33-EVIDENCE registry]', JSON.stringify(written));
    expect(Array.isArray(written)).toBe(true);
    expect(written.length).toBe(1);
    expect(written[0]).toMatchObject({ platform: 'chatgpt', origin: ORIGIN });
    expect(typeof written[0].scope).toBe('string');
    expect(written[0].scope.length).toBeGreaterThan(0);

    // 🔴 One immediate repaint must show the change; the user should not have to refresh by hand.
    const after = await popupNow(mod);
    console.log('[C33-EVIDENCE after the click]\n' + after.text);
    expect(after.targets.length).toBe(1);
    expect(after.block).not.toBe('no-targets');
    expect(after.view.startBackfill.visible).toBe(false);
  });

  it('🟢 guard one: with a target already present the button **does not appear** (it must not become permanent noise)', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    await tabHello(7);
    const { rememberTarget } = await import('../lib/backfill/alarm');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await rememberTarget(browserLocalStore(), {
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1', at: 1,
    });

    const now = await popupNow(mod);
    console.log('[C33-EVIDENCE a target already exists]\n' + now.text);
    expect(now.runtime.transportWired).toBe(true);
    expect(now.targets.length).toBe(1);
    expect(now.view.startBackfill.visible).toBe(false);
    expect(now.text).not.toContain(now.view.startBackfill.label);
  });

  it('🟢 guard two: with no channel the button **does not appear** (clicking it would do nothing)', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    // Not one platform page is open ⇒ no usable fetch channel.

    const now = await popupNow(mod);
    console.log('[C33-EVIDENCE no channel]\n' + now.text);
    expect(now.runtime.transportWired).toBe(false);
    expect(now.runtime.liveTarget ?? null).toBeNull();
    expect(now.targets).toEqual([]);
    expect(now.view.startBackfill.visible).toBe(false);
    expect(now.text).not.toContain(now.view.startBackfill.label);
  });

  it('🔴 after registration the alarm tick no longer reports no-targets (through the real runAlarmTick)', async () => {
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');
    await enableBackfill();
    const mod = await bootBackground();
    await tabHello(7);

    // Before registration: the alarm wakes with nothing to do, and the reason is no-targets.
    const dry = await mod.runAlarmTick();
    console.log('[C33-EVIDENCE alarm tick before registration]', dry.reason);
    expect(dry.reason).toBe('no-targets');

    await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });

    const wet = await mod.runAlarmTick();
    console.log('[C33-EVIDENCE alarm tick after registration]', wet.reason,
      '| URLs the content script sent on our behalf:', contentFetches,
      '| written down:', downloadCalls.filter((d) => !d.filename.endsWith('.part')).map((d) => d.filename));
    expect(wet.reason).not.toBe('no-targets');
    // The channel really is up too ⇒ this tick really ran, rather than the stuck-ness being reworded.
    expect(wet.reason).toBe('ran');
    expect(contentFetches.length).toBeGreaterThan(0);
  });

  it('wiring guard: the popup HTML really has this button, and main.ts really attaches it and repaints after the click', async () => {
    const html = readFileSync(new URL('../entrypoints/popup/index.html', import.meta.url), 'utf8');
    const main = readFileSync(new URL('../entrypoints/popup/main.ts', import.meta.url), 'utf8');
    // 🔴 What gets installed on a real machine is the build output: with no such element in the HTML, the button can never appear on screen.
    expect(html).toContain('id="start-backfill"');
    expect(main).toContain("getElementById('start-backfill')");
    // It must repaint itself after the click — "immediate" may not depend on the user closing and reopening by hand.
    expect(main).toMatch(/onStartBackfill[\s\S]*refresh\(\)/);
  });
});
