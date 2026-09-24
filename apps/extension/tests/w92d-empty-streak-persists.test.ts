/**
 * W92d · The empty-body guard must survive runs, and an unproven conversation must
 * never be written off.
 *
 * W92b made "N empty bodies in a row" halt the leg, but kept N as a run-local `let`
 * and (for the below-K case) dropped each empty id on sight as a `detail-empty`
 * failure. The review of that commit (nm/R92b-grok.log §1–§2) measured two losses:
 *
 *   1. an interleaved transient stop (transport-error / rate-limited / daily-cap /
 *      `shouldAbort`) or a next-build re-decision discards the counter, so an
 *      endpoint answering empty for **every** conversation is never recognised and
 *      the whole queue is written off `detail-empty` a few at a time, forever — and
 *      enumeration is already complete, so nothing re-enqueues them;
 *   2. a `detail-tree-incomplete` / `detail-paged-unsupported` between two empties
 *      zeroed the counter in the same run, so the guard never fired at all.
 *
 * These tests pin the CTO's fix, one scenario per property:
 *   · alternating empty / transport-throw across runs ⇒ nothing dropped, the leg
 *     halts at `DETAIL_EMPTY_HALT_STREAK` with every empty still pending;
 *   · empty / empty / tree-incomplete repeated ⇒ halts, drops no empty;
 *   · [empty, full] ⇒ the full conversation is archived and only then is the empty
 *     one written off, as `detail-empty`;
 *   · a halt, then a **new build's** re-decision ⇒ the counter persists and nothing
 *     further is written off;
 *   · a legacy state with neither new field reads as 0 / [].
 *
 * 🔴 Every fixture is synthetic (fixture ids, fixture list/body JSON, an injected
 *    clock, an injected plan lookup and http port). No network, no real account, no
 *    conversation text.
 *
 * 🔴 Red-before-green: each `it` fails against commit 621c5a0 (`emptyStreak` /
 *    `parkedEmpty` / `detail-empty-parked` do not exist there; the empty branch drops
 *    the id and never persists a streak).
 */

import { describe, it, expect } from 'vitest';
import {
  DETAIL_EMPTY_HALT_STREAK,
  loadState,
  runBackfill,
  type HttpResponse,
  type HttpPort,
} from '../lib/backfill/engine';
import {
  CHATGPT_LIST_PATH,
  CHATGPT_PLAN,
  type BackfillEnumPlan,
} from '../lib/backfill/enumerate';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { replaceDebtSet } from '../lib/backfill/debt-store';
import { stateFrom, stateKey, type BackfillHeader } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';

/** A clock the test advances by hand, so a transient backoff can be waited out. */
function stepClock(start: number): Clock & { at: (ms: number) => void } {
  let t = start;
  return {
    now: () => t,
    async sleep(ms: number) { t += ms; },
    at: (ms: number) => { t = ms; },
  };
}

function listBody(ids: readonly string[]): string {
  return JSON.stringify({ items: ids.map((id) => ({ id })), total: ids.length });
}

function idOf(url: string): string {
  const path = new URL(url).pathname;
  return decodeURIComponent(path.split('/backend-api/conversation/')[1] ?? '');
}

/**
 * A body in the same synthetic shape the W92b suite uses (`mapping: {}` passes the
 * chatgpt shape gate). The tag in `current_node` is the one field the injected parser
 * below reads, so no real payload is needed to select an outcome.
 */
function bodyFor(id: string, tag: 'empty' | 'tree' | 'full'): string {
  return JSON.stringify({ mapping: {}, current_node: `${tag}-${id}` });
}

function planWithParser(): BackfillEnumPlan {
  return {
    ...CHATGPT_PLAN,
    parseDetailPage: (text: string) => {
      if (text.includes('"current_node":"empty-')) return { ok: true, outcome: 'detail-empty-unverified' } as const;
      if (text.includes('"current_node":"tree-')) return { ok: true, outcome: 'detail-tree-incomplete' } as const;
      return { ok: true, outcome: 'non-empty' } as const;
    },
  };
}

interface Backend {
  http: HttpPort;
  calls: string[];
  detailRequests: () => number;
}

/** A backend that answers the list, then alternates empty / transport-throw per detail request. */
function alternatingBackend(ids: readonly string[]): Backend {
  const calls: string[] = [];
  let detailRequests = 0;
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    if (new URL(url).pathname === CHATGPT_LIST_PATH) return { status: 200, text: listBody(ids) };
    detailRequests += 1;
    if (detailRequests % 2 === 1) return { status: 200, text: bodyFor(idOf(url), 'empty') };
    throw new Error('synthetic transport failure');
  };
  return { http, calls, detailRequests: () => detailRequests };
}

/** A backend with a per-id tag: the id's index in `ids` decides empty / tree / full. */
function taggedBackend(ids: readonly string[], tags: readonly ('empty' | 'tree' | 'full')[]): Backend {
  const calls: string[] = [];
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    if (new URL(url).pathname === CHATGPT_LIST_PATH) return { status: 200, text: listBody(ids) };
    const id = idOf(url);
    const index = ids.indexOf(id);
    if (index < 0) throw new Error('unexpected conversation');
    return { status: 200, text: bodyFor(id, tags[index]!) };
  };
  return { http, calls, detailRequests: () => calls.filter((c) => !c.includes(CHATGPT_LIST_PATH)).length };
}

