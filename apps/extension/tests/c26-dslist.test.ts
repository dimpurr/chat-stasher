/**
 * C26 · The shape of DeepSeek's conversation list, and the business of **writing this down knowing it may be wrong**.
 *
 * 🔴 All fixtures are synthetic: not one line touches a real platform endpoint, no request was
 *    sent to deepseek.com, there is no logged-in state and no real conversation body or account.
 *    The http port is always injected explicitly.
 *
 * The field names DEEPSEEK_PLAN recognises were **reverse-engineered by cross-checking** four
 * mutually independent open-source implementations by R25; they are not an official contract (the
 * provenance and staleness risk are at the head of DEEPSEEK_PLAN). Reverse-engineered things can
 * be wrong at any time, so what this test really watches is not "it runs when right" but **what happens when it is wrong**:
 *
 *   1. two normal pages ⇒ the second really carries before_seq_id = the first page's smallest seq_id, and it stops after has_more:false;
 *   2. 🔴 no chat_sessions in the response ⇒ halt('shape-changed'),
 *      **and that must be a distinguishable path from "this user has no conversations"** (this file pins that sentence with a control);
 *   3. no seq_id in a record ⇒ only the first page is fetched, reported by name as 'cursor-missing', never pretending the whole thing was captured;
 *   4. updated_at is handled as a **number** (not an ISO string, and never new Date(string));
 *   5. ~~the body segment has no source ⇒ halt('detail-unsupported') before the first body is fetched, with the
 *      debts untouched.~~ 🔴 **Superseded by W8 (2026-09-14), and the fact is deliberately changed, not the
 *      assertion weakened.** DeepSeek's body segment is no longer null, so this file's 5th section now records
 *      that DeepSeek **leaves** the half-leg branch instead of entering it. The half-leg mechanism itself is
 *      unchanged and is still watched — by tests/c27-pplx.test.ts and tests/w44-halt-capability.test.ts,
 *      each of which injects a list-only plan: no platform is in that state any more, because W84/W84b
 *      filled in the last real one.
 *      DeepSeek's body segment has its own file: tests/w3-deepseek-detail.test.ts.
 *
 * 🔴 Scope note (W8): this file is about DeepSeek's **list** segment. Its shared `run()` helper stops before the
 *    body segment (`maxDetails: 0`), and the fixture below still throws on any non-list path — so "be.calls holds
 *    only list URLs" keeps proving exactly what it always proved: this leg issued no body request *in this test*.
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  BACKFILL_LIST_ONLY_PLATFORMS,
  BACKFILL_PARTIAL,
  BACKFILL_SUPPORTED_PLATFORMS,
  DEEPSEEK_DETAIL_PATH,
  DEEPSEEK_DETAIL_QUERY_KEY,
  DEEPSEEK_LIST_PATH,
  DEEPSEEK_PLAN,
  PERPLEXITY_PLAN,
  backfillPlanFor,
  parseDeepSeekListPage,
} from '../lib/backfill/enumerate';
import { isAllowedBackfillUrl } from '../lib/backfill/tab-port';
import type { Clock } from '../lib/backfill/pace';
import { stateKey } from '../lib/backfill/types';

const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';
const LIMIT = 100;

function fakeClock(): Clock {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

interface FixtureSession {
  id: string;
  seq_id?: number;
  updated_at?: unknown;
  title?: string;
}

/** One synthetic page. The envelope follows R25's data.biz_data / chat_sessions / has_more byte for byte. */
function pageBody(sessions: FixtureSession[], hasMore: boolean, opts: { omitHasMore?: boolean } = {}): string {
  const biz: Record<string, unknown> = { chat_sessions: sessions };
  if (!opts.omitHasMore) biz.has_more = hasMore;
  return JSON.stringify({ code: 0, msg: 'ok', data: { biz_data: biz } });
}

function session(n: number, over: Partial<FixtureSession> = {}): FixtureSession {
  return {
    id: `ds-${String(n).padStart(4, '0')}-aaaaaaaa`,
    seq_id: 1000 - n,
    // 🔴 A numeric timestamp (seconds), not an ISO string. All three sources agree.
    updated_at: 1_755_000_000 + n,
    title: 'synthetic-fixture',
    ...over,
  };
}

