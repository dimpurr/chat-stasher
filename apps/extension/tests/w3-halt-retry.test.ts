/**
 * W13 · A transient stop must not be permanent, and a busy tab must not be
 * forgotten.
 *
 * The two defects this file pins, both measured on a real Chrome profile:
 *
 *  1. `engine.ts` wrote `state.halted` on **any** stop and every later round
 *     returned `report('halted')` immediately — and no code anywhere cleared it.
 *     One round at 09:14 ended in `transport-error` ("message channel closed
 *     before a response was received") because the extension reloading a page tore
 *     the message channel; every round for the next hour returned `halted`, with
 *     7,391 debts owed and 0 archived, and nothing wrong with the account, the
 *     login, or the network.
 *  2. `pickLiveTab` struck a tab out of the registry after a **single** failed
 *     ping, and only a page load puts one back — so a renderer busy for a few
 *     seconds cost the whole fetch channel. The stored registry was found empty.
 *
 * 🔴 Everything here uses synthetic fixtures and an injected http port; not one
 *    line touches a platform endpoint. No fixture holds a real conversation body
 *    or a real conversation id.
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, loadState, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  haltClassOf,
  initialState,
  transientRetryDelayMs,
  legacyStateKey,
  type LegacyBackfillState,
  TRANSIENT_RETRY_BASE_MS,
  TRANSIENT_RETRY_MAX_MS,
  type HaltReason,
} from '../lib/backfill/types';
import { retryMinutesLeft } from '../lib/backfill/progress';
import { DEFAULT_DETAIL_PACE, DEFAULT_ENUM_PACE, type Clock } from '../lib/backfill/pace';
import { BACKFILL_TICK_DELAY_MIN_MINUTES } from '../lib/backfill/alarm';
import { forgetTab, loadTabs, pickLiveTab, rememberTab, TAB_PING_MISSES_BEFORE_FORGET } from '../lib/backfill/tab-port';

const ORIGIN = 'https://chatgpt.com';
const T0 = Date.parse('2026-09-14T09:00:00.000Z');

/** A clock whose time only moves when a test says so. `sleep` advances it, so the Pacers stay virtual. */
function stepClock(start: number): Clock & { at: (ms: number) => void; advance: (ms: number) => void } {
  let t = start;
  return {
    now: () => t,
    async sleep(ms: number) {
      t += ms;
    },
    at: (ms: number) => {
      t = ms;
    },
    advance: (ms: number) => {
      t += ms;
    },
  };
}

function ids(n: number, from = 0): string[] {
  return Array.from({ length: n }, (_, i) => `conv-${String(i + from).padStart(4, '0')}-aaaaaaaa`);
}

function listBody(all: string[]): string {
  return JSON.stringify({
    items: all.map((id) => ({ id, title: 'synthetic-fixture', create_time: 0 })),
    limit: 100,
    offset: 0,
    total: all.length,
  });
}

function detailBody(id: string): string {
  return JSON.stringify({
    title: 'synthetic-fixture',
    current_node: `${id}-node`,
    mapping: { [`${id}-node`]: { id: `${id}-node`, parent: null, children: [] } },
  });
}

/** Body fetches may be made to fail for a while, and every request is counted. */
function flakyBackend(all: string[]) {
  const calls: string[] = [];
  let failDetail = true;
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
    if (failDetail) {
      // The measured failure, verbatim in shape: a torn message channel.
      throw new Error('message channel closed before a response was received');
    }
    const id = decodeURIComponent(url.split('/backend-api/conversation/')[1]!.split('?')[0]!);
    return { status: 200, text: detailBody(id) };
  };
  return {
    http,
    calls,
    detailCalls: () => calls.filter((c) => c.includes('/backend-api/conversation/')),
    heal: () => {
      failDetail = false;
    },
  };
}

