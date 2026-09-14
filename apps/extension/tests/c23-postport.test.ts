/**
 * C23 · The backfill channel widened from "GET only" to "can express POST, with both method
 * and body constrained by an allowlist".
 *
 * 🔴 All fixtures are synthetic. Not one line touches a real platform endpoint; there is no
 * logged-in state and no real conversation body.
 * 🔴 **This change wires up no platform**: the production plan table still holds ChatGPT only
 * (GET), and kimi/gemini's parameters still have no source. The POST plans used below are
 * **built by the test itself** and injected through the engine's `plans` seam and tab-port's
 * `lookup` seam; neither of the two production call sites (background.ts's kickBackfill /
 * runAlarmTick) has either field.
 *
 * Three criteria:
 *   1. the GET path's behaviour is unchanged (a control test: argument count, wire message and
 *      fetch call all compared byte for byte)
 *   2. a POST can go out with a body, and both body and method are constrained by the allowlist
 *   3. a method or url outside the allowlist ⇒ refused, with a trace
 */

import { describe, it, expect, vi } from 'vitest';
import { runBackfill, type HttpPort, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { stateKey } from '../lib/backfill/types';
import {
  CHATGPT_PLAN,
  backfillPlanFor,
  detailRequestInit,
  listRequestInit,
  ALLOWED_BACKFILL_CONTENT_TYPES,
  ALLOWED_BACKFILL_METHODS,
  MAX_REQUEST_BODY_BYTES,
  type BackfillEnumPlan,
} from '../lib/backfill/enumerate';
import {
  BACKFILL_FETCH_MESSAGE,
  REFUSED_URL_REASON,
  checkBackfillRequest,
  handleBackfillMessage,
  serveBackfillFetch,
  tabHttpPort,
} from '../lib/backfill/tab-port';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const NO_WAIT = { enumerate: { minIntervalMs: 0, maxPerDay: null }, detail: { minIntervalMs: 0, maxPerDay: null } };

function fakeClock(): Clock {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

const IDS = ['conv-0000-aaaaaaaa', 'conv-0001-aaaaaaaa'];
const ID0 = IDS[0]!;

/** A synthetic list page in ChatGPT's shape. */
function listText(ids: string[]): string {
  return JSON.stringify({ items: ids.map((id) => ({ id })), total: ids.length });
}
/** A synthetic body in ChatGPT's shape (requiredPaths: mapping / current_node). */
const DETAIL_TEXT = JSON.stringify({ mapping: {}, current_node: 'synthetic' });

// ---------------------------------------------------------------------------
// A synthetic POST plan — 🔴 it exists only in the test and never enters BACKFILL_PLANS.
// The paths deliberately reuse ChatGPT's two, so that all three checks (same origin + platform
// table + path) pass and the case exercises **only the two new dimensions, method and body**.
// ---------------------------------------------------------------------------
const POST_PLAN: BackfillEnumPlan = {
  ...CHATGPT_PLAN,
  listUrl: (origin) => `${origin}${CHATGPT_PLAN.listPath}`,   // a POST segment's parameters are in the body; the URL carries no query
  listPost: {
    contentType: 'application/json',
    bodyKeys: ['offset', 'limit'],
    body: (_origin, offset, limit) => JSON.stringify({ offset, limit }),
  },
  detailPost: {
    contentType: 'application/json',
    bodyKeys: ['conversationId'],
    body: (_origin, id) => JSON.stringify({ conversationId: id }),
  },
};
const postLookup = (id: string): BackfillEnumPlan | null => (id === 'chatgpt' ? POST_PLAN : null);

// ===========================================================================
describe('C23-1 · the GET path is unchanged character for character (a control test)', () => {
  it('on a GET segment the engine calls http with **exactly one argument**, and CapturedFetch.method is still GET', async () => {
    const seen: Array<{ url: string; argc: number }> = [];
    const http: HttpPort = async function (this: unknown, url: string): Promise<HttpResponse> {
      // eslint-disable-next-line prefer-rest-params
      seen.push({ url, argc: arguments.length });
      return new URL(url).pathname === CHATGPT_PLAN.listPath
        ? { status: 200, text: listText(IDS) }
        : { status: 200, text: DETAIL_TEXT };
    };
    const methods: string[] = [];
    const report = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-1',
      store: memoryStore(), http, clock: fakeClock(), pace: NO_WAIT,
      sink: (c) => { methods.push(c.method); return { saved: true, sessionId: c.sessionId }; },
    });

    console.log('[C23-1] argument count of each http call:', seen.map((s) => s.argc));
    console.log('[C23-1] CapturedFetch.method:', methods);
    console.log('[C23-1] archived:', report.archivedThisRun, 'halted:', report.halted);

    expect(seen.every((s) => s.argc === 1)).toBe(true);   // 🔴 not one extra argument was passed
    expect(methods).toEqual(['GET', 'GET']);
    expect(report.archivedThisRun).toEqual(IDS);
    expect(report.halted).toBeNull();
    // The production chatgpt plan is still pure GET to this day — no POST was quietly filled in for it.
    expect(listRequestInit(CHATGPT_PLAN, ORIGIN, 0, 100)).toEqual({ method: 'GET' });
    expect(detailRequestInit(CHATGPT_PLAN, ORIGIN, ID0)).toEqual({ method: 'GET' });
    expect(backfillPlanFor('chatgpt')!.listPost).toBeUndefined();
    expect(backfillPlanFor('chatgpt')!.detailPost).toBeUndefined();
  });

  it('on a GET segment the wire message tabHttpPort sends is still {type, url}, byte for byte', async () => {
    const sent: unknown[] = [];
    const port = tabHttpPort(7, async (_id, msg) => { sent.push(msg); return { ok: true, status: 200, text: 'x' }; });
    await port(`${ORIGIN}${CHATGPT_PLAN.listPath}?offset=0&limit=100`);
    await port(`${ORIGIN}${CHATGPT_PLAN.detailPath}abc`, { method: 'GET' });
    console.log('[C23-1] wire message:', JSON.stringify(sent));
    expect(sent).toEqual([
      { type: BACKFILL_FETCH_MESSAGE, url: `${ORIGIN}${CHATGPT_PLAN.listPath}?offset=0&limit=100` },
      { type: BACKFILL_FETCH_MESSAGE, url: `${ORIGIN}${CHATGPT_PLAN.detailPath}abc` },
    ]);
  });

  it('on a GET segment the content script also calls fetch with exactly one argument', async () => {
    const argcs: number[] = [];
    const fetchImpl = async function (url: string) {
      // eslint-disable-next-line prefer-rest-params
      argcs.push(arguments.length);
      return { status: 200, text: async () => DETAIL_TEXT, url };
    };
    const reply = await handleBackfillMessage(
      { type: BACKFILL_FETCH_MESSAGE, url: `${ORIGIN}${CHATGPT_PLAN.detailPath}abc` },
      ORIGIN,
      fetchImpl as never,
    );
    console.log('[C23-1] content-script fetch argument counts:', argcs, 'reply:', JSON.stringify(await reply));
    expect(argcs).toEqual([1]);
    expect(await reply).toEqual({ ok: true, status: 200, text: DETAIL_TEXT });
  });
});

