/**
 * W29 · Gemini (gemini.google.com): the `batchexecute` plan, the page's own
 * bootstrap tokens, a paged conversation body, and a live leg that never stores
 * the first page of a long conversation as if it were the whole thing.
 *
 * ## What this file exists to stop changing back
 *  1. **The canonical conversation id is the `c_`-prefixed form**, in both legs.
 *     The list endpoint returns that form, so a debt key is one; the page URL
 *     (`/app/<bare id>`) is the one place the prefix is missing, and a live
 *     capture therefore names a conversation by the id inside the response it
 *     just saw. Two names for one conversation would file it twice.
 *  2. **A paged body is followed to the end, or not stored at all.** Consecutive
 *     detail pages are disjoint (measured), the token is opaque, and a page whose
 *     turns are all already seen is the cursor not advancing. Reaching the page
 *     cap is `detail-too-long` — a refusal, never "the pages we happened to get".
 *  3. **The page's tokens are filled in by the page-context wrapper and by nothing
 *     else.** A message that arrives carrying a credential is refused, the request
 *     that goes out without one is refused by the platform, and a refusal is a
 *     halt — never "you have no conversations".
 *  4. **The allowlist grew no wildcard.** Two pinned queries on one shared path,
 *     two closed field sets, and a batch whose arguments are checked structurally.
 *
 * 🔴 All fixtures are **synthetic**, hand-written from the field names and
 *    positions measured on 2026-09-14 (names, positions and counts only). No
 *    request goes to gemini.google.com, there is no logged-in state, and no real
 *    conversation, account, token or id appears anywhere below. The http port is
 *    always injected and always throws on a request it was not given, so "no
 *    request was sent" is proven by the run rather than asserted about it.
 *
 * 🔴 The envelope fixtures include the measured **+2 length quirk** (every declared
 *    frame length is the chunk's code-unit count plus two), because a fixture that
 *    only ever exercises the fallback would not be testing the envelope this
 *    platform really sends.
 */