function opts(store: ReturnType<typeof memoryStore>, http: (url: string) => Promise<HttpResponse>, clock: Clock) {
  return {
    platform: 'chatgpt',
    origin: ORIGIN,
    store,
    http,
    clock,
    pace: { enumerate: DEFAULT_ENUM_PACE, detail: { ...DEFAULT_DETAIL_PACE, minIntervalMs: 0 } },
    /**
     * 🔴 W16 · The backoff ladder is jittered now (full jitter, `[0.5, 1.0] ×`
     *    the exponential). `() => 1` is the **top of that band**, at which the
     *    draw is exactly the un-jittered exponential — so this file's ladder
     *    assertions (`[5, 10, 20, 30, 30, 30]` minutes) keep their exact values
     *    and now additionally pin the band's ceiling. `() => 0` (the floor, half
     *    of each) is pinned explicitly in tests/w3-jitter.test.ts.
     */
    random: () => 1,
  } as const;
}

describe('W13-1 · a transient stop waits, then resumes on its own, from the same place', () => {
  it('waiting round issues no request; the due round clears the stop and carries on', async () => {
    const store = memoryStore();
    const all = ids(3);
    const backend = flakyBackend(all);
    const clock = stepClock(T0);

    // ---- round 1: the list is read, then the message channel is torn ----
    const r1 = await runBackfill({ ...opts(store, backend.http, clock), scope: 'w13-transient', maxDetails: 1 });

    expect(r1.stopped).toBe('waiting-retry');
    expect(r1.halted).toMatchObject({ reason: 'transport-error' });
    expect(r1.halted?.detail).toContain('message channel closed');
    expect(r1.halted?.attempts).toBe(1);
    // 5 minutes = one alarm tick, the transport-error base of the ladder.
    expect(r1.halted?.retryAt).toBe(r1.halted!.at + 5 * 60_000);
    expect(backend.detailCalls()).toHaveLength(1);

    const retryAt = r1.halted!.retryAt!;
    const pendingAfterR1 = [...r1.state.pending];
    const callsAfterR1 = backend.calls.length;
    const archivedAfterR1 = [...r1.state.archived];
    const deliveredAfterR1 = r1.state.detailToday.count;

    // ---- round 2: inside the backoff ----
    clock.at(retryAt - 1);
    const r2 = await runBackfill({ ...opts(store, backend.http, clock), scope: 'w13-transient', maxDetails: 1 });

    expect(r2.stopped).toBe('waiting-retry');
    // 🔴 The waiting round must be free: not one request may go out while waiting.
    expect(backend.calls.length, 'a waiting round must not touch the platform').toBe(callsAfterR1);
    // 🔴 And it must not write anything off while it waits.
    expect(r2.archivedThisRun).toEqual([]);
    expect(r2.state.pending).toEqual(pendingAfterR1);
    expect(r2.state.archived).toEqual(archivedAfterR1);
    // Nothing was delivered, so nothing may be counted as delivered.
    expect(r2.state.detailToday.count).toBe(deliveredAfterR1);
    // The streak is still readable while waiting — the popup says "attempt N".
    expect(r2.halted?.attempts).toBe(1);
    expect(r2.progress).toContain('waiting to retry');

    // ---- round 3: the moment the backoff expires ----
    backend.heal();
    clock.at(retryAt);
    const r3 = await runBackfill({ ...opts(store, backend.http, clock), scope: 'w13-transient', maxDetails: 1 });

    expect(r3.stopped).not.toBe('waiting-retry');
    expect(r3.halted).toBeNull();
    // It really went back to the platform, and it resumed at the **same debt** the
    // first round stopped on — not at the head of a fresh set.
    expect(backend.calls.length).toBeGreaterThan(callsAfterR1);
    expect(r3.archivedThisRun).toEqual([pendingAfterR1[0]]);
    expect(r3.state.pending).toEqual(pendingAfterR1.slice(1));
    expect(r3.state.detailToday.count).toBe(deliveredAfterR1 + 1);
  });

  it('a stop that keeps recurring grows the streak and the wait, instead of retrying at a fixed rate', async () => {
    const store = memoryStore();
    const all = ids(1);
    const backend = flakyBackend(all);
    const clock = stepClock(T0);

    const waits: number[] = [];
    // The channel never heals, so every round that gets through fails the same way
    // and the streak is allowed to actually grow.
    for (let i = 0; i < 6; i += 1) {
      clock.at(clock.now());
      const run = await runBackfill({ ...opts(store, backend.http, clock), scope: 'w13-streak', maxDetails: 1 });
      expect(run.stopped).toBe('waiting-retry');
      expect(run.halted?.reason).toBe('transport-error');
      expect(run.halted?.attempts).toBe(i + 1);
      waits.push(run.halted!.retryAt! - run.halted!.at);
      // 🔴 Through six consecutive failures not one debt moved and not one was
      //    written off: the backoff is a pause, never a substitute for delivery.
      expect(run.archivedThisRun).toEqual([]);
      expect(run.state.archived).toEqual([]);
      expect(run.state.pending.length).toBe(1);
      // Move exactly to the moment the record says we may try again.
      clock.at(run.halted!.retryAt!);
    }

    // Strictly increasing while below the cap, then flat at it.
    expect(waits).toEqual([5 * 60_000, 10 * 60_000, 20 * 60_000, 30 * 60_000, 30 * 60_000, 30 * 60_000]);
    expect(waits.every((w) => w <= TRANSIENT_RETRY_MAX_MS['transport-error'])).toBe(true);

    // And the moment the channel heals, the very next due round delivers.
    backend.heal();
    clock.at(clock.now() + TRANSIENT_RETRY_MAX_MS['transport-error']);
    const recovered = await runBackfill({ ...opts(store, backend.http, clock), scope: 'w13-streak', maxDetails: 1 });
    expect(recovered.halted).toBeNull();
    expect(recovered.archivedThisRun).toEqual(ids(1));
  });
});