/**
 * 🔴 W8 · One **synthetic** single-conversation body, in the envelope the logged-in browser session
 * measured on 2026-09-13: { code, msg, data: { biz_code, biz_msg, biz_data: { chat_session: { id },
 * chat_messages: [...] } } }. Written by hand from the key names — no real conversation, no capture.
 * It carries both of lib/contract.ts's DeepSeek `requiredAnyPaths`, which is what the engine's
 * shape check reads.
 */
function deepSeekBody(id: string): string {
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: {
      biz_code: 0,
      biz_msg: 'ok',
      biz_data: {
        // 🔴 W42 · `current_message_id` and each message's `parent_id` are part of the envelope the
        //    2026-09-13 session measured (same names at tests/unsupported-transport.test.ts:141), and
        //    W42 made them load-bearing: DEEPSEEK_PLAN.parseDetailPage walks them, and a body without a
        //    readable leaf halts rather than passing as whole. The fixture is therefore given them, so
        //    that it keeps reproducing the shape it claims to reproduce. No assertion in this file changed.
        chat_session: { id, title: 'synthetic-fixture', current_message_id: 2 },
        chat_messages: [
          { message_id: 1, parent_id: null, role: 'USER', content: 'synthetic-turn-1' },
          { message_id: 2, parent_id: 1, role: 'ASSISTANT', content: 'synthetic-turn-2' },
        ],
      },
    },
  });
}

/**
 * A synthetic backend: it decides which page to return from the before_seq_id in the URL.
 * `calls` records every requested URL — "the second page really carried a cursor" and "not one body was sent" are both proven with it.
 *
 * 🔴 W8: `opts.body` is **absent by default**, and that default is the strict one — any
 *    non-list path throws. That is what makes "be.calls contains list URLs only" a real
 *    assertion rather than a description. A test that means to exercise the body segment
 *    passes `opts.body` explicitly (see section 5), so the two intents cannot be confused.
 */
function backend(pages: string[], opts: { body?: string } = {}) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname !== DEEPSEEK_LIST_PATH) {
      if (u.pathname === DEEPSEEK_DETAIL_PATH && opts.body !== undefined) {
        return { status: 200, text: opts.body };
      }
      // 🔴 On DeepSeek the backfill leg **may only** hit its two declared paths. Hitting anywhere else is when the test should go red.
      throw new Error(`unexpected path ${u.pathname}`);
    }
    const index = Math.min(calls.length - 1, pages.length - 1);
    return { status: 200, text: pages[index] ?? '' };
  };
  return { http, calls };
}

/**
 * 🔴 W8: `maxDetails: 0` — these tests are about the list segment, and DeepSeek now **has**
 *    a body segment, so "stop before the first body" has to be said out loud instead of
 *    happening by itself (it used to happen because detailPath was null). The engine takes
 *    the budget branch before issuing any body request, so `be.calls` stays list-only and
 *    the assertion keeps its original meaning.
 */
async function run(
  store: ReturnType<typeof memoryStore>,
  http: (url: string) => Promise<HttpResponse>,
  scope: string,
  opts: { maxDetails?: number } = {},
) {
  return runBackfill({
    platform: 'deepseek',
    origin: DEEPSEEK_ORIGIN,
    scope,
    store,
    http,
    clock: fakeClock(),
    listLimit: LIMIT,
    maxDetails: opts.maxDetails ?? 0,
  });
}

