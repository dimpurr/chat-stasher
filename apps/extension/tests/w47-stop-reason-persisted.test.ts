/**
 * W47 · **A tick that did nothing must say why, in storage — and so must a report
 * we refused.**
 *
 * ## The defect, and the day it cost
 *
 * `BackfillTickRecord` declares `stopped` and `halted`, and the comment above them
 * says the point is to turn "it ran and nothing moved" into a named fact. W36
 * filled them from the **run's** report — which exists only on a tick that got
 * past every gate. On a tick blocked at a gate they were `null`, and the record
 * read `{ran: false, reason: 'no-http-port', targets: 1, stopped: null, halted:
 * null}`: a person asking "why is nothing moving" gets the gate's name and no
 * answer to what the leg itself concluded.
 *
 * The same hole sat one layer down. `runBackfill` returns an `openLedger` refusal
 * as a report, and the ledger is written **only** on the normal path — so with the
 * tabs closed (the ordinary state of a laptop) nothing ever opened the ledger, the
 * unreadable record was named by nothing, and `state.halted` being absent from
 * storage did not mean no refusal had happened. And one layer down again, on the
 * hook path: a report background **received and declined** left no record and no
 * console line, so a page reporting `hook-was-replaced` every 5 seconds produced
 * the same state as a page reporting nothing. That is the day this file is about:
 * fourteen hypotheses, each refuted by the next measurement, because the only
 * evidence the product produced for these paths was a `console.warn` in an MV3
 * worker that is reclaimed between events.
 *
 * ## What is asserted here, and why each one is a different claim
 *
 *  1. **Every path that writes a trace names how it ended.** A tick blocked before
 *     any request carries its own named outcome in `stopped`; a run that halts
 *     carries the run's. Driven through the alarm's real entry point, not a
 *     hand-built record.
 *  2. **A refusal reaches storage without the run.** With an unreadable record in
 *     `storage.local` and **no platform tab open**, the trace names it — and the
 *     record it refused is still there, byte for byte. The engine's own route to
 *     the same refusal (a live tab, so the run really happens) is asserted beside
 *     it, because the two must agree rather than merely both exist.
 *  3. **A declined report is a fact.** Both decline reasons are written down, the
 *     accepted path is unchanged, and the popup renders it — a record nothing
 *     reads is exactly the "written down but never rendered" defect W36c exists
 *     for.
 *  4. **Nothing else moved.** A healthy tick is unchanged, and a trace written by
 *     a build that predates these fields still parses.
 *
 * 🔴 Everything is synthetic: fixture origins, fixture ids, an injected clock and a
 *    fixture HTTP port. No network, no real account, no conversation text.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { handleBackfillMessage } from '../lib/backfill/tab-port';
import type { BackfillRuntimeStatus } from '../lib/popup-view';

const ORIGIN = 'https://chatgpt.com';
const SCOPE = 'acct-fixture-1';
const STATE_KEY = `cs_backfill_v2:chatgpt:${SCOPE}`;
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

async function dispatch(message: unknown, tabId?: number): Promise<any> {
  const sender = tabId === undefined ? { id: 's' } : { id: 's', tab: { id: tabId } };
  return await new Promise((resolve) => {
    const ret = runtimeListeners[0]!(message, sender, resolve);
    if (ret !== true) resolve(undefined);
  });
}

/** One alarm wake, through the listener the browser would call, and settle the tick it starts. */
async function alarmTick(mod: any): Promise<void> {
  alarmListeners[0]!({ name: 'cs-backfill-tick' });
  await mod.backfillTickSettled();
}

/** The trace as it is in storage — the record a later reader has, not the in-memory one. */
async function trace(): Promise<any> {
  const { loadLastTick } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  return await loadLastTick(browserLocalStore());
}

/**
 * The popup's own answer, assembled the way `entrypoints/popup/main.ts` assembles it.
 *
 * 🔴 `hookDecline` is read **off the raw snapshot by its storage key**, not through
 *    `loadHookDecline`, and that is on purpose: this file has to be runnable against
 *    a build that predates every symbol W47 adds, or its red run would prove nothing
 *    but that an import failed. The key is the record's identity, and the popup test
 *    below is what keeps it in step with the constant the production path uses.
 */
const HOOK_DECLINED_KEY = 'cs_hook_declined_v1';

