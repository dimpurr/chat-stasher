/**
 * W21 · Grok (grok.com): the platform row, the live-capture path, the opaque-token list cursor,
 * the two-step conversation body, and the allowlist around it.
 *
 * ## What this file exists to stop changing back
 *  1. **The row matches the content response and nothing else.** The route family has three calls
 *     (a list, a skeleton, a content POST); only the third carries conversation data, and a row
 *     that matched all three would fire a shape-mismatch warning on every conversation opening.
 *  2. **The list cursor is opaque.** The sources disagree about whether the parameter is a
 *     `pageToken` or an integer `page`; this code never interprets it, passes back exactly what it
 *     got, and has a repeat-page guard so a backend that ignores the cursor halts with a trace
 *     rather than re-listing one page forever or claiming the account was fully listed.
 *  3. **The body is two requests, and still one body.** Step 2's request may only be built from
 *     step 1's own response through the plan's own builder; the pair counts once against the daily
 *     cap and the pacer; the delivered artefact is step 2's response and only step 2's.
 *  4. **The allowlist did not grow a wildcard.** A third named path, one declared array key, and
 *     refusals for everything else: other paths, other methods, other origins, other body keys.
 *
 * 🔴 All fixtures are **synthetic**, hand-written from the field names the sources carry. No
 *    request goes to grok.com, there is no logged-in state, and no real conversation, account or
 *    id appears anywhere below. The http port is always injected explicitly, and it throws on any
 *    path it was not given — so "no request was sent" is proven rather than described.
 *
 * 🔴 Scope note: this file stops at the sink (the on-disk write path needs a host). What it can
 *    answer purely, it does: the debt key, the file identity and the delivered body are compared
 *    against each other, and the memory store is the only state carrier.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  CONTENT_MATCHES,
  extractSessionId,
  isCapturedFetchShape,
  matchesResponseShape,
  pathSafeSessionId,
  PLATFORMS,
  platformForTraffic,
  type CapturedFetch,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import { runBackfill, type HttpResponse, type SinkOutcome, DETAIL_EMPTY_HALT_STREAK } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  GROK_DETAIL2_PATH,
  GROK_DETAIL_PATH,
  GROK_DETAIL_STEP_DELAY_MS,
  GROK_LIST_PATH,
  GROK_PLAN,
  GROK_STEP2_BODY_KEY,
  backfillPlanFor,
  canBackfillDetail,
  grokResponseIds,
  parseGrokDetailPage,
  parseGrokListPage,
} from '../lib/backfill/enumerate';
import { checkBackfillRequest, REFUSED_URL_REASON } from '../lib/backfill/tab-port';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://grok.com';
/** Synthetic ids in the shape the sources show (a long opaque token, here written as a UUID-like literal). */
const ID = '00000000-0000-4000-8000-0000000000aa';
const ID2 = '00000000-0000-4000-8000-0000000000bb';
const ID3 = '00000000-0000-4000-8000-0000000000cc';
const R1 = '11111111-0000-4000-8000-000000000001';
const R2 = '11111111-0000-4000-8000-000000000002';
const PAGE_URL = `${ORIGIN}/c/${ID}`;
const LOAD_URL = `${ORIGIN}${GROK_DETAIL2_PATH.replace('{id}', ID)}`;
const NODE_URL = `${ORIGIN}${GROK_DETAIL_PATH.replace('{id}', ID)}`;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

/** A fake clock that both advances on sleep and keeps every sleep, so "the two steps are 2–5 s apart" is assertable. */
function fakeClock(): Clock & { sleeps: number[] } {
  const sleeps: number[] = [];
  let t = Date.parse('2026-09-14T00:00:00.000Z');
  return {
    sleeps,
    now: () => t,
    async sleep(ms: number) {
      sleeps.push(ms);
      t += ms;
    },
  };
}

// ---------------------------------------------------------------------------
// Synthetic fixtures (field names taken from the sources; values invented here)
// ---------------------------------------------------------------------------

/**
 * A list page. `token` omitted ⇒ the field is absent entirely (the sources say
 * absent/empty means the last page); `token: null` writes an explicit empty string.
 */
function listPage(ids: string[], token?: string | null): string {
  const conversations = ids.map((conversationId, i) => ({
    conversationId,
    title: `synthetic-${i}`,
    starred: false,
    createTime: '2026-07-01T10:00:00.000000Z',
    modifyTime: '2026-07-01T10:01:10.000Z',
  }));
  const body: Record<string, unknown> = { conversations, textSearchMatches: [] };
  if (token !== undefined) body.nextPageToken = token === null ? '' : token;
  return JSON.stringify(body);
}

/** The skeleton: ids, sender and parent links, no content. */
function responseNodeBody(ids: string[]): string {
  return JSON.stringify({
    responseNodes: ids.map((responseId, i) => (
      i === 0 ? { responseId, sender: 'human' } : { responseId, sender: 'ASSISTANT', parentResponseId: ids[i - 1] }
    )),
    inflightResponses: [],
  });
}

