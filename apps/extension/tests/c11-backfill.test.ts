import { describe, it, expect } from 'vitest';
import { runBackfill, loadState, notWiredHttp, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { formatProgress, computeProgress } from '../lib/backfill/progress';
import { enqueueDebts, settleDebt } from '../lib/backfill/debts';
import { initialState, stateKey } from '../lib/backfill/types';
import { DEFAULT_DETAIL_PACE, DEFAULT_ENUM_PACE, type Clock } from '../lib/backfill/pace';
import { parseConversationListPage } from '../lib/backfill/enumerate';

/**
 * C11 backfill-leg skeleton tests.
 * 🔴 Everything uses synthetic fixtures — not one line really touches a platform endpoint (the
 *    http port is injected; without one it is notWiredHttp and calling it throws). No fixture
 *    contains a real conversation body.
 */

const ORIGIN = 'https://chatgpt.com';

/** A fake clock: sleep does not really wait, it only advances virtual time and records it. */
function fakeClock(): Clock & { sleeps: number[]; nowMs: () => number } {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  const sleeps: number[] = [];
  return {
    now: () => t,
    async sleep(ms: number) {
      sleeps.push(ms);
      t += ms;
    },
    sleeps,
    nowMs: () => t,
  };
}

/** A synthetic conversation list page. withTotal=false builds the "no denominator available" case. */
function listBody(ids: string[], total: number | null): string {
  const body: Record<string, unknown> = {
    items: ids.map((id) => ({ id, title: 'synthetic-fixture', create_time: 0 })),
    limit: 100,
    offset: 0,
  };
  if (total !== null) body.total = total;
  return JSON.stringify(body);
}

/** A synthetic conversation body. Satisfies chatgpt's mapping + current_node in lib/contract.ts. */
function detailBody(id: string): string {
  return JSON.stringify({
    title: 'synthetic-fixture',
    current_node: `${id}-node`,
    mapping: { [`${id}-node`]: { id: `${id}-node`, parent: null, children: [] } },
  });
}

function ids(n: number, from = 0): string[] {
  return Array.from({ length: n }, (_, i) => `conv-${String(i + from).padStart(4, '0')}-aaaaaaaa`);
}

/** A synthetic backend: a list of ids + paging, recording every URL that was requested. */
function fakeBackend(allIds: string[], opts: { total?: number | null; pageSize?: number } = {}) {
  const pageSize = opts.pageSize ?? 100;
  const total = opts.total === undefined ? allIds.length : opts.total;
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/backend-api/conversations') {
      const offset = Number(u.searchParams.get('offset') ?? 0);
      return { status: 200, text: listBody(allIds.slice(offset, offset + pageSize), total) };
    }
    const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
    return { status: 200, text: detailBody(id) };
  };
  const detailCalls = () => calls.filter((c) => c.includes('/backend-api/conversation/'));
  return { http, calls, detailCalls };
}

describe('C11 criterion 1 · stop-and-resume', () => {
  it('interrupted half way, a restart carries on from the breakpoint instead of from the start', async () => {
    const store = memoryStore();
    const backend = fakeBackend(ids(30));

    const run1 = await runBackfill({
      platform: 'chatgpt',
      origin: ORIGIN,
      scope: 'acct-fixture',
      store,
      http: backend.http,
      clock: fakeClock(),
      maxDetails: 3, // simulate "interrupted after 3 items"
    });
    console.log('[C11-1] run1 stopped =', run1.stopped, '| archived =', run1.archivedThisRun.join(','));
    console.log('[C11-1] run1 progress =', run1.progress);

    // The second entry is a brand-new run (new Pacer, new clock) sharing only the persisted state.
    const run2 = await runBackfill({
      platform: 'chatgpt',
      origin: ORIGIN,
      scope: 'acct-fixture',
      store,
      http: backend.http,
      clock: fakeClock(),
      maxDetails: 3,
    });
    console.log('[C11-1] run2 stopped =', run2.stopped, '| archived =', run2.archivedThisRun.join(','));
    console.log('[C11-1] run2 progress =', run2.progress);

    expect(run1.archivedThisRun).toHaveLength(3);
    expect(run2.archivedThisRun).toHaveLength(3);
    // Resuming from the breakpoint: run2 may not repeat a single one of run1's items
    expect(run2.archivedThisRun).not.toEqual(run1.archivedThisRun);
    expect(run2.state.archived).toHaveLength(6);
    expect(run2.state.pending).toHaveLength(24);
    // 🔴 W10 · This assertion changed from `run2.enumeratedPages === 0`, and the
    //    criterion it guards did not: "a restart carries on from the breakpoint
    //    instead of from the start" is still exactly what is asserted — only the
    //    evidence is now stronger. Two things moved at once, both deliberately:
    //      · a tick reads **at most one list page** (the body segment follows it
    //        in the same tick), so the first tick no longer pages the list to
    //        the end;
    //      · and the list is now declared finished by an **empty page**, not by
    //        `offset >= total` (see engine.ts: the total is not a stopping
    //        condition any more — a real account reported total=901 while
    //        holding 7,391 conversations).
    //    So run2 reads exactly one more list page — and it is the
    //    **continuation** (offset=30), which is what "resumed rather than
    //    restarted" means: a restart would have asked for offset=0 again.
    expect(run2.enumeratedPages).toBe(1);
    const listCalls = backend.calls.filter((u) => u.includes('/backend-api/conversations'));
    expect(listCalls.filter((u) => u.includes('offset=0'))).toHaveLength(1); // never asked for the first page twice
    expect(listCalls[listCalls.length - 1]).toContain('offset=30');
    // And with that empty page the enumeration really is finished — no third tick will page again.
    expect(run2.state.enumCursor.complete).toBe(true);
    const run3 = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture', store,
      http: backend.http, clock: fakeClock(), maxDetails: 0,
    });
    expect(run3.enumeratedPages).toBe(0);
  });
});

