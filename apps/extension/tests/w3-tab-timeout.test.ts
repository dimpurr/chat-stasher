/**
 * W7 · The backfill leg's page-fetch channel gets a timeout.
 *
 * ## The bug this file exists for
 * Measured in a real Chrome: the backfill leg enumerated 901 conversations, and
 * from then on **every** round for 15+ minutes recorded
 * `{ran:false, reason:'already-running'}` without fetching one body.
 *
 * The mechanism, read off the code:
 *  · `lib/backfill/schedule.ts:238-274` — `inFlight` is released in a `finally`,
 *    but that `finally` only runs once `await runBackfill(...)` (:259) **settles**.
 *  · `tabHttpPort` awaited `send(tabId, message)` (i.e. `browser.tabs.sendMessage`)
 *    with **no timeout at all**. When the page was reloaded, or the extension
 *    itself was reloaded, the content script's context is gone and that promise can
 *    simply never settle ⇒ the round never ends ⇒ the lock is never released ⇒
 *    every later round is refused as 'already-running'.
 *
 * ## What is asserted here
 *  (a) a `send` that never settles makes the port **reject**, in bounded time, with
 *      a message that carries only technical facts (no URL, no conversation id);
 *  (b) a `send` that answers quickly returns normally and leaves **no pending timer**;
 *      the production budget really is 90 s, armed and fired at 90 s, not before;
 *  (c) 🔴 end-to-end: driven through the **real entry point** (a `chat-captured`
 *      message → `kickBackfill` → `tickBackfill`), the first round stops with a
 *      transport error, the **second round is no longer refused as 'already-running'**
 *      (the lock really was released), and the debt is **still owed**.
 *
 * Zero real network and zero logged-in state throughout: the fetch is a pure
 * function in this file and the port is injected through background's own test seam.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import {
  tabHttpPort,
  BACKFILL_TAB_REPLY_TIMEOUT_MS,
  type TabSend,
} from '../lib/backfill/tab-port';
import type { CapturedFetch } from '../lib/contract';

const ORIGIN = 'https://chatgpt.com';
const LIST_PATH = '/backend-api/conversations';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
  'c3333333-0000-4000-8000-000000000003',
];

/** One conversation-body URL, used as the string the timeout message must NOT leak. */
const DETAIL_URL = `${ORIGIN}/backend-api/conversation/${IDS[0]}`;

/** A `send` that never settles: exactly what a dead content-script context looks like. */
const neverAnswers: TabSend = () => new Promise(() => { /* never settles */ });

// ===========================================================================
// (a) + (b): the port itself
// ===========================================================================

describe('W7 (a) · a page that never answers is a lost round, not an empty answer', () => {
  it('rejects in bounded time, and the message carries no URL and no conversation id', async () => {
    // 50 ms rather than the production 90 s: the timeout is injectable precisely so
    // this can be a fast test. The production value is asserted separately in (b2).
    const port = tabHttpPort(11, neverAnswers, 50);

    const started = Date.now();
    let caught: Error | null = null;
    try {
      await port(DETAIL_URL);
    } catch (err) {
      caught = err as Error;
    }
    const elapsed = Date.now() - started;

    console.log('[W7-a] rejected after', elapsed, 'ms with:', caught?.message);
    expect(caught).not.toBeNull();
    expect(caught!.message).toContain('tab 11');
    expect(caught!.message).toContain('did not answer the backfill fetch');
    // 🔴 Only technical facts: the tab id and the budget. The URL, the origin and the
    //    conversation id must not be carried into the halt detail or the log.
    expect(caught!.message).not.toContain(DETAIL_URL);
    expect(caught!.message).not.toContain(ORIGIN);
    expect(caught!.message).not.toContain(IDS[0]);

    // "In bounded time": it really was the timer that settled it, not something else.
    expect(elapsed).toBeGreaterThanOrEqual(40);
    expect(elapsed).toBeLessThan(5_000);
  });

  it('the POST route is bounded by the same timeout (no second, unbounded path)', async () => {
    const port = tabHttpPort(12, neverAnswers, 50);
    let caught: Error | null = null;
    try {
      await port(`${ORIGIN}/backend-api/conversation/${IDS[1]}`, {
        method: 'POST',
        body: '{"offset":0}',
        contentType: 'application/json',
      } as never);
    } catch (err) {
      caught = err as Error;
    }
    console.log('[W7-a] the POST route rejected with:', caught?.message);
    expect(caught).not.toBeNull();
    expect(caught!.message).toContain('tab 12');
  });
});

