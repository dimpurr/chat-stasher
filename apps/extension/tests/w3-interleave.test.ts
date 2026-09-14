/**
 * W10 · The list and the bodies are **interleaved**: one list page per tick, then
 * that tick's body budget.
 *
 * ## What was wrong (measured, not guessed)
 * A real account: backfill switched on, 50 minutes later, **not one body fetched**.
 * Not a hang — the engine logged `ran` on every tick. The structure was:
 *
 *     while (!enumCursor.complete) { one list page }   // list segment, runs to the end
 *     while (state.pending.length > 0) { one body }    // body segment, only after that
 *
 * so a heavy account (7,391 distinct conversations ≈ 74 pages) had to page through
 * its **entire** list — at 2 s per page, interrupted by every SW reclaim — before
 * the first body could be fetched. "The newest is archived live, the old is filled
 * in slowly, with a progress bar" is the product requirement; "wait for the whole
 * list" is not part of it.
 *
 * ## What these cases pin
 *  1. 🔴 **The first tick already delivers a body**, while the list cursor has
 *     moved exactly one page;
 *  2. over consecutive ticks the list finishes and every debt is fetched — with no
 *     id enqueued twice and no pacing interval bypassed;
 *  3. an API `total` **smaller than reality** (total=3, 10 rows in 5 pages) does
 *     not stop the listing early, and the progress line shows no percentage at all
 *     rather than a fabricated one.
 *
 * Every HTTP interaction is a synthetic fixture: this file never logs in, never
 * touches a real platform, and contains no real conversation body.
 */

import { describe, it, expect } from 'vitest';
import { loadState, runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { formatProgress, computeProgress, countsOf } from '../lib/backfill/progress';
import { backfillPlanFor, canBackfillDetail } from '../lib/backfill/enumerate';
import { DEFAULT_PACE, type Clock } from '../lib/backfill/pace';
import type { BackfillState } from '../lib/backfill/types';

const ORIGIN = 'https://chatgpt.com';
const LIST_PATH = '/backend-api/conversations';
const DETAIL_PATH = '/backend-api/conversation/';
const PAGE = 2;

/** A fake clock: sleep does not really wait, it only advances virtual time. */
function fakeClock(): Clock & { readonly nowMs: () => number } {
  let t = Date.parse('2026-09-14T00:00:00.000Z');
  return {
    now: () => t,
    sleep: async (ms: number) => { t += ms; },
    nowMs: () => t,
  };
}

function ids(n: number): string[] {
  return Array.from({ length: n }, (_, i) => `w10conv-${String(i).padStart(3, '0')}-aaaaaaaa`);
}

interface ServerOpts {
  ids: string[];
  /** The `total` the list endpoint reports. `null` = it reports none. */
  total: number | null;
  /** How many rows one page holds (the fixture's own page size). */
  pageSize: number;
  /** An id this page repeats, simulating "the same conversation enumerated twice". */
  dupeOnPage?: { page: number; id: string };
}

/** A synthetic ChatGPT backend. Records every URL it is asked for. */
function backend(opts: ServerOpts) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === LIST_PATH) {
      const offset = Number(u.searchParams.get('offset') ?? '0');
      const page = Math.floor(offset / opts.pageSize);
      const items = opts.ids.slice(offset, offset + opts.pageSize).map((id) => ({ id }));
      if (opts.dupeOnPage && opts.dupeOnPage.page === page) {
        items.push({ id: opts.dupeOnPage.id });
      }
      const body: Record<string, unknown> = { items, limit: opts.pageSize, offset };
      if (opts.total !== null) body.total = opts.total;
      return { status: 200, text: JSON.stringify(body) };
    }
    // The body route must satisfy chatgpt's responseShape (mapping + current_node).
    const id = decodeURIComponent(u.pathname.replace(DETAIL_PATH, ''));
    return {
      status: 200,
      text: JSON.stringify({ mapping: { n1: { id: 'n1' } }, current_node: 'n1', account_id: 'acct-w10' }),
    };
  };
  const listOffsets = () => calls
    .filter((c) => c.includes(LIST_PATH))
    .map((c) => Number(new URL(c).searchParams.get('offset')));
  const detailIds = () => calls
    .filter((c) => c.includes(DETAIL_PATH))
    .map((c) => decodeURIComponent(new URL(c).pathname.replace(DETAIL_PATH, '')));
  return { http, calls, listOffsets, detailIds };
}