import { describe, expect, it } from 'vitest';
import { findPlatformForUrl, matchesResponseShape, PLATFORMS, type CapturedFetch } from '../lib/contract';
import { runBackfill, type HttpResponse, type HttpPort, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  GEMINI_BATCHEXECUTE_PATH,
  GEMINI_DETAIL_PAGE_DELAY_MS,
  GEMINI_DETAIL_PAGE_SIZE,
  GEMINI_FORM_FIELD_AT,
  GEMINI_FORM_FIELD_BATCH,
  GEMINI_LIST_PAGE_SIZE,
  GEMINI_MAX_DETAIL_PAGES,
  GEMINI_PAGE_QUERY_KEYS,
  GEMINI_PLAN,
  GEMINI_QUERY_KEY_BL,
  GEMINI_QUERY_KEY_F_SID,
  GEMINI_QUERY_KEY_HL,
  GEMINI_QUERY_KEY_REQID,
  GEMINI_QUERY_RPCIDS,
  GEMINI_QUERY_SOURCE_PATH,
  GEMINI_QUERY_RT,
  GEMINI_RT_VALUE,
  GEMINI_SOURCE_PATH_VALUE,
  backfillPlanFor,
  detailRequestInit,
  expectedMethodFor,
  formSegmentFor,
  listRequestInit,
  listTokenPostInit,
  parseGeminiDetailPage,
  parseGeminiListPage,
  type BackfillEnumPlan,
} from '../lib/backfill/enumerate';
import {
  GEMINI_RPC_DETAIL,
  GEMINI_RPC_LIST,
  assembleDetailBundle,
  buildBatchExecuteBody,
  parseBatchExecute,
  readDetailBundle,
  readDetailResponse,
  readListResponse,
} from '../lib/gemini-rpc';
import { checkBackfillRequest, REFUSED_URL_REASON } from '../lib/backfill/tab-port';
import { describeFailureReason } from '../lib/backfill/failures';
import {
  GEMINI_FORM_CONTENT_TYPE,
  createGeminiAuthorizedFetch,
  needsGeminiTokens,
  type MinimalResponse,
} from '../lib/platform-auth';
import { completeGeminiLiveCapture, isGeminiDetailRequest } from '../lib/gemini-capture';
import type { GeminiBootstrapTokens } from '../lib/contract';
import type { BackfillPace, Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://gemini.google.com';
/** The `c_`-prefixed canonical form — what the list endpoint returns. */
const ID = 'c_1a2b3c4d5e6f708192a3b4c5d6e7f809';
const ID2 = 'c_0f9e8d7c6b5a49382716f5e4d3c2b1a0';
const ID3 = 'c_11223344556677889900112233445566';
/** The bare form, as it appears in a page URL. Deliberately *not* what anything is filed under. */
const BARE_ID = ID.slice(2);
const PAGE_URL = `${ORIGIN}/app/${BARE_ID}`;
/** Synthetic token literals. The measured `at` is ~42 characters; these are invented. */
const AT_TOKEN = 'synthetic-at-token-value';
const BL_LABEL = 'synthetic-build-label';
const F_SID = 'synthetic-session-number';

const NO_WAIT: BackfillPace = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock & { sleeps: number[] } {
  const sleeps: number[] = [];
  let time = Date.parse('2026-09-14T00:00:00.000Z');
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
// Synthetic envelope fixtures
// ---------------------------------------------------------------------------

/**
 * The guarded body: `)]}'`, then one length-prefixed frame per chunk.
 *
 * 🔴 The declared length is the chunk's UTF-16 length **+2**, which is what every
 *    frame of the 2026-09-14 capture declared. `parseBatchExecute` validates the
 *    number rather than obeying it, so this fixture exercises the real framing and
 *    not only the newline fallback.
 */
function guarded(frames: unknown[]): string {
  let out = ")]}'\n";
  for (const frame of frames) {
    const chunk = JSON.stringify(frame);
    out += `${chunk.length + 2}\n${chunk}\n`;
  }
  return out;
}

/** One `wrb.fr` entry: `[label, rpcid, "<inner JSON as a string>", …]`. */
function entry(rpcid: string, payload: unknown): unknown[] {
  return ['wrb.fr', rpcid, JSON.stringify(payload), null, null];
}

function listResponse(payload: unknown): string {
  return guarded([[entry(GEMINI_RPC_LIST, payload)]]);
}

function detailResponse(payload: unknown): string {
  return guarded([[entry(GEMINI_RPC_DETAIL, payload)]]);
}

/** A list payload: `[<unused>, token, items]`, each item `[id, title, …, [seconds, nanos]]`. */
function listPayload(ids: string[], token: string | null): unknown[] {
  return [
    null,
    token,
    ids.map((id, index) => [id, `synthetic-title-${index}`, null, null, null, [1_700_000_000 + index, 0]]),
  ];
}

/** A detail payload: `[turns, token]`, each turn `[[conversationId, responseId], …, [seconds, nanos]]`. */
function detailPayload(turns: [string, string][], token: string | null): unknown[] {
  return [
    turns.map(([conversationId, responseId], index) => [
      [conversationId, responseId],
      null,
      null,
      null,
      [1_700_000_000 + index, index],
    ]),
    token,
  ];
}

/** How many turns a synthetic page of a conversation holds — 5, so the fixtures stay small. */
const TURNS_PER_PAGE = 5;

function turnIds(page: number, conversationId = ID): [string, string][] {
  return Array.from(
    { length: TURNS_PER_PAGE },
    (_, index) => [conversationId, `r_${page}_${index}`] as [string, string],
  );
}

// ---------------------------------------------------------------------------
// The synthetic backend
// ---------------------------------------------------------------------------

interface RecordedCall {
  url: string;
  method: string;
  body: string;
}

/**
 * A backend that speaks this platform's two RPCs. Both arrive on **one** path, so
 * the rpcid in the form body is what routes them — and the recorded calls are what
 * "this leg sent exactly N requests" is asserted against, never assumed.
 *
 * Any request it cannot answer throws, so a leg that sends something unexpected
 * fails the test rather than being waved through.
 */
function geminiBackend(options: {
  listPages: string[];
  detailPages?: (id: string, page: number, token: string | null) => string;
}): { http: HttpPort; calls: RecordedCall[] } {
  const calls: RecordedCall[] = [];
  const listCursor = { at: 0 };
  const detailCursor = new Map<string, number>();
  const http: HttpPort = async (url, init) => {
    const body = init?.body ?? '';
    calls.push({ url, method: init?.method ?? 'GET', body });
    const fields = new URLSearchParams(body);
    const batch = JSON.parse(fields.get(GEMINI_FORM_FIELD_BATCH) ?? 'null') as unknown;
    // 🔴 `f.req` is `[[[rpcid, "<args>", null, "generic"]]]`: the outer level is
    //    the batch, the next is the group, and the call is inside that. Read the
    //    same way the allowlist reads it — from the measured shape, not from a
    //    convenient one.
    const call = (batch as unknown[][])[0]![0] as unknown[];
    const rpcid = call[0] as string;
    const args = JSON.parse(call[1] as string) as unknown[];
    if (rpcid === GEMINI_RPC_LIST) {
      const page = options.listPages[listCursor.at];
      if (page === undefined) throw new Error('the list was asked for a page it does not have');
      listCursor.at += 1;
      return { status: 200, text: page };
    }
    if (rpcid === GEMINI_RPC_DETAIL) {
      if (!options.detailPages) throw new Error('a detail request arrived where none was declared');
      const id = args[0] as string;
      const token = args[2] as string | null;
      const page = detailCursor.get(id) ?? 0;
      detailCursor.set(id, page + 1);
      return { status: 200, text: options.detailPages(id, page, token) };
    }
    throw new Error(`unexpected rpcid ${rpcid}`);
  };
  return { http, calls };
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: HttpPort,
  scope: string,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'gemini',
    origin: ORIGIN,
    scope,
    store,
    http,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink: (captured: CapturedFetch): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
    ...extra,
  });
}

/** The form body of one recorded call, as a URLSearchParams — the shape every request here has. */
function formOf(call: RecordedCall): URLSearchParams {
  expect(call.method).toBe('POST');
  return new URLSearchParams(call.body);
}

/**
 * 🔴 The decoded `f.req` batch of one recorded call, at the nesting the measured
 *    shape has: `[[[rpcid, "<args>", null, "generic"]]]` — batch, group, call.
 */