describe('W13-2 · the backoff ladder itself', () => {
  it('grows geometrically, is capped, and never grows past the cap however long the streak runs', () => {
    for (const reason of ['transport-error', 'rate-limited'] as const) {
      const base = TRANSIENT_RETRY_BASE_MS[reason];
      const cap = TRANSIENT_RETRY_MAX_MS[reason];
      /**
       * 🔴 W16 · Attempt 1 is no longer *exactly* `base`: full jitter multiplies
       *    it by `uniform[0.5, 1.0]`. The criterion the old `toBe(base)` guarded
       *    is "the first rung is one whole base tall, not a fraction of one", so
       *    it is kept as the band it now is — and the top of the band is still
       *    exactly `base`, which is asserted at `() => 1`.
       */
      expect(transientRetryDelayMs(reason, 1, () => 1)).toBe(base);
      expect(transientRetryDelayMs(reason, 1, () => 0)).toBe(base / 2);
      expect(transientRetryDelayMs(reason, 1)).toBeGreaterThanOrEqual(base / 2);
      expect(transientRetryDelayMs(reason, 1)).toBeLessThanOrEqual(base);
      let prev = 0;
      for (let attempt = 1; attempt <= 50; attempt += 1) {
        const d = transientRetryDelayMs(reason, attempt);
        expect(Number.isFinite(d), `${reason} attempt ${attempt} produced a non-finite delay`).toBe(true);
        expect(d).toBeGreaterThanOrEqual(prev);
        expect(d).toBeLessThanOrEqual(cap);
        prev = d;
      }
      // Long streaks settle on the cap rather than on Infinity — including at the
      // bottom of the jitter band, where the exponential is still far past the cap.
      expect(transientRetryDelayMs(reason, 999, () => 0)).toBe(cap);
      expect(transientRetryDelayMs(reason, 1e9)).toBe(cap);
    }
  });

  it('rate-limited starts higher and is capped higher than a transport error, as a reference implementation does for 429', () => {
    expect(TRANSIENT_RETRY_BASE_MS['rate-limited']).toBeGreaterThan(TRANSIENT_RETRY_BASE_MS['transport-error']);
    expect(TRANSIENT_RETRY_MAX_MS['rate-limited']).toBeGreaterThan(TRANSIENT_RETRY_MAX_MS['transport-error']);
    // 🔴 Both bases are at least one whole alarm tick. A base below the tick period
    //    would degenerate to "the next tick retries" — a backoff in name only, and
    //    the number would be a lie about what had been tuned.
    // 🔴 W16 · The tick gap is drawn from `[5, 10]` minutes now rather than fixed
    //    at 5, so the reference is its **floor** — the tightest case this bound
    //    has to hold against. Referencing the mean (7.5) would let a base sit
    //    below the shortest real gap and quietly become the "next tick retries"
    //    degenerate case the comment above rules out.
    const tick = BACKFILL_TICK_DELAY_MIN_MINUTES * 60_000;
    expect(TRANSIENT_RETRY_BASE_MS['transport-error']).toBeGreaterThanOrEqual(tick);
    expect(TRANSIENT_RETRY_BASE_MS['rate-limited']).toBeGreaterThan(tick);
  });

  it('the classification is one function, and every reason lands on a side', () => {
    const transient: HaltReason[] = ['transport-error', 'rate-limited'];
    const permanent: HaltReason[] = [
      'shape-changed',
      'storage-unavailable',
      'unsupported-platform',
      'detail-unsupported',
      'detail-empty-unverified',
    ];
    for (const r of transient) expect(haltClassOf(r), r).toBe('transient');
    for (const r of permanent) expect(haltClassOf(r), r).toBe('permanent');
  });

  it('a legacy record with no retryAt is due now, not due never', () => {
    const legacy = { reason: 'transport-error' as const, at: T0, detail: 'x' };
    expect(retryMinutesLeft(legacy, T0)).toBe(0);
    expect(retryMinutesLeft(legacy, T0 + 86_400_000)).toBe(0);
    // A record that does carry one rounds up, and never reports a negative wait.
    expect(retryMinutesLeft({ ...legacy, retryAt: T0 + 90_000 }, T0)).toBe(2);
    expect(retryMinutesLeft({ ...legacy, retryAt: T0 - 1 }, T0)).toBe(0);
  });
});