// ---------------------------------------------------------------------------
// 1 · Two normal pages: the cursor really is used, and it stops after has_more:false
// ---------------------------------------------------------------------------
describe('C26-1 · cursor paging', () => {
  it('the second page carries before_seq_id = the first page\'s smallest seq_id, and it stops after has_more:false', async () => {
    const store = memoryStore();
    const first = [session(1), session(2), session(3)];   // seq_id 999 / 998 / 997
    const second = [session(4), session(5)];              // seq_id 996 / 995
    const be = backend([pageBody(first, true), pageBody(second, false)]);

    // 🔴 W10 · Two pages are now two **ticks** (DeepSeek has a body segment, so a
    //    tick reads at most one list page and then spends its body budget — the
    //    segment order changed, the cursor rules did not). The whole assertion
    //    below is unchanged in substance: the second request carries the cursor
    //    the first tick persisted.
    const tick1 = await run(store, be.http, 'acct-two-pages');
    const report = await run(store, be.http, 'acct-two-pages');

    // 🔴 The first page carries no cursor; the second page's cursor = the **smallest** seq_id on the first page (997), not the first and not the largest.
    expect(be.calls).toEqual([
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}`,
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}&before_seq_id=997`,
    ]);
    // One page per tick, and the cursor survived the tick boundary — which is what
    // makes the second request the *continuation* rather than a first page again.
    expect(tick1.enumeratedPages).toBe(1);
    expect(report.enumeratedPages).toBe(1);
    expect(tick1.newDebts + report.newDebts).toBe(5);
    expect(report.state.pending).toEqual([...first, ...second].map((s) => s.id));
    // has_more:false ⇒ it finished normally, with **no** truncation marker at all.
    expect(report.state.enumCursor.complete).toBe(true);
    expect(report.enumTruncated).toBeNull();
    expect(report.state.enumCursor.truncated).toBeUndefined();
    // 🔴 There is no source for a total field in DeepSeek's list response ⇒ the denominator stays unknown, and the enumerated count is never passed off as one.
    expect(report.state.totalKnown).toBeNull();
    expect(report.state.totalSource).toBe('unknown');
    // 🔴 This page is far shorter than count (3 rows vs 100), but has_more:true means it must keep paging —
    //    "returned fewer than count ⇒ that is the end" treats an unknown as known, and this implementation does not infer it.
    //    (W10 moved the second request to the next tick; the number of requests it
    //     guards against — "stopped after the short first page" — is the same one.)
    expect(be.calls.length).toBeGreaterThan(1);
  });

  it('the cursor survives: stopping half way lets the next run carry on from the persisted cursor', async () => {
    const store = memoryStore();
    const first = [session(1), session(2)];
    const be1 = backend([pageBody(first, true)]);
    // First run: only the first page exists, and has_more:true ⇒ it will keep asking for a second (the fixture repeats the same page),
    // so shouldAbort stops it before the second page here, simulating the SW being reclaimed.
    let steps = 0;
    const report1 = await runBackfill({
      platform: 'deepseek',
      origin: DEEPSEEK_ORIGIN,
      scope: 'acct-resume',
      store,
      http: be1.http,
      clock: fakeClock(),
      listLimit: LIMIT,
      shouldAbort: () => steps++ >= 1,
    });
    expect(report1.stopped).toBe('aborted');
    expect(report1.state.enumCursor.cursor).toBe(998);

    // Second run: the new run reads the persisted cursor back, and its very first request must carry it.
    const be2 = backend([pageBody([session(3)], false)]);
    const report2 = await run(store, be2.http, 'acct-resume');
    expect(be2.calls[0]).toBe(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}&before_seq_id=998`);
    expect(report2.state.pending).toEqual(['ds-0001-aaaaaaaa', 'ds-0002-aaaaaaaa', 'ds-0003-aaaaaaaa']);
  });
});

// ---------------------------------------------------------------------------
// 2 · 🔴 This change's hardest line: no chat_sessions readable ⇒ "the shape changed", not "there are no conversations"
// ---------------------------------------------------------------------------
describe('C26-2 · an unknown must not be treated as empty', () => {
  it('no chat_sessions in the response ⇒ halt(shape-changed) persisted, never treated as an empty list', async () => {
    const store = memoryStore();
    // A synthetic "API change": the array was renamed while the envelope stayed.
    const drifted = JSON.stringify({ data: { biz_data: { sessions: [], has_more: false } } });
    const be = backend([drifted]);

    const report = await run(store, be.http, 'acct-drift');

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('chat_sessions');
    // 🔴 The trace must be persisted and still be there after a restart — the opposite of silence.
    const persisted = await store.load(stateKey('deepseek', 'acct-drift'));
    expect((persisted as { halted?: { reason: string } }).halted?.reason).toBe('shape-changed');

    // 🔴🔴 This assertion is the bullseye of this change: it is **not the same** as the "empty list" path.
    //     Enumeration was not marked complete — the backfill leg will never think it has finished.
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.newDebts).toBe(0);
    expect(report.stopped).not.toBe('queue-empty');
  });

  it('the control: chat_sessions really is an empty array ⇒ not a halt, but "listed it all, you have no history"', async () => {
    const store = memoryStore();
    const be = backend([pageBody([], false)]);

    const report = await run(store, be.http, 'acct-really-empty');

    // 🔴 Item-by-item against the case above: the two outcomes look completely different in the ledger.
    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.state.enumCursor.complete).toBe(true);
    expect(report.enumTruncated).toBeNull();
    expect(report.newDebts).toBe(0);
  });

  it('a changed envelope itself (no data / biz_data) also reports a shape change', () => {
    expect(parseDeepSeekListPage(JSON.stringify({ biz_data: { chat_sessions: [] } })).ok).toBe(false);
    expect(parseDeepSeekListPage(JSON.stringify({ data: { chat_sessions: [] } })).ok).toBe(false);
    expect(parseDeepSeekListPage('not json at all').ok).toBe(false);
  });

  it('🔴 the top-level business code (code / biz_code) has two sources in conflict ⇒ the parser does not depend on it', () => {
    const sessions = [session(1)];
    const withCode = JSON.stringify({ code: 0, data: { biz_data: { chat_sessions: sessions, has_more: false } } });
    const withBizCode = JSON.stringify({ biz_code: 500, data: { biz_data: { chat_sessions: sessions, has_more: false } } });
    const withNeither = JSON.stringify({ data: { biz_data: { chat_sessions: sessions, has_more: false } } });
    for (const text of [withCode, withBizCode, withNeither]) {
      const parsed = parseDeepSeekListPage(text);
      expect(parsed.ok).toBe(true);
    }
  });
});

// ---------------------------------------------------------------------------
// 3 · 🔴 no seq_id readable ⇒ only the first page is fetched, and it is **reported faithfully**
// ---------------------------------------------------------------------------
describe('C26-3 · if it cannot page, it says it cannot page', () => {
  it('no seq_id in a record ⇒ exactly one request is sent, enumTruncated=cursor-missing', async () => {
    const store = memoryStore();
    const noSeq = [session(1, { seq_id: undefined }), session(2, { seq_id: undefined })];
    // has_more says there is another page — but we cannot page to it.
    const be = backend([pageBody(noSeq, true), pageBody([session(9)], false)]);

    const report = await run(store, be.http, 'acct-no-seq');

    // 🔴 Only the first page was fetched: the second page's request never went out at all.
    expect(be.calls).toEqual([`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}`]);
    expect(report.enumeratedPages).toBe(1);
    expect(report.newDebts).toBe(2);
    // 🔴 Reported faithfully, and both **named** and **persisted**:
    //    complete=true here means "stopped here", and truncated is exactly what distinguishes the two.
    expect(report.enumTruncated).toBe('cursor-missing');
    expect(report.state.enumCursor.truncated).toBe('cursor-missing');
    const persisted = await store.load(stateKey('deepseek', 'acct-no-seq')) as {
      enumCursor: { complete: boolean; truncated?: string };
    };
    expect(persisted.enumCursor.truncated).toBe('cursor-missing');
    // In the control (C26-1's two normal pages) truncated is undefined — the two are distinguishable.
    expect(report.state.enumCursor.complete).toBe(true);
  });

  it('one unreadable seq_id on a page makes the whole page\'s cursor unusable (stop rather than page wrongly)', () => {
    const parsed = parseDeepSeekListPage(
      pageBody([session(1), session(2, { seq_id: undefined }), session(3)], true),
    );
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.nextCursor).toBeNull();
    expect(parsed.page.hasMore).toBe(true);
  });

  it('no has_more in the response ⇒ it may not be taken as "no next page"; report has-more-missing', async () => {
    const store = memoryStore();
    const be = backend([pageBody([session(1)], false, { omitHasMore: true })]);

    const report = await run(store, be.http, 'acct-no-hasmore');

    expect(report.enumeratedPages).toBe(1);
    expect(report.enumTruncated).toBe('has-more-missing');
    expect(report.state.enumCursor.truncated).toBe('has-more-missing');
    // 🔴 Distinguishable from "has_more:false (there really is no next page)": that one's enumTruncated is null.
    const clean = await run(memoryStore(), backend([pageBody([session(1)], false)]).http, 'acct-clean');
    expect(clean.enumTruncated).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// 4 · updated_at is a **number**
// ---------------------------------------------------------------------------
describe('C26-4 · updated_at is handled as a number', () => {
  it('a numeric timestamp is taken as-is, taking the page maximum, with no conversion at all', () => {
    const parsed = parseDeepSeekListPage(pageBody([
      session(1, { updated_at: 1_700_000_000 }),
      session(2, { updated_at: 1_755_123_456 }),
      session(3, { updated_at: 1_600_000_000 }),
    ], false));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    // 🔴 That exact number: not a Date, not a millisecond conversion, not a string.
    expect(parsed.page.newestUpdatedAt).toBe(1_755_123_456);
    expect(typeof parsed.page.newestUpdatedAt).toBe('number');
  });

  it('a millisecond-scale number is taken as-is too (we do not guess whether it is seconds or milliseconds)', () => {
    const parsed = parseDeepSeekListPage(pageBody([session(1, { updated_at: 1_755_123_456_789 })], false));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.newestUpdatedAt).toBe(1_755_123_456_789);
  });

  it('🔴 turning into an ISO string (or anything non-numeric) ⇒ judged a shape change; never leniently passed over with new Date(string)', () => {
    const iso = parseDeepSeekListPage(pageBody([session(1, { updated_at: '2026-08-17T00:00:00Z' })], false));
    expect(iso.ok).toBe(false);
    if (iso.ok) return;
    expect(iso.detail).toContain('updated_at');

    // "A number wrapped in a string" is the hardest drift to notice, so it gets a case of its own.
    expect(parseDeepSeekListPage(pageBody([session(1, { updated_at: '1755000000' })], false)).ok).toBe(false);
    expect(parseDeepSeekListPage(pageBody([session(1, { updated_at: { seconds: 1 } })], false)).ok).toBe(false);
  });

  it('the whole field absent ⇒ tolerated (enumeration does not need it), newestUpdatedAt=null', () => {
    const parsed = parseDeepSeekListPage(pageBody([session(1, { updated_at: undefined })], false));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.newestUpdatedAt).toBeNull();
    expect(parsed.page.ids).toEqual(['ds-0001-aaaaaaaa']);
  });
});

// ---------------------------------------------------------------------------
// 5 · 🔴 Half a leg: the list can be listed, the body segment has no source
//
// 🔴 W8 (2026-09-14) · **This section's premise is deliberately changed, not its assertions weakened.**
//    It used to prove "DeepSeek lists conversations but cannot fetch a single body". DeepSeek's body
//    segment now has evidence, so that sentence is false and the section now records the move:
//    DeepSeek **leaves** the half-leg branch. The mechanism it used to demonstrate is untouched and is
//    still watched — with an injected list-only plan, since no platform is in that state any more — by
//    tests/c27-pplx.test.ts and tests/w44-halt-capability.test.ts. DeepSeek's new body
//    segment has its own file, tests/w3-deepseek-detail.test.ts.
// ---------------------------------------------------------------------------
describe('C26-5 · being able to list conversations ≠ being able to backfill history', () => {
  it('🔴 changed fact: it no longer halts with detail-unsupported — the body segment really is reached now', async () => {
    const store = memoryStore();
    const be = backend([pageBody([session(1), session(2)], false)], { body: deepSeekBody('ds-0001-aaaaaaaa') });

    // Not `run()`: that helper stops before the body segment on purpose, and this test is about it.
    const report = await run(store, be.http, 'acct-half', { maxDetails: 1 });

    // 🔴 Before W8 this was 'detail-unsupported'. The halt is gone because the missing half was filled in,
    //    not because the branch was loosened — `partial` is still what expresses it, and no plan declares
    //    one today (Perplexity was the last; W84/W84b filled its body segment in).
    expect(report.halted?.reason).not.toBe('detail-unsupported');
    // 🔴 Its difference from 'unsupported-platform' is still this line: the list request really was sent, and so was a body request.
    expect(be.calls.length).toBe(2);
    expect(be.calls[1]).toBe(
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=ds-0001-aaaaaaaa`,
    );
    expect(report.state.archived).toEqual(['ds-0001-aaaaaaaa']);
  });

  it('the platform lists give this intermediate state its own place rather than rounding it to either side', () => {
    // 🔴 W84 (2026-09-23): Perplexity's body segment was filled in from the live probe, so it
    //    too left this list — which is now empty (every platform has both segments).
    expect(BACKFILL_LIST_ONLY_PLATFORMS).toEqual([]);
    // 🔴 "can backfill history" now holds four: being able to list conversations is still not enough
    //    on its own, and DeepSeek is here because its body segment was filled in — not because the test
    //    was broadened. 🔴 W21 added grok to the platform table with both segments declared (its body
    //    is a two-step pair), and 🔴 W22 filled kimi's two segments in from a logged-in probe, so the
    //    list grows by one row each time; the criterion is unchanged.
    // 🔴 W29 (2026-09-14) grew it by one more row in the same way: gemini's list and paged
    //    body were filled in from the 2026-09-14 probe plus the W20 research, so it sits on the
    //    supported side between chatgpt and kimi — which is where the platform table puts it.
    // 🔴 W31 (2026-09-14) grew it once more, and by evidence rather than by a decision: claude's
    //    three recorded gaps were closed by the W20 research (the organization resolver, the
    //    limit/offset paging, and the list response's array of `uuid` summaries), so it now has
    //    both segments and sits where the platform table puts it — between gemini and kimi.
    // 🔴 W84 (2026-09-23): Perplexity joins it, in platform-table order, since its body segment
    //    was filled in from the live probe. The list-only side is now empty.
    expect(BACKFILL_SUPPORTED_PLATFORMS)
      .toEqual(['deepseek', 'perplexity', 'chatgpt', 'gemini', 'claude', 'kimi', 'grok']);
    // 🔴 DeepSeek no longer declares a missing half; the field's absence is what "both segments work" means.
    expect(DEEPSEEK_PLAN.partial).toBeUndefined();
    // 🔴 Perplexity no longer declares a missing half either; the partial-notes catalog has emptied
    //    with it — the wording that said a platform cannot fetch bodies is gone from the catalog.
    expect(PERPLEXITY_PLAN.partial).toBeUndefined();
    expect(BACKFILL_PARTIAL).toEqual([]);
  });

  it('the plan\'s declarations and provenance: both segments are complete, and the unverified part is written down', () => {
    const plan = backfillPlanFor('deepseek');
    expect(plan).not.toBeNull();
    expect(plan!.listPath).toBe('/api/v0/chat_session/fetch_page');
    expect(plan!.listCursorUrl).toBeTypeOf('function');
    expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, null, 30))
      .toBe(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=30`);
    expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, 42, 30))
      .toBe(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=30&before_seq_id=42`);
    // 🔴 W8: previously asserted toBeNull(). Changed because the fact changed — the body segment now has
    //    evidence (a real logged-in browser session on 2026-09-13 + several independent implementations).
    expect(plan!.detailPath).toBe(DEEPSEEK_DETAIL_PATH);
    expect(plan!.detailUrl).not.toBeNull();
    expect(plan!.detailUrl!(DEEPSEEK_ORIGIN, 'ds-0001-aaaaaaaa')).toBe(
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=ds-0001-aaaaaaaa`,
    );
    // The id is URL-encoded, so an id with a reserved character cannot break out of the query value.
    expect(plan!.detailUrl!(DEEPSEEK_ORIGIN, 'a&b/c d')).toBe(
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=a%26b%2Fc%20d`,
    );
    expect(plan!.detailQueryKey).toBe(DEEPSEEK_DETAIL_QUERY_KEY);
    // Provenance is **mandatory**, and it must say "multi-source cross-check, not official documentation, at risk of staleness" — the conclusion alone is not enough.
    expect(plan!.provenance).toContain('before_seq_id');
    expect(plan!.provenance).toContain('2025-12');
    // 🔴 W8: which body route, on what evidence, and — just as load-bearing — **what was not verified**.
    //    An unknown may not be dropped from the provenance to make the plan read as finished.
    expect(plan!.provenance).toContain('chat/history_messages');
    expect(plan!.provenance).toContain('2026-09-13');
    expect(plan!.provenance).toMatch(/unverified|Unverified/i);
    expect(plan!.provenance).toMatch(/paging|page|truncat/i);
    // 🔴 A parameter name that appeared in none of the five sources may not enter the URL.
    for (const banned of ['offset=', 'page=', 'page_size=', 'limit=', 'cursor=']) {
      expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, 42, 30)).not.toContain(banned);
    }
    // 🔴 And the body URL may not quietly grow one either: W8 added no paging parameter.
    for (const banned of ['offset=', 'page=', 'page_size=', 'limit=', 'cursor=', 'count=']) {
      expect(plan!.detailUrl!(DEEPSEEK_ORIGIN, 'ds-0001-aaaaaaaa')).not.toContain(banned);
    }
  });
});