function batchOf(call: RecordedCall): unknown[][][] {
  const parsed = JSON.parse(formOf(call).get(GEMINI_FORM_FIELD_BATCH) ?? 'null') as unknown[][][];
  expect(JSON.stringify(parsed)).toMatch(/^\[\[\[/);
  return parsed;
}

// ---------------------------------------------------------------------------
// 1 · The envelope readers over the two payloads
// ---------------------------------------------------------------------------
describe('W29-1 · reading one page of each RPC', () => {
  it('reads the list page: ids in order, the token, and no invented total', () => {
    const page = parseGeminiListPage(listResponse(listPayload([ID, ID2], 'synthetic-token')));
    expect(page.ok).toBe(true);
    if (!page.ok) return;
    expect(page.page.ids).toEqual([ID, ID2]);
    // This endpoint prints no total; a made-up denominator would become a percentage.
    expect(page.page.total).toBeNull();
    expect(page.page.nextToken).toBe('synthetic-token');
  });

  it('a missing token and an empty page are the two end-of-list signals, and both are readable', () => {
    const exhausted = readListResponse(listResponse(listPayload([ID], null)));
    expect(exhausted.ok && exhausted.nextPageToken).toBeNull();
    const empty = readListResponse(listResponse(listPayload([], 'synthetic-token')));
    expect(empty.ok).toBe(true);
    if (!empty.ok) return;
    expect(empty.pageIsEmpty).toBe(true);
    expect(empty.ids).toEqual([]);
  });

  it('an item with no readable id is a named shape error, never a shorter page', () => {
    const read = readListResponse(listResponse([null, null, [[null, 'title', null, null, null, null]]]));
    expect(read).toEqual({ ok: false, reason: 'item-id-missing' });
  });

  it('reads the detail page: turns, the continuation token, and the c_-prefixed conversation', () => {
    const read = readDetailResponse(detailResponse(detailPayload(turnIds(0), 'synthetic-page-token')));
    expect(read.ok).toBe(true);
    if (!read.ok) return;
    expect(read.conversationId).toBe(ID);
    expect(read.responseIds).toEqual(['r_0_0', 'r_0_1', 'r_0_2', 'r_0_3', 'r_0_4']);
    expect(read.nextPageToken).toBe('synthetic-page-token');
  });

  it('a page belonging to another conversation is refused rather than concatenated', () => {
    const read = readDetailResponse(detailResponse(detailPayload(turnIds(1, ID2), null)), ID);
    expect(read).toEqual({ ok: false, reason: 'conversation-id-mismatch' });
  });

  it('the +2 length quirk is real in these fixtures: the declared length is not the chunk’s', () => {
    const body = detailResponse(detailPayload(turnIds(0), null));
    const parsed = parseBatchExecute(body);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    // Every frame declares two more units than it holds, so the parser had to fall
    // back to the newline chunk for every one of them — recorded, not hidden.
    expect(parsed.lengthPrefixMismatchCount).toBeGreaterThan(0);
    expect(parsed.entries[0]!.rpcid).toBe(GEMINI_RPC_DETAIL);
  });
});

// ---------------------------------------------------------------------------
// 2 · Enumeration: the list cursor, its two endings, and the repeat-page guard
// ---------------------------------------------------------------------------
describe('W29-2 · enumeration', () => {
  /**
   * 🔴 **One tick reads one list page** when the plan can fetch bodies
   *    (engine.ts's `listPagesThisTick`), so a two-page list is two runs here.
   *    That is not a test convenience — it is the shipped behaviour, and writing
   *    it any other way would assert an enumeration rate the product does not have.
   */
  it('two list pages ⇒ one debt per item, keyed by the c_-prefixed id the list returned', async () => {
    const store = memoryStore();
    const { http, calls } = geminiBackend({
      listPages: [
        listResponse(listPayload([ID, ID2], 'synthetic-token')),
        listResponse(listPayload([ID3], null)),
      ],
    });
    const first = await run(store, http, 'acct-one', { maxDetails: 0 });
    const second = await run(store, http, 'acct-one', { maxDetails: 0 });
    expect(first.halted).toBeNull();
    expect(second.halted).toBeNull();
    expect(second.enumTruncated).toBeNull();
    // Three distinct conversations across the two pages, and no duplicates.
    expect(second.state.pending.slice().sort()).toEqual([ID, ID2, ID3].sort());
    expect(second.newDebts).toBe(1);

    // The page size is the measured one, and the token the first page returned
    // goes out again on the second request, byte for byte.
    const pageOne = batchOf(calls[0]!);
    expect(pageOne[0]![0]![0]).toBe(GEMINI_RPC_LIST);
    // 🔴 The plan's own measured page size, not the engine's cross-platform
    //    default (100): 20 is the only value ever observed being honoured.
    expect(JSON.parse(pageOne[0]![0]![1] as string)).toEqual([GEMINI_LIST_PAGE_SIZE, null, [0, null, 1]]);
    expect(GEMINI_LIST_PAGE_SIZE).toBe(20);
    const pageTwo = batchOf(calls[1]!);
    expect(JSON.parse(pageTwo[0]![0]![1] as string)[1]).toBe('synthetic-token');
  });

  it('enumeration ends on a null token, and on an empty page, without either being called a failure', async () => {
    for (const last of [
      listResponse(listPayload([ID3], null)),
      listResponse(listPayload([], 'synthetic-token')),
    ]) {
      const store = memoryStore();
      const { http } = geminiBackend({
        listPages: [listResponse(listPayload([ID], 'synthetic-token')), last],
      });
      await run(store, http, 'acct-end', { maxDetails: 0 });
      const report = await run(store, http, 'acct-end', { maxDetails: 0 });
      expect(report.halted).toBeNull();
      expect(report.enumTruncated).toBeNull();
      expect(report.state.enumCursor.complete).toBe(true);
    }
  });

  it('the repeat-page guard: a page carrying only ids this enumeration already saw halts, it is not "the end"', async () => {
    const store = memoryStore();
    const { http } = geminiBackend({
      // Page 2 hands back page 1 — what a backend that ignores the cursor does.
      listPages: [
        listResponse(listPayload([ID, ID2], 'synthetic-token')),
        listResponse(listPayload([ID, ID2], 'synthetic-token-2')),
      ],
    });
    await run(store, http, 'acct-repeat', { maxDetails: 0 });
    const report = await run(store, http, 'acct-repeat', { maxDetails: 0 });
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('did not advance');
    // 🔴 Not an ending: "the cursor did not move" says nothing about the account.
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.state.archived).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// 3 · The paged body
// ---------------------------------------------------------------------------
describe('W29-3 · a conversation that arrives in pages', () => {
  /** Three disjoint pages: page 1 and 2 hand a token back, page 3 says there is no more. */
  function threePages(id: string, page: number): string {
    if (page === 0) return detailResponse(detailPayload(turnIds(0, id), 'page-token-1'));
    if (page === 1) return detailResponse(detailPayload(turnIds(1, id), 'page-token-2'));
    if (page === 2) return detailResponse(detailPayload(turnIds(2, id), null));
    throw new Error('the detail was asked for a page it does not have');
  }

  it('every page is fetched, in order, and ONE bundle is delivered — counted once against the cap and the pacer', async () => {
    const store = memoryStore();
    const delivered: CapturedFetch[] = [];
    const { http, calls } = geminiBackend({
      listPages: [listResponse(listPayload([ID], null))],
      detailPages: threePages,
    });
    const report = await run(store, http, 'acct-paged', {
      sink: (captured: CapturedFetch): SinkOutcome => {
        delivered.push(captured);
        return { saved: true, sessionId: captured.sessionId };
      },
    });

    expect(report.halted).toBeNull();
    expect(delivered).toHaveLength(1);
    expect(report.archivedThisRun).toEqual([ID]);
    // 🔴 The identity: the debt key and the file identity are the same value, in
    //    the canonical (c_-prefixed) form.
    expect(delivered[0]!.sessionId).toBe(ID);
    expect(store).toBeDefined();

    const bundle = readDetailBundle(delivered[0]!.text);
    expect(bundle.ok).toBe(true);
    if (!bundle.ok) return;
    expect(bundle.bundle.conversationId).toBe(ID);
    expect(bundle.bundle.pages).toHaveLength(3);
    // In order, verbatim: page 1's text is the first element, byte for byte.
    expect(bundle.bundle.pages[0]).toBe(detailResponse(detailPayload(turnIds(0), 'page-token-1')));
    expect(bundle.bundle.rpcid).toBe(GEMINI_RPC_DETAIL);

    // The whole loop is one body: one detail request in the trace... and four
    // requests in total (one list page + three detail pages).
    expect(calls).toHaveLength(4);
    expect(report.paceTrace.detail).toHaveLength(1);
    expect(report.state.detailToday.count).toBe(1);

    // The page size and the token the leg sent are the declared ones.
    const page2 = batchOf(calls[2]!);
    expect(JSON.parse(page2[0]![0]![1] as string)).toEqual([
      ID,
      GEMINI_DETAIL_PAGE_SIZE,
      'page-token-1',
      1,
      [0],
      [4],
      null,
      1,
    ]);
  });

  it('the wait between two pages of one conversation is the declared jitter band, and nothing outside it', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const { http } = geminiBackend({
      listPages: [listResponse(listPayload([ID], null))],
      detailPages: threePages,
    });
    const report = await run(store, http, 'acct-jitter', { clock, random: () => 0 });
    expect(report.halted).toBeNull();
    // random ⇒ 0 draws the documented minimum, so the band's floor is asserted
    // rather than sampled; the two page gaps are the only intra-body waits.
    const pageGaps = clock.sleeps.filter((ms) => ms === GEMINI_DETAIL_PAGE_DELAY_MS.min);
    expect(pageGaps).toHaveLength(2);
    expect(Math.min(...clock.sleeps)).toBeGreaterThanOrEqual(GEMINI_DETAIL_PAGE_DELAY_MS.min);
  });

  it('a page whose turns are all already seen is the cursor not advancing, not a complete conversation', async () => {
    const store = memoryStore();
    const delivered: CapturedFetch[] = [];
    const { http } = geminiBackend({
      listPages: [listResponse(listPayload([ID], null))],
      detailPages: (_id, page) => (page === 0
        ? detailResponse(detailPayload(turnIds(0), 'page-token-1'))
        // The same five turns again, with a fresh token.
        : detailResponse(detailPayload(turnIds(0), page === 1 ? 'page-token-2' : null))),
    });
    const report = await run(store, http, 'acct-stuck', {
      sink: (captured: CapturedFetch): SinkOutcome => {
        delivered.push(captured);
        return { saved: true, sessionId: captured.sessionId };
      },
    });
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('did not advance');
    expect(delivered).toEqual([]);
    expect(report.state.archived).toEqual([]);
  });

  it('a page that carries a token and no turns is refused by name instead of being followed', () => {
    const step = GEMINI_PLAN.detailPages!.nextPage(detailResponse(detailPayload([], 'page-token-1')), ID);
    expect(step.kind).toBe('unreadable');
    if (step.kind !== 'unreadable') return;
    expect(step.reason).toContain('continuation token and no turns');
  });
});

// ---------------------------------------------------------------------------
// 4 · The page cap is a refusal
// ---------------------------------------------------------------------------
describe('W29-4 · a conversation longer than the leg will fetch', () => {
  it('hitting maxPages records detail-too-long, archives nothing, and settles nothing', async () => {
    const store = memoryStore();
    const capped: BackfillEnumPlan = {
      ...GEMINI_PLAN,
      detailPages: { ...GEMINI_PLAN.detailPages!, maxPages: 2 },
    };
    const delivered: CapturedFetch[] = [];
    const { http, calls } = geminiBackend({
      listPages: [listResponse(listPayload([ID], null))],
      // Every page has a token: this conversation never ends on its own.
      detailPages: (_id, page) => detailResponse(detailPayload(turnIds(page), `page-token-${page + 1}`)),
    });
    const report = await run(store, http, 'acct-cap', {
      plans: (platform) => (platform === 'gemini' ? capped : backfillPlanFor(platform)),
      sink: (captured: CapturedFetch): SinkOutcome => {
        delivered.push(captured);
        return { saved: true, sessionId: captured.sessionId };
      },
    });

    expect(delivered).toEqual([]);
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.archived).toEqual([]);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun).toHaveLength(1);
    expect(report.failedThisRun[0]!.reason).toBe('detail-too-long');
    // The cap is a bound on requests too: two pages, then stop.
    expect(calls).toHaveLength(3);
    expect(report.state.detailToday.count).toBe(1);
    // And it is a sayable fact, not a raw code.
    expect(describeFailureReason('detail-too-long')).not.toContain('detail-too-long');
  });

  it('the shipped cap is the declared one, and it is a bound rather than a target', () => {
    expect(GEMINI_MAX_DETAIL_PAGES).toBe(20);
    expect(GEMINI_PLAN.detailPages!.maxPages).toBe(GEMINI_MAX_DETAIL_PAGES);
  });
});

// ---------------------------------------------------------------------------
// 5 · Auth: the tokens, and what happens without them
// ---------------------------------------------------------------------------
describe('W29-5 · the page’s own bootstrap tokens', () => {
  function tokens(over: Partial<GeminiBootstrapTokens> = {}): GeminiBootstrapTokens {
    return { at: AT_TOKEN, bl: BL_LABEL, fSid: F_SID, ...over };
  }

  const listUrl = GEMINI_PLAN.listUrl(ORIGIN, 0, GEMINI_LIST_PAGE_SIZE);

  it('only the two declared rpcids on the page’s own origin are touched', () => {
    expect(needsGeminiTokens(listUrl, ORIGIN)).toBe(true);
    expect(needsGeminiTokens(GEMINI_PLAN.detailUrl!(ORIGIN, ID), ORIGIN)).toBe(true);
    // A third rpcid on the same path is not this leg's business.
    expect(needsGeminiTokens(
      `${ORIGIN}${GEMINI_BATCHEXECUTE_PATH}?${GEMINI_QUERY_RPCIDS}=OtherRpc&rt=c`,
      ORIGIN,
    )).toBe(false);
    expect(needsGeminiTokens(listUrl, 'https://example.test')).toBe(false);
  });

  it('fills the tokens into the query and the body, and leaves the batch the plan built untouched', async () => {
    const sent: { url: string; init: RequestInit }[] = [];
    const fetchLike = async (url: string, init: RequestInit): Promise<MinimalResponse> => {
      sent.push({ url, init });
      return { status: 200, text: async () => 'synthetic' };
    };
    const authorized = createGeminiAuthorizedFetch(ORIGIN, fetchLike, {
      readTokens: async () => tokens(),
      language: 'en-GB',
    });
    const body = listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body!;
    // The message itself carries no credential: that is what the allowlist checks.
    expect(new URLSearchParams(body).get(GEMINI_FORM_FIELD_AT)).toBe('');
    await authorized(listUrl, { method: 'POST', body });

    const url = new URL(sent[0]!.url);
    expect(url.searchParams.get(GEMINI_QUERY_KEY_BL)).toBe(BL_LABEL);
    expect(url.searchParams.get(GEMINI_QUERY_KEY_F_SID)).toBe(F_SID);
    expect(url.searchParams.get(GEMINI_QUERY_KEY_HL)).toBe('en-GB');
    expect(url.searchParams.get(GEMINI_QUERY_KEY_REQID)).toBe('0');
    const fields = new URLSearchParams(sent[0]!.init.body as string);
    expect(fields.get(GEMINI_FORM_FIELD_AT)).toBe(AT_TOKEN);
    expect(fields.get(GEMINI_FORM_FIELD_BATCH)).toBe(new URLSearchParams(body).get(GEMINI_FORM_FIELD_BATCH));
    // The page's own request counter moves on with each request.
    await authorized(listUrl, { method: 'POST', body });
    expect(new URL(sent[1]!.url).searchParams.get(GEMINI_QUERY_KEY_REQID)).toBe('1');
  });

  it('a request it did not declare is passed through with no token and no rewrite', async () => {
    const sent: { url: string }[] = [];
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (url): Promise<MinimalResponse> => {
        sent.push({ url });
        return { status: 200, text: async () => 'synthetic' };
      },
      { readTokens: async () => tokens(), language: 'en-GB' },
    );
    const other = `${ORIGIN}/_/BardChatUi/data/other?x=1`;
    await authorized(other, { method: 'POST', body: 'a=b' });
    expect(sent[0]!.url).toBe(other);
  });

  it('without a token the request still goes out, and the platform’s own 400 is what comes back — with no retry', async () => {
    const sent: string[] = [];
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (url, init): Promise<MinimalResponse> => {
        sent.push(new URLSearchParams(String(init.body)).get(GEMINI_FORM_FIELD_AT) ?? '');
        return { status: 400, text: async () => 'synthetic refusal' };
      },
      { readTokens: async () => tokens({ at: null }), language: null },
    );
    const res = await authorized(listUrl, { method: 'POST', body: listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body! });
    // 🔴 A refusal, passed through. Never an empty page, and no second request for
    //    a user who is simply logged out.
    expect(res.status).toBe(400);
    expect(sent).toEqual(['']);
  });

  it('a 400 on a request that DID carry a token re-reads once and retries exactly once', async () => {
    const seen: string[] = [];
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (_url, init): Promise<MinimalResponse> => {
        const at = new URLSearchParams(String(init.body)).get(GEMINI_FORM_FIELD_AT) ?? '';
        seen.push(at);
        return { status: seen.length === 1 ? 400 : 200, text: async () => 'synthetic' };
      },
      { readTokens: async () => tokens(), language: null },
    );
    const res = await authorized(listUrl, { method: 'POST', body: listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body! });
    expect(res.status).toBe(200);
    expect(seen).toEqual([AT_TOKEN, AT_TOKEN]);

    // And a second refusal is the platform's answer: two attempts, then stop.
    const twice: string[] = [];
    const refused = createGeminiAuthorizedFetch(
      ORIGIN,
      async (_url, init): Promise<MinimalResponse> => {
        twice.push(new URLSearchParams(String(init.body)).get(GEMINI_FORM_FIELD_AT) ?? '');
        return { status: 400, text: async () => 'synthetic refusal' };
      },
      { readTokens: async () => tokens(), language: null },
    );
    const again = await refused(listUrl, { method: 'POST', body: listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body! });
    expect(again.status).toBe(400);
    expect(twice).toHaveLength(2);
  });

  it('a platform refusal halts the leg and is never written down as “you have no conversations”', async () => {
    const store = memoryStore();
    const http: HttpPort = async () => ({ status: 400, text: 'synthetic refusal' });
    const report = await run(store, http, 'acct-400');
    expect(report.halted).not.toBeNull();
    expect(report.halted!.detail).toContain('HTTP 400');
    expect(report.newDebts).toBe(0);
    // 🔴 The two are different facts and stay different: a halt, not a finished
    //    enumeration of an empty account.
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.progress).not.toContain('0 /');
  });
});