async function popupTextNow(): Promise<string> {
  const { browserLocalSnapshot, browserLocalStore } = await import('../lib/backfill/store');
  const { loadLastTick } = await import('../lib/backfill/alarm');
  const { hookStatusOf } = await import('../lib/hook-status');
  const { renderPopup, pickBackfillState, collectFailures, popupText } =
    await import('../lib/popup-view');
  const s = browserLocalStore();
  const snapshot = await browserLocalSnapshot();
  const view = renderPopup({
    enabled: true,
    block: null,
    state: pickBackfillState(snapshot),
    target: null,
    failures: collectFailures(snapshot),
    lastTick: await loadLastTick(s),
    hookStatus: hookStatusOf(snapshot),
    hookDecline: (snapshot?.[HOOK_DECLINED_KEY] ?? null) as never,
  });
  return popupText(view);
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  alarmBook.clear();
  liveTabs.clear();
  contentFetches.length = 0;
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

/** Switch on, and register the one target the alarm's path needs. Nothing here is a live tab. */
async function enabledWithTarget(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
  store['cs_backfill_targets_v1'] = [{ platform: 'chatgpt', origin: ORIGIN, scope: SCOPE, at: 1 }];
}

describe('W47 · every path that writes a trace names how it ended', () => {
  it('🔴 a tick blocked before any request carries its own named outcome, not a blank', async () => {
    await enabledWithTarget();
    const mod = await bootBackground();

    // No platform page is open, so the tick is blocked at the port gate and the
    // engine is never entered. That is the ordinary state of a laptop, and it is
    // the state in which the trace used to read `{stopped: null, halted: null}`.
    await alarmTick(mod);
    const blocked = await trace();
    expect(blocked).toMatchObject({
      ran: false,
      reason: 'no-http-port',
      targets: 1,
      // The field W47 exists for: a named stop on a path that stopped before any
      // request. It is the tick's own outcome — no new vocabulary.
      stopped: 'no-http-port',
    });
    expect(blocked.halted).toBeNull();
    expect(contentFetches).toEqual([]);

    // And the other blocked shape: the switch on, a channel possible, no target.
    delete store['cs_backfill_targets_v1'];
    const { resetTickLockForTest } = await import('../lib/backfill/schedule');
    resetTickLockForTest();
    await alarmTick(mod);
    expect(await trace()).toMatchObject({
      ran: false,
      reason: 'no-targets',
      targets: 0,
      stopped: 'no-targets',
    });
  });

  it('🟢 a tick that really runs carries the run\'s own stop, not the tick\'s name for it', async () => {
    await enabledWithTarget();
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    const status: BackfillRuntimeStatus = await dispatch({ type: 'cs-backfill-status' });
    expect(status.transportWired).toBe(true);

    await alarmTick(mod);
    const rec = await trace();
    expect(rec.ran).toBe(true);
    expect(rec.reason).toBe('ran');
    // 🔴 The run really happened and really fetched — and `stopped` is the run's
    // own conclusion, not a copy of `reason`. With no native host in this fixture
    // the delivery exit is unreachable, so the run pauses there: a *normal* stop
    // (`host-unavailable` is a pause, not a halt). The two fields therefore carry
    // two different values, which is the proof that `stopped` is sourced from the
    // report and not from the tick's own outcome.
    expect(contentFetches.length).toBeGreaterThan(0);
    expect(rec.stopped).toBe('host-unavailable');
    expect(rec.halted).toBeNull();
    expect(rec.targets).toBe(1);
  });

  it('🔴 a run that succeeded does not inherit a refusal about somebody else', async () => {
    // The preflight sweeps **every** state key in storage, not just the target's.
    // So a record belonging to a platform this tick never touches can be
    // unreadable while the tick's own leg runs perfectly well. Those are two
    // different facts, and `halted` is the field for the *run's* conclusion: a
    // run that ran and did not halt must say so, or the next person reading the
    // trace diagnoses a halt on a leg that never halted.
    store['cs_backfill_v2:gemini:someone-elses-scope'] = { not: 'a header', v: 99 };
    await enabledWithTarget();
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    await alarmTick(mod);
    const rec = await trace();
    // The run really happened — same shape as the healthy case above.
    expect(rec.ran).toBe(true);
    expect(rec.reason).toBe('ran');
    expect(contentFetches.length).toBeGreaterThan(0);
    expect(rec.stopped).toBe('host-unavailable');
    // 🔴 The claim: `state-unreadable` is about the gemini record, and this leg
    //    did not halt. Before the fix the preflight's refusal was written here
    //    unconditionally, so a healthy run reported a halt it never had.
    expect(rec.halted).toBeNull();
    expect(rec.detail ?? null).toBeNull();
    // And the record the refusal is about is still untouched.
    expect(store['cs_backfill_v2:gemini:someone-elses-scope']).toEqual({ not: 'a header', v: 99 });
  });

  it('🔴 a trace written before these fields existed still parses', async () => {
    const { loadLastTick, BACKFILL_LAST_TICK_KEY } = await import('../lib/backfill/alarm');
    const { browserLocalStore } = await import('../lib/backfill/store');
    // The exact record the real machine held on builds 0.1.0.10-12.
    store[BACKFILL_LAST_TICK_KEY] = { at: 1, ran: false, reason: 'no-http-port', targets: 1 };
    expect(await loadLastTick(browserLocalStore())).toMatchObject({
      at: 1,
      ran: false,
      reason: 'no-http-port',
      targets: 1,
    });
    expect((await loadLastTick(browserLocalStore()))!.stopped).toBeUndefined();
  });
});

describe('W47 · a refusal reaches storage even when no run happens', () => {
  it('🔴 an unreadable record is named with no page open, and the record itself is untouched', async () => {
    await enabledWithTarget();
    // A record this build cannot read, at a real scope's key. It is the thing the
    // refusal is *about*, and the whole point is that nothing may write over it.
    const planted = { not: 'a header', v: 99 };
    store[STATE_KEY] = planted;
    const before = JSON.stringify(store[STATE_KEY]);

    const mod = await bootBackground();
    await alarmTick(mod);

    const rec = await trace();
    expect(rec).toMatchObject({
      ran: false,
      reason: 'no-http-port',
      stopped: 'no-http-port',
      // 🔴 The fact that used to exist nowhere: a record this build cannot read.
      halted: 'state-unreadable',
    });
    expect(rec.detail).toContain(STATE_KEY);
    // Nothing was fetched, and nothing was written where the refusal is about.
    expect(contentFetches).toEqual([]);
    expect(JSON.stringify(store[STATE_KEY])).toBe(before);
    expect(store[STATE_KEY]).toEqual(planted);
  });

  it('🟢 the same refusal, reached by the run itself, names the same reason', async () => {
    await enabledWithTarget();
    store[STATE_KEY] = { not: 'a header', v: 99 };
    const before = JSON.stringify(store[STATE_KEY]);
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    // A live tab means the gates pass and the run really happens. It refuses, and
    // the trace must say exactly what the blocked path above said — two routes to
    // one fact, not two different sentences.
    await alarmTick(mod);
    const rec = await trace();
    expect(rec).toMatchObject({ ran: true, reason: 'ran', stopped: 'halted', halted: 'state-unreadable' });
    expect(rec.detail).toContain(STATE_KEY);
    expect(JSON.stringify(store[STATE_KEY])).toBe(before);
  });

  it('🟢 a readable record is not a refusal, and the probe writes nothing', async () => {
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { initialState } = await import('../lib/backfill/types');
    const { openLedger, saveHeader } = await import('../lib/backfill/ledger');
    await enabledWithTarget();
    // A current-layout record this build *can* read: the tick must not invent a
    // refusal out of it, and the tick's own preflight must agree with what the
    // engine's `openLedger` says about the same record.
    await saveHeader(browserLocalStore()!, initialState('chatgpt', SCOPE));
    expect((await openLedger(browserLocalStore()!, 'chatgpt', SCOPE)).ok).toBe(true);

    const mod = await bootBackground();
    await alarmTick(mod);
    const rec = await trace();
    expect(rec.halted).toBeNull();
    expect(rec.stopped).toBe('no-http-port');
    // The header is exactly as it was: the preflight read it and wrote nothing.
    expect(await openLedger(browserLocalStore()!, 'chatgpt', SCOPE)).toMatchObject({ ok: true });
  });
});

describe('W47 · a report background declines is a fact, not a silence', () => {
  const UNKNOWN_ORIGIN = 'https://not-a-matched-site.example';

  it('🔴 an observation declined for its origin leaves a readable reason', async () => {
    const { hookStatusKey } = await import('../lib/hook-status');
    await bootBackground();

    const reply = await dispatch({
      type: 'cs-hook-status',
      origin: UNKNOWN_ORIGIN,
      reason: 'hook-was-replaced',
      observedAt: 1_700_000_000_000,
    });
    expect(reply).toEqual({ ok: false, error: 'unknown origin' });

    // 🔴 What the day W47 is about looked like from outside: this was a blank.
    expect(store[HOOK_DECLINED_KEY]).toEqual({
      at: 1_700_000_000_000,
      reason: 'not-a-platform-origin',
      origin: UNKNOWN_ORIGIN,
      observation: 'hook-was-replaced',
      count: 1,
    });
    // The page's own record is still NOT written: a record naming an origin this
    // extension does not inject into would be a sentence about somebody else's page.
    expect(store[hookStatusKey(UNKNOWN_ORIGIN)]).toBeUndefined();

    // A second one is the same fact happening again, and the streak says so —
    // "every 5 seconds" has to be visible, not inferred from one timestamp.
    await dispatch({
      type: 'cs-hook-status',
      origin: UNKNOWN_ORIGIN,
      reason: 'hook-was-replaced',
      observedAt: 1_700_000_005_000,
    });
    expect(store[HOOK_DECLINED_KEY]).toMatchObject({ count: 2 });
  });

  it('🔴 a decline from a different origin starts its own streak', async () => {
    const OTHER_ORIGIN = 'https://a-second-unmatched-site.example';
    await bootBackground();

    await dispatch({
      type: 'cs-hook-status',
      origin: UNKNOWN_ORIGIN,
      reason: 'hook-was-replaced',
      observedAt: 1_700_000_000_000,
    });
    expect(store[HOOK_DECLINED_KEY]).toMatchObject({ origin: UNKNOWN_ORIGIN, count: 1 });

    // A different origin refused for the same reason is **not** the same thing
    // happening again. The streak answers "how long has this been going on", and
    // one record is shared by every origin — so counting across origins would
    // report `count: 2` for two unrelated pages that each spoke once, which reads
    // as a page in a loop. The origin printed beside the count would be the
    // second one, making the number a claim about a page it does not describe.
    await dispatch({
      type: 'cs-hook-status',
      origin: OTHER_ORIGIN,
      reason: 'hook-was-replaced',
      observedAt: 1_700_000_005_000,
    });
    expect(store[HOOK_DECLINED_KEY]).toMatchObject({
      origin: OTHER_ORIGIN,
      reason: 'not-a-platform-origin',
      count: 1,
    });

    // …and the streak still counts when the same origin really does repeat.
    await dispatch({
      type: 'cs-hook-status',
      origin: OTHER_ORIGIN,
      reason: 'hook-was-replaced',
      observedAt: 1_700_000_010_000,
    });
    expect(store[HOOK_DECLINED_KEY]).toMatchObject({ origin: OTHER_ORIGIN, count: 2 });
  });

  it('🔴 a report this build cannot read is recorded, and nothing is invented about it', async () => {
    await bootBackground();

    const reply = await dispatch({
      type: 'cs-hook-status',
      origin: 'https://chatgpt.com',
      reason: 'hook-was-replaced',
      observedAt: 'not a number',
    });
    expect(reply).toEqual({ ok: false, error: 'unreadable hook report' });

    // Neither the origin nor the observation is stored: they are exactly the parts
    // that did not check out, and a record a person reads may not guess at them.
    expect(store[HOOK_DECLINED_KEY]).toEqual({
      at: expect.any(Number),
      reason: 'unreadable-message',
      origin: null,
      observation: null,
      count: 1,
    });
    expect(store['cs_hook_v1:https://chatgpt.com']).toBeUndefined();
  });

  it('🟢 an accepted report is unchanged, and writes no decline', async () => {
    await bootBackground();

    expect(await dispatch({
      type: 'cs-hook-status',
      origin: ORIGIN,
      reason: 'hook-was-replaced',
      observedAt: 4_242,
    })).toEqual({ ok: true });
    expect(store[HOOK_DECLINED_KEY]).toBeUndefined();
    expect(store['cs_hook_v1:https://chatgpt.com']).toEqual({
      origin: ORIGIN,
      platform: 'chatgpt',
      reasons: [{ reason: 'hook-was-replaced', at: 4_242 }],
      at: 4_242,
    });
  });

  it('🔴 a decline written by a newer build is shown, not read as "no record"', async () => {
    // A reason code this build has never heard of. Reading it as "nothing was
    // declined" would be an unknown recorded as empty, one record along from the
    // W44 lesson — so the shape is accepted and the code is printed as itself.
    store[HOOK_DECLINED_KEY] = {
      at: 1_700_000_000_000,
      reason: 'a-check-this-build-does-not-know',
      origin: null,
      observation: null,
      count: 3,
    };
    await bootBackground();
    const text = await popupTextNow();
    expect(text).toContain('a-check-this-build-does-not-know');
    expect(text).toContain('does not know that reason code');
  });

  it('🔴 the popup says it — a record nothing reads is not visible', async () => {
    await bootBackground();
    expect(await popupTextNow()).not.toContain('not recorded');

    await dispatch({
      type: 'cs-hook-status',
      origin: UNKNOWN_ORIGIN,
      reason: 'hook-did-not-run',
      observedAt: 1_700_000_000_000,
    });
    const text = await popupTextNow();
    expect(text).toContain(UNKNOWN_ORIGIN);
    expect(text).toContain('not recorded');
    // The streak, so "this is happening on a timer" is legible from the popup.
    expect(text).toContain('refused in a row so far: 1');

    // And the unreadable case reads differently, because it is a different fact:
    // the origin is not known and the sentence may not pretend it is.
    await dispatch({ type: 'cs-hook-status', origin: ORIGIN, reason: 'nonsense', observedAt: 1 });
    const second = await popupTextNow();
    expect(second).not.toContain(UNKNOWN_ORIGIN);
    expect(second).toContain('could not read');
  });
});