// ===========================================================================
describe('C23-2 · a POST can go out with a body, with body and method constrained by the allowlist', () => {
  it('engine → tabHttpPort → content script → fetch: the POST body reaches the page side intact', async () => {
    const fetched: Array<{ url: string; init: unknown }> = [];
    const fetchImpl = async (url: string, init?: unknown) => {
      fetched.push({ url, init });
      const body = JSON.parse(String((init as { body?: string }).body));
      return {
        status: 200,
        text: async () => (new URL(url).pathname === CHATGPT_PLAN.listPath
          ? listText(IDS.slice(Number(body.offset), Number(body.offset) + Number(body.limit)))
          : DETAIL_TEXT),
      };
    };
    // The content-script hop goes through the **real** handleBackfillMessage, swapping only the plan table for the synthetic one.
    const port = tabHttpPort(9, async (_id, msg) =>
      handleBackfillMessage(msg, ORIGIN, fetchImpl as never, postLookup));

    const report = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-post',
      store: memoryStore(), http: port, clock: fakeClock(), pace: NO_WAIT,
      plans: postLookup,
      sink: (c) => ({ saved: true, sessionId: c.sessionId }),
    });

    console.log('[C23-2] the request the page side really received:', JSON.stringify(fetched, null, 1));
    console.log('[C23-2] archived:', report.archivedThisRun, 'halted:', report.halted);

    expect(report.halted).toBeNull();
    expect(report.archivedThisRun).toEqual(IDS);
    expect(fetched[0]).toEqual({
      url: `${ORIGIN}${CHATGPT_PLAN.listPath}`,
      init: { method: 'POST', body: JSON.stringify({ offset: 0, limit: 100 }), contentType: 'application/json' },
    });
    expect(fetched[1]!.init).toEqual({
      method: 'POST',
      body: JSON.stringify({ conversationId: ID0 }),
      contentType: 'application/json',
    });
  });

  it('all three closed sets — method / contentType / bodyKeys — are in the code, and a body may only hold scalar values under closed-set keys', () => {
    expect(ALLOWED_BACKFILL_METHODS).toEqual(['GET', 'POST']);
    expect(ALLOWED_BACKFILL_CONTENT_TYPES).toEqual(['application/json']);

    const url = `${ORIGIN}${CHATGPT_PLAN.listPath}`;
    const ct = 'application/json';
    const cases: Array<[string, unknown, boolean]> = [
      ['keys inside the closed set + scalar values', { offset: 0, limit: 100 }, true],
      ['one extra key', { offset: 0, limit: 100, callback: 'https://evil.example.com' }, false],
      ['a key in the closed set but a nested-object value', { offset: { $gt: 0 }, limit: 100 }, false],
      ['a key in the closed set but an array value', { offset: [1, 2, 3], limit: 100 }, false],
    ];
    for (const [label, body, allowed] of cases) {
      const v = checkBackfillRequest(
        { url, method: 'POST', body: JSON.stringify(body), contentType: ct }, ORIGIN, postLookup);
      console.log(`[C23-2] ${label} ⇒`, v.ok ? 'ALLOW' : `REFUSE(${(v as { detail: string }).detail})`);
      expect([label, v.ok]).toEqual([label, allowed]);
    }

    // A body that is not a JSON object / is oversized / has the wrong Content-Type is refused outright.
    const oversize = JSON.stringify({ offset: 'x'.repeat(MAX_REQUEST_BODY_BYTES), limit: 1 });
    for (const [label, spec] of [
      ['body is not JSON', { url, method: 'POST', body: 'not-json', contentType: ct }],
      ['body is an array', { url, method: 'POST', body: '[1,2]', contentType: ct }],
      ['body exceeds MAX_REQUEST_BODY_BYTES', { url, method: 'POST', body: oversize, contentType: ct }],
      ['Content-Type outside the closed set', { url, method: 'POST', body: '{"offset":0}', contentType: 'text/plain' }],
      ['POST with no body', { url, method: 'POST', contentType: ct }],
    ] as Array<[string, Parameters<typeof checkBackfillRequest>[0]]>) {
      const v = checkBackfillRequest(spec, ORIGIN, postLookup);
      console.log(`[C23-2] ${label} ⇒`, v.ok ? 'ALLOW' : `REFUSE(${(v as { detail: string }).detail})`);
      expect([label, v.ok]).toEqual([label, false]);
    }
  });
});

