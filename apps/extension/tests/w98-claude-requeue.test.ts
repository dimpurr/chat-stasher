/**
 * W98 · Recover the Claude conversations the pre-W92 walk dropped.
 *
 * The pre-W92 branch-root rule refused every real body and the engine then dropped
 * each refused id (`dropDebt`), while the enumeration cursor stayed `complete` — so
 * the list would never be read again and those conversations could never be
 * backfilled (nm/W95-OUT.md: 50 `detail-tree-incomplete` receipts, 147 ids dropped).
 *
 * The fix under test is a versioned, one-time re-enumeration: the first run of a
 * Claude scope whose ledger predates the W92 fix resets that scope's enumeration
 * cursor once and records a marker, so the ordinary list read brings the dropped ids
 * back through `enqueueDebts` (which skips ids already pending or archived).
 *
 * Every scenario below is synthetic (invented org, conversation ids and JSON), the
 * clock and http port are injected, and the backend **throws** on any path it was
 * not given, so "no request was sent" is proven by the run. No network, no real
 * account, no conversation text.
 *
 * 🔴 Red-before-green: on `main` nothing reads the list again for a `complete`
 *    scope, so `newDebts` is 0 and the three unseen ids never enter `pending`.
 */

import { describe, expect, it } from 'vitest';
import { loadState, runBackfill, type HttpResponse, type HttpPort } from '../lib/backfill/engine';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { replaceDebtSet } from '../lib/backfill/debt-store';
import { stateKey, type BackfillHeader } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://claude.ai';
/** A synthetic organization id in the shape `isClaudeOrgId` accepts. */
const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const LIST_PATH = `/api/organizations/${ORG}/chat_conversations`;

const ARCHIVED = ['a1-aaaaaaaa', 'a2-aaaaaaaa', 'a3-aaaaaaaa'];
const PENDING = ['p1-aaaaaaaa', 'p2-aaaaaaaa'];
const UNSEEN = ['u1-aaaaaaaa', 'u2-aaaaaaaa', 'u3-aaaaaaaa'];
const LIST = [...ARCHIVED, ...PENDING, ...UNSEEN];
/** Make the 8-row page a *full* page for this plan, so no short-page inference ends the listing. */
const LIST_LIMIT = LIST.length;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

/** A fixed clock: every migration timestamp in one test is a known number. */
function fixedClock(at: number): Clock {
  return { now: () => at, async sleep() { /* no real waiting in these tests */ } };
}

/** A bare-array list body, as claude.ai returns it. */
function listPage(ids: readonly string[]): string {
  return JSON.stringify(ids.map((uuid) => ({ uuid, name: `synthetic-${uuid}` })));
}

interface Backend {
  http: HttpPort;
  /** The `offset` of every list request, in order — the evidence that a cursor moved or was reset. */
  listOffsets: number[];
}

/** Answers the list by `offset`; **throws** on any path it was not given. */
function claudeBackend(pages: Record<number, string>): Backend {
  const listOffsets: number[] = [];
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    const u = new URL(url);
    if (u.pathname !== LIST_PATH) throw new Error(`unexpected path ${u.pathname}`);
    const offset = Number(u.searchParams.get('offset') ?? '0');
    listOffsets.push(offset);
    return { status: 200, text: pages[offset] ?? '[]' };
  };
  return { http, listOffsets };
}

interface Seed {
  pending: string[];
  archived: string[];
  cursor: BackfillHeader['enumCursor'];
  reenumerated?: Record<string, number>;
}

/** Write a pre-existing Claude ledger: the debt set (the authority) plus its header. */
async function seed(store: BackfillStore, scope: string, s: Seed): Promise<void> {
  await replaceDebtSet('claude', scope, {
    pending: s.pending,
    archived: s.archived,
    nextSeq: s.pending.length + s.archived.length + 1,
    // 🔴 W113 · These fixtures are about the re-enumeration, not about times. No time is recorded for any
    //    id here, which is exactly what a ledger written before the time field existed looks like.
    times: new Map(),
  });
  const header: BackfillHeader = {
    v: 2,
    platform: 'claude',
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: s.cursor,
    pendingCount: s.pending.length,
    archivedCount: s.archived.length,
    detailToday: { day: '2026-09-24', count: 0 },
    halted: null,
    // Omitted entirely for the "legacy ledger without the marker" scenario.
    ...(s.reenumerated === undefined ? {} : { reenumerated: s.reenumerated }),
  };
  await store.save(stateKey('claude', scope), header);
}

function runClaude(
  store: BackfillStore,
  http: HttpPort,
  scope: string,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'claude',
    origin: ORIGIN,
    scope,
    store,
    http,
    clock: fixedClock(Date.parse('2026-09-24T00:00:00.000Z')),
    pace: NO_WAIT,
    random: () => 0,
    listLimit: LIST_LIMIT,
    // No bodies: this test is about the list re-read and the debt set, not the body walk.
    maxDetails: 0,
    ...extra,
  });
}

