/**
 * W84 · Perplexity's conversation body: the GET /rest/thread/{slug} route, its
 * pinned query, and a completeness rule read off a real logged-in response.
 *
 * ## What this file exists to stop changing back
 *
 *  1. **The completeness rule is the signal, not a window.** The 2026-09-23
 *     logged-in probe measured the body envelope as carrying top-level
 *     `has_next_page` (boolean) and `next_cursor` (string | null). So a body is
 *     archived only when it declares there is no more; a body that declares more
 *     is `detail-paged-unsupported` (real content, explicitly incomplete), and an
 *     empty one is `detail-empty-unverified` (never a confirmed receipt).
 *  2. **A body with no signal at all is a shape change, not a complete thing.**
 *     Neither key present ⇒ {ok:false} ⇒ the leg halts `shape-changed`. A
 *     windowed response with no signal would be the silent-truncation failure
 *     this project exists to refuse.
 *  3. **The request is pinned exactly.** detailPath is the `{id}` form
 *     (`/rest/thread/{id}`, one segment, never a prefix wildcard); the query is
 *     the pinned five-key set; the id is the list's `slug`. The allowlist admits
 *     a body URL only when it is byte-for-byte this plan's own builder's output.
 *  4. **Perplexity is no longer list-only.** Both segments exist, so
 *     BACKFILL_SUPPORTED_PLATFORMS holds it and the list-only roster does not.
 *
 * 🔴 Every fixture is synthetic, hand-written from the field names and types the
 *    probe observed. No request goes to perplexity.ai; there is no logged-in
 *    state; no real conversation, account or id appears here.
 */

import { describe, expect, it } from 'vitest';
import {
  runBackfill,
  type HttpResponse,
  type SinkOutcome,
} from '../lib/backfill/engine';
import type { CapturedFetch } from '../lib/contract';
import { memoryStore } from '../lib/backfill/store';
import { checkBackfillRequest } from '../lib/backfill/tab-port';
import {
  BACKFILL_LIST_ONLY_PLATFORMS,
  BACKFILL_SUPPORTED_PLATFORMS,
  parsePerplexityDetailPage,
  parsePerplexityListPage,
  PERPLEXITY_DETAIL_PATH,
  PERPLEXITY_DETAIL_QUERY,
  PERPLEXITY_PLAN,
} from '../lib/backfill/enumerate';
import { describeFailureReason } from '../lib/backfill/failures';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://www.perplexity.ai';
/** The thread's identity as the page URL and the list carry it: the slug. */
const SLUG = 'how-do-i-rotate-a-secret-1aBcDeFg';
/** A second thread, for the end-to-end test. */
const SLUG2 = 'why-is-the-sky-blue-9zYx8WvU';

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock & { sleeps: number[] } {
  const sleeps: number[] = [];
  let time = Date.parse('2026-09-23T00:00:00.000Z');
  return {
    sleeps,
    now: () => time,
    async sleep(ms: number) {
      sleeps.push(ms);
      time += ms;
    },
  };
}

// ---------------------------------------------------------------------------
// Synthetic fixtures — the field names/types the probe saw, invented values
// ---------------------------------------------------------------------------

/** A complete (no-more) body: the shape a whole thread answers with. */
function completeBody(...slugs: string[]): string {
  return JSON.stringify({
    background_entries: [],
    entries: slugs.map((s) => ({
      uuid: `entry-uuid-for-${s}`,
      query_str: `synthetic question for ${s}`,
      blocks: [{ intended_usage: 'ask_text', plan_block: { answer: `synthetic answer ${s}` } }],
      updated_datetime: '2026-09-01T10:00:00.000Z',
      thread_title: `synthetic thread ${s}`,
    })),
    first_entry: null,
    has_next_page: false,
    latest_entry: null,
    next_cursor: null,
    status: 'success',
    thread_metadata: {},
  });
}

/** A body that says there is more of the thread than it carries. */
function moreBody(): string {
  return JSON.stringify({
    entries: [{ uuid: 'e1', query_str: 'q', blocks: [] }],
    has_next_page: true,
    next_cursor: 'gABhC',
    status: 'success',
  });
}

