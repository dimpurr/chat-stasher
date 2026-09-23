/**
 * W22 · Kimi (www.kimi.com): the measured row, the page's own bearer token, the
 * token-in-the-body list cursor, and a detail response that must never be archived
 * when it says it is incomplete.
 *
 * ## What this file exists to stop changing back
 *  1. **The token goes to two paths and nowhere else.** It is read from the page
 *     origin's `localStorage` at request time, kept in no variable of ours, attached
 *     to exactly Kimi's list and detail paths, re-read once after a 401, and never
 *     allowed to turn "we are not logged in" into "you have no conversations".
 *  2. **The list cursor travels inside the POST body, and is opaque.** Kimi is the
 *     first plan whose cursor is not in the URL: `{ page_size, page_token }` is
 *     built by the plan's own builder, the value that comes back goes out again
 *     byte for byte, and the same repeat-page guard W21 added covers this transport.
 *  3. **A detail response that declares more content than it holds is a failure, not
 *     a stored conversation.** A truncated conversation must never be settled as a
 *     complete one; the debt must not be archived, and the leg must carry on with
 *     the next conversation.
 *  4. **The allowlist grew no wildcard.** Two named POST paths, two closed key sets,
 *     and refusals for everything else — including other paths of the same service.
 *
 * 🔴 All fixtures are **synthetic**, hand-written from the field names measured in
 *    the logged-in probe of 2026-09-14. No request goes to kimi.com, there is no
 *    logged-in state, and no real conversation, account, token or id appears
 *    anywhere below. The http port is always injected and always throws on a path it
 *    was not given, so "no request was sent" is proven by the run rather than
 *    asserted about it.
 */

import { describe, expect, it, vi } from 'vitest';
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
import { runBackfill, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  KIMI_CHAT_FEED_TYPE,
  KIMI_DETAIL_CHAT_ID_KEY,
  KIMI_DETAIL_PATH,
  KIMI_DETAIL_TOKEN_KEYS,
  KIMI_LIST_PAGE_SIZE_KEY,
  KIMI_LIST_PAGE_TOKEN_KEY,
  KIMI_LIST_PATH,
  KIMI_PLAN,
  backfillPlanFor,
  canBackfillDetail,
  expectedMethodFor,
  listRequestInit,
  kimiDetailNextToken,
  parseKimiDetailPage,
  parseKimiListPage,
} from '../lib/backfill/enumerate';
import {
  checkBackfillRequest,
  isAllowedBackfillUrl,
  REFUSED_URL_REASON,
} from '../lib/backfill/tab-port';
import { describeFailureReason } from '../lib/backfill/failures';
import {
  createKimiAuthorizedFetch,
  KIMI_ACCESS_TOKEN_STORAGE_KEY,
  KIMI_LANGUAGE_HEADER,
  KIMI_PLATFORM_HEADER,
  KIMI_PLATFORM_HEADER_VALUE,
  needsKimiBearer,
  type MinimalResponse,
} from '../lib/platform-auth';
import { t } from '../lib/i18n';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://www.kimi.com';
const LIST_URL = `${ORIGIN}${KIMI_LIST_PATH}`;
const DETAIL_URL = `${ORIGIN}${KIMI_DETAIL_PATH}`;
/** Two synthetic ids in the two shapes the probe saw (a hex-like one and an alphanumeric one). */
const ID = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
const ID2 = 'kx9Qm2Zr7TvB4nLp';
const PAGE_URL = `${ORIGIN}/chat/${ID}`;
/** A token literal. It is never the real thing: the probe's token was a ~571-character JWT. */
const TOKEN = 'synthetic.jwt.token';

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

/** A fake clock that advances on sleep, so a run's timing is assertable rather than sampled. */
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
// Synthetic fixtures — field names from the probe, values invented here
// ---------------------------------------------------------------------------

/** One feed item of the kind this leg archives: a conversation. */
function chatItem(id: string): Record<string, unknown> {
  return {
    type: KIMI_CHAT_FEED_TYPE,
    chat: {
      id,
      name: `synthetic-${id}`,
      messageContent: 'synthetic preview text',
      createTime: '2026-09-01T10:00:00.000Z',
      updateTime: '2026-09-01T10:05:00.000Z',
    },
  };
}

/** A feed item of another kind — the feed carries more than conversations. */
function otherItem(type: string): Record<string, unknown> {
  return { type, ref: 'synthetic-ref' };
}

/**
 * One page of the list. `token` omitted ⇒ the field is absent entirely (the probe
 * measured absent = last page); `token: null` writes an explicit empty string.
 */
function feedPage(items: unknown[], token?: string | null): string {
  const body: Record<string, unknown> = { items };
  if (token !== undefined) body.nextPageToken = token === null ? '' : token;
  return JSON.stringify(body);
}

/** One message, in the measured key set. */
function message(id: string, parentId?: string): Record<string, unknown> {
  return {
    id,
    parentId: parentId ?? '',
    role: 'user',
    status: 'MESSAGE_STATUS_COMPLETED',
    blocks: [{ blockType: 'BLOCK_TYPE_TEXT', text: `synthetic ${id}` }],
    scenario: 'SCENARIO_K2',
    createTime: '2026-09-01T10:00:01.000Z',
    isGoal: false,
  };
}

/** A detail response: `messages` plus whatever extra top-level keys are asked for. */
function messagesBody(ids: string[], extra: Record<string, unknown> = {}): string {
  return JSON.stringify({ messages: ids.map((id, i) => message(id, ids[i - 1])), ...extra });
}

const GOOD_DETAIL = messagesBody(['m1', 'm2', 'm3']);

/** A cookie-only refusal, in the shape the probe measured (`code`, `details`). */
const REFUSAL_BODY = JSON.stringify({ code: 401, details: 'synthetic refusal' });

interface Recorder {
  calls: { url: string; init: unknown; at: number }[];
}

/**
 * A synthetic backend. Any path it was not given **throws**, so "this leg sent no
 * further request" is proven by the run rather than claimed.
 */