/**
 * The content response. The array is deliberately written out of request order
 * (assistant first), because the sources state that the order is not guaranteed —
 * this leg archives the raw body, so the order is preserved rather than fixed.
 */
function loadResponsesBody(items: { responseId: string; message: string }[]): string {
  return JSON.stringify({
    responses: items.map(({ responseId, message }) => ({
      responseId,
      message,
      sender: responseId === R1 ? 'human' : 'ASSISTANT',
      createTime: '2026-07-01T10:00:05.000Z',
      model: 'synthetic-model',
      webSearchResults: [],
      citedWebSearchResults: [],
      fileAttachmentAssetMetadata: [],
    })),
  });
}

const GOOD_CONTENT = loadResponsesBody([
  { responseId: R2, message: 'synthetic-answer' },
  { responseId: R1, message: 'synthetic-question' },
]);

interface Recorder {
  /** Every request the leg made, in order, with the clock reading at the moment it was made. */
  calls: { url: string; init: unknown; at: number }[];
}

/**
 * A synthetic backend. Any path it was not given **throws**, so "this leg sent no
 * further request" is proven by the run rather than asserted about it.
 */
function backend(
  clock: Clock,
  routes: Partial<Record<string, string | ((url: URL) => string)>>,
): Recorder & { http: (url: string, init?: unknown) => Promise<HttpResponse> } {
  const calls: { url: string; init: unknown; at: number }[] = [];
  const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
    calls.push({ url, init, at: clock.now() });
    const u = new URL(url);
    const route = routes[u.pathname];
    if (route === undefined) throw new Error(`unexpected path ${u.pathname}`);
    return { status: 200, text: typeof route === 'string' ? route : route(u) };
  };
  return { calls, http };
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: (url: string, init?: unknown) => Promise<HttpResponse>,
  scope: string,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'grok',
    origin: ORIGIN,
    scope,
    store,
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink: (captured: CapturedFetch): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
    ...extra,
  });
}

// ---------------------------------------------------------------------------
// 1 · The platform row
// ---------------------------------------------------------------------------
describe('W21-1 · the grok row in the platform table', () => {
  const row = PLATFORMS.find((platform) => platform.id === 'grok')!;

  it('one origin, the content route only, and the method that carries content', () => {
    expect(row).toBeDefined();
    expect(row.origins).toEqual(['https://grok.com']);
    // 🔴 The row names ONE path. The route family also has a list GET and a skeleton GET; neither
    //    carries conversation content, and matching them would turn the shape-mismatch warning into
    //    noise on every conversation the user opens (the same call the claude row makes).
    expect(row.pathHints).toEqual(['/load-responses']);
    expect(row.methods).toEqual(['POST']);
    expect(row.credibility).toBe('from-source');
    expect(row.webSocketCapture).toBe(false);

    expect(platformForTraffic(LOAD_URL, 'POST')?.id).toBe('grok');
    // The skeleton and the list are outside the row, on purpose.
    expect(platformForTraffic(NODE_URL, 'GET')).toBeNull();
    expect(platformForTraffic(`${ORIGIN}${GROK_LIST_PATH}?pageSize=30`, 'GET')).toBeNull();
  });

  it('the shape gate takes the content envelope and refuses the two routes that are not it', () => {
    expect(matchesResponseShape(row, GOOD_CONTENT)).toBe(true);
    // An EMPTY array passes: `[]` is a measurement, not a missing field.
    expect(matchesResponseShape(row, loadResponsesBody([]))).toBe(true);
    // The skeleton and the list are plainly NOT the content envelope.
    expect(matchesResponseShape(row, responseNodeBody([R1]))).toBe(false);
    expect(matchesResponseShape(row, listPage([ID]))).toBe(false);
    // Drift: same route, same 200, the content moved under a different key.
    expect(matchesResponseShape(row, JSON.stringify({ data: { responses: [] } }))).toBe(false);
    expect(matchesResponseShape(row, 'not json')).toBe(false);
  });

  it('the origin set is still closed, and grok added exactly one entry to it', () => {
    expect(CONTENT_MATCHES).toContain('https://grok.com/*');
    expect(CONTENT_MATCHES).not.toContain('<all_urls>');
    expect(CONTENT_MATCHES.some((match) => match.startsWith('*'))).toBe(false);
    for (const hostile of [
      // Look-alike host, a subdomain, plain http, and a path-only lookalike on another host.
      'https://grok.com.attacker.example/rest/app-chat/conversations/x/load-responses',
      'https://api.grok.com/rest/app-chat/conversations/x/load-responses',
      'http://grok.com/rest/app-chat/conversations/x/load-responses',
    ]) {
      expect(platformForTraffic(hostile, 'POST')).toBeNull();
      expect(isCapturedFetchShape({
        url: hostile,
        method: 'POST',
        status: 200,
        text: GOOD_CONTENT,
        pageUrl: PAGE_URL,
        capturedAt: Date.now(),
      })).toBe(false);
    }
  });
});