// ---------------------------------------------------------------------------
// 6 · The allowlist
// ---------------------------------------------------------------------------
describe('W29-6 · what the content script will and will not send', () => {
  const listUrl = GEMINI_PLAN.listUrl(ORIGIN, 0, GEMINI_LIST_PAGE_SIZE);
  const detailUrl = GEMINI_PLAN.detailUrl!(ORIGIN, ID);
  const listBody = listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body!;
  const detailInit = detailRequestInit(GEMINI_PLAN, ORIGIN, ID);
  const form = { method: 'POST', contentType: GEMINI_FORM_CONTENT_TYPE };

  it('the plan’s own two requests are accepted, on both pages of a conversation', () => {
    expect(checkBackfillRequest({ url: listUrl, ...form, body: listBody }, ORIGIN).ok).toBe(true);
    expect(checkBackfillRequest({ url: detailUrl, ...form, body: detailInit.body }, ORIGIN).ok).toBe(true);
    // A LATER page of the same conversation — the token inside the batch differs
    // and nothing else does. This is the request the paging loop really sends.
    const later = listTokenPostInit(GEMINI_PLAN, ORIGIN, 'synthetic-cursor', GEMINI_LIST_PAGE_SIZE);
    expect(checkBackfillRequest({ url: listUrl, ...form, body: later.body! }, ORIGIN).ok).toBe(true);
    const nextPageInit = GEMINI_PLAN.detailPages!.nextInit(ORIGIN, ID, 'synthetic-cursor');
    expect(checkBackfillRequest({ url: detailUrl, ...form, body: nextPageInit.body! }, ORIGIN).ok).toBe(true);
    // And the two segments are told apart by the RPC the query names, on one path.
    expect(formSegmentFor(GEMINI_PLAN, new URL(listUrl))).toBe('list');
    expect(formSegmentFor(GEMINI_PLAN, new URL(detailUrl))).toBe('detail');
    expect(expectedMethodFor(GEMINI_PLAN, 'list')).toBe('POST');
    expect(expectedMethodFor(GEMINI_PLAN, 'detail')).toBe('POST');
  });

  it('refuses every request that is not exactly the one the plan builds', () => {
    const refusals: [string, Parameters<typeof checkBackfillRequest>[0]][] = [
      ['another rpcid in the query', { url: listUrl.replace(GEMINI_RPC_LIST, 'SomeOtherRpc'), ...form, body: listBody }],
      ['an extra query key', { url: `${listUrl}&tt=1`, ...form, body: listBody }],
      ['a missing query key', {
        url: `${ORIGIN}${GEMINI_BATCHEXECUTE_PATH}?${GEMINI_QUERY_RPCIDS}=${GEMINI_RPC_LIST}`,
        ...form,
        body: listBody,
      }],
      ['another source path', {
        url: listUrl.replace(encodeURIComponent(GEMINI_SOURCE_PATH_VALUE), encodeURIComponent('/other')),
        ...form,
        body: listBody,
      }],
      ['another rt value', { url: listUrl.replace(GEMINI_RT_VALUE, 'x'), ...form, body: listBody }],
      ['a fragment', { url: `${listUrl}#x`, ...form, body: listBody }],
      ['another content-type', { url: listUrl, method: 'POST', contentType: 'application/json', body: listBody }],
      ['a GET on a POST segment', { url: listUrl, method: 'GET', body: listBody }],
      ['no body at all', { url: listUrl, ...form }],
      ['a batch that is not JSON', { url: listUrl, ...form, body: `${GEMINI_FORM_FIELD_BATCH}=not-json&${GEMINI_FORM_FIELD_AT}=` }],
      ['a batch naming another rpcid', {
        url: listUrl,
        ...form,
        body: buildBatchExecuteBody('SomeOtherRpc', [GEMINI_LIST_PAGE_SIZE, null, [0, null, 1]], ''),
      }],
      ['a batch with an argument tail this plan does not build', {
        url: listUrl,
        ...form,
        body: buildBatchExecuteBody(GEMINI_RPC_LIST, [GEMINI_LIST_PAGE_SIZE, null, [0, null, 7]], ''),
      }],
      ['a batch whose page size is not a number', {
        url: listUrl,
        ...form,
        body: buildBatchExecuteBody(GEMINI_RPC_LIST, ['20', null, [0, null, 1]], ''),
      }],
      ['a credential left in the message', {
        url: listUrl,
        ...form,
        body: `${GEMINI_FORM_FIELD_BATCH}=${encodeURIComponent(JSON.stringify([[[GEMINI_RPC_LIST, '[]', null, 'generic']]]))}&${GEMINI_FORM_FIELD_AT}=synthetic-token`,
      }],
      ['an extra form field', { url: listUrl, ...form, body: `${listBody}&extra=1` }],
      ['a detail body sent to the list URL', { url: listUrl, ...form, body: detailInit.body }],
      ['a list body sent to the detail URL', { url: detailUrl, ...form, body: listBody }],
      ['another origin', { url: listUrl.replace(ORIGIN, 'https://example.test'), ...form, body: listBody }],
      ['another path on the same origin', {
        url: `${ORIGIN}/_/BardChatUi/data/other?${GEMINI_QUERY_RPCIDS}=${GEMINI_RPC_LIST}`,
        ...form,
        body: listBody,
      }],
    ];
    for (const [what, request] of refusals) {
      const verdict = checkBackfillRequest(request, ORIGIN);
      expect(verdict.ok, `expected a refusal: ${what}`).toBe(false);
      if (verdict.ok) continue;
      // The wire value for a URL-dimension refusal is C22's sentence, unchanged.
      expect(typeof verdict.reason).toBe('string');
      expect(verdict.reason.length).toBeGreaterThan(0);
      expect(verdict.reason).not.toContain(AT_TOKEN);
    }
    // And a URL refusal keeps the original wire sentence, character for character.
    const urlRefusal = checkBackfillRequest({ url: `${ORIGIN}/somewhere-else`, ...form, body: listBody }, ORIGIN);
    expect(urlRefusal.ok).toBe(false);
    if (!urlRefusal.ok) expect(urlRefusal.reason).toBe(REFUSED_URL_REASON);
  });

  it('the closed sets and the pinned query are data the plan declares, not logic in the checker', () => {
    expect(GEMINI_PLAN.listTokenForm!.rpcids).toEqual([GEMINI_RPC_LIST]);
    expect(GEMINI_PLAN.detailForm!.rpcids).toEqual([GEMINI_RPC_DETAIL]);
    expect(GEMINI_PLAN.listTokenForm!.query.map((q) => q.key)).toEqual([
      GEMINI_QUERY_RPCIDS,
      GEMINI_QUERY_SOURCE_PATH,
      GEMINI_QUERY_RT,
    ]);
    // 🔴 The four page keys are NOT declared by the plan: the plan's own builders
    //    never emit them, and the wrapper adds them after this check.
    for (const key of GEMINI_PAGE_QUERY_KEYS) {
      expect(GEMINI_PLAN.listTokenForm!.query.some((q) => q.key === key)).toBe(false);
      expect(listUrl).not.toContain(`${key}=`);
    }
  });
});