function backend(
  clock: Clock,
  routes: Partial<Record<string, string | ((u: URL) => string)>>,
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
    platform: 'kimi',
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

/** The POST body of one recorded call, parsed. */
function bodyOf(call: { init: unknown }): Record<string, unknown> {
  const init = call.init as { method: string; body: string; contentType: string } | undefined;
  expect(init?.method).toBe('POST');
  expect(init?.contentType).toBe('application/json');
  return JSON.parse(init!.body) as Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// 1 · The platform row
// ---------------------------------------------------------------------------
describe('W22-1 · the kimi row in the platform table', () => {
  const row = PLATFORMS.find((platform) => platform.id === 'kimi')!;

  it('one origin, the message route only, the method that carries it, and the measured trust level', () => {
    expect(row).toBeDefined();
    expect(row.origins).toEqual(['https://www.kimi.com']);
    // 🔴 One path, and it is the route that carries a conversation. The measured
    //    conversation-INDEX route (FeedService/ListFeeds) carries previews, so it is
    //    deliberately outside the row: capturing it would file a summary as the data.
    expect(row.pathHints).toEqual(['ChatService/ListMessages']);
    expect(row.methods).toEqual(['POST']);
    // The probe measured the route in a logged-in session; the table's closed set of
    // levels has no "verified" value, and 'from-source' is the one it has.
    expect(row.credibility).toBe('from-source');
    expect(row.webSocketCapture).toBe(false);

    expect(platformForTraffic(DETAIL_URL, 'POST')?.id).toBe('kimi');
    // The index route and the page route are not captures.
    expect(platformForTraffic(LIST_URL, 'POST')).toBeNull();
    expect(platformForTraffic(PAGE_URL, 'GET')).toBeNull();
  });

  it('the shape gate takes the messages envelope and refuses everything that is not it', () => {
    expect(matchesResponseShape(row, GOOD_DETAIL)).toBe(true);
    // An EMPTY array passes: `[]` is a measurement, not a missing field.
    expect(matchesResponseShape(row, messagesBody([]))).toBe(true);
    // The feed page is plainly not the messages envelope.
    expect(matchesResponseShape(row, feedPage([chatItem(ID)]))).toBe(false);
    // Drift: same route, same 200, the envelope moved under a different key.
    expect(matchesResponseShape(row, JSON.stringify({ data: { messages: [] } }))).toBe(false);
    expect(matchesResponseShape(row, 'not json')).toBe(false);
  });

  it('the origin set is still closed, and kimi added exactly one entry to it', () => {
    expect(CONTENT_MATCHES).toContain('https://www.kimi.com/*');
    expect(CONTENT_MATCHES).not.toContain('<all_urls>');
    expect(CONTENT_MATCHES.some((match) => match.startsWith('*'))).toBe(false);
    for (const hostile of [
      'https://www.kimi.com.attacker.example/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'https://kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'https://kimi.moonshot.cn/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'http://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
    ]) {
      expect(platformForTraffic(hostile, 'POST')).toBeNull();
      expect(isCapturedFetchShape({
        url: hostile,
        method: 'POST',
        status: 200,
        text: GOOD_DETAIL,
        pageUrl: PAGE_URL,
        capturedAt: Date.now(),
      })).toBe(false);
    }
  });

  it('the backfill plan declares both segments, and both are POSTs with a closed body', () => {
    expect(backfillPlanFor('kimi')).toBe(KIMI_PLAN);
    expect(canBackfillDetail(KIMI_PLAN)).toBe(true);
    expect(expectedMethodFor(KIMI_PLAN, 'list')).toBe('POST');
    expect(expectedMethodFor(KIMI_PLAN, 'detail')).toBe('POST');
    expect(KIMI_PLAN.detailPath).toBe(KIMI_DETAIL_PATH);
    // The conversation id is in the body, so the body URL is one fixed route.
    expect(KIMI_PLAN.detailUrl!(ORIGIN, ID)).toBe(DETAIL_URL);
    // 🔴 The two answers must not disagree: this plan declares no `listPost` (its
    //    cursor is a body cursor), and the request builder still answers POST with
    //    the first page's body — never a GET the allowlist would refuse.
    expect(listRequestInit(KIMI_PLAN, ORIGIN, 0, 100)).toEqual({
      method: 'POST',
      contentType: 'application/json',
      body: JSON.stringify({ [KIMI_LIST_PAGE_SIZE_KEY]: 100, [KIMI_LIST_PAGE_TOKEN_KEY]: '' }),
    });
  });
});

// ---------------------------------------------------------------------------
// 2 · Session id: the page URL, and the file-name fragment
// ---------------------------------------------------------------------------
describe('W22-2 · which conversation this capture is', () => {
  it('the id comes out of the page URL, in both of the id shapes the probe saw', () => {
    // 🔴 The API path carries no id (it is in the request body), so the page URL is
    //    the only source — which is why the row keeps a page-URL pattern at all.
    expect(extractSessionId(DETAIL_URL, GOOD_DETAIL, PAGE_URL)).toBe(ID);
    expect(extractSessionId(DETAIL_URL, GOOD_DETAIL, `${ORIGIN}/chat/${ID2}`)).toBe(ID2);
    // No page URL, no id: it is never guessed from the body.
    expect(extractSessionId(DETAIL_URL, GOOD_DETAIL)).toBeNull();
    // A page URL that is not a conversation tells us nothing.
    expect(extractSessionId(DETAIL_URL, GOOD_DETAIL, `${ORIGIN}/`)).toBeNull();
  });

  it('the debt key and the file-name fragment are the same value, so they cannot disagree', () => {
    const sessionId = extractSessionId(DETAIL_URL, GOOD_DETAIL, PAGE_URL);
    expect(sessionId).toBe(ID);
    // The identity map (contract.ts pathSafeSessionId): both measured id shapes are
    // already safe, so nothing is renamed on the way to disk.
    expect(pathSafeSessionId(sessionId!)).toBe(ID);
    expect(pathSafeSessionId(ID2)).toBe(ID2);
  });

  it('a page payload cannot name the file it is written to', () => {
    expect(isCapturedFetchShape({
      url: DETAIL_URL,
      method: 'POST',
      status: 200,
      text: GOOD_DETAIL,
      pageUrl: PAGE_URL,
      capturedAt: Date.now(),
      sessionId: ID,
    })).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 3 · Live capture
// ---------------------------------------------------------------------------
describe('W22-3 · the passive hook', () => {
  function makeFakeWindow(responseBody: string) {
    const posted: unknown[] = [];
    const fakeWindow: any = {
      location: { origin: ORIGIN, href: PAGE_URL },
      async fetch() { return new Response(responseBody, { status: 200 }); },
      addEventListener() { /* the probe listener is irrelevant to these assertions */ },
      postMessage(msg: unknown) { posted.push(msg); },
    };
    return { fakeWindow, posted };
  }

  it('a ListMessages response on the /chat/<id> page becomes one bundle with that session id', async () => {
    const { fakeWindow, posted } = makeFakeWindow(GOOD_DETAIL);
    vi.stubGlobal('window', fakeWindow);
    try {
      installPageFetchHook(PAGE_HOOK_OPTIONS);
      await fakeWindow.fetch(DETAIL_URL, { method: 'POST' });
      await new Promise((resolve) => setTimeout(resolve, 0));

      const captures = posted.filter((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage) as any[];
      expect(captures).toHaveLength(1);
      const payload = captures[0].payload;
      expect(payload.url).toBe(DETAIL_URL);
      expect(payload.method).toBe('POST');
      expect(payload.status).toBe(200);
      expect(payload.pageUrl).toBe(PAGE_URL);
      // The payload is the raw body, untouched.
      expect(payload.text).toBe(GOOD_DETAIL);
      // And it passes the bridge's own gate, which is what makes it deliverable at all.
      expect(isCapturedFetchShape(payload)).toBe(true);
      expect(extractSessionId(payload.url, payload.text, payload.pageUrl)).toBe(ID);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it('a feed page passes by without a warning and without a capture', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(feedPage([chatItem(ID)]));
    vi.stubGlobal('window', fakeWindow);
    try {
      installPageFetchHook(PAGE_HOOK_OPTIONS);
      await fakeWindow.fetch(LIST_URL, { method: 'POST' });
      await new Promise((resolve) => setTimeout(resolve, 0));

      // 🔴 This is why pathHints is the message path: a feed page is not conversation
      //    data, so it is skipped silently rather than warned about on every page load.
      expect(warn).not.toHaveBeenCalled();
      expect(posted.some((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
    } finally {
      vi.restoreAllMocks();
      vi.unstubAllGlobals();
    }
  });

  it('a messages response whose shape drifted is refused and said out loud', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(JSON.stringify({ data: { messages: [] } }));
    vi.stubGlobal('window', fakeWindow);
    try {
      installPageFetchHook(PAGE_HOOK_OPTIONS);
      await fakeWindow.fetch(DETAIL_URL, { method: 'POST' });
      await new Promise((resolve) => setTimeout(resolve, 0));

      expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
      // Metadata-only: no URL, body or id in the warning.
      expect(warn.mock.calls[0]).toHaveLength(1);
      expect(posted.some((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
    } finally {
      vi.restoreAllMocks();
      vi.unstubAllGlobals();
    }
  });
});

// ---------------------------------------------------------------------------
// 4 · The page's own bearer token
// ---------------------------------------------------------------------------
describe('W22-4 · the Kimi token wrapper', () => {
  interface FetchCall { url: string; init: RequestInit }

  /** A raw fetch that answers from a queue and **throws** when the queue runs out. */
  function fetchQueue(replies: MinimalResponse[]) {
    const calls: FetchCall[] = [];
    const fetch = async (url: string, init: RequestInit): Promise<MinimalResponse> => {
      calls.push({ url, init });
      const reply = replies.shift();
      if (!reply) throw new Error(`unexpected request to ${new URL(url).pathname}`);
      return reply;
    };
    return { calls, fetch };
  }

  const ok = (text = GOOD_DETAIL): MinimalResponse => ({ status: 200, text: async () => text });
  const refused = (status = 401): MinimalResponse => ({ status, text: async () => REFUSAL_BODY });

  function headersOf(call: FetchCall): Record<string, string> {
    return (call.init.headers ?? {}) as Record<string, string>;
  }

  it('names exactly the two backfill paths on Kimi\'s own origin, and nothing else', () => {
    expect(needsKimiBearer(LIST_URL, ORIGIN)).toBe(true);
    expect(needsKimiBearer(DETAIL_URL, ORIGIN)).toBe(true);
    for (const url of [
      // A third path on the same service, with and without the path appended.
      `${ORIGIN}/apiv2/kimi.gateway.chat.v1.ChatService/SendMessage`,
      `${DETAIL_URL}/extra`,
      `${LIST_URL}/extra`,
      // The index and page routes.
      `${ORIGIN}/apiv2/kimi.gateway.feed.v1.FeedService/ListFeedsX`,
      PAGE_URL,
      `${ORIGIN}/`,
      // The old service spelling some sources used: the same call, a different path.
      `${ORIGIN}/apiv2/kimi.chat.v1.ChatService/ListMessages`,
      // Another origin entirely, a look-alike host, plain http.
      'https://evil.example/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'https://www.kimi.com.attacker.example/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'http://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'not a url',
    ]) {
      expect(needsKimiBearer(url, ORIGIN), url).toBe(false);
    }
    // The page origin must match too (a content script only ever fetches its own page's origin).
    expect(needsKimiBearer(LIST_URL, 'https://chat.deepseek.com')).toBe(false);
  });

  it('reads the token at request time, never caches it, and sends it only where it belongs', async () => {
    let stored: string | null = TOKEN;
    let reads = 0;
    const readToken = (): string | null => { reads += 1; return stored; };
    const queue = fetchQueue([ok(), ok(), ok()]);
    const authorized = createKimiAuthorizedFetch(ORIGIN, queue.fetch, { readToken, language: 'en-GB' });

    // 🔴 Constructing the wrapper reads nothing: the token is not fetched, prefetched
    //    or held — it does not exist anywhere until a request that needs it is made.
    expect(reads).toBe(0);

    await authorized(LIST_URL, { method: 'POST', body: '{}' });
    expect(reads).toBe(1);
    expect(headersOf(queue.calls[0]!)).toMatchObject({
      authorization: `Bearer ${TOKEN}`,
      [KIMI_PLATFORM_HEADER]: KIMI_PLATFORM_HEADER_VALUE,
      [KIMI_LANGUAGE_HEADER]: 'en-GB',
    });

    // The page rotated its token between two requests: the second call carries the new
    // one, which is what "read at request time, never cached" means in practice.
    stored = 'synthetic.jwt.token.2';
    await authorized(DETAIL_URL, { method: 'POST', body: '{}' });
    expect(headersOf(queue.calls[1]!).authorization).toBe('Bearer synthetic.jwt.token.2');

    // 🔴 A third Kimi path is fetched exactly as it was asked for — no authorization
    //    header, no extra headers, and not even a read of the storage.
    const before = reads;
    await authorized(`${ORIGIN}/apiv2/kimi.gateway.chat.v1.ChatService/SendMessage`, { method: 'POST' });
    expect(headersOf(queue.calls[2]!)).toEqual({});
    expect(reads).toBe(before);
  });

  it('a 401 re-reads once and retries once, and only a 401', async () => {
    let stored: string | null = 'stale';
    let reads = 0;
    const readToken = (): string | null => { reads += 1; return stored; };
    const queue = fetchQueue([refused(401), ok()]);
    const authorized = createKimiAuthorizedFetch(ORIGIN, queue.fetch, { readToken, language: null });

    const before = reads;
    const res = await authorized(LIST_URL, { method: 'POST', body: '{}' });
    // Two attempts, two reads — the retry re-reads rather than reusing the value that
    // was just refused, which is the one failure a retry can actually fix.
    expect(queue.calls).toHaveLength(2);
    expect(reads - before).toBe(2);
    expect(headersOf(queue.calls[0]!).authorization).toBe('Bearer stale');
    expect(res.status).toBe(200);

    // A 403 is the platform's answer, not a stale token: one attempt, no retry. (The
    // measured refusal for a cookie-only request is a 401, so this pins the boundary.)
    const forbidden = fetchQueue([refused(403), ok()]);
    await createKimiAuthorizedFetch(ORIGIN, forbidden.fetch, { readToken, language: null })(
      LIST_URL, { method: 'POST', body: '{}' },
    );
    expect(forbidden.calls).toHaveLength(1);
  });

  it('a 401 twice is returned as the platform\'s own refusal, never as an empty result', async () => {
    const queue = fetchQueue([refused(401), refused(401)]);
    const authorized = createKimiAuthorizedFetch(ORIGIN, queue.fetch, {
      readToken: () => TOKEN,
      language: null,
    });
    const res = await authorized(LIST_URL, { method: 'POST', body: '{}' });
    expect(res.status).toBe(401);
    // The body is passed through untouched — the caller decides what a refusal means.
    expect(await res.text()).toBe(REFUSAL_BODY);
    expect(queue.calls).toHaveLength(2);
  });

  it('with no readable token the request goes out without one, so the platform answers', async () => {
    for (const absent of [null, '', 'has\u0000acontrolcharacter']) {
      const queue = fetchQueue([refused(401)]);
      const authorized = createKimiAuthorizedFetch(ORIGIN, queue.fetch, {
        readToken: () => absent,
        language: null,
      });
      const res = await authorized(DETAIL_URL, { method: 'POST', body: `{"${KIMI_DETAIL_CHAT_ID_KEY}":"x"}` });

      // 🔴 No authorization header at all — not an empty one: `Bearer ` with nothing
      //    after it is a different request, and nobody has asked what the platform
      //    makes of it. The two other headers are still the page's own.
      const headers = headersOf(queue.calls[0]!);
      expect(headers.authorization).toBeUndefined();
      expect(headers[KIMI_PLATFORM_HEADER]).toBe(KIMI_PLATFORM_HEADER_VALUE);
      // And the platform's refusal is what comes back — nothing here turns it into a result.
      expect(res.status).toBe(401);
      // A value that cannot be carried in a header is treated as no token, so this is
      // also one attempt: there is nothing to re-read into a retry.
      expect(queue.calls).toHaveLength(1);
    }
  });

  it('the language header is the page\'s own locale, and is omitted when there is none', async () => {
    const queue = fetchQueue([ok(), ok()]);
    const authorized = createKimiAuthorizedFetch(ORIGIN, queue.fetch, {
      readToken: () => TOKEN,
      language: 'zh-CN',
    });
    await authorized(LIST_URL, { method: 'POST', body: '{}' });
    expect(headersOf(queue.calls[0]!)[KIMI_LANGUAGE_HEADER]).toBe('zh-CN');

    const none = fetchQueue([ok()]);
    await createKimiAuthorizedFetch(ORIGIN, none.fetch, { readToken: () => TOKEN, language: null })(
      LIST_URL, { method: 'POST', body: '{}' },
    );
    expect(KIMI_LANGUAGE_HEADER in headersOf(none.calls[0]!)).toBe(false);
  });

  it('the storage key the wrapper reads is the one the page writes', () => {
    // The measured key. Named as a constant so the bridge and this test cannot drift.
    expect(KIMI_ACCESS_TOKEN_STORAGE_KEY).toBe('access_token');
  });
});

// ---------------------------------------------------------------------------
// 5 · The list parser
// ---------------------------------------------------------------------------
describe('W22-5 · parsing one page of the feed', () => {
  it('keeps the chat items, skips the other kinds, and carries only a non-empty token forward', () => {
    expect(parseKimiListPage(feedPage([chatItem(ID), otherItem('FEED_TYPE_DOC'), chatItem(ID2)], 'tok-1')))
      .toEqual({ ok: true, page: { ids: [ID, ID2], total: null, nextToken: 'tok-1' } });
    // Absent ⇒ the API's own "there is no next page"; an explicit empty string means the same.
    expect(parseKimiListPage(feedPage([chatItem(ID)])))
      .toEqual({ ok: true, page: { ids: [ID], total: null, nextToken: null } });
    expect(parseKimiListPage(feedPage([chatItem(ID)], null)))
      .toEqual({ ok: true, page: { ids: [ID], total: null, nextToken: null } });
    // A page of nothing but non-chat items is a readable page with zero rows, not a shape
    // problem — 🔴 and its token is still carried, because the token is what says "more".
    expect(parseKimiListPage(feedPage([otherItem('FEED_TYPE_DOC')], 'tok-2')))
      .toEqual({ ok: true, page: { ids: [], total: null, nextToken: 'tok-2' } });
    // An empty page is likewise readable, with zero rows.
    expect(parseKimiListPage(feedPage([], 'tok-1')))
      .toEqual({ ok: true, page: { ids: [], total: null, nextToken: 'tok-1' } });
  });

  it('never turns an unrecognised or changed shape into an empty list', () => {
    // No `items` array ⇒ shape-changed, NOT "this account has no conversations".
    expect(parseKimiListPage(JSON.stringify({ data: { items: [] } })).ok).toBe(false);
    expect(parseKimiListPage(JSON.stringify([chatItem(ID)])).ok).toBe(false);
    expect(parseKimiListPage('not json').ok).toBe(false);
    // 🔴 An item this code cannot CLASSIFY is a shape halt, not a skip: it might be a
    //    conversation, and dropping it would lose that conversation silently.
    const unclassified = parseKimiListPage(feedPage([{ chat: { id: ID } }]));
    expect(unclassified.ok).toBe(false);
    expect(unclassified.ok === false && unclassified.detail).toContain('type');
    // A chat item whose `chat.id` cannot be read is the same kind of halt.
    const noId = parseKimiListPage(feedPage([{ type: KIMI_CHAT_FEED_TYPE, chat: { name: 'x' } }]));
    expect(noId.ok).toBe(false);
    expect(noId.ok === false && noId.detail).toContain('chat.id');
    // An element that is not an object at all, and a chat item with no `chat` object.
    expect(parseKimiListPage(feedPage(['not-an-object'])).ok).toBe(false);
    expect(parseKimiListPage(feedPage([{ type: KIMI_CHAT_FEED_TYPE }])).ok).toBe(false);
    // 🔴 The token's *type* changing is drift, and must not be rounded into "we reached the end".
    const drifted = parseKimiListPage(JSON.stringify({ items: [chatItem(ID)], nextPageToken: 7 }));
    expect(drifted.ok).toBe(false);
    expect(drifted.ok === false && drifted.detail).toContain('nextPageToken');
  });
});

// ---------------------------------------------------------------------------
// 6 · The list segment in the engine: the cursor in the body
// ---------------------------------------------------------------------------
describe('W22-6 · enumerating with the cursor inside the request body', () => {
  it('posts the closed body, ends on an absent token, and says so as a confirmed ending', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [KIMI_LIST_PATH]: feedPage([chatItem(ID)]),
      [KIMI_DETAIL_PATH]: GOOD_DETAIL,
    });
    const report = await run(memoryStore(), be.http, 'w22-end-token', { clock });

    expect(report.enumeratedPages).toBe(1);
    expect(report.state.enumCursor.complete).toBe(true);
    // 🔴 A confirmed ending, not a client inference: no truncation is recorded.
    expect(report.enumTruncated).toBeNull();
    expect(report.state.enumCursor.token ?? null).toBeNull();

    // The list request is a POST whose body is exactly the two declared keys, with the
    // page size the caller asked for and an empty token on the first page.
    const listCall = be.calls[0]!;
    expect(new URL(listCall.url).pathname).toBe(KIMI_LIST_PATH);
    expect(new URL(listCall.url).search).toBe('');
    expect(bodyOf(listCall)).toEqual({ [KIMI_LIST_PAGE_SIZE_KEY]: 100, [KIMI_LIST_PAGE_TOKEN_KEY]: '' });
    expect(report.state.archived).toEqual([ID]);
  });

  it('hands the token back byte for byte, and ends on an empty page', async () => {
    const clock = fakeClock();
    // 🔴 Awkward characters on purpose: the value that goes out must be the value that
    //    came in, not a re-encoded or parsed form, and the server's answer depends on it.
    const CURSOR = 'cursor/A+B==';
    /**
     * Unlike the path-keyed backend above, this list route's answer depends on the
     * REQUEST BODY — which is the only place this platform's cursor appears.
     */
    const sent: Record<string, unknown>[] = [];
    const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
      const path = new URL(url).pathname;
      // The conversation body is served too, so the run reaches the end of its tick
      // rather than stopping at a missing route.
      if (path === KIMI_DETAIL_PATH) return { status: 200, text: GOOD_DETAIL };
      expect(path).toBe(KIMI_LIST_PATH);
      const body = bodyOf({ init });
      sent.push(body);
      return {
        status: 200,
        text: body[KIMI_LIST_PAGE_TOKEN_KEY] === CURSOR ? feedPage([]) : feedPage([chatItem(ID)], CURSOR),
      };
    };
    const store = memoryStore();
    const first = await run(store, http as never, 'w22-end-page', { clock });
    expect(first.state.enumCursor.complete).toBe(false);
    expect(first.state.enumCursor.token).toBe(CURSOR);
    expect(sent).toHaveLength(1);
    // The first page asks for no cursor, and says so with an empty string rather than by
    // omitting the key (see KIMI_PLAN.listTokenPost for why those are the same message).
    expect(sent[0]).toEqual({ [KIMI_LIST_PAGE_SIZE_KEY]: 100, [KIMI_LIST_PAGE_TOKEN_KEY]: '' });

    // The next tick asks for the next page — with the cursor exactly as it arrived.
    const second = await run(store, http as never, 'w22-end-page', { clock });
    expect(sent).toHaveLength(2);
    expect(sent[1]![KIMI_LIST_PAGE_TOKEN_KEY]).toBe(CURSOR);
    // Page two is empty ⇒ the list is over, recorded as complete rather than truncated.
    expect(second.state.enumCursor.complete).toBe(true);
    expect(second.enumTruncated).toBeNull();
  });

  it('a page that repeats what this enumeration already listed halts, not as an ending', async () => {
    const clock = fakeClock();
    // Both pages answer with the same ids whatever cursor they are given: the symptom of
    // a backend that ignores the cursor parameter.
    const calls: Record<string, unknown>[] = [];
    const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
      expect(new URL(url).pathname).toBe(KIMI_LIST_PATH);
      calls.push(bodyOf({ init }));
      return { status: 200, text: feedPage([chatItem(ID), chatItem(ID2)], 'stuck-token') };
    };
    const detailHttp = async (url: string, init?: unknown): Promise<HttpResponse> => {
      calls.push(bodyOf({ init }));
      return { status: 200, text: GOOD_DETAIL };
    };
    const store = memoryStore();
    // The first page needs its detail fetch too, so drive both through one port.
    const both = async (url: string, init?: unknown): Promise<HttpResponse> =>
      new URL(url).pathname === KIMI_LIST_PATH ? http(url, init) : detailHttp(url, init);

    const first = await run(store, both as never, 'w22-repeat', { clock });
    expect(first.state.archived).toEqual([ID, ID2]);
    expect(first.state.enumCursor.complete).toBe(false);

    const second = await run(store, both as never, 'w22-repeat', { clock });
    expect(second.stopped).toBe('halted');
    expect(second.halted?.reason).toBe('shape-changed');
    expect(second.halted?.detail).toContain('did not advance');
    // 🔴 NOT an ending: the cursor is left where it was and complete stays false.
    expect(second.state.enumCursor.complete).toBe(false);
    expect(second.enumTruncated).toBeNull();
    // 🔴 Nothing was settled and nothing was lost.
    expect(second.state.archived).toEqual([ID, ID2]);
    expect(second.state.pending).toEqual([]);
    expect(second.newDebts).toBe(0);
    // A permanent stop: the next tick sends no request at all.
    calls.length = 0;
    const third = await run(store, both as never, 'w22-repeat', { clock });
    expect(third.stopped).toBe('halted');
    expect(calls).toHaveLength(0);
  });

  it('a list page whose shape drifted halts without settling and without touching a body', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [KIMI_LIST_PATH]: JSON.stringify({ data: { items: [chatItem(ID)] } }),
      [KIMI_DETAIL_PATH]: GOOD_DETAIL,
    });
    const report = await run(memoryStore(), be.http, 'w22-list-drift', { clock });
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.state.pending).toEqual([]);
    expect(report.state.archived).toEqual([]);
    // Only the list request went out (the backend throws on anything it was not given).
    expect(be.calls).toHaveLength(1);
  });

  it('a 401 on the list is an auth halt, never "you have no conversations"', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      // A cookie-only answer, exactly as measured. The backend has the detail route too,
      // so a leg that read the refusal as an empty list WOULD go on to fetch bodies.
      [KIMI_LIST_PATH]: JSON.stringify({ code: 401, details: 'synthetic refusal' }),
      [KIMI_DETAIL_PATH]: GOOD_DETAIL,
    });
    // The refusal arrives as a status, which is how the leg sees it in a browser.
    const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
      const res = await be.http(url, init);
      return new URL(url).pathname === KIMI_LIST_PATH ? { status: 401, text: res.text } : res;
    };
    const report = await run(memoryStore(), http as never, 'w22-auth', { clock });

    // 🔴 W64 · This assertion used to read `'halted'`, and that premise was the defect
    //    this test was named for rather than a fact about Kimi: `'halted'` is the
    //    **permanent** stop, so a 401 — which this test's own title calls an auth halt
    //    — was recorded as a permanent one and the leg never asked again. A 401 is a
    //    credential refusal, it is transient, and `'waiting-retry'` is what says so.
    //    Nothing else in this test moved: the detail still names the status, the two
    //    mistakes it forbids are still forbidden, and the reason is now pinned beside
    //    them instead of being left to the generic sentence.
    expect(report.stopped).toBe('waiting-retry');
    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.halted?.detail).toContain('401');
    // 🔴 The two mistakes this forbids: an empty list recorded (nothing pending, complete
    //    true) and a refusal rounded into progress (a body fetched anyway).
    expect(report.state.pending).toEqual([]);
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.enumTruncated).toBeNull();
    expect(be.calls.filter((c) => new URL(c.url).pathname === KIMI_DETAIL_PATH)).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// 7 · The detail segment: one POST, and the incomplete-body refusal
// ---------------------------------------------------------------------------
describe('W22-7 · one conversation per body request', () => {
  it('posts the id in the closed body, and the bundle\'s identity is the debt key', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [KIMI_LIST_PATH]: feedPage([chatItem(ID)]),
      [KIMI_DETAIL_PATH]: GOOD_DETAIL,
    });
    const captured: CapturedFetch[] = [];
    const report = await run(memoryStore(), be.http, 'w22-detail', {
      clock,
      sink: (c: CapturedFetch): SinkOutcome => { captured.push(c); return { saved: true, sessionId: c.sessionId }; },
    });

    expect(be.calls.map((c) => new URL(c.url).pathname)).toEqual([KIMI_LIST_PATH, KIMI_DETAIL_PATH]);
    // The detail request's body is exactly the one declared key, carrying the list's id.
    expect(bodyOf(be.calls[1]!)).toEqual({ [KIMI_DETAIL_CHAT_ID_KEY]: ID });
    expect(new URL(be.calls[1]!.url).search).toBe('');

    // 🔴 The delivered artefact carries the debt key as its identity — the same value the
    //    file name comes from, so the two cannot disagree.
    expect(captured).toHaveLength(1);
    expect(captured[0]!.sessionId).toBe(ID);
    /**
     * 🔴 `pageUrl` here is the engine's generic `${origin}/c/<id>` — a shape that is
     * right for ChatGPT and Grok and **wrong for Kimi** (its page is `/chat/<id>`) and
     * for DeepSeek. Asserted for the property that matters rather than for the exact
     * string, because the exact string is not this platform's page: it is a shared
     * line in engine.ts that this change deliberately did not touch (it appears in no
     * archive — the bundle's own `url` is the API URL — and it is used only as a
     * same-origin fallback). Named here rather than smoothed over; see the report.
     */
    expect(new URL(captured[0]!.pageUrl!).origin).toBe(ORIGIN);
    expect(captured[0]!.pageUrl).toContain(ID);
    expect(captured[0]!.text).toBe(GOOD_DETAIL);
    expect(report.state.archived).toEqual([ID]);
    expect(report.state.pending).toEqual([]);
    expect(report.stopped).toBe('queue-empty');
  });

  it('a body that says it is incomplete is refused, recorded as a failure, and never archived', async () => {
    const clock = fakeClock();
    const PAGED = messagesBody(['m1', 'm2'], { nextPageToken: 'more-of-this-conversation' });
    const be = backend(clock, {
      // The second conversation is complete: it proves the leg carried on rather than
      // halting, and that only the long one was written off.
      [KIMI_LIST_PATH]: feedPage([chatItem(ID), chatItem(ID2)]),
      [KIMI_DETAIL_PATH]: GOOD_DETAIL,
    });
    const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
      const res = await be.http(url, init);
      const wantsId = new URL(url).pathname === KIMI_DETAIL_PATH
        && bodyOf({ init })[KIMI_DETAIL_CHAT_ID_KEY] === ID;
      return wantsId ? { status: 200, text: PAGED } : res;
    };
    const report = await run(memoryStore(), http as never, 'w22-paged', { clock });

    // 🔴 The whole point: the truncated conversation is NOT in the archive, and its debt is
    //    not settled. Archiving it would put a partial answer in the archive with nothing
    //    saying it was partial.
    expect(report.state.archived).toEqual([ID2]);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['detail-paged-unsupported']);
    expect(report.state.failures?.map((f) => f.reason)).toEqual(['detail-paged-unsupported']);
    // The receipt locates the conversation without carrying it: the first 8 characters of
    // the id, this platform, the reason code, and when.
    expect(report.state.failures?.[0]!.shortId).toBe(ID.slice(0, 8));
    expect(report.state.failures?.[0]!.platform).toBe('kimi');
    // The request really went out, so it counts against the day's quota like any other.
    expect(report.state.detailToday.count).toBe(2);
    expect(report.stopped).toBe('queue-empty');
    // And the reason reads as an observed fact in the user's language.
    expect(describeFailureReason('detail-paged-unsupported')).toBe(t('failure.detailPagedUnsupported'));
    expect(describeFailureReason('detail-paged-unsupported')).not.toContain('detail-paged-unsupported');
  });

  it('the other spelling of a next-page token is read the same way', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [KIMI_LIST_PATH]: feedPage([chatItem(ID)]),
      [KIMI_DETAIL_PATH]: messagesBody(['m1'], { next_page_token: 'more' }),
    });
    const report = await run(memoryStore(), be.http, 'w22-paged-snake', { clock });
    expect(report.state.archived).toEqual([]);
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['detail-paged-unsupported']);
    // 🔴 Both spellings are declared in one place, and only these two are looked at.
    expect([...KIMI_DETAIL_TOKEN_KEYS]).toEqual(['nextPageToken', 'next_page_token']);
  });

  it('a null, empty or absent token field means the body is whole', async () => {
    for (const extra of [{}, { nextPageToken: null }, { nextPageToken: '' }, { nextToken: 'not-read' }]) {
      expect(kimiDetailNextToken(JSON.stringify({ messages: [], ...extra }))).toEqual({ kind: 'none' });
    }
    // 🔴 A token field that is present but not a string is NOT "no more pages".
    expect(kimiDetailNextToken(JSON.stringify({ messages: [], nextPageToken: 7 })))
      .toEqual({ kind: 'unreadable' });
    expect(parseKimiDetailPage(JSON.stringify({ messages: [], nextPageToken: {} })).ok).toBe(false);
    // A null token field is a body with nothing after it — not drift.
    expect(parseKimiDetailPage(JSON.stringify({ messages: ['x'], next_page_token: null })))
      .toEqual({ ok: true, outcome: 'non-empty' });
  });

  it('an unreadable token field halts the leg and leaves the debt pending', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [KIMI_LIST_PATH]: feedPage([chatItem(ID)]),
      [KIMI_DETAIL_PATH]: messagesBody(['m1'], { nextPageToken: 7 }),
    });
    const report = await run(memoryStore(), be.http, 'w22-token-drift', { clock });
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    // 🔴 Pending, not written off: the field's type changing is a wire fact, and this
    //    conversation has not been shown to be incomplete OR complete.
    expect(report.state.pending).toEqual([ID]);
    expect(report.state.archived).toEqual([]);
    expect(report.state.failures ?? []).toEqual([]);
  });

  it('a body that carries no messages is refused, and so is one where `messages` is not an array', async () => {
    const clock = fakeClock();
    // `{messages: "text"}` PASSES the row's own shape gate (the key is present and non-null),
    // so this is the plan's parser catching what the generic gate cannot.
    const be = backend(clock, {
      [KIMI_LIST_PATH]: feedPage([chatItem(ID)]),
      [KIMI_DETAIL_PATH]: JSON.stringify({ messages: 'not-an-array' }),
    });
    const report = await run(memoryStore(), be.http, 'w22-detail-shape', { clock });
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.state.pending).toEqual([ID]);
    expect(report.state.archived).toEqual([]);
  });

  it('an empty messages array is delivered rather than refused — and that is a documented choice', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [KIMI_LIST_PATH]: feedPage([chatItem(ID)]),
      [KIMI_DETAIL_PATH]: messagesBody([]),
    });
    const captured: CapturedFetch[] = [];
    const report = await run(memoryStore(), be.http, 'w22-empty-body', {
      clock,
      sink: (c: CapturedFetch): SinkOutcome => { captured.push(c); return { saved: true, sessionId: c.sessionId }; },
    });
    // 🔴 From one response alone, "this conversation has no messages" and "this is a window
    //    with nothing in it" are not distinguishable; the two mistakes are not equal in
    //    size, and the window case is covered by the page-token rule instead. (Grok's
    //    two-step plan makes the opposite call for its own route — see parseGrokDetailPage.)
    expect(report.state.archived).toEqual([ID]);
    expect(captured[0]!.text).toBe(messagesBody([]));
    expect(parseKimiDetailPage(messagesBody([]))).toEqual({ ok: true, outcome: 'non-empty' });
  });
});