/** A thread with no turns at all, declaring no more. */
function emptyBody(): string {
  return JSON.stringify({
    entries: [],
    has_next_page: false,
    next_cursor: null,
    status: 'success',
  });
}

function threadRecord(slug: string): Record<string, unknown> {
  return {
    slug,
    uuid: 'b7c2f0e3-4a51-4d8f-9c31-0f2a6e5d7b90',
    title: `synthetic-${slug}`,
    has_next_page: false,
    total_threads: 2,
  };
}

function pageBody(slugs: string[]): string {
  return JSON.stringify(slugs.map(threadRecord));
}

/** The body URL the plan's own builder emits for one slug. */
function detailUrlFor(slug: string): string {
  // 🔴 W84 · The body segment is declared, so detailUrl is set; `!` is the type-safety
  //    spelling of "this plan has a body route", exactly as the other full plans assert.
  return PERPLEXITY_PLAN.detailUrl!(ORIGIN, slug);
}

/** The pinned query as it appears in the built URL. */
function pinnedQueryString(): string {
  return PERPLEXITY_DETAIL_QUERY.map(({ key, value }) => `${key}=${value}`).join('&');
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: (url: string) => Promise<HttpResponse>,
  scope: string,
  sink?: (captured: CapturedFetch) => SinkOutcome,
) {
  return runBackfill({
    platform: 'perplexity',
    origin: ORIGIN,
    scope,
    store,
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    listLimit: 2,
    sink: sink ?? ((captured: CapturedFetch): SinkOutcome => ({ saved: true, sessionId: captured.sessionId })),
  });
}

describe('W84-1 · parsePerplexityDetailPage decides completeness from the signal', () => {
  it('a non-empty body declaring no more is archived: non-empty', () => {
    expect(parsePerplexityDetailPage(completeBody(SLUG))).toEqual({
      ok: true,
      outcome: 'non-empty',
    });
  });

  it('a body declaring more is refused per conversation, never archived', () => {
    expect(parsePerplexityDetailPage(moreBody())).toEqual({
      ok: true,
      outcome: 'detail-paged-unsupported',
    });
    // The same, when only `next_cursor` carries the "more" claim.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1' }], has_next_page: false, next_cursor: 'abc' }),
    )).toEqual({ ok: true, outcome: 'detail-paged-unsupported' });
  });

  it('an empty body declaring no more is detail-empty-unverified, never confirmed', () => {
    expect(parsePerplexityDetailPage(emptyBody())).toEqual({
      ok: true,
      outcome: 'detail-empty-unverified',
    });
  });

  it('a body with no completeness signal at all is a shape change, not complete', () => {
    const noSignal = JSON.stringify({ entries: [{ uuid: 'e1' }] });
    const parsed = parsePerplexityDetailPage(noSignal);
    expect(parsed.ok).toBe(false);
    expect(parsed.ok ? null : parsed.detail).toContain('no completeness signal');
  });

  it('an empty `next_cursor` (with has_next_page false) is unproven, not the end: detail-unverified', () => {
    // The observed no-more value is `next_cursor: null`; `""` is not null, so the
    // pair is not a confirmed end and the body must not settle.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], has_next_page: false, next_cursor: '' }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
  });

  it('a single-key body (one of the two signal keys missing) is unproven: detail-unverified', () => {
    // Only `has_next_page: false`, no `next_cursor`.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], has_next_page: false }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
    // Only `next_cursor: null`, no `has_next_page`.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], next_cursor: null }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
    // Only `next_cursor: ""`, no `has_next_page`.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], next_cursor: '' }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
  });

  it('a single present key that says more is detail-paged-unsupported, not unverified', () => {
    // Only `has_next_page: true`.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], has_next_page: true }),
    )).toEqual({ ok: true, outcome: 'detail-paged-unsupported' });
    // Only a non-empty `next_cursor`.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], next_cursor: 'abc' }),
    )).toEqual({ ok: true, outcome: 'detail-paged-unsupported' });
  });

  it('a present-but-wrong-typed signal key is unproven, not a halt: detail-unverified', () => {
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], has_next_page: 'false', next_cursor: null }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], has_next_page: false, next_cursor: 7 }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
  });

  it('an entry with no readable content is unproven: detail-unverified', () => {
    // `blocks: []` and no `text` at all.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [] }], has_next_page: false, next_cursor: null }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
    // A `null` entry.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [null], has_next_page: false, next_cursor: null }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
    // A non-object entry.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: ['oops'], has_next_page: false, next_cursor: null }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
    // An empty `text` string and no `blocks`.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', text: '' }], has_next_page: false, next_cursor: null }),
    )).toEqual({ ok: true, outcome: 'detail-unverified' });
  });

  it('either non-empty `blocks` or non-empty `text` makes an entry readable: non-empty', () => {
    // The schematized shape: blocks, no text.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', blocks: [{}] }], has_next_page: false, next_cursor: null }),
    )).toEqual({ ok: true, outcome: 'non-empty' });
    // The minimal shape: text, no blocks.
    expect(parsePerplexityDetailPage(
      JSON.stringify({ entries: [{ uuid: 'e1', text: 'hello' }], has_next_page: false, next_cursor: null }),
    )).toEqual({ ok: true, outcome: 'non-empty' });
  });

  it('a body that is not the entries envelope is a shape change', () => {
    expect(parsePerplexityDetailPage('not json').ok).toBe(false);
    expect(parsePerplexityDetailPage(JSON.stringify({ data: { entries: [] } })).ok).toBe(false);
    expect(parsePerplexityDetailPage('[]').ok).toBe(false);
    expect(parsePerplexityDetailPage(JSON.stringify({ entries: 'oops' })).ok).toBe(false);
  });
});