describe('W13-3 · the permanent stops are unchanged (the regression nail)', () => {
  it('a shape-changed stop stays stopped no matter how much time passes, and issues no request', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const r1 = await runBackfill({
      ...opts(store, async () => ({ status: 200, text: JSON.stringify({ conversations: [] }) }), clock),
      scope: 'w13-shape',
    });
    expect(r1.stopped).toBe('halted');
    expect(r1.halted?.reason).toBe('shape-changed');
    // 🔴 No retry furniture on a permanent record: 'halted' has to keep meaning
    //    "a human looks at this", and a `retryAt` on it would make the popup promise
    //    a retry that the engine will never perform.
    expect(r1.halted?.retryAt).toBeUndefined();
    expect(r1.halted?.attempts).toBeUndefined();

    // A hundred days later, with a healthy backend: still stopped, still free.
    clock.at(T0 + 100 * 24 * 3600_000);
    const r2 = await runBackfill({
      ...opts(store, async () => {
        throw new Error('MUST NOT be called after a permanent halt');
      }, clock),
      scope: 'w13-shape',
    });
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.reason).toBe('shape-changed');
    expect(r2.archivedThisRun).toEqual([]);
  });

  it('an unsupported-platform stop is permanent too, and fires before any request', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    let called = 0;
    const http = async (): Promise<HttpResponse> => {
      called += 1;
      throw new Error('MUST NOT be called');
    };
    // A platform that cannot enumerate at all, which is the *other* permanent family.
    // 🔴 W31 (2026-09-14) · This used to name claude, which was the last row of
    //    BACKFILL_UNSUPPORTED. Its plan was filled in from the W20 research, the table is now
    //    empty, and the protagonist moves to the lookup that used to answer for it — the family,
    //    the reasoning and every assertion below are unchanged.
    const claude = {
      ...opts(store, http, clock),
      platform: 'claude',
      origin: 'https://claude.ai',
      scope: 'w13-noplan',
      plans: () => null,
    };

    const r1 = await runBackfill(claude);
    expect(r1.stopped).toBe('halted');
    expect(r1.halted?.reason).toBe('unsupported-platform');
    expect(haltClassOf(r1.halted!.reason)).toBe('permanent');
    expect(r1.halted?.retryAt).toBeUndefined();
    expect(called, 'an unsupported platform must fire before any request').toBe(0);

    clock.at(T0 + 100 * 24 * 3600_000);
    const r2 = await runBackfill(claude);
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.reason).toBe('unsupported-platform');
    expect(called).toBe(0);
  });
});