// ---------------------------------------------------------------------------
// 7 · The live leg
// ---------------------------------------------------------------------------
describe('W29-7 · a page load that carried only the first page', () => {
  function observed(text: string): CapturedFetch {
    return {
      url: GEMINI_PLAN.detailUrl!(ORIGIN, ID),
      method: 'POST',
      status: 200,
      text,
      pageUrl: PAGE_URL,
      capturedAt: 1_700_000_000_000,
    };
  }

  const complete = detailResponse(detailPayload(turnIds(0), null));
  const firstOfThree = detailResponse(detailPayload(turnIds(0), 'page-token-1'));
  /** The observed response, deliberately from a *different* page than the one the leg fetches. */
  const observedLaterPage = detailResponse(detailPayload(turnIds(7), 'page-token-8'));

  it('routes only the detail rpcid through the completing path', () => {
    expect(isGeminiDetailRequest(observed(complete).url)).toBe(true);
    expect(isGeminiDetailRequest(GEMINI_PLAN.listUrl(ORIGIN, 0, GEMINI_LIST_PAGE_SIZE))).toBe(false);
    expect(findPlatformForUrl(observed(complete).url)?.id).toBe('gemini');
    expect(matchesResponseShape(
      PLATFORMS.find((platform) => platform.id === 'gemini')!,
      complete,
    )).toBe(true);
  });

  it('the bundle is anchored at page 1 fetched by this leg, not at whatever page was observed', async () => {
    const fetched: { url: string; token: unknown }[] = [];
    const pages = [complete];
    const result = await completeGeminiLiveCapture(observed(observedLaterPage), {
      pageOrigin: ORIGIN,
      random: () => 0,
      sleep: async () => {},
      fetchPage: async (url, init) => {
        const batch = JSON.parse(new URLSearchParams(init.body!).get(GEMINI_FORM_FIELD_BATCH)!) as unknown[][][];
        fetched.push({ url, token: JSON.parse(batch[0]![0]![1] as string)[2] });
        const page = pages.shift();
        if (page === undefined) throw new Error('more pages were fetched than the fixture has');
        return { ok: true, status: 200, text: page };
      },
    });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.payload.sessionId).toBe(ID);
    // 🔴 One request, to the plan's own URL, with a null page token — the page the
    //    plan's builder asks for. Never the observed page's bytes.
    expect(fetched).toEqual([{ url: GEMINI_PLAN.detailUrl!(ORIGIN, ID), token: null }]);
    const bundle = readDetailBundle(result.payload.text);
    expect(bundle.ok).toBe(true);
    if (!bundle.ok) return;
    expect(bundle.bundle.pages).toEqual([complete]);
    expect(bundle.bundle.pages).not.toContain(observedLaterPage);
  });

  it('a long conversation is followed page by page, in order, with the tokens the server handed back', async () => {
    const fetched: unknown[] = [];
    const sleeps: number[] = [];
    const stream = [
      detailResponse(detailPayload(turnIds(0), 'page-token-1')),
      detailResponse(detailPayload(turnIds(1), 'page-token-2')),
      detailResponse(detailPayload(turnIds(2), null)),
    ];
    const result = await completeGeminiLiveCapture(observed(observedLaterPage), {
      pageOrigin: ORIGIN,
      random: () => 0,
      sleep: async (ms) => { sleeps.push(ms); },
      fetchPage: async (_url, init) => {
        const batch = JSON.parse(new URLSearchParams(init.body!).get(GEMINI_FORM_FIELD_BATCH)!) as unknown[][][];
        fetched.push(JSON.parse(batch[0]![0]![1] as string)[2]);
        const page = stream.shift();
        if (page === undefined) throw new Error('more pages were fetched than the fixture has');
        return { ok: true, status: 200, text: page };
      },
    });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    const bundle = readDetailBundle(result.payload.text);
    expect(bundle.ok).toBe(true);
    if (!bundle.ok) return;
    // Page 1 first, then each page the previous one's token led to — verbatim.
    expect(bundle.bundle.pages).toEqual([
      detailResponse(detailPayload(turnIds(0), 'page-token-1')),
      detailResponse(detailPayload(turnIds(1), 'page-token-2')),
      detailResponse(detailPayload(turnIds(2), null)),
    ]);
    expect(fetched).toEqual([null, 'page-token-1', 'page-token-2']);
    // The wait is *between* pages: two gaps for three pages, never before the first.
    expect(sleeps).toEqual([GEMINI_DETAIL_PAGE_DELAY_MS.min, GEMINI_DETAIL_PAGE_DELAY_MS.min]);
  });

  it('a response that cannot be read is not captured, and says why', async () => {
    const result = await completeGeminiLiveCapture(observed('not a batchexecute body at all'), {
      pageOrigin: ORIGIN,
      random: () => 0,
      sleep: async () => {},
      fetchPage: async () => { throw new Error('nothing may be fetched for an unreadable response'); },
    });
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.reason).toContain('could not be read');
  });

  it('a conversation longer than the cap is not captured either — the pages in hand are not the conversation', async () => {
    let page = 0;
    const result = await completeGeminiLiveCapture(observed(firstOfThree), {
      pageOrigin: ORIGIN,
      random: () => 0,
      sleep: async () => {},
      fetchPage: async () => {
        page += 1;
        return { ok: true, status: 200, text: detailResponse(detailPayload(turnIds(page), `page-token-${page + 1}`)) };
      },
    });
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.reason).toContain('detail-too-long');
    // The cap bounds the requests too, and it counts the first page.
    expect(page).toBe(GEMINI_MAX_DETAIL_PAGES);
  });

  it('a later page that fails is a refusal, not a truncated archive entry', async () => {
    let call = 0;
    const result = await completeGeminiLiveCapture(observed(firstOfThree), {
      pageOrigin: ORIGIN,
      random: () => 0,
      sleep: async () => {},
      fetchPage: async () => {
        call += 1;
        if (call === 1) return { ok: true, status: 200, text: firstOfThree };
        return { ok: false, error: 'refused: url is not a same-origin backfill endpoint' };
      },
    });
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.reason).toContain('a later page could not be fetched');
  });
});