// ---------------------------------------------------------------------------
// 8 · The allowlist
// ---------------------------------------------------------------------------
describe('W22-8 · what the content script lets through for kimi, and what it refuses', () => {
  const listInit = {
    url: LIST_URL,
    method: 'POST',
    body: JSON.stringify({ [KIMI_LIST_PAGE_SIZE_KEY]: 100, [KIMI_LIST_PAGE_TOKEN_KEY]: '' }),
    contentType: 'application/json',
  };
  const detailInit = {
    url: DETAIL_URL,
    method: 'POST',
    body: JSON.stringify({ [KIMI_DETAIL_CHAT_ID_KEY]: ID }),
    contentType: 'application/json',
  };

  it('accepts exactly the two declared requests', () => {
    expect(checkBackfillRequest(listInit, ORIGIN)).toMatchObject({ ok: true, method: 'POST' });
    expect(checkBackfillRequest(detailInit, ORIGIN)).toMatchObject({ ok: true, method: 'POST' });
    // The token-carrying form of the list body, which is the one the engine actually sends
    // on a second page, is admitted by the same rule.
    expect(checkBackfillRequest({
      ...listInit,
      body: JSON.stringify({ [KIMI_LIST_PAGE_SIZE_KEY]: 100, [KIMI_LIST_PAGE_TOKEN_KEY]: 'tok/A+B==' }),
    }, ORIGIN).ok).toBe(true);
  });

  it('refuses a third path, a look-alike path, and any other origin', () => {
    for (const url of [
      `${ORIGIN}/apiv2/kimi.gateway.chat.v1.ChatService/SendMessage`,
      `${DETAIL_URL}/extra`,
      `${LIST_URL}/extra`,
      `${ORIGIN}/apiv2/kimi.chat.v1.ChatService/ListMessages`,
      `${ORIGIN}/apiv2/`,
      PAGE_URL,
      'https://evil.example/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'https://www.kimi.com.attacker.example/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'http://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages',
      'not a url',
    ]) {
      const verdict = checkBackfillRequest({ ...detailInit, url }, ORIGIN);
      expect(verdict.ok, url).toBe(false);
      expect(verdict.ok === false && verdict.reason).toBe(REFUSED_URL_REASON);
    }
    // Same URL, wrong page: the content script only ever serves its own origin.
    expect(checkBackfillRequest(detailInit, 'https://chat.deepseek.com').ok).toBe(false);
  });

  it('refuses a wrong method in both directions', () => {
    // The method is the plan's, not the request's.
    expect(checkBackfillRequest({ ...listInit, method: 'GET' }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({ ...detailInit, method: 'GET' }, ORIGIN).ok).toBe(false);
    // …and a GET segment with a body is refused too (the ChatGPT/DEEPSEEK side of the rule).
    const chatgptList = 'https://chatgpt.com/backend-api/conversations';
    expect(checkBackfillRequest({ url: chatgptList, body: '{}', contentType: 'application/json' }, 'https://chatgpt.com').ok)
      .toBe(false);
  });

  it('refuses a body outside the declared key set, a nested value, and a wrong Content-Type', () => {
    const refused = (init: Record<string, unknown>, origin = ORIGIN): boolean =>
      checkBackfillRequest(init as never, origin).ok;

    // A key outside the closed set, however plausible the name looks.
    expect(refused({ ...listInit, body: JSON.stringify({ pageSize: 100, page_token: '' }) })).toBe(false);
    expect(refused({ ...detailInit, body: JSON.stringify({ chat_id: ID, extra: 1 }) })).toBe(false);
    // A nested structure hanging under a declared key.
    expect(refused({ ...detailInit, body: JSON.stringify({ chat_id: { id: ID } }) })).toBe(false);
    // Not JSON, not an object, and no body at all.
    expect(refused({ ...detailInit, body: 'not json' })).toBe(false);
    expect(refused({ ...detailInit, body: '[1]' })).toBe(false);
    expect(refused({ url: detailInit.url, method: 'POST', contentType: 'application/json' })).toBe(false);
    // A wrong or absent Content-Type.
    expect(refused({ ...detailInit, contentType: 'text/plain' })).toBe(false);
    expect(refused({ url: detailInit.url, method: 'POST', body: detailInit.body })).toBe(false);
    // The URL's query and fragment: this plan declares neither, so neither may appear.
    expect(refused({ ...detailInit, url: `${DETAIL_URL}?a=1` })).toBe(false);
    expect(refused({ ...detailInit, url: `${DETAIL_URL}#frag` })).toBe(false);
    // An over-long body is refused by the byte ceiling, not by a key check.
    expect(refused({ ...detailInit, body: JSON.stringify({ chat_id: 'x'.repeat(5000) }) })).toBe(false);
  });

  it('the URL allowlist is the same decision, so the two cannot tell different stories', () => {
    // No method ⇒ the segment's default, which for both of kimi's segments is POST, so neither
    // path may be sent as a bare URL.
    expect(isAllowedBackfillUrl(DETAIL_URL, ORIGIN)).toBe(false);
    expect(isAllowedBackfillUrl(LIST_URL, ORIGIN)).toBe(false);
    // A kimi path that is not one of the two is refused here as well.
    expect(isAllowedBackfillUrl(`${ORIGIN}/apiv2/kimi.gateway.feed.v1.FeedService/ListFeeds`, ORIGIN)).toBe(false);
  });
});
