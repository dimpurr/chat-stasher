/**
 * W92b · An empty conversation must not halt a platform's whole backfill.
 *
 * Measured on a live Claude leg (W92 §Task 5): an opened-but-never-sent
 * conversation answers HTTP 200 with `chat_messages: []` and no
 * `current_leaf_message_uuid`. C28's first rule halted the whole leg on that one
 * body, and because the halt leaves the debt at the head of pending (FIFO), every
 * later run halted on the same conversation — Claude archived nothing, and
 * Perplexity (`entries: []`) has the same shape.
 *
 * The rule these tests pin:
 *   1. one empty body is the per-conversation outcome `detail-empty`: the debt
 *      leaves pending with a failure receipt, nothing is archived, and the leg
 *      carries on to the next conversation;
 *   2. `DETAIL_EMPTY_HALT_STREAK` (3) empty bodies **in a row** in one run still
 *      halt the leg with `detail-empty-unverified`, because a whole endpoint
 *      answering empty is the contract change C28 was written for;
 *   3. any body that is not an unverified empty resets the streak, so a legitimate
 *      empty between two real bodies never accumulates.
 *
 * 🔴 Every fixture is synthetic ("full" here is the C28 synthetic body the whole
 *    existing suite uses — an empty `mapping` — not a real conversation). No
 *    request goes to any platform: the http port is injected, and it throws on
 *    any path it was not handed, so "the conversation behind the halt was never
 *    fetched" is proven by the run rather than described.
 */

import { describe, expect, it } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import {
  CHATGPT_LIST_PATH,
  CHATGPT_PLAN,
  type BackfillEnumPlan,
} from '../lib/backfill/enumerate';
import { describeFailureReason } from '../lib/backfill/failures';
import { memoryStore } from '../lib/backfill/store';
import { DETAIL_EMPTY_HALT_STREAK } from '../lib/backfill/engine';
import { t } from '../lib/i18n';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';

function fakeClock(): Clock {
  let now = Date.parse('2026-09-24T00:00:00.000Z');
  return { now: () => now, async sleep(ms: number) { now += ms; } };
}

function listBody(ids: string[]): string {
  return JSON.stringify({ items: ids.map((id) => ({ id })), total: ids.length });
}

/**
 * A synthetic body in the shape the chatgpt row accepts. The one field C28 made
 * meaningful is the one this fixture moves: `current_node` carries the word
 * `empty` for a conversation whose body is empty, and the plan's parser below
 * reads exactly that.
 */
function detailBody(id: string, empty: boolean): string {
  return JSON.stringify({ mapping: {}, current_node: `${empty ? 'empty' : 'full'}-${id}` });
}

/** The real chatgpt plan with one answer injected: the parser this test needs. */
function planWithParser(): BackfillEnumPlan {
  return {
    ...CHATGPT_PLAN,
    parseDetailPage: (text: string) => (text.includes('"current_node":"empty-')
      ? { ok: true, outcome: 'detail-empty-unverified' } as const
      : { ok: true, outcome: 'non-empty' } as const),
  };
}

/**
 * A backend for a named queue of conversations. `kinds[i]` says whether the i-th
 * id's body is empty. Every request is recorded, and a request for an id the
 * queue does not name throws — so "this leg sent no further request" is proven.
 */
function backend(ids: string[], kinds: readonly boolean[]) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === CHATGPT_LIST_PATH) return { status: 200, text: listBody(ids) };
    const id = decodeURIComponent(u.pathname.split('/backend-api/conversation/')[1] ?? '');
    const index = ids.indexOf(id);
    if (index < 0) throw new Error(`unexpected conversation ${id}`);
    return { status: 200, text: detailBody(id, kinds[index]!) };
  };
  return { http, calls, detailCallsFor: (id: string) => calls.filter((c) => c.includes(encodeURIComponent(id))) };
}

interface Outcome {
  stopped: string;
  haltedReason: string | null;
  archivedThisRun: string[];
  pending: string[];
  failures: string[];
  detailOutcomes: { sessionId: string; outcome: string; complete: boolean }[];
  fullIds: string[];
}