describe('W84-6 · the `detail-unverified` failure reason reads as a named fact', () => {
  it('the sentence is the observed fact, not the raw reason code', () => {
    const sentence = describeFailureReason('detail-unverified');
    expect(sentence).toContain('nothing was stored');
    expect(sentence).not.toContain('detail-unverified');
  });
});

describe('W84-2 · the plan declares the body segment', () => {
  it('detailPath is the {id} form, detailUrl feeds the slug, and the query is the pinned set', () => {
    expect(PERPLEXITY_PLAN.detailPath).toBe(PERPLEXITY_DETAIL_PATH);
    expect(PERPLEXITY_PLAN.detailPath).toBe('/rest/thread/{id}');
    expect(PERPLEXITY_PLAN.detailUrl).toBeTypeOf('function');
    expect(detailUrlFor(SLUG)).toBe(`${ORIGIN}/rest/thread/${SLUG}?${pinnedQueryString()}`);
    expect(PERPLEXITY_PLAN.detailQueryPinned).toEqual(PERPLEXITY_DETAIL_QUERY);
    // No remaining "missing half": both segments exist.
    expect(PERPLEXITY_PLAN.partial).toBeUndefined();
  });
});

describe('W84-3 · the allowlist admits exactly this body request', () => {
  it('the plan\'s own body URL with GET is allowed', () => {
    expect(checkBackfillRequest({ url: detailUrlFor(SLUG), method: 'GET' }, ORIGIN).ok).toBe(true);
  });

  it('a body URL whose query is not the pinned set is refused', () => {
    // A sibling GET shares the {id} path shape; only the pinned query lets a
    // thread through — this is how list_recent & friends are stopped.
    expect(checkBackfillRequest(
      { url: `${ORIGIN}/rest/thread/list_recent?version=2.18&source=default`, method: 'GET' },
      ORIGIN,
    ).ok).toBe(false);
    // Same path, no query at all.
    expect(checkBackfillRequest(
      { url: `${ORIGIN}/rest/thread/${SLUG}`, method: 'GET' },
      ORIGIN,
    ).ok).toBe(false);
    // A query with one extra key.
    expect(checkBackfillRequest(
      { url: `${detailUrlFor(SLUG)}&limit=100`, method: 'GET' },
      ORIGIN,
    ).ok).toBe(false);
    // The list route is a POST and stays a POST; it can never be read as a body.
    // (It is sent with the plan's own declared body, exactly as the engine builds it.)
    expect(checkBackfillRequest({
      url: `${ORIGIN}/rest/thread/list_ask_threads?version=2.18&source=default`,
      method: 'POST',
      body: JSON.stringify({ limit: 2, offset: 0, ascending: false, search_term: '' }),
      contentType: 'application/json',
    }, ORIGIN).ok).toBe(true);
    // The body route is a GET; a POST of it is refused.
    expect(checkBackfillRequest({ url: detailUrlFor(SLUG), method: 'POST' }, ORIGIN).ok).toBe(false);
    // Cross-origin is refused.
    expect(checkBackfillRequest(
      { url: detailUrlFor(SLUG).replace(ORIGIN, 'https://evil.example'), method: 'GET' },
      ORIGIN,
    ).ok).toBe(false);
  });
});