describe('W7 (b) · an answered round returns normally and leaves no timer pending', () => {
  it('🔴 after a fast reply, zero timers are still armed', async () => {
    vi.useFakeTimers();
    try {
      const sent: unknown[] = [];
      const port = tabHttpPort(5, async (_id, msg) => {
        sent.push(msg);
        return { ok: true, status: 200, text: 'x' };
      });

      const res = await port(DETAIL_URL);

      console.log('[W7-b] reply:', res, '· messages sent:', sent.length, '· timers left:', vi.getTimerCount());
      expect(res).toEqual({ status: 200, text: 'x' });
      // The GET message stays byte-for-byte `{type, url}`: the timeout changed nothing on the wire.
      expect(sent).toEqual([{ type: 'cs-backfill-fetch', url: DETAIL_URL }]);
      // 🔴 The clearTimeout really ran: a fast round leaves nothing hanging.
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it('🔴 the production budget is 90 s: the timer is armed, still pending at 89.999 s, and fires at 90 s', async () => {
    vi.useFakeTimers();
    try {
      expect(BACKFILL_TAB_REPLY_TIMEOUT_MS).toBe(90_000);

      // 🔴 No third argument: this is the production call shape.
      const port = tabHttpPort(3, neverAnswers);
      const outcome = port(DETAIL_URL).then(
        () => 'resolved',
        (err) => (err as Error).message,
      );

      expect(vi.getTimerCount()).toBe(1);
      await vi.advanceTimersByTimeAsync(89_999);
      // Still hanging one millisecond short of the budget.
      expect(await Promise.race([outcome, Promise.resolve('still-pending')])).toBe('still-pending');

      await vi.advanceTimersByTimeAsync(1);
      const message = await outcome;
      console.log('[W7-b] the production timeout produced:', message);
      expect(message).toContain('tab 3 did not answer the backfill fetch within 90 s');
      // And it cleaned up after itself on the way out.
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });
});

// ===========================================================================
// (c): end-to-end lock release, driven from the real entry point
//
// 🔴 Nothing below calls tickBackfill / runBackfill directly. The round starts
//    from a real `chat-captured` message arriving at background's registered
//    runtime listener, exactly as a content script sends it.
// ===========================================================================

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
let host: SyntheticHost;

/** The URLs the port was asked for, in order: evidence of how far the round got. */
const detailAttempts: string[] = [];

/**
 * 🔴 The injected port. It answers the **list** page like a healthy page would (so
 * real debts exist), and then never answers a **body** fetch at all — the failure
 * mode the bug report describes.
 * The budget is 50 ms instead of 90 s purely to keep the test fast; the production
 * default is asserted in (b2).
 */
function halfDeadPort() {
  return tabHttpPort(1, async (_id, msg) => {
    const m = msg as { url: string };
    if (new URL(m.url).pathname === LIST_PATH) {
      return {
        ok: true,
        status: 200,
        text: JSON.stringify({ items: IDS.map((id) => ({ id })), total: IDS.length }),
      };
    }
    detailAttempts.push(m.url);
    return new Promise(() => { /* never settles */ });
  }, 50);
}

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
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
    // The live leg's own write path is not what this file tests; the round reaches
    // the port through configureBackfillTransport, so this is never called.
    async sendMessage() { throw new Error('the test port is injected through the transport seam'); },
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

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

/** The one `cs_backfill_v1:*` state record this round wrote, whatever the scope key turned out to be. */
function backfillState(): any {
  const key = Object.keys(store).find((k) => k.startsWith('cs_backfill_v1:chatgpt:'));
  expect(key, 'the round must have written a backfill state record').toBeDefined();
  return store[key!];
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  detailAttempts.length = 0;
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('W7 (c) · 🔴 a round that times out releases the single-flight lock', () => {
  it('the second round is not refused as already-running, and the debt is still owed', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    // 🔴 The existing test seam: the production port is replaced, nothing else is.
    mod.configureBackfillTransport(halfDeadPort());

    // ---- round 1: the page answers the list and then goes silent ----
    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 42);
    await mod.backfillTickSettled();

    const first = mod.lastBackfillTick()!;
    console.log('[W7-c] round 1:', {
      reason: first.reason,
      stopped: first.report?.stopped,
      halt: first.report?.halted,
      detailAttempts: detailAttempts.length,
      pending: backfillState().pending.length,
    });

    // It really ran a round (this is the round that used to never finish).
    expect(first.reason).not.toBe('already-running');
    expect(first.reason).toBe('ran');
    // 🔴 And it stopped as a **transport error** with a trace — not as "nothing to do".
    // 🔴 W13 · changed semantics, stated rather than quietly relaxed.
    //    before: `expect(first.report.stopped).toBe('halted')`.
    //    after:  'waiting-retry'. The timeout is a transient transport condition, so
    //            the leg is now in a state that heals itself; 'halted' is reserved for
    //            the ones a human has to clear. The reason and the detail — the two
    //            things this test is really about — are asserted unchanged below.
    expect(first.report.stopped).toBe('waiting-retry');
    expect(first.report.halted).toMatchObject({ reason: 'transport-error' });
    expect(first.report.halted.detail).toContain('did not answer the backfill fetch');

    // 🔴 The debt is still owed: the body was never delivered, so nothing may be
    //    archived and nothing may leave `pending`.
    expect(detailAttempts.length).toBe(1);
    expect(backfillState().archived).toEqual([]);
    expect(backfillState().pending).toEqual(IDS);

    // ---- round 2: immediately after, with no waiting at all ----
    //
    // 🔴 This is the assertion the bug fails. Before the timeout, round 1 never
    //    settled, so `inFlight` stayed true and this round was answered
    //    `{ran:false, reason:'already-running'}` — forever.
    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 42);
    await mod.backfillTickSettled();
    const second = mod.lastBackfillTick()!;

    console.log('[W7-c] round 2:', { reason: second.reason, stopped: second.report?.stopped });
    expect(second.reason).not.toBe('already-running');
    // It went back in and found the persisted stop: the stop is still sayable, and
    // it is still the same transport error rather than a fresh "all done".
    // 🔴 W13 · changed semantics, stated rather than quietly relaxed.
    //    before: `expect(second.report.stopped).toBe('halted')`.
    //    after:  'waiting-retry', because the round is inside the backoff the first
    //            round wrote. 🔴 And this round is now asserted to be *free*: not one
    //            request may go out while waiting. That is a strictly stronger claim
    //            than the old assertion made, and it is the property that keeps a
    //            backoff from becoming a busy-loop.
    expect(second.report.stopped).toBe('waiting-retry');
    expect(second.report.halted).toMatchObject({ reason: 'transport-error' });
    expect(detailAttempts.length, 'a waiting round must not touch the platform').toBe(1);

    // Nothing was silently struck off across either round.
    expect(backfillState().archived).toEqual([]);
    expect(backfillState().pending).toEqual(IDS);
  });

  it('🔴 a later round really runs again after the halt is cleared (the lock is not merely idle)', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    mod.configureBackfillTransport(halfDeadPort());

    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 42);
    await mod.backfillTickSettled();
    expect(mod.lastBackfillTick()!.report.halted).toMatchObject({ reason: 'transport-error' });

    // A human looked at the trace and cleared it; this time the page answers.
    const state = backfillState();
    state.halted = null;
    const key = Object.keys(store).find((k) => k.startsWith('cs_backfill_v1:chatgpt:'))!;
    store[key] = state;
    detailAttempts.length = 0;
    mod.configureBackfillTransport(tabHttpPort(1, async (_id, msg) => {
      const m = msg as { url: string };
      if (new URL(m.url).pathname === LIST_PATH) {
        return { ok: true, status: 200, text: JSON.stringify({ items: [], total: 0 }) };
      }
      return { ok: true, status: 200, text: JSON.stringify({ mapping: {}, current_node: 'n0' }) };
    }, 50));

    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 42);
    await mod.backfillTickSettled();
    const after = mod.lastBackfillTick()!;
    console.log('[W7-c] the round after the halt was cleared:', { reason: after.reason, stopped: after.report?.stopped });

    // 🔴 The third round is not 'already-running' either: the lock has been free
    //    since round 1 settled, not just since the halt was cleared.
    expect(after.reason).not.toBe('already-running');
    expect(after.report.stopped).not.toBe('halted');
  });
});
