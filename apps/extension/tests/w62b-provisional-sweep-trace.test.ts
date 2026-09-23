/**
 * W62b · **A trace written mid-tick may not report a concluded one.**
 *
 * ## The defect this file pins
 *
 * W62 moved the tick's trace *before* the recovery sweep, so that a no-tab tick
 * is not held open by liveness pings that cannot change its result. That first
 * write passed `null` for the sweep, and `null` is not a neutral placeholder in
 * this field: W51 defines it as **"this tick never swept"**, and W51's whole
 * reason for existing is that "we never looked" and "we looked and found
 * nothing" must stay two different facts. A tick that is *about to* sweep is
 * neither.
 *
 * It is not a theoretical window. The provisional record is the one that stays
 * if the worker is reclaimed while the sweep is in flight, if the sweep throws
 * after `tabs.query` has already pruned or registered rows, or if the tick's
 * final save fails — so the record a person reads afterwards can say the tick
 * never looked, about a tick that did look.
 *
 * ## What is asserted, and how it is made to fail
 *
 * The sweep is held open deliberately: `tabs.query` returns a promise this file
 * resolves by hand, so the tick is stopped *between* its provisional write and
 * its final one and the record can be read from storage at exactly the moment
 * the defect is about. That is the real entry point — the alarm listener, then
 * `backfillTickSettled()` — not `recordAlarmTick` called directly, because the
 * ordering of the two writes inside the tick is the property under test.
 *
 * Four facts are pinned, and the last three are there so the first cannot be
 * satisfied by collapsing the set:
 *
 *  · mid-sweep the record says **no outcome yet** — `isSweepNotConcluded`, and
 *    in particular not `null` ("never swept") and not either `looked` value;
 *  · a tick that finishes still writes its **concluded** sweep, with counts
 *    (W47/W51 semantics unchanged);
 *  · a tick that never reaches the port gate still writes `null`, so "never
 *    swept" remains reachable and the new state did not swallow it;
 *  · the popup prints the **unfinished** sentence for a provisional record and
 *    not the "that tick did nothing at all" one, while a concluded record still
 *    prints the latter.
 *
 * No network, no real browser profile, no real conversation data.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { popupText, renderPopup, NO_FAILURES, type PopupModel } from '../lib/popup-view';
import { handleBackfillMessage, type TabQueryRow } from '../lib/backfill/tab-port';

const ORIGIN = 'https://chatgpt.com';
const SCOPE = 'acct-fixture-w62b';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
];

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
const alarmBook = new Map<string, { periodInMinutes?: number }>();
/** Tabs whose content script will answer a ping / fetch. */
const liveTabs = new Map<number, string>();
const queriedTabs: TabQueryRow[] = [];
let queryCalls = 0;

/**
 * 🔴 The hold. When set, `tabs.query` waits on it before answering, which parks
 *    the tick inside its sweep — after the provisional write, before the final one.
 */
let sweepGate: Promise<void> | null = null;

function syntheticPageFetch(url: string) {
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
      account_id: SCOPE,
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
    async query() {
      queryCalls += 1;
      if (sweepGate) await sweepGate;
      return queriedTabs.map((t) => ({ ...t }));
    },
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
  mod.configureBackfillPace({ clock: runtimeClock, random: () => 0 });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

async function trace(): Promise<any> {
  const { loadLastTick } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  return await loadLastTick(browserLocalStore());
}

async function enabledWithTarget(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
  store['cs_backfill_targets_v1'] = [{ platform: 'chatgpt', origin: ORIGIN, scope: SCOPE, at: 1 }];
}

/** Let every microtask and one macrotask drain, so an async tick can reach its next await. */
async function settle(): Promise<void> {
  for (let i = 0; i < 50; i += 1) await Promise.resolve();
  await new Promise((r) => setTimeout(r, 0));
}

/**
 * 🔴 Drive the tick until it is parked inside the sweep, and no further.
 *
 * `tabs.query` is called only from inside the sweep, which runs *after* the
 * provisional write — so seeing it called proves the provisional record has
 * been written, without this test having to know where that write is.
 */
async function tickUntilSweeping(mod: any): Promise<void> {
  alarmListeners[0]!({ name: 'cs-backfill-tick' });
  for (let i = 0; i < 200 && queryCalls === 0; i += 1) await settle();
  expect(queryCalls, 'the tick reached its sweep — the provisional write is behind us').toBe(1);
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  alarmBook.clear();
  liveTabs.clear();
  queriedTabs.length = 0;
  queryCalls = 0;
  sweepGate = null;
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  (globalThis as any).indexedDB = new IDBFactory();
});