describe('C11 criterion 2 · fetch nothing twice', () => {
  it('an archived id is never enqueued again and its body is never fetched again', async () => {
    const store = memoryStore();
    const backend = fakeBackend(ids(10));

    const run1 = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture', store,
      http: backend.http, clock: fakeClock(), maxDetails: 4,
    });
    const firstBatch = backend.detailCalls().slice();

    // Simulate "a day later, enumerate again": reset the enumeration cursor and re-enumerate the same ids.
    const st = await loadState(store, 'chatgpt', 'acct-fixture');
    st.enumCursor = { offset: 0, complete: false };
    await store.save(stateKey('chatgpt', 'acct-fixture'), st);

    const run2 = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture', store,
      http: backend.http, clock: fakeClock(), maxDetails: 4,
    });

    console.log('[C11-2] re-enumerating 10 rows: newly enqueued =', run2.newDebts,
      '| blocked as already archived =', run2.skippedAlreadyArchived,
      '| blocked as already pending =', run2.skippedAlreadyPending);
    const secondBatch = backend.detailCalls().slice(firstBatch.length);
    console.log('[C11-2] run1 bodies fetched =', firstBatch.length, '; run2 bodies fetched =', secondBatch.length);
    const overlap = secondBatch.filter((u) => firstBatch.includes(u));
    console.log('[C11-2] intersection of the two body-fetch URL sets =', overlap.length);

    expect(run1.archivedThisRun).toHaveLength(4);
    expect(run2.skippedAlreadyArchived).toBe(4); // 4 already settled => never enqueued again
    expect(run2.skippedAlreadyPending).toBe(6); // 6 still owed => not enqueued twice either
    expect(run2.newDebts).toBe(0);
    expect(overlap).toHaveLength(0);
    // Not one archived id came back into the debt set
    for (const done of run1.archivedThisRun) {
      expect(run2.state.pending).not.toContain(done);
    }
  });

  it('enqueueDebts blocks duplicates at the pure-function level', () => {
    const st = initialState('chatgpt', 'acct-fixture');
    enqueueDebts(st, ['a', 'b', 'c']);
    settleDebt(st, 'b');
    const added = enqueueDebts(st, ['a', 'b', 'c', 'd']);
    expect(added).toEqual(['d']); // a and c are already pending, b is already settled
    expect(st.pending).toEqual(['a', 'c', 'd']);
    expect(st.archived).toEqual(['b']);
  });
});

