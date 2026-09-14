/**
 * W8 · DeepSeek's backfill **body** segment: the plan's URL, the content script's allowlist for it,
 * and one full engine round-trip from a listed conversation to a settled debt.
 *
 * ## What changed, and what this file is here to stop changing back
 * Until W8, DEEPSEEK_PLAN.detailPath / detailUrl were both null: the list segment had a four-source
 * provenance, the body segment had none, and the engine halted with 'detail-unsupported' before
 * issuing a single body request. The reason recorded for the null was "no cross-checked source for a
 * single conversation's route". W8 removed that reason with evidence, so the plan now carries the
 * body segment — and this file pins the three things that fill-in has to get right:
 *
 *   1. **the URL is the one we wrote down**, built by the plan itself, with the id URL-encoded;
 *   2. **the allowlist let exactly that through** — and still refuses the same path with a query key
 *      the plan did not declare, a lookalike path riding on a prefix, and an empty id. Filling in a
 *      body route is exactly the moment an allowlist is most likely to be loosened by accident;
 *   3. **a full round works**: the debt is handed to the sink and settled, and — the other half of
 *      the same criterion — a response whose shape is wrong stops with 'shape-changed' and **does
 *      not settle**. "We could not read it" may never be recorded as "we read it and it was fine".
 *
 * 🔴 All fixtures are **synthetic**, written by hand from the key names the live leg measured. No
 *    request goes to deepseek.com, there is no logged-in state, and no real conversation or account
 *    appears anywhere below. The http port is always injected explicitly.
 *
 * 🔴 Scope note: this file stops at the sink. It does not exercise the on-disk write path (that needs
 *    a host); the identity question it *can* answer purely is answered in the last section, by
 *    checking that the URL route and the response body route agree on the same conversation id.
 */

import { describe, it, expect } from 'vitest';
import { loadState, runBackfill, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { stateKey } from '../lib/backfill/types';
import { extractSessionId, getPlatformByOrigin, matchesResponseShape, type CapturedFetch } from '../lib/contract';
import {
  DEEPSEEK_DETAIL_PATH,
  DEEPSEEK_DETAIL_QUERY_KEY,
  DEEPSEEK_LIST_PATH,
  DEEPSEEK_PLAN,
  backfillPlanFor,
  canBackfillDetail,
} from '../lib/backfill/enumerate';
import { checkBackfillRequest, REFUSED_URL_REASON } from '../lib/backfill/tab-port';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chat.deepseek.com';
const ID = 'ds-0001-aaaaaaaa';
const OTHER_ID = 'ds-0002-aaaaaaaa';
const SCOPES = {
  round: 'acct-w8-round',
  shape: 'acct-w8-shape',
  empty: 'acct-w8-empty',
  identity: 'acct-w8-identity',
} as const;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock {
  let t = Date.parse('2026-09-14T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

/** The body URL this plan builds. Every assertion about "the right request went out" compares to this one value. */
const BUILT = `${ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=${ID}`;

/** A synthetic list page in the shape R25 cross-checked (data.biz_data.chat_sessions + has_more). */
function listPage(ids: string[]): string {
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: {
      biz_data: {
        chat_sessions: ids.map((id, i) => ({ id, seq_id: 1000 - i, updated_at: 1_755_000_000 + i })),
        has_more: false,
      },
    },
  });
}

/**
 * A synthetic single-conversation body, in the envelope a logged-in browser session measured on
 * 2026-09-13: { code, msg, data: { biz_code, biz_msg, biz_data: { chat_session: { id },
 * chat_messages: [...] } } }. `omit` deletes one nested key, which is how the shape-drift case below
 * is built without writing a second fixture that could drift from this one.
 */
function bodyFor(id: string, omit?: readonly string[]): string {
  const bizData: Record<string, unknown> = {
    chat_session: { id, title: 'synthetic-fixture' },
    chat_messages: [
      { message_id: 1, role: 'USER', content: 'synthetic-turn-1' },
      { message_id: 2, role: 'ASSISTANT', content: 'synthetic-turn-2' },
    ],
  };
  for (const key of omit ?? []) delete bizData[key];
  return JSON.stringify({ code: 0, msg: 'ok', data: { biz_code: 0, biz_msg: 'ok', biz_data: bizData } });
}

interface Recorder {
  /** Every request the leg made, in order. */
  calls: { url: string; init: unknown }[];
}

/**
 * A synthetic backend. `detail` decides what the body endpoint returns — `null` means "this leg may
 * not touch it", and then any body request throws, so "no body request was sent" is proven rather
 * than described.
 */
function backend(detail: string | null, listIds: string[] = [ID]): Recorder & { http: (url: string, init?: unknown) => Promise<HttpResponse> } {
  const calls: { url: string; init: unknown }[] = [];
  const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
    calls.push({ url, init });
    const u = new URL(url);
    if (u.pathname === DEEPSEEK_LIST_PATH) return { status: 200, text: listPage(listIds) };
    if (u.pathname === DEEPSEEK_DETAIL_PATH && detail !== null) return { status: 200, text: detail };
    throw new Error(`unexpected path ${u.pathname}`);
  };
  return { calls, http };
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: (url: string, init?: unknown) => Promise<HttpResponse>,
  scope: string,
  sink?: (captured: CapturedFetch) => SinkOutcome | void,
) {
  return runBackfill({
    platform: 'deepseek',
    origin: ORIGIN,
    scope,
    store,
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink,
  });
}

