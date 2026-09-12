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
 *   5. the body segment has no source ⇒ halt('detail-unsupported') before the first body is fetched, with the debts untouched.
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { t } from '../lib/i18n';
import { memoryStore } from '../lib/backfill/store';
import {
  BACKFILL_LIST_ONLY_PLATFORMS,
  BACKFILL_SUPPORTED_PLATFORMS,
  DEEPSEEK_LIST_PATH,
  DEEPSEEK_PLAN,
  backfillPlanFor,
  parseDeepSeekListPage,
} from '../lib/backfill/enumerate';
import { isAllowedBackfillUrl } from '../lib/backfill/tab-port';
import type { Clock } from '../lib/backfill/pace';

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
 * A synthetic backend: it decides which page to return from the before_seq_id in the URL.
 * `calls` records every requested URL — "the second page really carried a cursor" and "not one body was sent" are both proven with it.
 */
function backend(pages: string[]) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname !== DEEPSEEK_LIST_PATH) {
      // 🔴 On DeepSeek the backfill leg **may only** hit this one list path. Hitting anywhere else is when the test should go red.
      throw new Error(`unexpected path ${u.pathname}`);
    }
    const index = Math.min(calls.length - 1, pages.length - 1);
    return { status: 200, text: pages[index] ?? '' };
  };
  return { http, calls };
}

async function run(store: ReturnType<typeof memoryStore>, http: (url: string) => Promise<HttpResponse>, scope: string) {
  return runBackfill({
    platform: 'deepseek',
    origin: DEEPSEEK_ORIGIN,
    scope,
    store,
    http,
    clock: fakeClock(),
    listLimit: LIMIT,
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

    const report = await run(store, be.http, 'acct-two-pages');

    // 🔴 The first page carries no cursor; the second page's cursor = the **smallest** seq_id on the first page (997), not the first and not the largest.
    expect(be.calls).toEqual([
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}`,
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}&before_seq_id=997`,
    ]);
    expect(report.enumeratedPages).toBe(2);
    expect(report.newDebts).toBe(5);
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
    expect(report.enumeratedPages).toBeGreaterThan(1);
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
    const persisted = await store.load('cs_backfill_v1:deepseek:acct-drift');
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
    const persisted = await store.load('cs_backfill_v1:deepseek:acct-no-seq') as {
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
// ---------------------------------------------------------------------------
describe('C26-5 · being able to list conversations ≠ being able to backfill history', () => {
  it('after enumerating it halts with detail-unsupported, not one body request was sent, and the debts are untouched', async () => {
    const store = memoryStore();
    const be = backend([pageBody([session(1), session(2)], false)]);

    const report = await run(store, be.http, 'acct-half');

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('detail-unsupported');
    // 🔴 Its difference from 'unsupported-platform' is this line: the list request really was sent.
    expect(be.calls.length).toBe(1);
    // 🔴 The debts stay; not one was cleared, and none was passed off as archived.
    expect(report.state.pending.length).toBe(2);
    expect(report.state.archived).toEqual([]);
    expect(report.archivedThisRun).toEqual([]);
    // The trace has to name what is missing, not just say "not supported".
    expect(report.halted?.detail).toContain('missing:');
    expect(report.halted?.detail).toContain('detailPath');
  });

  it('the platform lists give this intermediate state its own place rather than rounding it to either side', () => {
    expect(BACKFILL_LIST_ONLY_PLATFORMS).toEqual(['deepseek', 'perplexity']);
    // 🔴 "can backfill history" still holds chatgpt alone — being able to list conversations does not count as backfilling history.
    expect(BACKFILL_SUPPORTED_PLATFORMS).toEqual(['chatgpt']);
    expect(DEEPSEEK_PLAN.partial?.missing.length ?? 0).toBeGreaterThan(0);
    expect(t(DEEPSEEK_PLAN.partial!.userNoteKey)).not.toMatch(/backfilling now|is backfilling|in progress/);
  });

  it('the plan\'s declarations and provenance: the list segment is complete and the body segment is null (not filled in with anything)', () => {
    const plan = backfillPlanFor('deepseek');
    expect(plan).not.toBeNull();
    expect(plan!.listPath).toBe('/api/v0/chat_session/fetch_page');
    expect(plan!.listCursorUrl).toBeTypeOf('function');
    expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, null, 30))
      .toBe(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=30`);
    expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, 42, 30))
      .toBe(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=30&before_seq_id=42`);
    expect(plan!.detailPath).toBeNull();
    expect(plan!.detailUrl).toBeNull();
    // Provenance is **mandatory**, and it must say "multi-source cross-check, not official documentation, at risk of staleness" — the conclusion alone is not enough.
    expect(plan!.provenance).toContain('before_seq_id');
    expect(plan!.provenance).toContain('2025-12');
    // 🔴 A parameter name that appeared in none of the five sources may not enter the URL.
    for (const banned of ['offset=', 'page=', 'page_size=', 'limit=', 'cursor=']) {
      expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, 42, 30)).not.toContain(banned);
    }
  });
});

// ---------------------------------------------------------------------------
// 6 · The allowlist: it goes through the existing mechanism, and only the path the plan wrote down itself is permitted
// ---------------------------------------------------------------------------
describe('C26-6 · the content script\'s allowlist', () => {
  it('the list path is allowed; a body path, an adjacent path and cross-origin are all refused', () => {
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=100`, DEEPSEEK_ORIGIN)).toBe(true);
    expect(isAllowedBackfillUrl(
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=100&before_seq_id=997`, DEEPSEEK_ORIGIN)).toBe(true);
    // 🔴 The body segment has no source ⇒ not one body URL is allowed through.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat/history_messages`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 It did not become a prefix wildcard: a lookalike that is not equal byte for byte is still refused.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/fetch_page2`, DEEPSEEK_ORIGIN)).toBe(false);
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 The same-origin rule did not loosen: a page on another origin is refused.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}`, 'https://chatgpt.com')).toBe(false);
    // 🔴 ChatGPT's path is not allowed on DeepSeek just because DeepSeek now has a plan.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/backend-api/conversations`, DEEPSEEK_ORIGIN)).toBe(false);
  });
});
