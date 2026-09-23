/**
 * 🔴 W64c · **A Gemini 400 is a login refusal only when the wrapper says so.**
 *
 * W64b made a Gemini 400 halt `auth-refused`, on the strength of a real measurement
 * (the wrapper's own header: a `batchexecute` request whose `at` is missing or stale
 * is answered HTTP 400). The re-review found the rule was applied to **every** Gemini
 * 400 — including one nothing had been measured about, such as a malformed batch this
 * leg built itself — because the classifier inferred the credential story from the
 * status and the platform id alone (`nm/R64b-grok.log`, finding 2: "The classifier
 * does not require that this 400 survived `createGeminiAuthorizedFetch`'s re-read.
 * Any Gemini 400 is mapped").
 *
 * So the fact is now **carried** rather than inferred. Gemini's wrapper is the only
 * code that knows whether a refusal came back from its credential path (it read the
 * page's `at`, and re-read it once before accepting the refusal as the answer), and
 * it marks the answer it returns. The mark travels the whole transport — wrapper →
 * content script → message → `tabHttpPort` → engine — and the classifier requires it:
 *
 *   · a Gemini 400 **with** the mark ⇒ `auth-refused` (transient, as W64b intended);
 *   · a Gemini 400 **without** it ⇒ the pre-W64b answer, `shape-changed`;
 *   · every other status, and every other platform, untouched.
 *
 * The mark is a fact about **evidence**, not about the status: `401` is still an auth
 * refusal on every platform without any mark, because a 401 states the credential
 * itself.
 *
 * 🔴 W64d · The wrapper’s answer is `{ response, survivedCredentialReread }` — two
 *    things side by side — and these cases read the fact and the status off it. Their
 *    stubs are plain objects, which is enough for a rule about *classification*; the
 *    response that carries a real `Response` through this path is
 *    `w64d-gemini-response-identity.test.ts`.
 */