// ---------------------------------------------------------------------------
// 2 · Session id: the API URL, the page URL, and the file-name fragment
// ---------------------------------------------------------------------------
describe('W21-2 · which conversation this capture is', () => {
  it('the id comes out of the API path, and out of the page URL when the API URL is not enough', () => {
    expect(extractSessionId(LOAD_URL, GOOD_CONTENT)).toBe(ID);
    expect(extractSessionId(LOAD_URL, GOOD_CONTENT, PAGE_URL)).toBe(ID);
    // Page-URL-only: the same id the address bar carries.
    expect(extractSessionId(`${ORIGIN}/rest/other`, GOOD_CONTENT, PAGE_URL)).toBe(ID);
    // A body with no id anywhere yields nothing — never a guessed one.
    expect(extractSessionId(`${ORIGIN}/rest/other`, GOOD_CONTENT)).toBeNull();
  });

  it('the debt key and the file-name fragment are the same value, so they cannot disagree', () => {
    const sessionId = extractSessionId(LOAD_URL, GOOD_CONTENT);
    expect(sessionId).toBe(ID);
    // The identity map (contract.ts pathSafeSessionId): this id needs no escaping, so nothing is
    // renamed on the way to disk and two debt keys cannot collapse onto one name.
    expect(pathSafeSessionId(sessionId!)).toBe(ID);
  });

  it('a page payload cannot name the file it is written to', () => {
    // 🔴 The authoritative-identity channel is extension-internal; a page filling it is a page
    //    choosing which conversation's file gets overwritten.
    expect(isCapturedFetchShape({
      url: LOAD_URL,
      method: 'POST',
      status: 200,
      text: GOOD_CONTENT,
      pageUrl: PAGE_URL,
      capturedAt: Date.now(),
      sessionId: ID,
    })).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 3 · Live capture
// ---------------------------------------------------------------------------
describe('W21-3 · the passive hook', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  function makeFakeWindow(responseBody: string) {
    const posted: unknown[] = [];
    const fakeWindow: any = {
      location: { origin: ORIGIN, href: PAGE_URL },
      async fetch() { return new Response(responseBody, { status: 200 }); },
      addEventListener() { /* the probe listener is irrelevant to these assertions */ },
      postMessage(message: unknown) { posted.push(message); },
    };
    return { fakeWindow, posted };
  }

  it('a load-responses body on the /c/<id> page becomes one bundle with that session id', async () => {
    const { fakeWindow, posted } = makeFakeWindow(GOOD_CONTENT);
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(LOAD_URL, { method: 'POST' });
    await new Promise((resolve) => setTimeout(resolve, 0));

    const captures = posted.filter((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage) as any[];
    expect(captures).toHaveLength(1);
    const payload = captures[0].payload;
    expect(payload.url).toBe(LOAD_URL);
    expect(payload.method).toBe('POST');
    expect(payload.status).toBe(200);
    expect(payload.pageUrl).toBe(PAGE_URL);
    // The payload is the raw body, untouched.
    expect(payload.text).toBe(GOOD_CONTENT);
    // And it passes the bridge's own gate, which is what makes it deliverable at all.
    expect(isCapturedFetchShape(payload)).toBe(true);
    // The bundle's identity: the conversation id, from the API path, with the page URL as fallback.
    expect(extractSessionId(payload.url, payload.text, payload.pageUrl)).toBe(ID);
  });

  it('the skeleton GET passes by without a warning and without a capture', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(responseNodeBody([R1, R2]));
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(NODE_URL, { method: 'GET' });
    await new Promise((resolve) => setTimeout(resolve, 0));

    // 🔴 This is why pathHints is the content path and not the route family: a skeleton response is
    //    not conversation data, so it is skipped silently rather than warned about.
    expect(warn).not.toHaveBeenCalled();
    expect(posted.some((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
  });

  it('a load-responses response whose shape drifted is refused and said out loud', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(JSON.stringify({ data: { responses: [] } }));
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(LOAD_URL, { method: 'POST' });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
    // Metadata-only: no URL, body or id in the warning.
    expect(warn.mock.calls[0]).toHaveLength(1);
    expect(posted.some((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 4 · The list parser
// ---------------------------------------------------------------------------
describe('W21-4 · parsing one page of the conversation list', () => {
  it('reads the ids, reports no total, and only carries a non-empty token forward', () => {
    const withToken = parseGrokListPage(listPage([ID, ID2], 'token-1'));
    expect(withToken).toEqual({
      ok: true,
      page: { ids: [ID, ID2], total: null, nextToken: 'token-1' },
    });
    // Absent ⇒ the API's own "there is no next page"; an explicit empty string means the same thing.
    expect(parseGrokListPage(listPage([ID]))).toEqual({
      ok: true,
      page: { ids: [ID], total: null, nextToken: null },
    });
    expect(parseGrokListPage(listPage([ID], null))).toEqual({
      ok: true,
      page: { ids: [ID], total: null, nextToken: null },
    });
    // An empty page is a readable page with zero rows — not a shape problem.
    expect(parseGrokListPage(listPage([], 'token-1'))).toEqual({
      ok: true,
      page: { ids: [], total: null, nextToken: 'token-1' },
    });
  });

  it('never turns an unrecognised or changed shape into an empty list', () => {
    // No `conversations` array ⇒ shape-changed, NOT "this account has no conversations".
    expect(parseGrokListPage(JSON.stringify({ items: [] })).ok).toBe(false);
    expect(parseGrokListPage(JSON.stringify([{ conversationId: ID }])).ok).toBe(false);
    expect(parseGrokListPage('not json').ok).toBe(false);
    // An item without the id field.
    expect(parseGrokListPage(JSON.stringify({ conversations: [{ title: 'x' }] })).ok).toBe(false);
    // 🔴 The token's *type* changing is drift, and must not be rounded into "we reached the end".
    const drifted = parseGrokListPage(JSON.stringify({ conversations: [], nextPageToken: 7 }));
    expect(drifted.ok).toBe(false);
    expect(drifted.ok === false && drifted.detail).toContain('nextPageToken');
  });
});

// ---------------------------------------------------------------------------
// 5 · The list segment in the engine: end of list, and the repeat-page guard
// ---------------------------------------------------------------------------
describe('W21-5 · enumerating with an opaque cursor', () => {
  it('ends on an absent token, in one page, and says so as a confirmed ending', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([ID]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    const report = await run(memoryStore(), be.http, 'w21-end-token', { clock });

    expect(report.enumeratedPages).toBe(1);
    expect(report.state.enumCursor.complete).toBe(true);
    // 🔴 A confirmed ending, not a client inference: no truncation is recorded.
    expect(report.enumTruncated).toBeNull();
    expect(report.state.enumCursor.token ?? null).toBeNull();
  });

  it('ends on an empty page, and the token it sends back is the one it was given, byte for byte', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      // The first page carries the cursor; the page asked for with that cursor is empty.
      // 🔴 This is the whole point of the round trip: the server's answer depends on the exact
      //    token, so a leg that mangled it would ask for the wrong page here.
      [GROK_LIST_PATH]: (u) => (u.searchParams.get('pageToken') === 'tok/A+B=='
        ? listPage([])
        : listPage([ID], 'tok/A+B==')),  // deliberately awkward characters
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    const store = memoryStore();
    const first = await run(store, be.http, 'w21-end-page', { clock });
    expect(first.state.enumCursor.complete).toBe(false);
    expect(first.state.enumCursor.token).toBe('tok/A+B==');

    // The next tick asks for the next page.
    be.calls.length = 0;
    const second = await run(store, be.http, 'w21-end-page', { clock });
    const listCalls = be.calls.filter((c) => new URL(c.url).pathname === GROK_LIST_PATH);
    expect(listCalls).toHaveLength(1);
    // 🔴 Opaque: the value that went out is the value that came in, not a re-encoded or parsed form.
    expect(new URL(listCalls[0]!.url).searchParams.get('pageToken')).toBe('tok/A+B==');
    expect(new URL(listCalls[0]!.url).searchParams.get('pageSize')).toBe('100');

    // Page two is empty ⇒ the list is over, and it is recorded as complete rather than truncated.
    expect(second.state.enumCursor.complete).toBe(true);
    expect(second.enumTruncated).toBeNull();
  });

  it('a page that repeats what this enumeration already listed halts as shape-changed, not as an ending', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      // Both pages answer with the same ids whatever cursor they are given — the symptom of a
      // backend that ignores the cursor parameter (the open question the sources disagree about).
      [GROK_LIST_PATH]: listPage([ID, ID2], 'stuck-token'),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL_PATH.replace('{id}', ID2)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
      [GROK_DETAIL2_PATH.replace('{id}', ID2)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    const store = memoryStore();
    const first = await run(store, be.http, 'w21-repeat', { clock });
    expect(first.state.enumCursor.complete).toBe(false);
    expect(first.state.archived).toEqual([ID, ID2]);

    be.calls.length = 0;
    const second = await run(store, be.http, 'w21-repeat', { clock });

    expect(second.stopped).toBe('halted');
    expect(second.halted?.reason).toBe('shape-changed');
    expect(second.halted?.detail).toContain('did not advance');
    // 🔴 NOT an ending: the cursor is left exactly where it was and complete stays false.
    expect(second.state.enumCursor.complete).toBe(false);
    expect(second.enumTruncated).toBeNull();
    // 🔴 Nothing was settled and nothing was lost: the second page's ids were already known.
    expect(second.state.archived).toEqual([ID, ID2]);
    expect(second.state.pending).toEqual([]);
    expect(second.newDebts).toBe(0);
    // A permanent stop: the next tick does not send a request at all.
    be.calls.length = 0;
    const third = await run(store, be.http, 'w21-repeat', { clock });
    expect(third.stopped).toBe('halted');
    expect(be.calls).toHaveLength(0);
  });

  it('a first page whose rows are all already archived is NOT a repeat page', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([ID], 'only-token'),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    const store = memoryStore();
    // Pre-seed the ledger as if this conversation had already been archived live.
    const seeded = await run(store, be.http, 'w21-first-page', { clock });
    expect(seeded.state.archived).toEqual([ID]);

    // Fresh scope, same store: a first page (no token yet) naming an already-archived id is a
    // legitimate "nothing new here", not a stuck cursor — the guard only fires on non-first pages.
    const fresh = await run(memoryStore(), be.http, 'w21-first-page-second', { clock });
    expect(fresh.stopped).not.toBe('halted');
    expect(fresh.skippedAlreadyArchived + fresh.newDebts).toBe(1);
  });

  it('a list page whose shape drifted halts without settling and without touching a body', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: JSON.stringify({ items: [ID] }),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    const report = await run(memoryStore(), be.http, 'w21-list-drift', { clock });
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.state.pending).toEqual([]);
    expect(report.state.archived).toEqual([]);
    // Only the list request went out (the backend throws on anything it was not given).
    expect(be.calls).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// 6 · The two-step body
// ---------------------------------------------------------------------------
describe('W21-6 · one conversation, two requests, one body', () => {
  it('sends the skeleton first and builds the content request only from its ids', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([ID]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1, R2]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: GOOD_CONTENT,
    });
    const captured: CapturedFetch[] = [];
    const report = await run(memoryStore(), be.http, 'w21-round', {
      clock,
      sink: (c: CapturedFetch): SinkOutcome => { captured.push(c); return { saved: true, sessionId: c.sessionId }; },
    });

    // Exactly three requests: the list, the skeleton, the content.
    expect(be.calls.map((c) => `${new URL(c.url).pathname}`)).toEqual([
      GROK_LIST_PATH,
      GROK_DETAIL_PATH.replace('{id}', ID),
      GROK_DETAIL2_PATH.replace('{id}', ID),
    ]);
    // 🔴 A GET segment is sent as `http(url)` with no second argument at all — the C22 call shape,
    //    byte for byte, even on a plan that declares a second step.
    expect(be.calls[1]!.init).toBeUndefined();
    // 🔴 The body is the plan's own, built from step 1's ids — the engine never assembles it.
    const init = be.calls[2]!.init as { method: string; body: string; contentType: string };
    expect(init.method).toBe('POST');
    expect(init.contentType).toBe('application/json');
    expect(JSON.parse(init.body)).toEqual({ [GROK_STEP2_BODY_KEY]: [R1, R2] });

    // The delivered artefact is step 2's response, and it is recorded as such.
    expect(captured).toHaveLength(1);
    expect(captured[0]!.url).toBe(LOAD_URL);
    expect(captured[0]!.method).toBe('POST');
    expect(captured[0]!.text).toBe(GOOD_CONTENT);
    expect(captured[0]!.sessionId).toBe(ID);
    expect(captured[0]!.pageUrl).toBe(PAGE_URL);

    expect(report.state.archived).toEqual([ID]);
    expect(report.state.pending).toEqual([]);
    expect(report.stopped).toBe('queue-empty');
  });

  it('waits between the two steps, and the wait is drawn inside the documented band', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([ID]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    await run(memoryStore(), be.http, 'w21-gap', { clock });
    expect(clock.sleeps).toHaveLength(1);
    expect(clock.sleeps[0]).toBeGreaterThanOrEqual(GROK_DETAIL_STEP_DELAY_MS.min);
    expect(clock.sleeps[0]).toBeLessThanOrEqual(GROK_DETAIL_STEP_DELAY_MS.max);
    // …and it really sits between the two calls, not after both.
    const nodeAt = be.calls[1]!.at;
    const loadAt = be.calls[2]!.at;
    expect(loadAt - nodeAt).toBeGreaterThanOrEqual(GROK_DETAIL_STEP_DELAY_MS.min);

    // The two documented boundaries are asserted, not sampled: random()=0 ⇒ the floor, =1 ⇒ the ceiling.
    const floorClock = fakeClock();
    const floor = backend(floorClock, {
      [GROK_LIST_PATH]: listPage([ID]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    await run(memoryStore(), floor.http, 'w21-gap-floor', { clock: floorClock, random: () => 0 });
    expect(floorClock.sleeps).toEqual([GROK_DETAIL_STEP_DELAY_MS.min]);

    const ceilClock = fakeClock();
    const ceil = backend(ceilClock, {
      [GROK_LIST_PATH]: listPage([ID]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    await run(memoryStore(), ceil.http, 'w21-gap-ceil', { clock: ceilClock, random: () => 1 });
    expect(ceilClock.sleeps).toEqual([GROK_DETAIL_STEP_DELAY_MS.max]);
  });

  it('counts the pair as ONE body against the daily cap and the pacer', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([ID, ID2]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL_PATH.replace('{id}', ID2)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
      [GROK_DETAIL2_PATH.replace('{id}', ID2)]: loadResponsesBody([{ responseId: R1, message: 'synthetic' }]),
    });
    const report = await run(memoryStore(), be.http, 'w21-cap', {
      clock,
      pace: { enumerate: { minIntervalMs: 0, maxPerDay: null }, detail: { minIntervalMs: 0, maxPerDay: 1 } },
      random: () => 0,
    });

    // One pair was fetched, and the cap (1) stopped the leg — so the pair counted once, not twice.
    expect(report.stopped).toBe('daily-cap');
    expect(report.state.detailToday.count).toBe(1);
    expect(report.state.archived).toHaveLength(1);
    expect(be.calls.filter((c) => new URL(c.url).pathname.includes('load-responses'))).toHaveLength(1);
    // The pacer gated once for the whole pair (one entry in the detail trace for two requests).
    expect(report.paceTrace.detail).toHaveLength(1);
  });

  it('a skeleton naming nothing sends no second request and settles nothing', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([ID]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([]),
      // No load-responses route at all: if the leg sent one, the backend would throw.
    });
    const report = await run(memoryStore(), be.http, 'w21-empty-skeleton', { clock });

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    // 🔴 The invariant this protects: an empty skeleton is not an empty conversation. The debt is
    //    left pending, not settled, and not written off.
    expect(report.state.pending).toEqual([ID]);
    expect(report.state.archived).toEqual([]);
    expect(be.calls).toHaveLength(2);
  });

  it('one empty content answer is a per-conversation failure, not an empty conversation and not a leg halt', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([ID]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: loadResponsesBody([]),
    });
    const captured: CapturedFetch[] = [];
    const report = await run(memoryStore(), be.http, 'w21-empty-content', {
      clock,
      sink: (c: CapturedFetch): SinkOutcome => { captured.push(c); return { saved: true, sessionId: c.sessionId }; },
    });

    /**
     * 🔴 W92b · **This assertion set changed, and the change is C28's, not a
     *    weakening of it.** One empty body used to halt the leg; that left the debt
     *    at the head of pending (FIFO), so every later run halted on the same
     *    conversation and the platform archived nothing. Now the single empty body
     *    is the per-conversation outcome `detail-empty`: the debt leaves pending
     *    with a receipt, nothing is archived, and the leg carries on. C28's concern
     *    (a whole endpoint answering empty) is pinned separately, by the
     *    K-consecutive test just below.
     */
    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    // Nothing reached the sink: an empty body may not be handed on as a conversation.
    expect(captured).toHaveLength(0);
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.archived).toEqual([]);
    // The debt left pending with a named receipt — neither archived nor still owed.
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['detail-empty']);
    // The receipt is the durable half: `complete:false` is the difference between
    // "we saw nothing" and "there was nothing".
    expect(report.detailOutcomes).toEqual([
      expect.objectContaining({ sessionId: ID, outcome: 'detail-empty-unverified', complete: false }),
    ]);
  });

  it(`🔴 ${DETAIL_EMPTY_HALT_STREAK} empty content answers in a row still halt the leg`, async () => {
    const clock = fakeClock();
    const ids = [ID, ID2, ID3];
    const routes: Record<string, string> = { [GROK_LIST_PATH]: listPage(ids) };
    for (const id of ids) {
      routes[GROK_DETAIL_PATH.replace('{id}', id)] = responseNodeBody([R1]);
      routes[GROK_DETAIL2_PATH.replace('{id}', id)] = loadResponsesBody([]);
    }
    const be = backend(clock, routes);
    const report = await run(memoryStore(), be.http, 'w21-empty-streak', { clock });

    // A whole endpoint answering empty is the contract change C28 exists for.
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('detail-empty-unverified');
    expect(report.archivedThisRun).toEqual([]);
    // The first K-1 empties left pending with receipts; the Kth is the halt, so it
    // stays owed rather than being written off.
    expect(report.failedThisRun.map((f) => f.reason))
      .toEqual(Array(DETAIL_EMPTY_HALT_STREAK - 1).fill('detail-empty'));
    expect(report.state.pending).toEqual([ID3]);
    expect(report.detailOutcomes.map((d) => d.sessionId)).toEqual(ids);
  });

  it('a content body whose shape drifted halts instead of being stored', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([ID]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody([R1]),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: JSON.stringify({ data: { responses: [] } }),
    });
    const captured: CapturedFetch[] = [];
    const report = await run(memoryStore(), be.http, 'w21-content-drift', {
      clock,
      sink: (c: CapturedFetch): SinkOutcome => { captured.push(c); return { saved: true, sessionId: c.sessionId }; },
    });
    expect(report.halted?.reason).toBe('shape-changed');
    expect(captured).toHaveLength(0);
    expect(report.state.archived).toEqual([]);
    expect(report.state.pending).toEqual([ID]);
  });
});

// ---------------------------------------------------------------------------
// 7 · The allowlist
// ---------------------------------------------------------------------------
describe('W21-7 · what the content script lets through for grok, and what it refuses', () => {
  const plan = backfillPlanFor('grok')!;
  const step2Body = JSON.stringify({ [GROK_STEP2_BODY_KEY]: [R1, R2] });

  it('the plan declares both segments, with the second one named rather than wildcarded', () => {
    expect(plan).not.toBeNull();
    expect(plan.platform).toBe('grok');
    expect(canBackfillDetail(plan)).toBe(true);
    expect(plan.partial).toBeUndefined();
    expect(GROK_PLAN.detailPath).toBe(GROK_DETAIL_PATH);
    expect(GROK_PLAN.detailStep2?.path).toBe(GROK_DETAIL2_PATH);
    // 🔴 The id sits in the MIDDLE of both routes, which is why these are templates and not prefixes.
    expect(GROK_DETAIL_PATH).toContain('{id}');
    expect(GROK_DETAIL2_PATH).toContain('{id}');
    // The cursor is opaque: declared as a token builder, and NOT as the numeric one.
    expect(plan.listTokenUrl).toBeTypeOf('function');
    expect(plan.listCursorUrl).toBeUndefined();
  });

  it('accepts exactly the three requests the plan itself builds', () => {
    const list = new URL(plan.listTokenUrl!(ORIGIN, null, 100));
    expect(list.pathname).toBe(GROK_LIST_PATH);
    expect(checkBackfillRequest({ url: list.toString() }, ORIGIN).ok).toBe(true);

    expect(checkBackfillRequest({ url: NODE_URL }, ORIGIN)).toEqual(
      { ok: true, url: NODE_URL, method: 'GET' },
    );
    expect(checkBackfillRequest({
      url: LOAD_URL,
      method: 'POST',
      body: step2Body,
      contentType: 'application/json',
    }, ORIGIN)).toEqual({
      ok: true, url: LOAD_URL, method: 'POST', body: step2Body, contentType: 'application/json',
    });
  });

  it('refuses every other path on the same origin — including the routes next door', () => {
    for (const pathname of [
      // The conversation route with no suffix at all.
      `${GROK_LIST_PATH}/${ID}`,
      // The skeleton route with something appended: the template's suffix is exact.
      `${GROK_DETAIL_PATH.replace('{id}', ID)}/extra`,
      // 🔴 A real sibling endpoint (a `responses` route the closed-source build shows). It is NOT
      //    the one this plan declared, so it is not permitted — this is the assertion that would
      //    fail if the allowlist ever grew a prefix or a wildcard.
      `${GROK_LIST_PATH}/${ID}/responses`,
      // The template's middle segment may not contain a '/'.
      `${ORIGIN}/rest/app-chat/conversations/${ID}/x/response-node`,
      // A completely different route family on the same origin.
      `${ORIGIN}/rest/app-chat/conversations/soft/${ID}`,
      `${ORIGIN}/rest/user-settings`,
    ]) {
      const verdict = checkBackfillRequest({ url: pathname }, ORIGIN);
      expect(verdict.ok, `${pathname} must be refused`).toBe(false);
      expect(verdict.ok === false && verdict.reason).toBe(REFUSED_URL_REASON);
    }
  });

  it('refuses the right path with the wrong method, in both directions', () => {
    // The skeleton is a GET; a POST to it is refused.
    expect(checkBackfillRequest({ url: NODE_URL, method: 'POST', body: '{}', contentType: 'application/json' }, ORIGIN).ok).toBe(false);
    // The content route is a POST; a GET to it is refused.
    expect(checkBackfillRequest({ url: LOAD_URL, method: 'GET' }, ORIGIN).ok).toBe(false);
    // The list is a GET; a POST is refused.
    expect(checkBackfillRequest({ url: `${ORIGIN}${GROK_LIST_PATH}`, method: 'POST', body: '{}', contentType: 'application/json' }, ORIGIN).ok).toBe(false);
    // A method outside the closed set never gets as far as the plan.
    expect(checkBackfillRequest({ url: NODE_URL, method: 'DELETE' }, ORIGIN).ok).toBe(false);
  });

  it('refuses a cross-origin or look-alike URL even when the path is right', () => {
    for (const url of [
      `https://evil.example${GROK_LIST_PATH}`,
      `https://grok.com.attacker.example${GROK_DETAIL_PATH.replace('{id}', ID)}`,
      `http://grok.com${GROK_DETAIL_PATH.replace('{id}', ID)}`,
    ]) {
      expect(checkBackfillRequest({ url }, ORIGIN).ok).toBe(false);
    }
  });

  it('refuses a query nobody declared — including the one a source shows on the skeleton', () => {
    // 🔴 One source appends ?includeThreads=true and the other two do not. It is recorded, not
    //    adopted: no query was declared for either detail path, so none is permitted.
    expect(checkBackfillRequest({ url: `${NODE_URL}?includeThreads=true` }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({ url: `${LOAD_URL}?x=1`, method: 'POST', body: step2Body, contentType: 'application/json' }, ORIGIN).ok).toBe(false);
    // A fragment is not permitted either.
    expect(checkBackfillRequest({ url: `${NODE_URL}#frag` }, ORIGIN).ok).toBe(false);
  });

  it('refuses a body with a foreign key, a wrong value shape, or a wrong Content-Type', () => {
    const base = { url: LOAD_URL, method: 'POST', contentType: 'application/json' };
    const refused = (body: unknown, over: Record<string, unknown> = {}) =>
      checkBackfillRequest({ ...base, body: typeof body === 'string' ? body : JSON.stringify(body), ...over }, ORIGIN).ok;

    expect(refused({ [GROK_STEP2_BODY_KEY]: [R1], extra: 1 })).toBe(false);
    expect(refused({ [GROK_STEP2_BODY_KEY]: [R1], responseIds2: [] })).toBe(false);
    // 🔴 The rule is "top-level keys are a SUBSET of the declared set", unchanged by W21 — so an
    //    empty object is accepted (it carries nothing at all), and there is no assertion here
    //    pretending otherwise. What the array rule adds is the *value* shape, tested next.
    expect(refused({ [GROK_STEP2_BODY_KEY]: R1 })).toBe(false);          // not an array
    expect(refused({ [GROK_STEP2_BODY_KEY]: [R1, 2] })).toBe(false);      // not strings
    expect(refused({ [GROK_STEP2_BODY_KEY]: [''] })).toBe(false);         // empty string
    expect(refused({ [GROK_STEP2_BODY_KEY]: [[R1]] })).toBe(false);       // nested
    expect(refused({ [GROK_STEP2_BODY_KEY]: [{ id: R1 }] })).toBe(false); // nested
    expect(refused('not json')).toBe(false);
    expect(refused(`{"${GROK_STEP2_BODY_KEY}":[]}`, { contentType: 'text/plain' })).toBe(false);
    expect(refused(`{"${GROK_STEP2_BODY_KEY}":[]}`, { contentType: undefined })).toBe(false);
    // A body on a GET segment is never allowed.
    expect(checkBackfillRequest({ url: NODE_URL, body: '{}' }, ORIGIN).ok).toBe(false);
    // An over-long array is refused by count as well as by bytes.
    expect(refused({ [GROK_STEP2_BODY_KEY]: Array.from({ length: 5001 }, (_, i) => `r${i}`) })).toBe(false);
    // …and the shape that the plan itself produces is of course allowed.
    expect(checkBackfillRequest({ ...base, body: step2Body }, ORIGIN).ok).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 8 · The two plan-level helpers, on their own
// ---------------------------------------------------------------------------
describe('W21-8 · the step-1 reader and the empty-content parser', () => {
  it('reads the skeleton ids in order, and refuses anything that is not a skeleton', () => {
    expect(grokResponseIds(responseNodeBody([R1, R2]))).toEqual([R1, R2]);
    // 🔴 An empty skeleton is null ("cannot build a request"), never an empty request.
    expect(grokResponseIds(responseNodeBody([]))).toBeNull();
    expect(grokResponseIds(JSON.stringify({ inflightResponses: [] }))).toBeNull();
    expect(grokResponseIds(JSON.stringify({ responseNodes: [{ sender: 'human' }] }))).toBeNull();
    expect(grokResponseIds(JSON.stringify({ responseNodes: [{ responseId: '' }] }))).toBeNull();
    expect(grokResponseIds('not json')).toBeNull();
    expect(grokResponseIds(JSON.stringify([R1]))).toBeNull();
    // A node that is not an object is a shape problem, not a skippable row.
    expect(grokResponseIds(JSON.stringify({ responseNodes: [R1] }))).toBeNull();
  });

  it('names only the empty content answer as unverified', () => {
    expect(parseGrokDetailPage(loadResponsesBody([])))
      .toEqual({ ok: true, outcome: 'detail-empty-unverified' });
    expect(parseGrokDetailPage(GOOD_CONTENT)).toEqual({ ok: true, outcome: 'non-empty' });
    // A response that carries no text is NOT judged empty — a turn can legitimately have none.
    expect(parseGrokDetailPage(JSON.stringify({ responses: [{ responseId: R1 }] })))
      .toEqual({ ok: true, outcome: 'non-empty' });
    expect(parseGrokDetailPage(JSON.stringify({ data: { responses: [] } })).ok).toBe(false);
    expect(parseGrokDetailPage('not json').ok).toBe(false);
  });
});
