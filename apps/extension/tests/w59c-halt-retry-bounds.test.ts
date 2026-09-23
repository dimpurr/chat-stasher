/**
 * W59c · **Round 3 of the stale-halt retry: the two bounds W59b left open, and the
 * burst it left reachable.**
 *
 * An adversarial review of W59b (nm/R59b-grok.log) found four things. Three of them are
 * cases here; the fourth — the weakened W44-2 table — is restored in
 * tests/w44-halt-capability.test.ts, next to the table it was weakened in.
 *
 *   1. **A capability expiry wrote no attempt at all.** W59b bounded the *build* class
 *      by writing the attempt marker before the platform was asked, and left the
 *      capability class exempt, on the grounds that its answer is recomputed from the
 *      plan table with no request. The requests are in the run that follows the expiry
 *      — the run the record was holding back — so a capability lift whose verdict never
 *      landed left `halted: null` on disk and the platform was asked on every later
 *      tick. Here: both classes suspend the record and spend the attempt, so a lift
 *      that fails leaves a record that holds.
 *   2. **A killed worker was read as an answered one.** `markScopeRetried` lands, the
 *      MV3 worker is reclaimed before the resolver returns, and the next tick — the
 *      page never asked again — ran the engine with the sentinel scope and wrote an
 *      `org-unresolved` stamped as this build's judgement. Here: a stop that has
 *      observed nothing about the account is not stamped, and the attempt it was
 *      reached under stays unspent.
 *   4. **The tick after the re-decision read the whole list.** The cap that made the
 *      re-decision gentle was `redecting ? 1 : canBackfillDetail(plan) ? 1 : Infinity`,
 *      so a list-only plan went straight back to 74 pages in one alarm wake. Here:
 *      `LIST_PAGES_PER_TICK` bounds every plan, and the ending that lets it not
 *      truncate is the same one W59b added.
 *
 * 🔴 Everything here is synthetic: fixture ids, fixture responses, an injected clock,
 *    an injected build id. No network, no real account, no conversation text.
 */

import { describe, it, expect } from 'vitest';

import {
  LIST_PAGES_PER_TICK,
  loadState,
  markScopeRetried,
  runBackfill,
  type HttpResponse,
  type HttpPort,
} from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { stateKey, type BackfillHeader } from '../lib/backfill/types';
import { DEFAULT_DETAIL_PACE, DEFAULT_ENUM_PACE, type Clock } from '../lib/backfill/pace';
import { TEST_BUILD_ID } from './i18n-harness';

const ORIGIN = 'https://chatgpt.com';
const CLAUDE_ORIGIN = 'https://claude.ai';
const PPLX_ORIGIN = 'https://www.perplexity.ai';
const T0 = Date.parse('2026-09-23T12:00:00.000Z');
/** A build that is not this one. */
const OLDER_BUILD = '0.1.0.9';