import { describe, it, expect } from 'vitest';
import { PLATFORMS } from '../lib/contract';
import { runBackfill } from '../lib/backfill/engine';
import { haltClassOf } from '../lib/backfill/types';
import { memoryStore } from '../lib/backfill/store';
import { GEMINI_PLAN, listRequestInit, listTokenPostInit } from '../lib/backfill/enumerate';
import {
  createGeminiAuthorizedFetch,
  type MinimalResponse,
} from '../lib/platform-auth';
import {
  BACKFILL_FETCH_MESSAGE,
  serveBackfillFetch,
  tabHttpPort,
} from '../lib/backfill/tab-port';
import type { GeminiBootstrapTokens } from '../lib/contract';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://gemini.google.com';
const OTHER_ORIGIN = 'https://chatgpt.com';
const AT_TOKEN = 'synthetic-at-token-value';
const GEMINI_LIST_PAGE_SIZE = 10;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock {
  let t = Date.parse('2026-09-23T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

const sink = (): { saved: true } => ({ saved: true });

function tokens(over: Partial<GeminiBootstrapTokens> = {}): GeminiBootstrapTokens {
  return { at: AT_TOKEN, bl: 'synthetic-build-label', fSid: '1', ...over };
}

/** The list segment only — the classifier is what these tests are about. */
function runGemini(status: number, survivedCredentialReread?: boolean) {
  const row = PLATFORMS.find((p) => p.id === 'gemini')!;
  const http = async (): Promise<Record<string, unknown>> => {
    const res: Record<string, unknown> = { status, text: 'synthetic refusal' };
    if (survivedCredentialReread !== undefined) res.survivedCredentialReread = survivedCredentialReread;
    return res;
  };
  return runBackfill({
    platform: 'gemini',
    origin: row.origins[0]!,
    scope: 'w64c',
    store: memoryStore(),
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink,
  });
}

// ---------------------------------------------------------------------------
// 1 · The classifier requires the evidence
// ---------------------------------------------------------------------------

describe('W64c-1 · a Gemini 400 without the wrapper’s fact is not called a login refusal', () => {
  it('🔴 a bare Gemini 400 is shape-changed — the pre-W64b answer — not auth-refused', async () => {
    const report = await runGemini(400);
    // 🔴 The defect this pins: a 400 nothing is known about (a malformed batch this leg
    //    built, a plan body the platform refused as malformed) was reported to the user as
    //    "your login was refused" and retried on the 30 min / 2 h ladder.
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.reason).not.toBe('auth-refused');
    expect(report.stopped).toBe('halted');
  });

  it('🔴 the same 400 WITH the wrapper’s fact is auth-refused, transient — W64b, kept', async () => {
    const report = await runGemini(400, true);
    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.stopped).toBe('waiting-retry');
    expect(haltClassOf(report.halted!.reason)).toBe('transient');
    // The trace still says which HTTP answer this was.
    expect(report.halted?.detail).toContain('400');
  });

  it('the mark is Gemini’s alone: a Kimi 400 carrying it is still shape-changed', async () => {
    const row = PLATFORMS.find((p) => p.id === 'kimi')!;
    const report = await runBackfill({
      platform: 'kimi',
      origin: row.origins[0]!,
      scope: 'w64c',
      store: memoryStore(),
      http: (async () => ({ status: 400, text: 'synthetic refusal', survivedCredentialReread: true })) as never,
      clock: fakeClock(),
      pace: NO_WAIT,
      sink,
    });
    expect(report.halted?.reason).toBe('shape-changed');
  });

  it('nothing else about the classification moved', async () => {
    // 401 states the credential itself: an auth refusal with or without the mark.
    expect((await runGemini(401)).halted?.reason).toBe('auth-refused');
    expect((await runGemini(401, true)).halted?.reason).toBe('auth-refused');
    // The "not now" ladder and a genuine wire fact are unchanged either way.
    expect((await runGemini(403, true)).halted?.reason).toBe('rate-limited');
    expect((await runGemini(429, true)).halted?.reason).toBe('rate-limited');
    expect((await runGemini(500, true)).halted?.reason).toBe('rate-limited');
    expect((await runGemini(404, true)).halted?.reason).toBe('shape-changed');
  });
});

// ---------------------------------------------------------------------------
// 2 · The wrapper is the only thing that sets it
// ---------------------------------------------------------------------------