function runOnce(
  store: BackfillStore,
  be: Backend,
  scope: string,
  clock: Clock,
  build?: string,
) {
  return runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope,
    store,
    http: be.http,
    clock,
    pace: {
      enumerate: { minIntervalMs: 0, maxPerDay: null },
      detail: { minIntervalMs: 0, maxPerDay: null },
    },
    random: () => 0,
    plans: (platform) => platform === 'chatgpt' ? planWithParser() : null,
    sink: (captured) => ({ saved: true, sessionId: captured.sessionId }),
    ...(build === undefined ? {} : { build }),
  });
}

const DAY = 24 * 3600_000;

describe('W92d · the empty streak is persisted and survives interleaved transient stops', () => {
  it('alternating empty / transport-throw across runs drops nothing and halts at K with every empty pending', async () => {
    const ids = ['e1-aaaaaaaa', 'e2-aaaaaaaa', 'e3-aaaaaaaa', 'e4-aaaaaaaa', 'e5-aaaaaaaa'];
    const scope = 'w92d-alternating';
    const store = memoryStore();
    const be = alternatingBackend(ids);
    const clock = stepClock(Date.parse('2026-09-24T00:00:00.000Z'));

    // Run 1: empty (parked, streak 1), then a transport throw (transient halt).
    const r1 = await runOnce(store, be, scope, clock);
    expect(r1.halted?.reason).toBe('transport-error');
    expect(r1.stopped).toBe('waiting-retry');
    expect(r1.state.emptyStreak).toBe(1);
    expect(r1.state.parkedEmpty).toEqual([ids[0]]);

    // Run 2: the same two events one conversation further on.
    clock.at(clock.now() + DAY);
    const r2 = await runOnce(store, be, scope, clock);
    expect(r2.halted?.reason).toBe('transport-error');
    // 🔴 The persisted counter is the whole point: W92b would have restarted at 0.
    expect(r2.state.emptyStreak).toBe(2);
    expect([...r2.state.parkedEmpty ?? []].sort()).toEqual([ids[0], ids[1]].sort());

    // Run 3: the third empty reaches the guard.
    clock.at(clock.now() + DAY);
    const r3 = await runOnce(store, be, scope, clock);
    expect(r3.stopped).toBe('halted');
    expect(r3.halted?.reason).toBe('detail-empty-unverified');
    expect(r3.state.emptyStreak).toBe(DETAIL_EMPTY_HALT_STREAK);

    // 🔴 Nothing was dropped, ever: no `detail-empty` receipt is claimed, and every
    //    one of the five conversations is still owed.
    expect(r3.failedThisRun).toEqual([]);
    expect((r3.state.failures ?? []).map((f) => f.reason)).not.toContain('detail-empty');
    expect([...r3.state.pending].sort()).toEqual([...ids].sort());
    const persisted = await loadState(store, 'chatgpt', scope);
    expect([...persisted.pending].sort()).toEqual([...ids].sort());
    expect(persisted.emptyStreak).toBe(DETAIL_EMPTY_HALT_STREAK);
  });
});

describe('W92d · a per-conversation outcome between empties no longer resets the streak', () => {
  it('empty / empty / tree-incomplete repeated halts at K and drops no empty', async () => {
    const ids = ['e1-aaaaaaaa', 'e2-aaaaaaaa', 't1-aaaaaaaa', 'e3-aaaaaaaa', 'e4-aaaaaaaa', 't2-aaaaaaaa'];
    const tags = ['empty', 'empty', 'tree', 'empty', 'empty', 'tree'] as const;
    const scope = 'w92d-tree-between';
    const be = taggedBackend(ids, tags);

    const report = await runOnce(memoryStore(), be, scope, stepClock(Date.parse('2026-09-24T00:00:00.000Z')));

    // The contract-change guard fires: the tree-incomplete between the empties no
    // longer zeroes the counter (R92b §1's "second split").
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('detail-empty-unverified');
    // Dropped nothing of the empties: the only failure is the tree-incomplete one,
    // which is a per-conversation fact and is not what this guard is about.
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['detail-tree-incomplete']);
    // Every empty is still owed (the three seen and the two not yet reached); the
    // tree-incomplete conversation (`t1`) is the only one that left pending, and it
    // left with its own receipt rather than as a `detail-empty`.
    const stillOwed = ids.filter((id) => id !== 't1-aaaaaaaa');
    expect([...report.state.pending].sort()).toEqual([...stillOwed].sort());
    for (const id of ids.filter((id) => id.startsWith('e'))) {
      expect(report.state.pending).toContain(id);
    }
    expect(report.state.emptyStreak).toBe(DETAIL_EMPTY_HALT_STREAK);
  });
});