// ---------------------------------------------------------------------------
// 8 · Fixtures this suite must not let drift
// ---------------------------------------------------------------------------
describe('W29-8 · the bundle reader', () => {
  it('refuses a stored bundle whose page still carries a token', () => {
    const read = readDetailBundle(assembleDetailBundle(ID, [detailResponse(detailPayload(turnIds(0), 'page-token-1'))]));
    expect(read.ok).toBe(false);
    if (read.ok) return;
    expect(read.reason).toContain('continuation token');
  });

  it('refuses a stored bundle whose page belongs to another conversation', () => {
    const read = readDetailBundle(assembleDetailBundle(ID, [detailResponse(detailPayload(turnIds(0, ID2), null))]));
    expect(read.ok).toBe(false);
    if (read.ok) return;
    expect(read.reason).toContain('conversation-id-mismatch');
  });

  it('accepts a whole bundle and hands its pages back verbatim', () => {
    const pages = [
      detailResponse(detailPayload(turnIds(0), 'page-token-1')),
      detailResponse(detailPayload(turnIds(1), null)),
    ];
    const read = readDetailBundle(assembleDetailBundle(ID, pages));
    expect(read.ok).toBe(true);
    if (!read.ok) return;
    expect(read.bundle.pages).toEqual(pages);
    expect(parseGeminiDetailPage(assembleDetailBundle(ID, pages)).ok).toBe(true);
  });

  it('the shape gate on the whole bundle passes, because the raw pages are inside it', () => {
    const row = PLATFORMS.find((platform) => platform.id === 'gemini')!;
    const bundle = assembleDetailBundle(ID, [detailResponse(detailPayload(turnIds(0), null))]);
    expect(matchesResponseShape(row, bundle)).toBe(true);
    // The URL a live capture is filed under still resolves to this platform.
    expect(findPlatformForUrl(GEMINI_PLAN.detailUrl!(ORIGIN, ID))?.id).toBe('gemini');
  });
});