/** A clock whose time only moves when a test says so. `sleep` advances it, so the Pacers stay virtual. */
function stepClock(start: number): Clock & { at: (ms: number) => void } {
  let t = start;
  return {
    now: () => t,
    async sleep(ms: number) {
      t += ms;
    },
    at: (ms: number) => {
      t = ms;
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

function headerWith(platform: string, scope: string, claimed: Partial<BackfillHeader> = {}): BackfillHeader {
  return {
    v: 2,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pendingCount: 0,
    archivedCount: 0,
    detailToday: { day: '2026-09-23', count: 0 },
    halted: null,
    ...claimed,
  };
}

function opts(store: ReturnType<typeof memoryStore>, http: HttpPort, clock: Clock) {
  return {
    platform: 'chatgpt',
    origin: ORIGIN,
    store,
    http,
    clock,
    build: TEST_BUILD_ID,
    pace: { enumerate: DEFAULT_ENUM_PACE, detail: { ...DEFAULT_DETAIL_PACE, minIntervalMs: 0 } },
    random: () => 1,
  } as const;
}

/** An http port that blows up: on the paths asserted below, not one request may be attempted. */
const mustNotFetch: HttpPort = async (url: string) => {
  throw new Error(`MUST NOT be called (${url})`);
};

/** A wire this build does not read: `{}` is not a list. */
function unrecognisedList(): { http: HttpPort; calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    http: async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      return { status: 200, text: '{}' };
    },
  };
}

// ---------------------------------------------------------------------------
// 1 · A licence to fetch has to cost the same whether it is written or not
// ---------------------------------------------------------------------------

describe('W59c-1 · a lift whose verdict never landed leaves a record that holds', () => {
  /**
   * 🔴 The capability class, and the window W59b left open.
   *
   * On 6f639bf this fixture ends the run with `halted: null` on disk: the expiry
   * **deleted** the capability record before the run it lifted went to the wire, and
   * the verdict the run would have written is refused. So the leg finds no record on
   * the next tick, has nothing to judge, and asks the platform again — the same
   * per-tick question W59b removed for the other class, one class over.
   */
  it('🔴 a capability expiry whose verdict write is refused is not a licence to fetch again', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-capability-lift-refused';
    const key = stateKey('chatgpt', scope);
    await store.save(key, headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      // An older build's record of a capability this build has: `chatgpt` is a full
      // plan, so the record is the W44 case — it does not apply, and the run runs.
      halted: {
        reason: 'detail-unsupported',
        capability: 'list-only',
        at: T0 - 3_600_000,
        detail: 'synthetic: written by a build whose chatgpt plan had no body segment',
      },
    }));
    /**
     * 🔴 The premise, and the shape of the failure: the write that carries **this
     *    build's own verdict** is refused. Everything else lands — the expiry trace,
     *    and the attempt marker the expiry spends — so the run reaches the wire and
     *    then cannot write down what it found. An MV3 reclaim produces exactly the
     *    same disk state (the write is never reached), which is why it is the failure
     *    modelled rather than a hypothetical.
     */
    const real = store.save.bind(store);
    store.save = async (k: string, v: unknown): Promise<void> => {
      const header = v as { halted?: { build?: unknown } } | null;
      if (header?.halted?.build === TEST_BUILD_ID) throw new Error('storage.local refused the write');
      return real(k, v);
    };

    const be = unrecognisedList();
    const r1 = await runBackfill({ ...opts(store, be.http, clock), scope }).catch((e: unknown) => e as Error);
    expect(r1, 'the premise is a run that really did reach the wire').toBeInstanceOf(Error);
    expect(be.calls.length, 'the lift asked the platform once').toBe(1);

    // ---- what the refused write left behind.
    const afterKill = await loadState(store, 'chatgpt', scope);
    expect(afterKill.haltRetried?.build, 'the attempt is on disk before the wire is touched').toBe(TEST_BUILD_ID);

    /**
     * 🔴 Round 2 is the assertion. The attempt is spent, so the leg holds: the record
     *    is reported unchanged, and **nothing is asked** — not the platform (the port
     *    below blows up), and not a second lift of the same record.
     */
    const r2 = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.reason, 'the record stands, and it is the older build’s').toBe('detail-unsupported');
    // 🔴 Held, not re-lifted: the record on disk is the one the refused write left —
    //    same instant, still naming no build of its own — rather than a fresh verdict
    //    from this run.
    expect(r2.halted?.at).toBe(T0 - 3_600_000);
    expect(r2.halted?.build).toBeUndefined();
    expect(be.calls.length, 'a lift that could not write its verdict must not become a per-tick fetch').toBe(1);

    // ---- and a third tick is the same answer, not a slow re-ask.
    const r3 = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });
    expect(r3.stopped).toBe('halted');
    expect(be.calls.length).toBe(1);
  });

  it('🔴 and a lift that does write its verdict holds nothing', async () => {
    // The control, and the half that must not be lost: the hold above is bounded by
    // *this build's* attempt. Once the run's verdict lands — here, the same wire, a
    // working store — the record and the marker are both gone and the leg is the plan
    // table's again. A hold that survived a written verdict would stop a leg the
    // build's own plan table says can run, which is the W44 defect exactly.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-capability-lift-written';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      halted: {
        reason: 'detail-unsupported',
        capability: 'list-only',
        at: T0 - 3_600_000,
        detail: 'synthetic: written by a build whose chatgpt plan had no body segment',
      },
    }));

    const all = ids(1);
    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
      const id = decodeURIComponent(url.split('/backend-api/conversation/')[1]!.split('?')[0]!);
      return { status: 200, text: JSON.stringify({ title: 'synthetic-fixture', current_node: `${id}-node`, mapping: { [`${id}-node`]: { id: `${id}-node`, parent: null, children: [] } } }) };
    };
    const r1 = await runBackfill({ ...opts(store, http, clock), scope, maxDetails: 1 });

    expect(r1.state.haltExpired).toMatchObject({ because: 'capability' });
    expect(r1.halted, 'the re-decision found the condition gone').toBeNull();
    expect(r1.archivedThisRun).toEqual(all);
    const persisted = await loadState(store, 'chatgpt', scope);
    expect(persisted.halted, 'nothing is left suspended by a run that wrote its verdict').toBeNull();
    expect(persisted.haltRetried, 'and the attempt went with it').toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// 2 · "Never asked" may not be written down as "asked, and the answer was no"
// ---------------------------------------------------------------------------