describe('W98 · a complete Claude scope that predates the fix re-enumerates once', () => {
  it('brings the three unseen ids back, duplicates nothing, and does not refetch the archived', async () => {
    const scope = ORG;
    const store = memoryStore();
    await seed(store, scope, { pending: PENDING, archived: ARCHIVED, cursor: { offset: 5, complete: true } });
    const be = claudeBackend({ 0: listPage(LIST), [LIST_LIMIT]: '[]' });

    const report = await runClaude(store, be.http, scope);

    // The cursor was reset and the list was really read from the start…
    expect(be.listOffsets).toEqual([0]);
    // …and the three ids the old walk dropped are owed again.
    expect(report.newDebts).toBe(UNSEEN.length);
    expect([...report.state.pending].sort()).toEqual([...PENDING, ...UNSEEN].sort());
    // Nothing was duplicated, and no archived id re-entered the queue.
    expect(new Set(report.state.pending).size).toBe(report.state.pending.length);
    expect(report.state.pending.some((id) => ARCHIVED.includes(id))).toBe(false);
    expect([...report.state.archived].sort()).toEqual([...ARCHIVED].sort());
    // 🔴 The archived ids are not merely absent from pending: they were never requested.
    //    (The backend throws on any path that is not the list, so a body fetch would fail this run.)

    // The one-time marker is on the header, with the moment it ran.
    expect(report.state.reenumerated?.['claude-detail-walk-w92']).toBe(Date.parse('2026-09-24T00:00:00.000Z'));
    const persisted = await loadState(store, 'claude', scope);
    expect(persisted.reenumerated?.['claude-detail-walk-w92']).toBe(Date.parse('2026-09-24T00:00:00.000Z'));
    expect([...persisted.pending].sort()).toEqual([...PENDING, ...UNSEEN].sort());
  });

  it('a second build run does not re-enumerate: the cursor continues, it does not go back to the first page', async () => {
    const scope = ORG;
    const store = memoryStore();
    await seed(store, scope, { pending: PENDING, archived: ARCHIVED, cursor: { offset: 5, complete: true } });
    const be = claudeBackend({ 0: listPage(LIST), [LIST_LIMIT]: '[]' });

    const first = await runClaude(store, be.http, scope);
    expect(be.listOffsets).toEqual([0]);
    const markerAfterFirst = first.state.reenumerated;

    // A different build runs next: the marker must stop the reset from repeating.
    const second = await runClaude(store, be.http, scope, { build: 'w98-build-B' });

    // 🔴 The second run read page 2 (offset 8), not page 1 again — proof it did not reset.
    expect(be.listOffsets).toEqual([0, LIST_LIMIT]);
    expect(second.state.enumCursor.complete).toBe(true);
    // The marker is not rewritten, and no unseen id was duplicated by the second read.
    expect(second.state.reenumerated).toEqual(markerAfterFirst);
    expect([...second.state.pending].sort()).toEqual([...PENDING, ...UNSEEN].sort());
  });

  it('a legacy ledger with no marker field still re-enumerates (absent reads as "never run")', async () => {
    const scope = ORG;
    const store = memoryStore();
    // No `reenumerated` key at all: exactly the pre-W98 layout.
    await seed(store, scope, { pending: PENDING, archived: ARCHIVED, cursor: { offset: 5, complete: true } });
    const be = claudeBackend({ 0: listPage(LIST), [LIST_LIMIT]: '[]' });

    const report = await runClaude(store, be.http, scope);

    expect(be.listOffsets).toEqual([0]);
    expect(report.newDebts).toBe(UNSEEN.length);
    expect(report.state.reenumerated?.['claude-detail-walk-w92']).toBeDefined();
  });
});

describe('W98 · other platforms are untouched', () => {
  it('a complete ChatGPT scope issues no list request and gains no Claude marker', async () => {
    const scope = 'w98-chatgpt-scope';
    const store = memoryStore();
    await replaceDebtSet('chatgpt', scope, { pending: ['c1-aaaaaaaa'], archived: ['c2-aaaaaaaa'], nextSeq: 3, times: new Map() });
    await store.save(stateKey('chatgpt', scope), {
      v: 2,
      platform: 'chatgpt',
      scope,
      totalKnown: null,
      totalSource: 'unknown',
      enumCursor: { offset: 2, complete: true },
      pendingCount: 1,
      archivedCount: 1,
      detailToday: { day: '2026-09-24', count: 0 },
      halted: null,
    } satisfies BackfillHeader);

    let calls = 0;
    const http: HttpPort = async (): Promise<HttpResponse> => {
      calls += 1;
      throw new Error('no request may be sent for a complete, non-Claude scope');
    };

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope,
      store,
      http,
      clock: fixedClock(Date.parse('2026-09-24T00:00:00.000Z')),
      pace: NO_WAIT,
      random: () => 0,
      maxDetails: 0,
    });

    expect(calls).toBe(0);
    expect(report.state.reenumerated).toEqual({});
    expect(report.state.enumCursor.complete).toBe(true);
  });
});
