/**
 * C28 · When the body endpoint succeeds but the content is empty, that empty must not be
 * taken as "there is nothing".
 *
 * All fixtures are synthetic: a test plan is injected into the engine only to demonstrate the
 * seam for a future detail parser. No production plan changes, no body route is added for
 * DeepSeek, and no request goes to any platform.
 */

import { describe, expect, it } from 'vitest';
import { loadState, runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { CHATGPT_LIST_PATH, CHATGPT_PLAN, type BackfillEnumPlan } from '../lib/backfill/enumerate';
import { memoryStore } from '../lib/backfill/store';
import { stateKey, type BackfillState } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const ID = 'conv-c28-empty-aaaaaaaa';

function fakeClock(): Clock {
  let now = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => now, async sleep(ms: number) { now += ms; } };
}

function listBody(): string {
  return JSON.stringify({ items: [{ id: ID }], total: 1 });
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

function backend(): (url: string) => Promise<HttpResponse> {
  return async (url: string) => new URL(url).pathname === CHATGPT_LIST_PATH
    ? { status: 200, text: listBody() }
    : { status: 200, text: detailBody() };
}

async function run(
  outcome: 'detail-empty-unverified' | 'detail-empty-confirmed',
  scope: string,
) {
  const store = memoryStore();
  const report = await runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope,
    store,
    http: backend(),
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
  it('HTTP 200 + correct shape + empty content ⇒ a named halt, the debt untouched and the body complete=false', async () => {
    const { report, persisted } = await run('detail-empty-unverified', 'acct-c28-unverified');

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('detail-empty-unverified');
    expect(report.halted?.reason).not.toBe('shape-changed');
    expect(report.state.pending).toEqual([ID]);
    expect(report.state.archived).toEqual([]);
    expect(report.archivedThisRun).toEqual([]);
    expect(report.detailOutcomes).toEqual([
      { sessionId: ID, outcome: 'detail-empty-unverified', complete: false, at: expect.any(Number) },
    ]);
    expect(report.state.detailOutcomes).toEqual(report.detailOutcomes);
    expect(persisted.pending).toEqual([ID]);
    expect(persisted.archived).toEqual([]);
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