describe('C11 criterion 3 · never show a percentage when the denominator is unknown (a veto)', () => {
  it('the list endpoint gives no total ⇒ the output contains no % character', async () => {
    const store = memoryStore();
    // total is missing; an empty page is appended at the end so enumeration knows it is done.
    const backend = fakeBackend(ids(5), { total: null, pageSize: 5 });

    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'no-total', store,
      http: backend.http, clock: fakeClock(), maxDetails: 2,
    });

    console.log('[C11-3] progress text with an unknown denominator =>', run.progress);
    expect(run.state.totalSource).toBe('unknown');
    expect(run.state.totalKnown).toBeNull();
    expect(computeProgress(run.state).percent).toBeNull();
    expect(run.progress).not.toContain('%');
    expect(run.progress).toContain('total unknown');
  });

  it('the whole enumeration segment fails (not one page fetched) ⇒ still no %', async () => {
    const store = memoryStore();
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'enum-dead', store,
      http: async () => ({ status: 429, text: '' }),
      clock: fakeClock(),
    });
    console.log('[C11-3] progress text while enumeration is rate-limited =>', run.progress);
    expect(run.stopped).toBe('halted');
    expect(run.progress).not.toContain('%');
  });

  it('all three bogus denominators are refused: a non-API total / a non-positive integer / archived exceeding total', () => {
    const a = initialState('chatgpt', 'x');
    a.totalKnown = 100; // there is a number, but it did not come from the API
    a.totalSource = 'unknown';
    expect(computeProgress(a).percent).toBeNull();
    expect(formatProgress(a)).not.toContain('%');

    const b = initialState('chatgpt', 'x');
    b.totalSource = 'response-total';
    b.totalKnown = 0;
    expect(computeProgress(b).percent).toBeNull();
    expect(formatProgress(b)).not.toContain('%');

    const c = initialState('chatgpt', 'x');
    c.totalSource = 'response-total';
    c.totalKnown = 2;
    c.archived = ['a', 'b', 'c'];
    expect(computeProgress(c).percent).toBeNull();
    expect(formatProgress(c)).not.toContain('%');
  });

  it('a percentage is allowed only when the denominator really came from the API', () => {
    const st = initialState('chatgpt', 'x');
    st.totalSource = 'response-total';
    st.totalKnown = 1000;
    st.archived = ids(120);
    st.pending = ids(10, 900);
    console.log('[C11-3] progress text with a trustworthy denominator =>', formatProgress(st));
    expect(computeProgress(st).percent).toBe(12);
    expect(formatProgress(st)).toContain('12%');
  });
});

describe('C11 criterion 4 · throttling takes effect (enumeration and body-fetching paced separately)', () => {
  it('bodies go at 20s each and enumeration at 2s per page, the two segments not interfering', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    // 6 rows => 3 pages of 2, plus the empty page that confirms the list is finished.
    const backend = fakeBackend(ids(6), { pageSize: 2 });
    const tick = (maxDetails: number) => runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'pace', store,
      http: backend.http, clock, maxDetails,
    });

    // 🔴 W10 · The tick is now "one list page, then this tick's body budget", so a
    //    3-page list is three ticks rather than one loop. The criterion this case
    //    guards is untouched — each segment makes up **its own** interval from the
    //    persisted anchor and neither borrows the other's — and it is asserted on
    //    exactly the seams the production alarm drives: first the list pages (with
    //    the body budget at 0 so nothing else advances the clock), then the bodies
    //    once the list is finished.
    const listTicks = [await tick(0), await tick(0), await tick(0), await tick(0)];
    const enumWaits = listTicks.flatMap((r) => r.paceTrace.enumerate);
    // The first page does not wait; every later one makes up the full 2000ms.
    expect(enumWaits).toEqual([0, 2000, 2000, 2000]);
    // The list segment and the body segment do not touch each other's pacing.
    expect(listTicks.every((r) => r.paceTrace.detail.length === 0)).toBe(true);
    expect(listTicks[listTicks.length - 1]!.state.enumCursor.complete).toBe(true);

    const run = await tick(4);
    console.log('[C11-4] defaults: enumerate', DEFAULT_ENUM_PACE.minIntervalMs, 'ms/page; detail',
      DEFAULT_DETAIL_PACE.minIntervalMs, 'ms/item, daily cap', DEFAULT_DETAIL_PACE.maxPerDay);
    console.log('[C11-4] actual enumerate wait sequence (ms) =', JSON.stringify(enumWaits));
    console.log('[C11-4] actual detail wait sequence (ms) =', JSON.stringify(run.paceTrace.detail));
    console.log('[C11-4] total virtual-clock advance =', clock.nowMs() - Date.parse('2026-08-17T00:00:00.000Z'), 'ms');

    // List finished ⇒ the body tick fetches no page at all (segment two alone).
    expect(run.paceTrace.enumerate).toEqual([]);
    // 4 bodies: the first does not wait, each later one makes up the full 20000ms
    expect(run.paceTrace.detail).toEqual([0, 20000, 20000, 20000]);
    // Nothing really waited: the virtual-clock advance = the sum of all sleeps
    expect(clock.sleeps.reduce((a, b) => a + b, 0)).toBe(6000 + 60000);
  });

  it('hitting the daily cap stops gently (not a halt — a daily-cap)', async () => {
    const store = memoryStore();
    const backend = fakeBackend(ids(10));
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'cap', store,
      http: backend.http, clock: fakeClock(),
      pace: { enumerate: DEFAULT_ENUM_PACE, detail: { minIntervalMs: 20_000, maxPerDay: 3 } },
    });
    console.log('[C11-4] daily cap of 3 => stopped =', run.stopped, '| fetched today =', run.state.detailToday.count);
    expect(run.stopped).toBe('daily-cap');
    expect(run.archivedThisRun).toHaveLength(3);
    expect(run.halted).toBeNull(); // a normal gentle pause should leave no halt trace
  });
});