// ===========================================================================
describe('C23-3 · a method or url outside the allowlist ⇒ refused, with a trace', () => {
  it('a GET segment asked to send a POST ⇒ refused, fetch never called, trace left', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const calls: string[] = [];
    const fetchImpl = async (url: string) => { calls.push(url); return { status: 200, text: async () => '{}' }; };
    // Both segments of the production plan (chatgpt) are GET — so the POST below must be refused.
    const reply = await serveBackfillFetch(
      { url: `${ORIGIN}${CHATGPT_PLAN.listPath}`, method: 'POST', body: '{"offset":0}', contentType: 'application/json' },
      ORIGIN,
      fetchImpl as never,
    );
    console.log('[C23-3] reply to a POST on a GET segment:', JSON.stringify(reply));
    console.log('[C23-3] trace:', warn.mock.calls.map((c) => String(c[0])));
    expect(reply).toEqual({ ok: false, error: 'refused: chatgpt list segment must be GET, got POST' });
    expect(calls).toEqual([]);                                    // 🔴 fetch was never called at all
    expect(warn.mock.calls.map((c) => String(c[0]))).toEqual([
      '[chat-stasher] backfill fetch refused: refused: chatgpt list segment must be GET, got POST',
    ]);
    warn.mockRestore();
  });

  it('a method outside the closed set (DELETE/PUT/…) ⇒ refused, trace left', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const calls: string[] = [];
    const fetchImpl = async (url: string) => { calls.push(url); return { status: 200, text: async () => '{}' }; };
    for (const method of ['DELETE', 'PUT', 'PATCH', 'OPTIONS']) {
      const reply = await serveBackfillFetch(
        { url: `${ORIGIN}${CHATGPT_PLAN.detailPath}abc`, method }, ORIGIN, fetchImpl as never);
      console.log(`[C23-3] method=${method} ⇒`, JSON.stringify(reply));
      expect(reply).toEqual({ ok: false, error: `refused: method ${method} is not in the allowed set` });
    }
    expect(calls).toEqual([]);
    expect(warn.mock.calls.length).toBe(4);                       // 🔴 four refusals, four traces
    warn.mockRestore();
  });

  it('a url outside the allowlist (cross-origin / not a backfill path) ⇒ refused with a trace, and the wire reason is byte-identical to C22', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const calls: string[] = [];
    const fetchImpl = async (url: string) => { calls.push(url); return { status: 200, text: async () => '{}' }; };
    for (const url of [
      'https://evil.example.com/steal',                 // cross-origin
      `${ORIGIN}/backend-api/accounts/deactivate`,      // same origin, but not a backfill path
      `${ORIGIN}/backend-api/conversations/../../admin`,
    ]) {
      const reply = await serveBackfillFetch(url, ORIGIN, fetchImpl as never);
      console.log('[C23-3] url refused:', url, '⇒', JSON.stringify(reply));
      expect(reply).toEqual({ ok: false, error: REFUSED_URL_REASON });
    }
    expect(calls).toEqual([]);
    console.log('[C23-3] traces on the url dimension:', warn.mock.calls.map((c) => String(c[0])));
    expect(warn.mock.calls.length).toBe(3);
    warn.mockRestore();
  });

  it('🔴 a refusal takes the same path as a failed fetch: {ok:false} → tabHttpPort throws → the engine stops and persists', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    // A malformed combination where "the plan says GET but the engine sends POST": the content
    // script has to stop it, and the engine has to take its existing transport-error halt path
    const store = memoryStore();
    const port = tabHttpPort(3, async (_id, msg) =>
      handleBackfillMessage(msg, ORIGIN, (async () => { throw new Error('should not be reached'); }) as never));
    const report = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-halt',
      store, http: (url) => port(url, { method: 'POST', body: '{"offset":0}', contentType: 'application/json' }),
      clock: fakeClock(), pace: NO_WAIT,
      sink: () => ({ saved: true }),
    });
    console.log('[C23-3] halt record:', JSON.stringify(report.halted));
    console.log('[C23-3] stopped:', report.stopped);
    // 🔴 W13 · changed semantics, stated rather than quietly relaxed.
    //    before: `expect(report.stopped).toBe('halted')`.
    //    after:  'waiting-retry'. A refused/errored transport is a transient stop, so
    //            the leg has not "stopped needing a human", it is waiting out a
    //            backoff. What this test is *for* — that a POST refusal takes the
    //            same line as a GET failure, and leaves a persisted trace — is
    //            asserted below and is unchanged.
    expect(report.stopped).toBe('waiting-retry');
    expect(report.halted?.reason).toBe('transport-error');
    expect(report.halted?.detail).toContain('must be GET, got POST');
    // 🔴 W13: and the record now carries the two fields that make the self-healing
    //    possible — this is the difference between "a trace you can read" and "a leg
    //    that comes back", which is what the real account needed.
    expect(report.halted?.attempts).toBe(1);
    expect(report.halted?.retryAt).toBe(report.halted!.at + 5 * 60_000);
    // 🔴 The trace was persisted — it is still visible after a restart, not only in memory.
    const persisted = await store.load(stateKey('chatgpt', 'acct-halt')) as { halted?: unknown } | null;
    console.log('[C23-3] the persisted halted:', JSON.stringify(persisted?.halted));
    expect(persisted?.halted).toBeTruthy();
    warn.mockRestore();
  });
});