describe('W92d · an empty id is written off only once a real body proves the endpoint works', () => {
  it('[empty, full] ⇒ the full conversation is archived, and only then is the empty one dropped as detail-empty', async () => {
    const ids = ['e1-aaaaaaaa', 'f1-aaaaaaaa'];
    const scope = 'w92d-empty-then-full';
    const store = memoryStore();
    const be = taggedBackend(ids, ['empty', 'full']);

    const report = await runOnce(store, be, scope, stepClock(Date.parse('2026-09-24T00:00:00.000Z')));

    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    // The real body is archived…
    expect(report.archivedThisRun).toEqual([ids[1]]);
    expect(report.state.archived).toEqual([ids[1]]);
    // …and that proof is what turns the parked empty into a recorded loss.
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['detail-empty']);
    expect(report.state.emptyStreak).toBe(0);
    expect(report.state.parkedEmpty).toEqual([]);
    expect(report.detailOutcomes).toEqual([
      { sessionId: ids[0], outcome: 'detail-empty-unverified', complete: false, at: expect.any(Number) },
    ]);
  });

  it('an empty with no proof behind it is never written off (the single-id case stops as detail-empty-parked)', async () => {
    const ids = ['e1-aaaaaaaa'];
    const scope = 'w92d-empty-alone';
    const be = taggedBackend(ids, ['empty']);

    const report = await runOnce(memoryStore(), be, scope, stepClock(Date.parse('2026-09-24T00:00:00.000Z')));

    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('detail-empty-parked');
    expect(report.state.pending).toEqual([ids[0]]);
    expect(report.failedThisRun).toEqual([]);
    // No request was issued for the parked id the second time round.
    expect(be.detailRequests()).toBe(1);
  });
});

describe('W92d · the streak survives a new build’s re-decision', () => {
  it('after a detail-empty halt, a re-decision re-halts without writing anything off', async () => {
    const ids = ['e1-aaaaaaaa', 'e2-aaaaaaaa', 'e3-aaaaaaaa'];
    const scope = 'w92d-build-redecision';
    const store = memoryStore();
    const be = taggedBackend(ids, ['empty', 'empty', 'empty']);
    const clock = stepClock(Date.parse('2026-09-24T00:00:00.000Z'));

    const r1 = await runOnce(store, be, scope, clock, 'w92d-build-A');
    expect(r1.stopped).toBe('halted');
    expect(r1.halted?.reason).toBe('detail-empty-unverified');
    expect(r1.halted?.build).toBe('w92d-build-A');
    expect(r1.state.emptyStreak).toBe(DETAIL_EMPTY_HALT_STREAK);

    // A different build re-decides the permanent record once (W59). The persisted
    // counter is not reset by that re-decision, so the very first empty re-halts.
    clock.at(clock.now() + DAY);
    const r2 = await runOnce(store, be, scope, clock, 'w92d-build-B');
    expect(r2.state.haltExpired, 'the older build’s record is re-decided once').toMatchObject({
      because: 'build',
      reason: 'detail-empty-unverified',
    });
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.reason).toBe('detail-empty-unverified');
    expect(r2.halted?.build).toBe('w92d-build-B');
    // Nothing further written off, and all three empties still owed.
    expect(r2.failedThisRun).toEqual([]);
    expect([...r2.state.pending].sort()).toEqual([...ids].sort());
  });
});

describe('W92d · a legacy state reads the new fields as 0 / []', () => {
  const legacyHeader = (scope: string): BackfillHeader => ({
    v: 2,
    platform: 'chatgpt',
    scope,
    totalKnown: 1,
    totalSource: 'response-total',
    enumCursor: { offset: 1, complete: true },
    pendingCount: 1,
    archivedCount: 0,
    detailOutcomes: [],
    detailToday: { day: '2026-09-24', count: 0 },
    failures: [],
    failuresDropped: 0,
    halted: null,
  });

  it('stateFrom turns absent emptyStreak / parkedEmpty into 0 / []', () => {
    const state = stateFrom(legacyHeader('w92d-legacy-pure'), [], []);
    expect(state.emptyStreak).toBe(0);
    expect(state.parkedEmpty).toEqual([]);
  });

  it('an engine run against a header with neither field parks the empty and writes the fields back', async () => {
    const scope = 'w92d-legacy-engine';
    const id = 'e1-aaaaaaaa';
    const store = memoryStore();
    // Exactly the pre-W92d layout on disk: header without the two fields, and the
    // pending id in the debt store (the authority for what is owed).
    await replaceDebtSet('chatgpt', scope, { pending: [id], archived: [], nextSeq: 2 });
    await store.save(stateKey('chatgpt', scope), legacyHeader(scope));
    const be = taggedBackend([id], ['empty']);

    const report = await runOnce(store, be, scope, stepClock(Date.parse('2026-09-24T00:00:00.000Z')));

    // Absent fields were read as 0 / []: the first empty parks and the run stops.
    expect(report.stopped).toBe('detail-empty-parked');
    expect(report.state.emptyStreak).toBe(1);
    expect(report.state.parkedEmpty).toEqual([id]);
    expect(report.state.pending).toEqual([id]);
    const persisted = await loadState(store, 'chatgpt', scope);
    expect(persisted.emptyStreak).toBe(1);
    expect(persisted.parkedEmpty).toEqual([id]);
  });
});