describe('W59c-2 · a killed worker cannot convert “never asked” into a verdict', () => {
  /**
   * 🔴 The Claude scope path, and the window an MV3 reclaim opens.
   *
   * `markScopeRetried` is written **before** the resolver asks the page — that ordering
   * is what bounds the asking (W59b). It also means the marker cannot say whether the
   * question was ever answered: the write that records the answer is a later, separate
   * `storage.local` call, and a worker reclaimed in between leaves the marker alone on
   * disk.
   *
   * On 6f639bf the next tick does two things with that: `scopeRetryDue` reads the marker
   * and does not ask the page, and the engine — handed the sentinel scope — writes
   * `org-unresolved` **stamped as this build's judgement**. That record then holds for
   * the rest of this build's life (`haltExpiredBecause`), so the page is never asked
   * again and the popup says the account could not be named — a conclusion nobody
   * reached.
   */
  it('🔴 a stop that heard nothing about the account is not this build’s judgement', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-killed-ask';
    // The state a reclaim leaves: the attempt is on disk, and nothing else is.
    expect(await markScopeRetried(store, { platform: 'claude', scope, build: TEST_BUILD_ID })).toBe(true);

    const r = await runBackfill({
      ...opts(store, mustNotFetch, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope,
    });

    expect(r.stopped).toBe('halted');
    expect(r.halted?.reason, 'the leg still stops, and the popup still says what to do').toBe('org-unresolved');
    const persisted = await loadState(store, 'claude', scope);
    expect(persisted.halted?.build, 'no answer came back, so this build has no verdict to stamp').toBeUndefined();
    expect(
      persisted.haltRetried?.build,
      'and a stop that judged nothing does not spend the attempt it was reached under',
    ).toBe(TEST_BUILD_ID);

    // 🔴 A later build re-decides it: the record names no build, so it is not anybody's
    //    judgement — which is the difference between "we do not know" and a verdict.
    const later = await runBackfill({
      ...opts(store, mustNotFetch, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope,
      build: OLDER_BUILD,
    });
    expect(later.state.haltExpired, 'a record nobody judged is re-decided, and the page asked again').toBeDefined();
  });

  it('🔴 a refusal the resolver did write stays this build’s, and holds', async () => {
    // The control: when the page **did** answer, the write-back stamps this build and
    // the leg is held by it — the state W31c relies on, and the one the fix above must
    // not disturb. `recordBackfillHalt` is the production writer for that answer.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-answered-ask';
    const { recordBackfillHalt } = await import('../lib/backfill/engine');
    expect(await recordBackfillHalt(store, {
      platform: 'claude', scope, reason: 'org-unresolved', detail: 'synthetic: the page named no organization',
    })).toBe(true);

    const r = await runBackfill({
      ...opts(store, mustNotFetch, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope,
    });
    expect(r.stopped).toBe('halted');
    const persisted = await loadState(store, 'claude', scope);
    expect(persisted.halted?.build, 'an answer this build heard is its own judgement').toBe(TEST_BUILD_ID);
    expect(persisted.haltRetried, 'and the attempt was spent on the question that produced it').toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// 4 · The tick after the re-decision
// ---------------------------------------------------------------------------

describe('W59c-4 · a list-only plan reads a bounded number of pages per tick', () => {
  /** Perplexity: a real list segment, no body segment. The plan whose ordinary tick was uncapped. */
  function perplexity(store: ReturnType<typeof memoryStore>, clock: Clock, scope: string, pages: string[]) {
    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      // Page N is pages[N-1]; running past the fixture returns the empty page, which is
      // this platform's own end-of-list signal.
      return { status: 200, text: pages[calls.length - 1] ?? '[]' };
    };
    return {
      calls,
      run: {
        ...opts(store, http, clock),
        platform: 'perplexity',
        origin: PPLX_ORIGIN,
        scope,
        listLimit: 2,
      },
    };
  }

  function onlyPage(from: number): string {
    return JSON.stringify([
      { slug: `pplx-${String(from).padStart(4, '0')}-aaaaaaaa` },
      { slug: `pplx-${String(from + 1).padStart(4, '0')}-aaaaaaaa` },
    ]);
  }

  /**
   * 🔴 The burst, and the number that ends it.
   *
   * Twelve full pages then the empty one. On 6f639bf a single ordinary tick reads all
   * thirteen (`listPagesThisTick` is `Infinity` for this plan) — the burst the task's
   * "automation must be gentle" rule forbids, and the reason the cap cannot be left to
   * the re-decision tick alone. With the cap, tick 1 reads `LIST_PAGES_PER_TICK` pages
   * and ends on its budget; the cursor carries; tick 2 reads the rest and reaches the
   * same `detail-unsupported` stop with the whole list on disk.
   */
  it('🔴 one alarm wake is not the whole list, and the cursor carries the rest', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-listonly-cap';
    const pages = Array.from({ length: 12 }, (_, i) => onlyPage(i * 2 + 1));
    const { calls, run } = perplexity(store, clock, scope, pages);

    const r1 = await runBackfill(run);
    // 🔴 Written against the fixture's size rather than against the constant, so what
    //    this fails on is the burst itself: on 6f639bf one tick reads all thirteen
    //    pages, and `13 < 12` is the assertion that says so.
    expect(calls.length, 'the whole list is not one tick’s budget').toBeLessThan(pages.length);
    expect(calls.length, 'and a budget of zero requests is not a budget').toBeGreaterThan(0);
    expect(calls.length, 'and the bound is the product’s own number').toBe(LIST_PAGES_PER_TICK);
    // Perplexity's cursor advances by `listLimit` per page, so this is "the cursor is at
    // the end of what was read" without naming the cap.
    expect(r1.state.enumCursor.offset).toBe(calls.length * 2);
    expect(r1.state.enumCursor.complete, 'nothing was read to its end, so nothing says it was').toBe(false);
    /**
     * 🔴 The ending matters as much as the cap. This plan's tick ends in
     *    `halt('detail-unsupported')`, and that record — written back stamped with this
     *    build — applies from the next tick on. A capped tick that reached it would cut
     *    the list off at page `LIST_PAGES_PER_TICK` for good, with the debts of every
     *    later page never even recorded. So the capped tick ends on its budget instead:
     *    nothing is claimed, and the next tick carries on from the same cursor.
     */
    expect(r1.stopped).toBe('budget-exhausted');
    expect(r1.halted, 'a list-only plan must not be halted mid-list').toBeNull();
    const persisted = await loadState(store, 'perplexity', scope);
    expect(persisted.halted, 'and it must not be halted on disk either').toBeNull();
    expect(persisted.enumCursor.truncated, 'the cap is not a truncation the loop recorded').toBeUndefined();

    // ---- the next tick: the rest of the list, and the stop it was always going to reach.
    const r2 = await runBackfill(run);
    expect(calls.length, 'the rest of the list, read by the tick that could read it').toBe(pages.length + 1);
    expect(r2.halted?.reason).toBe('detail-unsupported');
    expect(r2.halted?.capability).toBe('list-only');
    expect(r2.state.enumCursor.truncated).toBe('empty-page-inferred');
    expect(r2.state.enumCursor.complete, 'an inferred ending is not a complete enumeration').toBe(false);

    // ---- and from here the record applies: the third tick is free.
    const after = calls.length;
    const r3 = await runBackfill({ ...run, http: mustNotFetch });
    expect(r3.stopped).toBe('halted');
    expect(calls.length).toBe(after);
  });

  it('🔴 a plan that can fetch bodies is unchanged by the cap', async () => {
    // The control: this plan was already capped at one page per tick (W10), which is
    // below `LIST_PAGES_PER_TICK`, so the new bound must change nothing about it — and
    // in particular it must not take the list-only detour above.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-capable-unchanged';
    const all = ids(1);
    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
      const id = decodeURIComponent(url.split('/backend-api/conversation/')[1]!.split('?')[0]!);
      return { status: 200, text: JSON.stringify({ title: 'synthetic-fixture', current_node: `${id}-node`, mapping: { [`${id}-node`]: { id: `${id}-node`, parent: null, children: [] } } }) };
    };
    const r1 = await runBackfill({ ...opts(store, http, clock), scope, maxDetails: 1 });

    expect(r1.stopped).not.toBe('budget-exhausted');
    expect(r1.halted).toBeNull();
    expect(calls.filter((u) => u.includes('/backend-api/conversations')).length, 'one list page').toBe(1);
    expect(r1.archivedThisRun).toEqual(all);
  });
});

// ---------------------------------------------------------------------------
// The reason vocabulary the classification above rests on
// ---------------------------------------------------------------------------

describe('W59c · the number this round introduced', () => {
  it('🔴 is small, finite, and below the lists it has to bound', () => {
    // A tripwire rather than a behaviour: `Infinity` is what this number replaced, and
    // a "budget" that no list can exceed is not a bound. The account W10 measured had
    // 74 pages; the number has to be small enough that reading them all is a number of
    // alarm wakes rather than one burst, and it is asserted here so that raising it to
    // `Infinity` — or to something list-sized — fails a test that says why.
    expect(Number.isFinite(LIST_PAGES_PER_TICK)).toBe(true);
    expect(LIST_PAGES_PER_TICK).toBeGreaterThanOrEqual(1);
    expect(LIST_PAGES_PER_TICK).toBeLessThanOrEqual(10);
  });
});