describe('W84-4 · end to end: a listed conversation is archived once its body reads complete', () => {
  it('list, then exactly one GET /rest/thread/{slug} per id, and each stored body is its own', async () => {
    const store = memoryStore();
    const calls: string[] = [];
    const storedText: Record<string, string> = {};
    const http = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      const u = new URL(url);
      if (u.pathname === PERPLEXITY_PLAN.listPath) {
        // One full page of two, then an empty page: the list reaches its own end.
        return calls.length === 1
          ? { status: 200, text: pageBody([SLUG, SLUG2]) }
          : { status: 200, text: '[]' };
      }
      // A body request: return the body for exactly the asked slug, so the two
      // threads carry distinct text.
      const slug = decodeURIComponent(u.pathname.split('/').pop() ?? '');
      return { status: 200, text: completeBody(slug) };
    };

    const report = await run(store, http, 'w84-e2e', (captured) => {
      storedText[captured.sessionId ?? ''] = captured.text;
      return { saved: true, sessionId: captured.sessionId ?? '' };
    });

    // The list was read (to its end), then both bodies were fetched by slug.
    expect(report.state.archived).toEqual(expect.arrayContaining([SLUG, SLUG2]));
    // 🔴 Exactly ONE body request per id — a second request would re-request a
    //    thread whose completeness leg was not finished, and must fail this test.
    for (const slug of [SLUG, SLUG2]) {
      expect(calls.filter((c) => c === detailUrlFor(slug)).length).toBe(1);
    }
    // Stored text is per-thread: each slug archived the body that answered it,
    // and the two stored bodies are distinct from each other.
    expect(storedText[SLUG]).toContain(`synthetic question for ${SLUG}`);
    expect(storedText[SLUG2]).toContain(`synthetic question for ${SLUG2}`);
    expect(storedText[SLUG]).not.toBe(storedText[SLUG2]);
    // Neither leg halted: this is a full platform now, not list-only.
    expect(report.stopped).not.toBe('halted');
  });

  it('a body that is not proven whole is a named failure, never archived', async () => {
    const store = memoryStore();
    let bodyCalls = 0;
    const http = async (url: string): Promise<HttpResponse> => {
      const u = new URL(url);
      if (u.pathname === PERPLEXITY_PLAN.listPath) {
        return { status: 200, text: pageBody([SLUG]) };
      }
      // The body declares the confirmed pair but its only entry has no readable
      // content (`blocks: []`, no `text`) — so it is detail-unverified.
      bodyCalls += 1;
      return { status: 200, text: JSON.stringify({ entries: [{ uuid: 'e1', blocks: [] }], has_next_page: false, next_cursor: null }) };
    };

    const report = await run(store, http, 'w84-unverified');

    expect(bodyCalls).toBe(1);
    expect(report.state.archived).not.toContain(SLUG);
    // A named per-conversation failure — the leg keeps going, nothing stored.
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['detail-unverified']);
    expect(report.stopped).not.toBe('halted');
  });
});

describe('W84-5 · the roster: Perplexity is no longer list-only', () => {
  it('BACKFILL_SUPPORTED holds it and the list-only roster does not', () => {
    expect(BACKFILL_SUPPORTED_PLATFORMS).toContain('perplexity');
    expect(BACKFILL_LIST_ONLY_PLATFORMS).not.toContain('perplexity');
    // The list parser still reads `slug` for its ids (the join between the legs).
    expect(parsePerplexityListPage(pageBody([SLUG]))).toEqual({
      ok: true,
      page: { ids: [SLUG], total: null },
    });
  });
});