interface TickOptions {
  store: ReturnType<typeof memoryStore>;
  http: (url: string) => Promise<HttpResponse>;
  clock: Clock;
  scope: string;
  maxDetails?: number;
  sink?: (captured: { url: string; sessionId?: string }) => { saved: boolean };
}

/** One tick, exactly as the production alarm drives it: `maxDetails` defaults to 1. */
function tick(opts: TickOptions) {
  return runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope: opts.scope,
    store: opts.store,
    http: opts.http,
    clock: opts.clock,
    listLimit: PAGE,
    maxDetails: opts.maxDetails ?? 1,
    sink: opts.sink as never,
    /**
     * 🔴 W16 · This file is about the **interleave** — how much of a tick the
     *    list page takes before the body gate gets its turn — and every one of
     *    its assertions is an exact number of milliseconds (`the wait is > 0`,
     *    `the wait is <= the interval`, and the total clock advance is exactly
     *    `9 × 20_000`). Those are assertions about the *seam*, not about the
     *    jitter, so the draw is pinned to the bottom of every band, where each
     *    gap is exactly the documented minimum — the same numbers this file was
     *    written against. The jittered case has its own file: tests/w3-jitter.test.ts.
     */
    random: () => 0,
  });
}

// ---------------------------------------------------------------------------
// 1 · 🔴 The whole point: the first tick delivers a body
// ---------------------------------------------------------------------------
describe('W10-1 · the first tick fetches a body while the list has only moved one page', () => {
  it('one list page, one body delivered, one page of cursor movement', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const all = ids(10);
    const be = backend({ ids: all, total: 10, pageSize: PAGE });
    const delivered: Array<{ id?: string }> = [];

    const report = await tick({
      store, http: be.http, clock, scope: 'acct-w10-first',
      sink: (c) => { delivered.push({ id: c.sessionId }); return { saved: true }; },
    });

    console.log('[W10-1] list offsets requested =', be.listOffsets());
    console.log('[W10-1] body ids fetched =', be.detailIds());
    console.log('[W10-1] delivered to the sink =', delivered);

    // 🔴 The sink really was called, and the debt really was cleared.
    expect(delivered).toHaveLength(1);
    expect(delivered[0]!.id).toBe(all[0]);
    expect(report.archivedThisRun).toEqual([all[0]]);
    expect(report.state.archived).toEqual([all[0]]);
    expect(report.state.pending).toEqual([all[1]]);   // the second row of page 1 is still owed

    // 🔴 And the list moved exactly one page: one request, cursor at PAGE.
    expect(be.listOffsets()).toEqual([0]);
    expect(report.enumeratedPages).toBe(1);
    expect(report.state.enumCursor.offset).toBe(PAGE);
    expect(report.state.enumCursor.complete).toBe(false);
    // Exactly one body request went out — the body budget was not widened by this change.
    expect(be.detailIds()).toEqual([all[0]]);
  });

  it('the control: the page cap is tied to "this plan has a body segment", not to every platform', () => {
    // 🔴 The interleave is bounded by "is there a body segment to starve". A
    //    list-only platform has none, and capping its list at one page per tick
    //    would end its tick in halt('detail-unsupported') with the rest of its
    //    history never named — a persisted halt stops every later tick, so the
    //    second page would never be read at all. The engine therefore asks the
    //    plan, and this pins the answer the plan table gives today.
    //    (tests/c27-pplx.test.ts drives the real list-only platform end to end.)
    const chatgpt = backfillPlanFor('chatgpt');
    const deepseek = backfillPlanFor('deepseek');
    const perplexity = backfillPlanFor('perplexity');
    expect(chatgpt && canBackfillDetail(chatgpt)).toBe(true);
    expect(deepseek && canBackfillDetail(deepseek)).toBe(true);
    expect(perplexity && canBackfillDetail(perplexity)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 2 · Consecutive ticks: the list finishes, every debt is fetched, nothing twice
// ---------------------------------------------------------------------------
describe('W10-2 · consecutive ticks converge, with no id enqueued twice and no interval bypassed', () => {
  it('the list finishes, all bodies are fetched, and the pacing intervals are respected across ticks', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const start = clock.nowMs();
    const all = ids(10);
    const be = backend({ ids: all, total: 10, pageSize: PAGE });

    const traces: Array<{ enumerate: number[]; detail: number[] }> = [];
    const newDebtsPerTick: number[] = [];
    const listOffsetsPerTick: number[] = [];

    // The production tick: `maxDetails` = 1, driven until it says the queue is empty.
    for (let i = 0; i < 40; i += 1) {
      const before = be.listOffsets().length;
      const report = await tick({ store, http: be.http, clock, scope: 'acct-w10-converge' });
      traces.push(report.paceTrace);
      newDebtsPerTick.push(report.newDebts);
      listOffsetsPerTick.push(be.listOffsets().length - before);
      if (report.stopped === 'queue-empty') break;
    }

    const state = (await loadState(store, 'chatgpt', 'acct-w10-converge'));
    console.log('[W10-2] list offsets per tick =', listOffsetsPerTick);
    console.log('[W10-2] page offsets requested, in order =', be.listOffsets());
    console.log('[W10-2] new debts per tick =', newDebtsPerTick);
    console.log('[W10-2] detail waits per tick (ms) =', traces.map((t) => t.detail));
    console.log('[W10-2] enumerate waits per tick (ms) =', traces.map((t) => t.enumerate));
    console.log('[W10-2] virtual clock advanced (ms) =', clock.nowMs() - start);

    // 🔴 At most one list page per tick — this is the change itself.
    expect(listOffsetsPerTick.every((n) => n <= 1)).toBe(true);
    // The list really finished, and it finished on the empty page (offset 10 —
    // the engine advances by the rows it was handed, and the last data page
    // carries the last two ids).
    expect(state.enumCursor.complete).toBe(true);
    expect(be.listOffsets()).toEqual([0, 2, 4, 6, 8, 10]);
    // Every conversation was named exactly once and fetched exactly once.
    expect(newDebtsPerTick.reduce((a, b) => a + b, 0)).toBe(all.length);
    expect(state.pending).toEqual([]);
    expect(state.archived).toEqual(all);
    expect(new Set(state.archived).size).toBe(all.length);
    expect(be.detailIds()).toEqual(all);
    expect(be.detailIds().length).toBe(new Set(be.detailIds()).size);

    // 🔴 Pacing was not bypassed. The instrument first: a detail wait is recorded
    //    for every body actually fetched (one per tick here).
    expect(traces.filter((t) => t.detail.length === 1)).toHaveLength(all.length);
    // The first body has no "previous one" to speak of ⇒ 0; every later one makes
    // up its interval from the persisted anchor, and never more than the interval.
    expect(traces[0]!.detail).toEqual([0]);
    for (const t of traces.slice(1)) {
      expect(t.detail).toHaveLength(1);
      expect(t.detail[0]!).toBeGreaterThan(0);
      expect(t.detail[0]!).toBeLessThanOrEqual(DEFAULT_PACE.detail.minIntervalMs);
    }
    // An enumerate wait exists for exactly the ticks that read a page.
    expect(traces.map((t) => t.enumerate.length)).toEqual(listOffsetsPerTick);
    // 🔴 And the bodies really are spread out, by exactly the interval: each
    //    interval is made up in full, split between the two segments of the tick
    //    that owed it (the list page takes its 2 s first, the body gate the rest).
    //    10 bodies ⇒ 9 intervals, and not one millisecond of them was skipped.
    expect(clock.nowMs() - start).toBe((all.length - 1) * DEFAULT_PACE.detail.minIntervalMs);
  });

  it('a page that repeats a row does not enqueue or fetch that conversation twice', async () => {
    // 🔴 "Fetch nothing twice" is the rule this guards, and it has to survive the
    //    interleave: the duplicate arrives on a page read by a *later tick* than
    //    the original, so the dedup has to work across ticks (it lives in the
    //    persisted debt set, not in a run-local map).
    const store = memoryStore();
    const clock = fakeClock();
    const all = ids(10);
    // Page index 2 (offset 4) carries ids[0] again — a row that was named two
    // ticks earlier, so the dedup has to reach across the tick boundary and not
    // through a run-local set.
    const be = backend({ ids: all, total: 10, pageSize: PAGE, dupeOnPage: { page: 2, id: all[0]! } });

    // "Blocked" = the row was recognised as already settled or already owed.
    const blockedPerTick: number[] = [];
    for (let i = 0; i < 40; i += 1) {
      const report = await tick({ store, http: be.http, clock, scope: 'acct-w10-dupe' });
      blockedPerTick.push(report.skippedAlreadyArchived + report.skippedAlreadyPending);
      if (report.stopped === 'queue-empty' && report.state.enumCursor.complete) break;
    }

    const state = (await loadState(store, 'chatgpt', 'acct-w10-dupe'));
    console.log('[W10-2] duplicate row · blocked rows per tick =', blockedPerTick);
    console.log('[W10-2] duplicate row · archived =', state.archived.length,
      'unique =', new Set(state.archived).size);
    console.log('[W10-2] duplicate row · body ids fetched =', be.detailIds());

    // The instrument first: an ordinary page blocks nothing.
    expect(blockedPerTick[0]).toBe(0);
    // The repeated row was recognised when it came round again — by then that
    // conversation had already been archived, so this tick reads the same id and
    // blocks it instead of naming it a second time.
    expect(blockedPerTick[2]).toBe(1);
    // No conversation is archived twice, and none is fetched twice.
    expect(new Set(state.archived).size).toBe(state.archived.length);
    expect(be.detailIds().length).toBe(new Set(be.detailIds()).size);
    expect(state.pending).toEqual([]);
    // And the listing still terminates. (The extra row shifts the row window the
    // offset pages over — a property of offset paging, untouched by W10 — so one
    // id falls outside the window; what this case guards is that nothing is
    // counted or fetched twice, not that a duplicated page yields ten rows.)
    expect(state.enumCursor.complete).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 3 · 🔴 total=3, really 10 rows: no early stop, and no fabricated percentage
// ---------------------------------------------------------------------------
describe('W10-3 · an API total smaller than reality neither truncates the listing nor becomes a denominator', () => {
  it('all 10 rows are listed although the API claims 3, and the progress line shows no % at all', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const all = ids(10);
    // 3 rows per page ⇒ offsets 0,3,6,9 and then the empty page at 12. The API
    // says 3 the whole way through — the real account's shape, scaled down
    // (measured: total=901, 7,391 distinct ids returned).
    const be = backend({ ids: all, total: 3, pageSize: 3 });

    let last: Awaited<ReturnType<typeof tick>> | null = null;
    for (let i = 0; i < 40; i += 1) {
      last = await tick({ store, http: be.http, clock, scope: 'acct-w10-total', maxDetails: 10 });
      // "queue-empty" alone is not enough: it only says no debt is owed *right now*,
      // and the list may still have pages left to name.
      if (last.stopped === 'queue-empty' && last.state.enumCursor.complete) break;
    }

    const state = (await loadState(store, 'chatgpt', 'acct-w10-total'));
    console.log('[W10-3] page offsets requested, in order =', be.listOffsets());
    console.log('[W10-3] enumCursor =', state.enumCursor, '| totalKnown =', state.totalKnown,
      '| totalSource =', state.totalSource);
    console.log('[W10-3] progress line =', formatProgress(countsOf(state)));

    // 🔴 It did not stop at the number the API printed: rows 3..9 were fetched too.
    //    (The engine advances the cursor by the rows it was handed — 3+3+3+1 — and
    //     the last, empty page confirms the end.)
    expect(be.listOffsets()).toEqual([0, 3, 6, 9, 10]);
    expect(state.enumCursor.complete).toBe(true);
    expect(state.archived).toEqual(all);
    expect(state.pending).toEqual([]);

    // 🔴 The total was disproved by measurement, and is not a denominator afterwards.
    expect(state.totalKnown).toBe(3);                    // what the API said (kept as the record)
    expect(state.totalSource).toBe('contradicted');

    const view = computeProgress(countsOf(state));
    expect(view.percent).toBeNull();
    expect(view.totalContradicted).toBe(true);
    expect(view.listed).toBe(10);                        // the measured row count, not the API's claim
    const line = formatProgress(countsOf(state));
    expect(line).not.toContain('%');
    expect(line).toContain('at least 10 listed');
    // The wording must say what is known, not claim the total was never given.
    expect(line).not.toContain('total unknown');
  });

  it('the control: with a total that is not contradicted, the same fixture does produce a percentage', async () => {
    // 🔴 The instrument has to be shown able to see the opposite, or "no %" proves
    //    nothing. Same 10 rows, same 5 pages; only `total` differs.
    const store = memoryStore();
    const clock = fakeClock();
    const all = ids(10);
    const be = backend({ ids: all, total: 10, pageSize: 3 });

    let last: Awaited<ReturnType<typeof tick>> | null = null;
    for (let i = 0; i < 40; i += 1) {
      last = await tick({ store, http: be.http, clock, scope: 'acct-w10-oktotal', maxDetails: 10 });
      if (last.stopped === 'queue-empty' && last.state.enumCursor.complete) break;
    }

    const state = (await loadState(store, 'chatgpt', 'acct-w10-oktotal'));
    console.log('[W10-3] control · progress line =', formatProgress(countsOf(state)));
    expect(state.totalSource).toBe('response-total');
    expect(computeProgress(countsOf(state)).percent).toBe(100);
    expect(formatProgress(countsOf(state))).toContain('100%');
  });

  it('the contradiction is sticky: a later page claiming a bigger total does not put the denominator back', async () => {
    // 🔴 A total that moves is itself evidence about how much the number is worth.
    //    Once rows in hand have outnumbered it, a later page reporting a bigger
    //    number must not restore it as a denominator.
    const store = memoryStore();
    const clock = fakeClock();
    const scope = 'acct-w10-sticky';
    // 🔴 W18 · The debt ids live in the debt store now, so "what is on disk" is read
    //    through the production load path rather than out of one storage key. The
    //    read itself still happens after the two ticks below, at the same point.
    const first = backend({ ids: ids(10), total: 3, pageSize: 3 });

    // Two pages are enough to disprove total=3 (offset 6 after page 2), and the
    // list is deliberately left unfinished so the next tick reads another page.
    await tick({ store, http: first.http, clock, scope, maxDetails: 1 });
    await tick({ store, http: first.http, clock, scope, maxDetails: 1 });
    const disproved = await loadState(store, 'chatgpt', 'acct-w10-sticky');
    expect(disproved.totalSource).toBe('contradicted');
    expect(disproved.enumCursor.complete).toBe(false);

    // The endpoint now claims a much larger total on the next page.
    const second = backend({ ids: ids(30), total: 500, pageSize: 3 });
    const after = await tick({ store, http: second.http, clock, scope, maxDetails: 1 });
    console.log('[W10-3] after a later page claiming total=500:', after.state.totalSource,
      '| totalKnown =', after.state.totalKnown, '| progress =', after.progress);
    expect(after.state.totalKnown).toBe(500);          // the newest claim is recorded…
    expect(after.state.totalSource).toBe('contradicted'); // …and still not trusted
    expect(after.progress).not.toContain('%');
  });
});