// ---------------------------------------------------------------------------
// 1 · The plan's body URL
// ---------------------------------------------------------------------------
describe('W8-1 · the body segment the plan writes down', () => {
  it('the route is a single endpoint (no trailing slash) and the id travels in one declared query key', () => {
    const plan = backfillPlanFor('deepseek');
    expect(plan).not.toBeNull();
    expect(plan!.detailPath).toBe('/api/v0/chat/history_messages');
    // 🔴 No trailing '/': this names ONE endpoint, not a directory the id is appended to. The content
    //    script reads exactly that to decide between an exact and a prefix comparison (tab-port.ts).
    expect(DEEPSEEK_DETAIL_PATH.endsWith('/')).toBe(false);
    expect(plan!.detailQueryKey).toBe(DEEPSEEK_DETAIL_QUERY_KEY);
    // 🔴 Pinned as a **literal**, not through the constant. The key is a wire fact measured on
    //    2026-09-13, and every other assertion in this file compares the plan against itself — so a
    //    rename of the constant would satisfy all of them and still send the wrong request. This line
    //    and the extractSessionId cross-check below are the two that would notice.
    expect(DEEPSEEK_DETAIL_QUERY_KEY).toBe('chat_session_id');
    expect(BUILT).toContain('?chat_session_id=');
    // 🔴 W8's whole point: this platform can now fetch bodies, so it is no longer in the half-leg state.
    expect(canBackfillDetail(plan!)).toBe(true);
    expect(plan!.partial).toBeUndefined();
  });

  it('detailUrl builds exactly this URL, and URL-encodes the id so it cannot break out of the query value', () => {
    expect(DEEPSEEK_PLAN.detailUrl).not.toBeNull();
    expect(DEEPSEEK_PLAN.detailUrl!(ORIGIN, ID)).toBe(BUILT);
    // A synthetic id made only of characters that must be escaped — if the encoding were dropped, any
    // of these would end the value early or start a new parameter.
    expect(DEEPSEEK_PLAN.detailUrl!(ORIGIN, 'a&b=c d/e?f#g')).toBe(
      `${ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=a%26b%3Dc%20d%2Fe%3Ff%23g`,
    );
    // 🔴 And the id it produces is the id it takes: the round trip the allowlist check below relies on.
    const encoded = DEEPSEEK_PLAN.detailUrl!(ORIGIN, 'a&b=c d/e?f#g');
    expect(new URL(encoded).searchParams.get(DEEPSEEK_DETAIL_QUERY_KEY)).toBe('a&b=c d/e?f#g');
  });
});