describe('W13-4 · the record already sitting in a real user storage', () => {
  /**
   * The shape actually found on disk: a transient reason, and a detail that starts
   * `list offset=`. It has **no** `retryAt`, because it predates this change.
   */
  function legacyState(scope: string, reason: HaltReason): LegacyBackfillState {
    // 🔴 W18 · `v: 1` on purpose. This fixture is documented as "the shape actually
    //    found on disk" on a real user's machine, and as of W18 the record that is
    //    really there is the pre-W18 one: a whole state, ids included, under the
    //    `cs_backfill_v1:*` key. Seeding it there means the case runs through the
    //    migration exactly as a real upgrade will, instead of through a shortcut
    //    that no user's storage has.
    const state = { ...initialState('chatgpt', scope), v: 1 } as LegacyBackfillState;
    state.enumCursor = { offset: 3, complete: true };
    state.pending = ids(2);
    state.archived = [];
    state.detailToday = { day: '2026-09-14', count: 0 };
    state.totalSource = 'contradicted';
    state.halted = {
      reason,
      at: T0 - 3600_000,
      detail: 'list offset=3: message channel closed before a response was received',
    };
    return state;
  }

  it('a legacy transport-error record resumes on the next round, with nobody clearing storage by hand', async () => {
    const store = memoryStore();
    const all = ids(2);
    const backend = flakyBackend(all);
    backend.heal();
    const clock = stepClock(T0);
    const legacy = legacyState('w13-legacy', 'transport-error');
    // The fixture must really have the shape of the measured record.
    expect(legacy.halted!.detail.startsWith('list offset=')).toBe(true);
    expect(legacy.halted!.retryAt).toBeUndefined();
    await store.save(legacyStateKey('chatgpt', 'w13-legacy'), legacy);

    const run = await runBackfill({ ...opts(store, backend.http, clock), scope: 'w13-legacy', maxDetails: 1 });

    // 🔴 This is the assertion the real account needed: the leg comes back by itself.
    expect(run.stopped).toBe('budget-exhausted');
    expect(run.halted).toBeNull();
    expect(run.archivedThisRun).toEqual([ids(2)[0]]);
    expect(run.state.pending).toEqual([ids(2)[1]]);
    // And it did not re-enumerate: the cursor it resumed from is the one on disk.
    expect(run.state.enumCursor.offset).toBe(3);
  });

  it('a legacy permanent record stays stopped, exactly as before', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    await store.save(legacyStateKey('chatgpt', 'w13-legacy-shape'), legacyState('w13-legacy-shape', 'shape-changed'));

    const run = await runBackfill({
      ...opts(store, async () => {
        throw new Error('MUST NOT be called');
      }, clock),
      scope: 'w13-legacy-shape',
    });
    expect(run.stopped).toBe('halted');
    expect(run.halted?.reason).toBe('shape-changed');
  });

  it('the resume is persisted: a restart in the middle sees the stop cleared, not re-armed', async () => {
    const store = memoryStore();
    const all = ids(1);
    const backend = flakyBackend(all);
    const clock = stepClock(T0);
    await store.save(legacyStateKey('chatgpt', 'w13-persist'), legacyState('w13-persist', 'transport-error'));

    // Round 1 resumes and then fails again transiently (the body still throws).
    const r1 = await runBackfill({ ...opts(store, backend.http, clock), scope: 'w13-persist', maxDetails: 1 });
    expect(r1.stopped).toBe('waiting-retry');
    expect(r1.halted?.attempts).toBe(2); // the legacy record counted as attempt 1

    // The record on disk is the new one, so a brand-new run reads it back whole.
    const reloaded = await loadState(store, 'chatgpt', 'w13-persist');
    expect(reloaded.halted?.attempts).toBe(2);
    expect(reloaded.halted?.retryAt).toBe(r1.halted!.retryAt);
  });
});

