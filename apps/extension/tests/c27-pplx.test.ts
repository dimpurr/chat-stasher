/**
 * C27 · Perplexity's conversation list: there is no has_more, so an empty page and a short page
 * can only be two unreliable but traceable client inferences.
 *
 * Every HTTP interaction is a synthetic fixture; this file neither logs in nor sends a request to perplexity.ai.
 */

import { describe, expect, it } from 'vitest';
import { runBackfill, type HttpPort, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { checkBackfillRequest } from '../lib/backfill/tab-port';
import {
  BACKFILL_LIST_ONLY_PLATFORMS,
  BACKFILL_SUPPORTED_PLATFORMS,
  PERPLEXITY_LIST_PATH,
  PERPLEXITY_PLAN,
  backfillPlanFor,
  type BackfillEnumPlan,
  parsePerplexityListPage,
} from '../lib/backfill/enumerate';
import type { Clock } from '../lib/backfill/pace';
import { stateKey } from '../lib/backfill/types';

const ORIGIN = 'https://www.perplexity.ai';
const LIMIT = 2;

function fakeClock(): Clock {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

/**
 * One record of the list response, keyed the way the live endpoint keys it.
 *
 * 🔴 W65: the field is `slug`. C27 wrote `thread_id` here, and a live probe
 * (2026-09-23) found the endpoint's items carry 36 keys with no `thread_id`
 * among them and `slug` / `uuid` / `context_uuid` as three separate strings. The
 * probe and the reasoning are in `w65-pplx-list-shape.test.ts`.
 */
function thread(n: number): { slug: string } {
  return { slug: `pplx-${String(n).padStart(4, '0')}-aaaaaaaa` };
}

function pageBody(items: Array<{ slug: string }>): string {
  // R26's list outcome depends only on the returned array's length; there is no total / has_more / count.
  return JSON.stringify(items);
}

interface Call {
  url: string;
  init?: { method?: string; body?: string; contentType?: string };
}

/**
 * 🔴 W84 · A *list-only* plan, injected for the run tests below.
 *
 * C27 is the list test. When Perplexity was a list-only plan the leg enumerated
 * to the end and halted `detail-unsupported` in one unbounded run, which is the
 * shape every assertion here was written against. W84 filled the real plan's
 * body in, so the real plan now runs capped enumeration and fetches bodies —
 * neither of which C27 is about. So the run tests inject this list-only plan
 * (it uses the **real** `parsePerplexityListPage`, so the list parsing under test
 * is unchanged) and keep testing the list exactly as designed; the full-plan,
 * body-fetching behaviour is the business of tests/w84-pplx-detail.test.ts.
 */
const PERPLEXITY_LIST_ONLY_PLAN: BackfillEnumPlan = {
  platform: 'perplexity',
  listPath: PERPLEXITY_LIST_PATH,
  listUrl: (origin) => `${origin}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
  listPost: {
    contentType: 'application/json',
    bodyKeys: ['limit', 'offset', 'ascending', 'search_term'],
    body: (_origin, offset, limit) => JSON.stringify({ limit, offset, ascending: false, search_term: '' }),
  },
  parseListPage: parsePerplexityListPage,
  detailPath: null,
  detailUrl: null,
  provenance: 'synthetic list-only plan: keeps the C27 list tests list-scoped',
};

function backend(pages: string[]): { http: HttpPort; calls: Call[] } {
  const calls: Call[] = [];
  const http: HttpPort = async (url, init) => {
    calls.push({ url, init });
    expect(new URL(url).pathname).toBe(PERPLEXITY_LIST_PATH);
    return { status: 200, text: pages[calls.length - 1] ?? '[]' };
  };
  return { http, calls };
}

async function run(store: ReturnType<typeof memoryStore>, pages: string[], scope: string) {
  const be = backend(pages);
  const report = await runBackfill({
    platform: 'perplexity',
    origin: ORIGIN,
    scope,
    store,
    http: be.http,
    clock: fakeClock(),
    listLimit: LIMIT,
    // 🔴 W84 · the list-only plan, so this list test keeps the unbounded-enum +
    //    halt-detail-unsupported behaviour it was written against.
    plans: (platform: string) =>
      platform === 'perplexity' ? PERPLEXITY_LIST_ONLY_PLAN : backfillPlanFor(platform),
  });
  return { report, calls: be.calls };
}

function requestBody(offset: number): string {
  return JSON.stringify({ limit: LIMIT, offset, ascending: false, search_term: '' });
}

describe('C27-1 · POST paging parameters', () => {
  it('two pages requested: the second page has offset=limit, and the fixed parameters are kept as-is', async () => {
    const { report, calls } = await run(
      memoryStore(),
      [pageBody([thread(1), thread(2)]), pageBody([thread(3)])],
      'acct-two-pages',
    );

    expect(calls).toHaveLength(2);
    expect(calls.map((call) => call.url)).toEqual([
      `${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
      `${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
    ]);
    expect(calls.map((call) => JSON.parse(call.init?.body ?? 'null'))).toEqual([
      { limit: LIMIT, offset: 0, ascending: false, search_term: '' },
      { limit: LIMIT, offset: LIMIT, ascending: false, search_term: '' },
    ]);
    expect(calls.every((call) => call.init?.method === 'POST')).toBe(true);
    expect(calls.every((call) => call.init?.contentType === 'application/json')).toBe(true);
    expect(report.enumeratedPages).toBe(2);
    expect(report.newDebts).toBe(3);
  });

  it('the plan URL smuggles in no paging query, and the body closed key set and fixed values can be checked item by item', () => {
    expect(backfillPlanFor('perplexity')).toBe(PERPLEXITY_PLAN);
    expect(PERPLEXITY_PLAN.listUrl(ORIGIN, 999, LIMIT))
      .toBe(`${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`);
    expect(PERPLEXITY_PLAN.listPost?.bodyKeys).toEqual([
      'limit', 'offset', 'ascending', 'search_term',
    ]);
    expect(JSON.parse(PERPLEXITY_PLAN.listPost!.body(ORIGIN, LIMIT, LIMIT)))
      .toEqual({ limit: LIMIT, offset: LIMIT, ascending: false, search_term: '' });
    // 🔴 W84 · The body segment is filled in; it is no longer the half-leg this
    //    test used to assert was null.
    expect(PERPLEXITY_PLAN.detailPath).toBe('/rest/thread/{id}');
    expect(PERPLEXITY_PLAN.detailUrl).toBeTypeOf('function');
    expect(PERPLEXITY_PLAN.detailUrl!(ORIGIN, 'pplx-0001-aaaaaaaa'))
      .toBe(`${ORIGIN}/rest/thread/pplx-0001-aaaaaaaa?with_parent_info=true`
        + '&with_schematized_response=true&version=2.18&source=default&from_first=true');
  });
});

describe('C27-2 · with no termination field, an empty page and a short page must be traced separately', () => {
  it('stops on an empty page: records empty-page-inferred, and complete is not true', async () => {
    const store = memoryStore();
    const { report, calls } = await run(
      store,
      [pageBody([thread(1), thread(2)]), pageBody([])],
      'acct-empty-page',
    );

    expect(calls).toHaveLength(2);
    expect(report.enumTruncated).toBe('empty-page-inferred');
    expect(report.state.enumCursor.truncated).toBe('empty-page-inferred');
    expect(report.state.enumCursor.complete).toBe(false);
    const persisted = await store.load(stateKey('perplexity', 'acct-empty-page')) as {
      enumCursor: { complete: boolean; truncated?: string };
    };
    expect(persisted.enumCursor).toEqual({
      offset: LIMIT * 2,
      complete: false,
      truncated: 'empty-page-inferred',
    });
  });

  it('stops on a short page: records short-page-inferred, which is not the same outcome as an empty page', async () => {
    const { report } = await run(
      memoryStore(),
      [pageBody([thread(1), thread(2)]), pageBody([thread(3)])],
      'acct-short-page',
    );

    expect(report.enumTruncated).toBe('short-page-inferred');
    expect(report.state.enumCursor.truncated).toBe('short-page-inferred');
    expect(report.enumTruncated).not.toBe('empty-page-inferred');
    expect(report.state.enumCursor.complete).toBe(false);
    // The list segment really was read, and (with the injected list-only plan) the
    // body segment has no source, so this is not "the user has no conversations".
    expect(report.halted?.reason).toBe('detail-unsupported');
  });
});

describe('C27-3 · shape drift and "no conversations" must be distinguishable', () => {
  it('no top-level array ⇒ shape-changed, which must not be taken as the user having no conversations', async () => {
    const store = memoryStore();
    const { report, calls } = await run(store, [JSON.stringify({ threads: [] })], 'acct-shape');

    expect(calls).toHaveLength(1);
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('top-level array');
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.state.pending).toEqual([]);
    expect(report.stopped).not.toBe('queue-empty');
    expect((await store.load(stateKey('perplexity', 'acct-shape')) as any).halted.reason)
      .toBe('shape-changed');
  });

  it('a genuinely empty array is an inferred stopping point, not shape-changed', async () => {
    const { report } = await run(memoryStore(), [pageBody([])], 'acct-no-history');
    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.enumTruncated).toBe('empty-page-inferred');
    expect(report.state.enumCursor.complete).toBe(false);
  });

  it('the parser recognises only the list array and the `slug`, and reads no unverified time field', () => {
    expect(parsePerplexityListPage(pageBody([thread(1)]) )).toEqual({
      ok: true,
      page: { ids: ['pplx-0001-aaaaaaaa'], total: null },
    });
    expect(parsePerplexityListPage(JSON.stringify({ threads: [] })).ok).toBe(false);
    expect(parsePerplexityListPage(JSON.stringify([{ id: 'wrong-field' }])).ok).toBe(false);
  });
});

describe('C27-4 · the Perplexity backfill allowlist', () => {
  it('the exact list path + a correct POST is allowed; a similar prefix, a body path and cross-origin are refused', () => {
    const listUrl = `${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`;
    const valid = {
      url: listUrl,
      method: 'POST',
      body: requestBody(0),
      contentType: 'application/json',
    } as const;
    expect(checkBackfillRequest(valid, ORIGIN).ok).toBe(true);
    expect(checkBackfillRequest({
      ...valid,
      url: `${ORIGIN}${PERPLEXITY_LIST_PATH}2?version=2.18&source=default`,
    }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({
      ...valid,
      url: `${ORIGIN}/rest/thread/get_thread/pplx-0001`,
    }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({
      ...valid,
      url: `https://evil.example${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
    }, ORIGIN).ok).toBe(false);
  });

  it('the platform lists: Perplexity now has both segments, so no platform is list-only', () => {
    // 🔴 W84 (2026-09-23) · The fact changed, not the criterion: Perplexity's body segment was
    //    filled in from the live probe (completeness signal observed), so it left the list-only
    //    side and joined the supported one. That side is now empty — every platform backfills
    //    both segments — which is a state worth naming rather than discovering from an empty array.
    expect(BACKFILL_LIST_ONLY_PLATFORMS).toEqual([]);
    // Perplexity is backfillable: in platform-table order.
    expect(BACKFILL_SUPPORTED_PLATFORMS)
      .toEqual(['deepseek', 'perplexity', 'chatgpt', 'gemini', 'claude', 'kimi', 'grok']);
  });
});