describe('C11 · a rate limit / a shape change must stop with a trace', () => {
  it('a 429 while fetching a body => halt(rate-limited) persisted, and the next run refuses to continue', async () => {
    const store = memoryStore();
    const all = ids(5);
    let detailHits = 0;
    const http = async (url: string): Promise<HttpResponse> => {
      if (url.includes('/backend-api/conversations')) {
        return { status: 200, text: listBody(all, all.length) };
      }
      detailHits += 1;
      return detailHits >= 3 ? { status: 429, text: '' } : { status: 200, text: detailBody('x') };
    };

    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'limited', store, http, clock: fakeClock(),
    });
    console.log('[C11-halt] halt record =', JSON.stringify(run.halted));
    console.log('[C11-halt] progress text after the halt =>', run.progress);
    expect(run.stopped).toBe('halted');
    expect(run.halted?.reason).toBe('rate-limited');
    expect(run.progress).toContain('stopped');

    // The trace must be persistent: after a restart it does not retry against the platform on its own
    const again = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'limited', store,
      http: async () => { throw new Error('MUST NOT be called after halt'); },
      clock: fakeClock(),
    });
    expect(again.stopped).toBe('halted');
    expect(again.halted?.reason).toBe('rate-limited');
    console.log('[C11-halt] after restart =', again.stopped, '(no endpoint was hit again)');
  });

  it('the list shape changed => halt(shape-changed), no guessing and no silence', async () => {
    const store = memoryStore();
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'shape', store,
      http: async () => ({ status: 200, text: JSON.stringify({ conversations: [] }) }),
      clock: fakeClock(),
    });
    console.log('[C11-halt] unrecognised shape =>', JSON.stringify(run.halted));
    expect(run.halted?.reason).toBe('shape-changed');
    expect(run.progress).not.toContain('%');
  });

  it('the body shape changed => also halts (reusing the live leg matchesResponseShape)', async () => {
    const store = memoryStore();
    const all = ids(2);
    const http = async (url: string): Promise<HttpResponse> =>
      url.includes('/backend-api/conversations')
        ? { status: 200, text: listBody(all, all.length) }
        : { status: 200, text: JSON.stringify({ nodes: [], head: 'x' }) }; // missing mapping/current_node
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'shape2', store, http, clock: fakeClock(),
    });
    console.log('[C11-halt] body shape mismatch =>', JSON.stringify(run.halted));
    expect(run.halted?.reason).toBe('shape-changed');
  });

  it('no persistent storage => halts outright rather than pretending to run', async () => {
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'nostore', store: null,
      http: async () => { throw new Error('MUST NOT be called without storage'); },
      clock: fakeClock(),
    });
    console.log('[C11-halt] no store =>', JSON.stringify(run.halted));
    console.log('[C11-halt] progress text with no store =>', run.progress);
    expect(run.stopped).toBe('halted');
    expect(run.halted?.reason).toBe('storage-unavailable');
    expect(run.progress).not.toContain('%');
  });
});

describe('C11 · no wiring means no request, and the write-down exit has the live leg shape', () => {
  it('the default http port throws when called (a structural guarantee: with no login it will not even try)', async () => {
    await expect(notWiredHttp(`${ORIGIN}/backend-api/conversations?offset=0&limit=100`)).rejects.toThrow(
      /not wired/,
    );
  });

  it('the sink receives a CapturedFetch-shaped object that can go straight down the live leg write-down path', async () => {
    const store = memoryStore();
    const backend = fakeBackend(ids(1));
    const seen: Array<{ url: string; method: string; status: number; pageUrl?: string }> = [];
    await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'sink', store,
      http: backend.http, clock: fakeClock(),
      sink: (c) => { seen.push({ url: c.url, method: c.method, status: c.status, pageUrl: c.pageUrl }); },
    });
    console.log('[C11-sink] the sink received =', JSON.stringify(seen[0]));
    expect(seen).toHaveLength(1);
    expect(seen[0]!.method).toBe('GET');
    expect(seen[0]!.url).toContain('/backend-api/conversation/');
  });

  it('parseConversationListPage answers ok:false for every malformed input', () => {
    expect(parseConversationListPage('not json').ok).toBe(false);
    expect(parseConversationListPage('[]').ok).toBe(false);
    expect(parseConversationListPage('{"items":{}}').ok).toBe(false);
    expect(parseConversationListPage('{"items":[{"no_id":1}]}').ok).toBe(false);
    const good = parseConversationListPage('{"items":[{"id":"a"}],"total":7}');
    expect(good.ok && good.page.total).toBe(7);
  });
});