describe('W13-5 · the tab registry keeps a busy tab and still converges on a dead one', () => {
  const dead = async () => {
    throw new Error('no content script here');
  };
  const silent = async () => ({ ok: false, error: 'busy' });
  const alive = async () => ({ ok: true });

  it('a legacy entry with no miss count survives its first failed ping', async () => {
    const store = memoryStore();
    // Exactly what the content script's hello writes in entrypoints/background.ts.
    await rememberTab(store, { tabId: 7, origin: ORIGIN, at: 1 });
    expect((await loadTabs(store))[0]!.misses).toBeUndefined();

    expect(await pickLiveTab(store, ORIGIN, dead)).toBeNull();
    const tabs = await loadTabs(store);
    expect(tabs.map((t) => t.tabId), 'one failed ping must not evict a tab').toEqual([7]);
    expect(tabs[0]!.misses).toBe(1);
  });

  it('the second consecutive failure is what evicts it, and it converges', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: 7, origin: ORIGIN, at: 1 });

    expect(await pickLiveTab(store, ORIGIN, dead)).toBeNull();
    expect(await loadTabs(store)).toHaveLength(1);
    expect(await pickLiveTab(store, ORIGIN, dead)).toBeNull();
    expect(await loadTabs(store), 'a tab that never answers does leave the list').toEqual([]);

    // A {ok:false} reply is a miss too, not a success.
    await rememberTab(store, { tabId: 8, origin: ORIGIN, at: 1 });
    expect(await pickLiveTab(store, ORIGIN, silent)).toBeNull();
    expect(await loadTabs(store)).toHaveLength(1);
    expect(await pickLiveTab(store, ORIGIN, silent)).toBeNull();
    expect(await loadTabs(store)).toEqual([]);
  });

  it('a successful ping clears the count, so the tab gets its full budget again', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: 9, origin: ORIGIN, at: 1 });

    expect(await pickLiveTab(store, ORIGIN, dead)).toBeNull();
    expect((await loadTabs(store))[0]!.misses).toBe(1);

    // The renderer comes back.
    expect((await pickLiveTab(store, ORIGIN, alive))?.tabId).toBe(9);
    expect((await loadTabs(store))[0]!.misses).toBe(0);

    // 🔴 The reset is real, not cosmetic: it takes two more misses to evict it.
    expect(await pickLiveTab(store, ORIGIN, dead)).toBeNull();
    expect((await loadTabs(store)).map((t) => t.tabId), 'a cleared tab is not evicted by the next single miss').toEqual([9]);
    expect((await loadTabs(store))[0]!.misses).toBe(1);
  });

  it('picking keeps the registry order, so which tab is tried first does not drift with failures', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: 1, origin: ORIGIN, at: 1 });
    await rememberTab(store, { tabId: 2, origin: ORIGIN, at: 2 });

    await pickLiveTab(store, ORIGIN, dead);
    expect((await loadTabs(store)).map((t) => t.tabId)).toEqual([2, 1]);
    expect((await loadTabs(store)).map((t) => t.misses)).toEqual([1, 1]);
  });

  it('a healthy registry is not rewritten on every pick', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: 5, origin: ORIGIN, at: 1 });
    const before = store.writes;
    expect((await pickLiveTab(store, ORIGIN, alive))?.tabId).toBe(5);
    expect(store.writes, 'a tab that was never missed costs no write').toBe(before);
    // for the same reason: forgetTab on a tab that is not there must not be reached.
    await forgetTab(store, 12345);
    expect((await loadTabs(store)).map((t) => t.tabId)).toEqual([5]);
  });

  it('the threshold is the named constant, and it is more than one', () => {
    expect(TAB_PING_MISSES_BEFORE_FORGET).toBe(2);
  });
});