async function run(scope: string, ids: string[], kinds: readonly boolean[]): Promise<Outcome> {
  const be = backend(ids, kinds);
  const report = await runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope,
    store: memoryStore(),
    http: be.http,
    clock: fakeClock(),
    pace: {
      enumerate: { minIntervalMs: 0, maxPerDay: null },
      detail: { minIntervalMs: 0, maxPerDay: null },
    },
    plans: (platform) => platform === 'chatgpt' ? planWithParser() : null,
    sink: (captured) => ({ saved: true, sessionId: captured.sessionId }),
  });
  return {
    stopped: report.stopped,
    haltedReason: report.halted?.reason ?? null,
    archivedThisRun: report.archivedThisRun,
    pending: report.state.pending,
    failures: (report.state.failures ?? []).map((f) => f.reason),
    detailOutcomes: report.detailOutcomes.map((d) => ({
      sessionId: d.sessionId,
      outcome: d.outcome,
      complete: d.complete,
    })),
    // The ids the leg actually sent a body request for — the "was it fetched" proof.
    fullIds: ids.filter((id) => be.detailCallsFor(id).length > 0),
  };
}

const empty1 = 'w92b-e1-aaaaaaaa';
const empty2 = 'w92b-e2-aaaaaaaa';
const empty3 = 'w92b-e3-aaaaaaaa';
const full1 = 'w92b-f1-aaaaaaaa';
const full2 = 'w92b-f2-aaaaaaaa';

describe('W92b · an empty body is a per-conversation outcome, not a leg-wide halt', () => {
  it('[empty, full] ⇒ the full conversation is archived, the empty one is recorded detail-empty, and the leg does not halt', async () => {
    const out = await run('w92b-empty-then-full', [empty1, full1], [true, false]);

    expect(out.haltedReason).toBeNull();
    expect(out.stopped).toBe('queue-empty');
    // 🔴 The conversation behind the empty one is archived in the same run.
    expect(out.archivedThisRun).toEqual([full1]);
    expect(out.fullIds).toEqual([empty1, full1]);
    // 🔴 The empty id is neither archived nor left pending: it left the queue with
    //    a named receipt.
    expect(out.pending).toEqual([]);
    expect(out.failures).toEqual(['detail-empty']);
    // C28's receipt is still written — `complete:false` is the durable half.
    expect(out.detailOutcomes).toEqual([
      { sessionId: empty1, outcome: 'detail-empty-unverified', complete: false },
    ]);
  });

  it(`[empty×${DETAIL_EMPTY_HALT_STREAK}, full] ⇒ the leg halts with detail-empty-unverified and the full conversation is NOT fetched`, async () => {
    const ids = [empty1, empty2, empty3, full1];
    const out = await run('w92b-three-then-full', ids, [true, true, true, false]);

    // 🔴 The contract-change guard survives: three empties in a row is a halt.
    expect(out.haltedReason).toBe('detail-empty-unverified');
    expect(out.stopped).toBe('halted');
    // Nothing was archived, and the conversation behind the third empty was not fetched.
    expect(out.archivedThisRun).toEqual([]);
    expect(out.fullIds).toEqual([empty1, empty2, empty3]);
    // 🔴 W92d · All three empties are parked and still owed (no `detail-empty`
    //    failure is claimed while the endpoint is unproven), and `full1` is still
    //    pending behind them — the halt fired before it could be fetched.
    expect([...out.pending].sort()).toEqual([empty1, empty2, empty3, full1].sort());
    expect(out.failures).toEqual([]);
    expect(out.detailOutcomes).toEqual([
      { sessionId: empty1, outcome: 'detail-empty-unverified', complete: false },
      { sessionId: empty2, outcome: 'detail-empty-unverified', complete: false },
      { sessionId: empty3, outcome: 'detail-empty-unverified', complete: false },
    ]);
  });

  it('[empty, empty, full, empty, full] ⇒ no halt, because a non-empty body resets the streak', async () => {
    const ids = [empty1, empty2, full1, empty3, full2];
    const out = await run('w92b-reset', ids, [true, true, false, true, false]);

    expect(out.haltedReason).toBeNull();
    expect(out.stopped).toBe('queue-empty');
    // Both real conversations are archived; neither empty one is, and neither halts.
    expect(out.archivedThisRun).toEqual([full1, full2]);
    expect(out.pending).toEqual([]);
    expect(out.failures).toEqual(['detail-empty', 'detail-empty', 'detail-empty']);
    expect(out.detailOutcomes.map((d) => d.sessionId)).toEqual([empty1, empty2, empty3]);
    expect(out.detailOutcomes.every((d) => d.complete === false)).toBe(true);
  });
});

describe('W92b · the failure receipt reads as an observed fact, not a reason code', () => {
  it('describes detail-empty in the user\'s language and never prints the code', () => {
    // Mirrors the W22/W42 pattern: a reason code must not reach the user as one.
    expect(describeFailureReason('detail-empty')).toBe(t('failure.detailEmpty'));
    expect(describeFailureReason('detail-empty')).not.toContain('detail-empty');
  });
});