describe('W62b · the provisional trace says it is provisional', () => {
  it('🔴 a tick parked in its sweep does not report that it never swept', async () => {
    await enabledWithTarget();
    // Nothing answers: the tick is genuinely at `no-http-port`, which is the
    // branch W62 writes the provisional record from.
    queriedTabs.length = 0;

    let release!: () => void;
    sweepGate = new Promise<void>((r) => { release = r; });

    const mod = await bootBackground();
    await tickUntilSweeping(mod);

    // 🔴 The assertions that go red before the fix, and they are stated as the
    //    *behaviour* — what a reader of storage sees — not as "a symbol exists".
    //    Before the fix the provisional write passes `null`, and `null` in this
    //    field is W51's "this tick never swept": a false statement about a tick
    //    that is at this moment inside its sweep. This record is the one that
    //    survives if the tick never finishes, which is what makes it matter.
    const mid = await trace();
    expect(mid, 'the provisional record exists at all — this is not a "no trace yet" test').toBeTruthy();
    expect(mid.tabSweep, 'not null — that is "this tick never swept", and this tick is sweeping').not.toBeNull();
    expect(mid.tabSweep, 'not "we tried to look and could not"').not.toEqual({ looked: false });
    expect(
      'looked' in mid.tabSweep,
      'a provisional record carries no `looked` value at all — `looked` stays a boolean',
    ).toBe(false);

    release();
    sweepGate = null;
    await mod.backfillTickSettled();

    // The tick finished, so the record is now a conclusion — and it is the real
    // one, with the sweep's own counts (W47/W51 semantics unchanged).
    const { isSweepNotConcluded } = await import('../lib/backfill/alarm');
    const done = await trace();
    expect(isSweepNotConcluded(done.tabSweep)).toBe(false);
    expect(done.tabSweep).toEqual({
      looked: true,
      queried: 0,
      pruned: 0,
      pinged: 0,
      registered: 0,
      deferred: 0,
      crowded: 0,
    });
  });

  it('🔴 a tick that never reached the port gate still writes null, so the new state did not swallow it', async () => {
    // No target: the alarm writes `no-targets` and never asks whether a tab could
    // fetch. This is W51's "never swept", and it has to stay reachable — a fix
    // that made every record say "in progress" would be the same collapse in the
    // other direction.
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);

    const mod = await bootBackground();
    alarmListeners[0]!({ name: 'cs-backfill-tick' });
    await mod.backfillTickSettled();

    const rec = await trace();
    expect(rec?.reason).toBe('no-targets');
    expect(queryCalls, 'no registered target ⇒ no sweep, so nothing was held open').toBe(0);
    expect(rec?.tabSweep ?? null, 'absent/null: this tick never swept').toBeNull();
  });
});

describe('W62b · the popup does not read a provisional record as a verdict', () => {
  const AT = Date.parse('2026-09-23T09:15:00.000Z');

  function model(overrides: Partial<PopupModel> = {}): PopupModel {
    return { enabled: true, block: null, state: null, target: null, failures: NO_FAILURES, ...overrides };
  }

  it('🔴 a provisional trace prints "had not finished" — not "that tick did nothing at all"', async () => {
    // 🔴 Written as the literal, not via the exported constant, on purpose: this
    //    test is about what the *reader* does with the state, so it must be able
    //    to run against a build that has the state in storage and not the rule.
    //    (That the writer really produces this value is the first test's job.)
    // The record is the gate's answer so far with no outcome yet; `ran: false`
    // and `reason: 'no-http-port'` are what make the head sentence the *wrong*
    // one to lead with.
    const trace = {
      at: AT,
      ran: false,
      reason: 'no-http-port' as const,
      targets: 1,
      stopped: 'no-http-port' as const,
      halted: null,
      detail: null,
      tabSweep: { sweeping: true } as const,
    };

    const notes = renderPopup(model({ lastTick: trace })).notes.join('\n');
    expect(notes).toContain('had not finished');
    // 🔴 The sentence that must NOT be there. It is what the popup printed before
    //    this fix, and it is a claim about a tick that was still running.
    expect(notes, 'not the finished-skip sentence').not.toContain('did nothing at all');
    expect(popupText(renderPopup(model({ lastTick: trace })))).toContain('had not finished');
    // …and the constant the writer passes is that same value, named once.
    const { SWEEP_NOT_CONCLUDED } = await import('../lib/backfill/alarm');
    expect(SWEEP_NOT_CONCLUDED).toEqual({ sweeping: true });
  });

  it('a concluded trace still prints the skip sentence, so the two did not collapse into one', async () => {
    const trace = {
      at: AT,
      ran: false,
      reason: 'no-http-port' as const,
      targets: 1,
      stopped: 'no-http-port' as const,
      halted: null,
      detail: null,
      tabSweep: {
        looked: true,
        queried: 0,
        pruned: 0,
        pinged: 0,
        registered: 0,
        deferred: 0,
        crowded: 0,
      },
    };

    const notes = renderPopup(model({ lastTick: trace })).notes.join('\n');
    expect(notes).toContain('did nothing at all');
    expect(notes).not.toContain('had not finished');
  });
});