describe('W64c-2 · Gemini’s wrapper marks the refusals its credential path produced', () => {
  const listUrl = GEMINI_PLAN.listUrl(ORIGIN, 0, GEMINI_LIST_PAGE_SIZE);
  const listBody = listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body!;

  it('🔴 a 400 that survived the credential re-read carries the mark', async () => {
    const seen: string[] = [];
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (): Promise<MinimalResponse> => {
        seen.push('call');
        return { status: 400, text: async () => 'synthetic refusal' };
      },
      { readTokens: async () => tokens(), language: null },
    );
    const answer = await authorized(listUrl, { method: 'POST', body: listBody });
    expect(answer.response.status).toBe(400);
    // Two attempts: the credential was re-read and the refusal is the retried request's.
    expect(seen).toHaveLength(2);
    expect(answer.survivedCredentialReread).toBe(true);
  });

  it('🔴 a 400 with no credential at all is marked too, and still costs one request', async () => {
    // 🔴 The measured shape the wrapper's header records: a request whose `at` is blank
    //    is answered 400. The wrapper re-reads the page's credential once before
    //    accepting that answer — if the page has one now, the request is retried with
    //    it; if not, the refusal stands and is marked as the credential statement it is.
    const seen: string[] = [];
    let reads = 0;
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (): Promise<MinimalResponse> => {
        seen.push('call');
        return { status: 400, text: async () => 'synthetic refusal' };
      },
      { readTokens: async () => { reads += 1; return tokens({ at: null }); }, language: null },
    );
    const answer = await authorized(listUrl, { method: 'POST', body: listBody });
    expect(answer.response.status).toBe(400);
    expect(seen).toHaveLength(1);
    expect(reads).toBe(2);
    expect(answer.survivedCredentialReread).toBe(true);
  });

  it('a signed-in page whose re-read turns up a token is retried, not refused', async () => {
    // The re-read is not a formality: if the page has a credential by the time it
    // happens, the request goes out again with it.
    const sent: string[] = [];
    let reads = 0;
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (_url, init): Promise<MinimalResponse> => {
        sent.push(new URLSearchParams(String(init.body)).get('at') ?? '');
        return { status: sent.length === 1 ? 400 : 200, text: async () => 'synthetic' };
      },
      {
        readTokens: async () => { reads += 1; return tokens({ at: reads === 1 ? null : AT_TOKEN }); },
        language: null,
      },
    );
    const answer = await authorized(listUrl, { method: 'POST', body: listBody });
    expect(answer.response.status).toBe(200);
    expect(sent).toEqual(['', AT_TOKEN]);
    expect(answer.survivedCredentialReread).toBe(true);
  });

  it('a request the wrapper does not credential is never marked', async () => {
    const other = `${ORIGIN}/_/BardChatUi/data/other?x=1`;
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (): Promise<MinimalResponse> => ({ status: 400, text: async () => 'synthetic refusal' }),
      { readTokens: async () => tokens(), language: null },
    );
    expect((await authorized(other, { method: 'POST', body: 'a=b' })).survivedCredentialReread).toBeUndefined();
  });

  it('a 200 on the credentialed path is not marked — there is no refusal to explain', async () => {
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (): Promise<MinimalResponse> => ({ status: 200, text: async () => 'synthetic' }),
      { readTokens: async () => tokens(), language: null },
    );
    expect((await authorized(listUrl, { method: 'POST', body: listBody })).survivedCredentialReread).toBeUndefined();
  });

  it('a 401 is passed through and marked: the platform stated the credential itself', async () => {
    const authorized = createGeminiAuthorizedFetch(
      ORIGIN,
      async (): Promise<MinimalResponse> => ({ status: 401, text: async () => 'synthetic refusal' }),
      { readTokens: async () => tokens(), language: null },
    );
    const answer = await authorized(listUrl, { method: 'POST', body: listBody });
    expect(answer.response.status).toBe(401);
    expect(answer.survivedCredentialReread).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 3 · It survives the trip from the page to the engine
// ---------------------------------------------------------------------------

describe('W64c-3 · the fact crosses the transport, and its absence stays absence', () => {
  it('serveBackfillFetch carries it into the reply, and tabHttpPort onto the response', async () => {
    const marked = await serveBackfillFetch(
      { url: GEMINI_PLAN.listUrl(ORIGIN, 0, GEMINI_LIST_PAGE_SIZE), method: 'POST', body: listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body!, contentType: 'application/x-www-form-urlencoded' },
      ORIGIN,
      (async () => ({ status: 400, text: async () => 'synthetic refusal', survivedCredentialReread: true })) as never,
    );
    expect(marked).toMatchObject({ ok: true, status: 400, survivedCredentialReread: true });

    const port = tabHttpPort(7, async (_id, msg) => {
      // The reply the content script would give for that fetch.
      expect(msg).toMatchObject({ type: BACKFILL_FETCH_MESSAGE });
      return marked as never;
    });
    const res = await port(GEMINI_PLAN.listUrl(ORIGIN, 0, GEMINI_LIST_PAGE_SIZE), {
      method: 'POST',
      body: listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body!,
      contentType: 'application/x-www-form-urlencoded',
    });
    expect(res.status).toBe(400);
    expect(res.survivedCredentialReread).toBe(true);
  });

  it('a reply with no such fact produces a response with no such field', async () => {
    const port = tabHttpPort(8, async () => ({ ok: true, status: 400, text: 'synthetic refusal' }));
    const res = await port(`${OTHER_ORIGIN}/backend-api/conversation`);
    expect(res.status).toBe(400);
    expect('survivedCredentialReread' in res).toBe(false);
  });
});