// ---------------------------------------------------------------------------
// 2 · The allowlist, in both directions
// ---------------------------------------------------------------------------
describe('W8-2 · what the content script lets through, and what it still refuses', () => {
  it('the plan-built body URL is allowed, as a GET with no body and no content-type', () => {
    const verdict = checkBackfillRequest({ url: BUILT }, ORIGIN);
    expect(verdict.ok).toBe(true);
    if (!verdict.ok) return;
    // 🔴 The id is in the query, so the request stays a GET: filling in a body route did not turn it
    //    into the POST channel, whose body allowlist would then also have to be satisfied.
    expect(verdict.method).toBe('GET');
    expect(verdict.body).toBeUndefined();
    expect(verdict.contentType).toBeUndefined();
  });

  it('a query key the plan did not declare, an empty id, a repeated key and a bare path are all refused', () => {
    const refused = [
      // The plan declares one key; a second one is not part of any URL it builds.
      `${BUILT}&cache_version=0`,
      `${ORIGIN}${DEEPSEEK_DETAIL_PATH}?other=${ID}`,
      // An absent id is not a smaller request, it is a different one.
      `${ORIGIN}${DEEPSEEK_DETAIL_PATH}`,
      `${ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=`,
      // Ambiguous: which of the two is the conversation?
      `${BUILT}&${DEEPSEEK_DETAIL_QUERY_KEY}=${OTHER_ID}`,
      // The same id spelled a way this plan would never have written (searchParams reads '+' as a
      // space, and the plan's builder writes '%20' for one).
      `${ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=a+b`,
      // A fragment is not part of any URL this plan builds.
      `${BUILT}#frag`,
    ];
    for (const url of refused) {
      const verdict = checkBackfillRequest({ url }, ORIGIN);
      expect(verdict.ok, `${url} must be refused`).toBe(false);
      if (verdict.ok) continue;
      // 🔴 The URL dimension keeps C22's wire sentence verbatim: this change did not invent a new
      //    refusal code for callers to have to learn.
      expect(verdict.reason).toBe(REFUSED_URL_REASON);
    }
  });

  it('the path is not a prefix: a lookalike endpoint riding on it is refused', () => {
    for (const path of [
      `${DEEPSEEK_DETAIL_PATH}_export`,
      `${DEEPSEEK_DETAIL_PATH}/extra`,
      `${DEEPSEEK_DETAIL_PATH}2`,
    ]) {
      expect(checkBackfillRequest({ url: `${ORIGIN}${path}?${DEEPSEEK_DETAIL_QUERY_KEY}=${ID}` }, ORIGIN).ok)
        .toBe(false);
    }
    // Control: the exact path is still allowed, so the three above fail for the path and not by accident.
    expect(checkBackfillRequest({ url: BUILT }, ORIGIN).ok).toBe(true);
  });

  it('the same-origin rule and the method rule did not loosen for this new URL', () => {
    expect(checkBackfillRequest({ url: BUILT }, 'https://chatgpt.com').ok).toBe(false);
    expect(checkBackfillRequest({ url: BUILT, method: 'POST' }, ORIGIN).ok).toBe(false);
    // The list path stays exactly the list path: adding a body route did not widen it either.
    expect(checkBackfillRequest({ url: `${ORIGIN}${DEEPSEEK_LIST_PATH}2?count=100` }, ORIGIN).ok).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 3 · One full engine round: listed → body fetched → sink → debt settled
// ---------------------------------------------------------------------------
describe('W8-3 · a listed conversation really is carried through to the sink now', () => {
  it('the debt is handed to the sink with the right URL/identity, the verdict passes, and the debt is settled', async () => {
    const store = memoryStore();
    const be = backend(bodyFor(ID));
    const seen: CapturedFetch[] = [];

    const report = await run(store, be.http, SCOPES.round, (captured) => {
      seen.push(captured);
      // What the production exit reports: the identity the write-down path used. The engine put the
      // debt key in, and the sink hands the same value back, so the two are reconciled rather than assumed equal.
      return { saved: true, sessionId: captured.sessionId };
    });

    // 🔴 Two requests: the list, then exactly one body. The body one is the plan's own URL, character for character.
    expect(be.calls.map((c) => c.url)).toEqual([`${ORIGIN}${DEEPSEEK_LIST_PATH}?count=100`, BUILT]);
    // 🔴 It is a GET and carries no init at all — the engine's back-compat path, unchanged by W8.
    expect(be.calls[1]!.init).toBeUndefined();

    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(seen.length).toBe(1);
    expect(seen[0]!.url).toBe(BUILT);
    expect(seen[0]!.method).toBe('GET');
    expect(seen[0]!.status).toBe(200);
    expect(seen[0]!.text).toBe(bodyFor(ID));
    // 🔴 The identity is the debt key, not something scraped out of the URL a second time.
    expect(seen[0]!.sessionId).toBe(ID);

    // 🔴 Only now: the debt moved from pending to archived, and it is the same conversation at both ends.
    expect(report.archivedThisRun).toEqual([ID]);
    expect(report.state.archived).toEqual([ID]);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun).toEqual([]);
    // And the ledger on disk says the same thing, so a restart cannot re-fetch it.
    // 🔴 W18 · Through the production load path: `stateKey(...)` holds the header
    //    alone now, and the ids are in the debt store.
    const persisted = await loadState(store, 'deepseek', SCOPES.round);
    expect(persisted.archived).toEqual([ID]);
    expect(persisted.pending).toEqual([]);
  });

  it('the sink is what decides: a sink that cannot save does not leave an archived debt behind', async () => {
    const store = memoryStore();
    const be = backend(bodyFor(ID));

    const report = await run(store, be.http, SCOPES.round, () => ({
      saved: false,
      reason: 'synthetic-host-nack',
    }));

    // 🔴 The request did go out, so the body was fetched — but "fetched" is not "stored", and the debt
    //    may not be cleared on the strength of the fetch alone.
    expect(be.calls.length).toBe(2);
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.archived).toEqual([]);
    expect(report.state.pending).toEqual([]);   // judged dead: out of the queue, with a receipt
    // 🔴 The receipt carries the SHORT id only (failures.ts trims it): a failure list is not a place
    //    to accumulate whole conversation ids.
    expect(report.failedThisRun.map((f) => f.shortId)).toEqual([ID.slice(0, 8)]);
  });

  it('an exit that reports the wrong identity is caught rather than written over another conversation', async () => {
    const store = memoryStore();
    const be = backend(bodyFor(ID));

    const report = await run(store, be.http, SCOPES.identity, () => ({
      saved: true,
      sessionId: OTHER_ID,
    }));

    // 🔴 The sink said "saved", and it still is not archived: a file named after a different
    //    conversation is a loss, not a success.
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.archived).toEqual([]);
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['identity-mismatch']);
  });
});

