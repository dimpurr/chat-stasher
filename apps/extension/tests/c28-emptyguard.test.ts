/**
 * C28 · When the body endpoint succeeds but the content is empty, that empty must not be
 * taken as "there is nothing".
 *
 * All fixtures are synthetic: a test plan is injected into the engine only to demonstrate the
 * seam for a future detail parser. No production plan changes, no body route is added for
 * DeepSeek, and no request goes to any platform.
 */

import { describe, expect, it } from 'vitest';
import {
  DETAIL_EMPTY_HALT_STREAK,
  loadState,
  runBackfill,
  type HttpResponse,
} from '../lib/backfill/engine';
import { CHATGPT_LIST_PATH, CHATGPT_PLAN, type BackfillEnumPlan } from '../lib/backfill/enumerate';
import { memoryStore } from '../lib/backfill/store';
import { stateKey, type BackfillState } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const ID = 'conv-c28-empty-aaaaaaaa';
const ID2 = 'conv-c28-empty-bbbbbbbb';
const ID3 = 'conv-c28-empty-cccccccc';

function fakeClock(): Clock {
  let now = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => now, async sleep(ms: number) { now += ms; } };
}

function listBody(ids: string[]): string {
  return JSON.stringify({ items: ids.map((id) => ({ id })), total: ids.length });
}

/** The structure is good; C28 tests "good structure but empty content", not shape-changed. */
function detailBody(): string {
  return JSON.stringify({ mapping: {}, current_node: 'synthetic-node' });
}

function planFor(outcome: 'detail-empty-unverified' | 'detail-empty-confirmed'): BackfillEnumPlan {
  return {
    ...CHATGPT_PLAN,
    parseDetailPage: () => ({ ok: true, outcome }),
  };
}

function backend(ids: string[]): (url: string) => Promise<HttpResponse> {
  return async (url: string) => new URL(url).pathname === CHATGPT_LIST_PATH
    ? { status: 200, text: listBody(ids) }
    : { status: 200, text: detailBody() };
}

async function run(
  outcome: 'detail-empty-unverified' | 'detail-empty-confirmed',
  scope: string,
  ids: string[] = [ID],
) {
  const store = memoryStore();
  const report = await runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope,
    store,
    http: backend(ids),
    clock: fakeClock(),
    pace: {
      enumerate: { minIntervalMs: 0, maxPerDay: null },
      detail: { minIntervalMs: 0, maxPerDay: null },
    },
    plans: (platform) => platform === 'chatgpt' ? planFor(outcome) : null,
    sink: () => ({ saved: true, sessionId: ID }),
  });
  // 🔴 W18 · The ids are in the debt store now, so the persisted ledger is read
  //    through the production load path (`stateKey(...)` alone holds the header).
  const persisted = await loadState(store, 'chatgpt', scope);
  return { report, persisted };
}

describe('C28 · the empty-body guardrail', () => {
  /**
   * 🔴 W92b · **The K this file's streak test pins, pinned as a value.** The
   *    behaviour is asserted below by building exactly `DETAIL_EMPTY_HALT_STREAK`
   *    empty bodies; this line fails if the constant itself is moved, so the two
   *    cannot drift apart.
   */
  it('the guard is a named constant, and it is 3', () => {
    expect(DETAIL_EMPTY_HALT_STREAK).toBe(3);
  });

  it('one empty body is parked, not written off: nothing is archived, no failure is claimed, and the leg does not halt', async () => {
    const { report, persisted } = await run('detail-empty-unverified', 'acct-c28-unverified');

    // 🔴 W92d changed this from W92b. W92b dropped the id as a `detail-empty`
    //    failure on sight, and R92b §2 measured that as irreversible: enumeration is
    //    already complete, so nothing ever re-enqueues it. The id is now **parked** —
    //    it stays in `pending`, no failure is claimed, and the run stops only because
    //    every remaining id is parked (`detail-empty-parked`). It is written off with
    //    a `detail-empty` receipt only once a real body proves the endpoint works.
    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('detail-empty-parked');
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.archived).toEqual([]);
    // Still owed, and named as parked in the report's stop reason.
    expect(report.state.pending).toEqual([ID]);
    expect(persisted.pending).toEqual([ID]);
    // No failure is claimed while the endpoint is unproven: "empty" is a question.
    expect(report.failedThisRun).toEqual([]);
    expect(report.state.failures ?? []).toEqual([]);
    // C28's receipt is still written, and `complete:false` is the durable half:
    // "we saw nothing" is not "there was nothing".
    expect(report.detailOutcomes).toEqual([
      { sessionId: ID, outcome: 'detail-empty-unverified', complete: false, at: expect.any(Number) },
    ]);
    expect(report.state.detailOutcomes).toEqual(report.detailOutcomes);
    expect(persisted.archived).toEqual([]);
    expect(persisted.detailOutcomes).toEqual(report.detailOutcomes);
  });

  it(`🔴 ${DETAIL_EMPTY_HALT_STREAK} empty bodies in a row still halt, so a contract change is never silently written off`, async () => {
    const ids = [ID, ID2, ID3];
    const { report, persisted } = await run('detail-empty-unverified', 'acct-c28-streak', ids);

    // A whole endpoint answering empty is the contract change C28 was written for.
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('detail-empty-unverified');
    expect(report.halted?.reason).not.toBe('shape-changed');
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.archived).toEqual([]);
    // 🔴 W92d · The K-1 empties before the halt are **parked, not dropped**: no
    //    `detail-empty` failure is claimed while the endpoint is unproven, and every
    //    empty id is still owed. R92b §1 measured the old write-off as permanent.
    expect(report.failedThisRun).toEqual([]);
    expect([...report.state.pending].sort()).toEqual([...ids].sort());
    expect([...persisted.pending].sort()).toEqual([...ids].sort());
    expect(report.detailOutcomes).toEqual([
      { sessionId: ID, outcome: 'detail-empty-unverified', complete: false, at: expect.any(Number) },
      { sessionId: ID2, outcome: 'detail-empty-unverified', complete: false, at: expect.any(Number) },
      { sessionId: ID3, outcome: 'detail-empty-unverified', complete: false, at: expect.any(Number) },
    ]);
    expect(persisted.detailOutcomes).toEqual(report.detailOutcomes);
  });

  it('"it was a legitimately empty conversation" is a different named value, and is distinguishable from an unverified empty in both the report and the persisted state', async () => {
    const { report, persisted } = await run('detail-empty-confirmed', 'acct-c28-confirmed');

    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.detailOutcomes).toEqual([
      { sessionId: ID, outcome: 'detail-empty-confirmed', complete: true, at: expect.any(Number) },
    ]);
    expect(report.detailOutcomes[0]?.outcome).not.toBe('detail-empty-unverified');
    expect(report.state.detailOutcomes).toEqual(report.detailOutcomes);
    expect(persisted.detailOutcomes).toEqual(report.detailOutcomes);
    expect(report.state.pending).toEqual([]);
    expect(report.state.archived).toEqual([ID]);
  });
});