// ---------------------------------------------------------------------------
// 6 · The allowlist: it goes through the existing mechanism, and only the path the plan wrote down itself is permitted
// ---------------------------------------------------------------------------
describe('C26-6 · the content script\'s allowlist', () => {
  it('the list path is allowed; an adjacent path and cross-origin are refused', () => {
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=100`, DEEPSEEK_ORIGIN)).toBe(true);
    expect(isAllowedBackfillUrl(
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=100&before_seq_id=997`, DEEPSEEK_ORIGIN)).toBe(true);
    // 🔴 It did not become a prefix wildcard: a lookalike that is not equal byte for byte is still refused.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/fetch_page2`, DEEPSEEK_ORIGIN)).toBe(false);
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 The same-origin rule did not loosen: a page on another origin is refused.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}`, 'https://chatgpt.com')).toBe(false);
    // 🔴 ChatGPT's path is not allowed on DeepSeek just because DeepSeek now has a plan.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/backend-api/conversations`, DEEPSEEK_ORIGIN)).toBe(false);
  });

  it('🔴 W8 · the body URL is allowed only with its one declared query key, and only as this plan builds it', () => {
    const built = `${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=ds-0001-aaaaaaaa`;
    // 🔴 Changed fact, same criterion as the line above it: this URL used to be refused because the
    //    body segment had no source. It is allowed now because the plan wrote the path down itself.
    expect(isAllowedBackfillUrl(built, DEEPSEEK_ORIGIN)).toBe(true);
    // 🔴 The bare path is **still** refused: it carries no id, so it is not the URL this plan builds.
    //    (Under C26 this was refused for the other reason — the body segment was null. Same verdict,
    //    different mechanism, and the mechanism is what this test is about.)
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 The path is **not** a prefix: a lookalike endpoint riding on it is refused.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}_export?${DEEPSEEK_DETAIL_QUERY_KEY}=x`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 A query key nobody declared is refused, even alongside the declared one.
    expect(isAllowedBackfillUrl(`${built}&cache_version=0`, DEEPSEEK_ORIGIN)).toBe(false);
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?other=x`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 An empty id is refused: an absent id is not a smaller request, it is a different one.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 Repeated key ⇒ which one is the id is ambiguous, so it is refused rather than resolved by picking one.
    expect(isAllowedBackfillUrl(`${built}&${DEEPSEEK_DETAIL_QUERY_KEY}=ds-0002-aaaaaaaa`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 The value must be exactly what the plan's own builder produces for it, so the **encoding**
    //    is checked too: '%20' is what encodeURIComponent writes for a space, and '+' is the same
    //    value spelled a way this plan would never have written, so it is refused.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=a%20b`, DEEPSEEK_ORIGIN)).toBe(true);
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=a+b`, DEEPSEEK_ORIGIN)).toBe(false);
    // A literal '+' inside an id is a legal id character and survives the round trip.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=a%2Bb`, DEEPSEEK_ORIGIN)).toBe(true);
    // 🔴 A fragment is not part of any URL this plan builds.
    expect(isAllowedBackfillUrl(`${built}#x`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 And it is still a GET: the id travels in the query, not in a body.
    expect(isAllowedBackfillUrl(built, 'https://chatgpt.com')).toBe(false);
  });
});