// ---------------------------------------------------------------------------
// 4 · 🔴 The invariant: a shape we cannot read stops the leg and settles nothing
// ---------------------------------------------------------------------------
describe('W8-4 · "we could not read it" may never be recorded as "we read it"', () => {
  it('a body with no data.biz_data.chat_messages ⇒ halt(shape-changed), the debt stays, the sink is never called', async () => {
    const store = memoryStore();
    // A synthetic "API change": the array was renamed while the envelope stayed, AND the session id
    // moved out of `chat_session` — so neither of the two paths the deepseek row accepts is present.
    // (Dropping only `chat_messages` would NOT be a shape change: `data.biz_data.chat_session.id` is
    // the other accepted path, and it would still match. Asserted by the control line below.)
    const drifted = JSON.stringify({
      code: 0,
      msg: 'ok',
      data: { biz_code: 0, biz_msg: 'ok', biz_data: { chat_session: { title: 'synthetic-fixture' }, messages: [] } },
    });
    const be = backend(drifted);
    let sinkCalls = 0;

    const report = await run(store, be.http, SCOPES.shape, () => { sinkCalls += 1; return { saved: true }; });

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    // The trace names the platform whose shape changed, and it is persisted rather than only logged.
    expect(report.halted?.detail).toContain('deepseek');
    // 🔴 W18 · Production load path (header + debt store), as above.
    const persisted = await loadState(store, 'deepseek', SCOPES.shape);
    expect(persisted.halted?.reason).toBe('shape-changed');

    // 🔴 The three things that must NOT have happened: nothing delivered, nothing archived, nothing cleared.
    expect(sinkCalls).toBe(0);
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.archived).toEqual([]);
    expect(persisted.pending).toEqual([ID]);
    // Control: the same fixture **with** the array is accepted, so this case fails on the missing
    // paths and not because the fixture is malformed in some other way.
    expect(matchesResponseShape(getPlatformByOrigin(ORIGIN)!, bodyFor(ID))).toBe(true);
    expect(matchesResponseShape(getPlatformByOrigin(ORIGIN)!, drifted)).toBe(false);
  });

  it('an empty chat_messages array is a shape the contract accepts — it is not read as an empty body here', async () => {
    const store = memoryStore();
    const be = backend(JSON.stringify({ data: { biz_data: { chat_session: { id: ID }, chat_messages: [] } } }));
    const seen: CapturedFetch[] = [];

    const report = await run(store, be.http, SCOPES.empty, (captured) => {
      seen.push(captured);
      return { saved: true, sessionId: captured.sessionId };
    });

    // 🔴 Deliberately only this much: W8 declares no parseDetailPage for DeepSeek, so it makes no claim
    //    about whether an empty conversation is legitimate. What it does claim is that the raw body is
    //    carried to the sink untouched, so a later parser can decide with the payload in hand.
    expect(report.halted).toBeNull();
    expect(seen.length).toBe(1);
    expect(seen[0]!.text).toContain('"chat_messages":[]');
    expect(report.detailOutcomes).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// 5 · Identity: the two independent ways of naming this conversation agree
// ---------------------------------------------------------------------------
describe('W8-5 · the URL route and the body route name the same conversation', () => {
  it('extractSessionId reads the same id out of the URL this plan builds', () => {
    // The backfill leg does not go through extractSessionId (the engine sets sessionId itself), but
    // this pattern is what the live leg uses when it captures the same endpoint while browsing. If the
    // two disagreed, the archive would file the same conversation under two identities.
    expect(extractSessionId(BUILT, bodyFor(ID))).toBe(ID);
    expect(extractSessionId(`${ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=a%20b`, '{}')).toBe('a b');
  });

  it('🔴 an id whose characters would end the query early is recovered intact on both routes', () => {
    const gnarly = 'id&x=y z';
    const url = DEEPSEEK_PLAN.detailUrl!(ORIGIN, gnarly);
    // One parameter, one value: the '&' inside the id did not become a second one.
    expect(new URL(url).searchParams.getAll(DEEPSEEK_DETAIL_QUERY_KEY)).toEqual([gnarly]);
    expect(extractSessionId(url, '{}')).toBe(gnarly);
  });
});
