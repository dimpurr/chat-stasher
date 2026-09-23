/**
 * W64b · the two defects the review of `d6f0f67` found in the Kimi 401 work.
 *
 * ## 1 · The Kimi bearer rode along on a redirect
 * `createKimiAuthorizedFetch` attaches `authorization: Bearer <the page's own
 * access_token>` and forwarded `init` with no `redirect`, so the browser default
 * (`follow`) applied. A **same-origin** 30x keeps the `Authorization` header (only a
 * cross-origin hop drops it), so a redirect from `KIMI_LIST_PATH` or
 * `KIMI_DETAIL_PATH` re-sent the page's own bearer to whatever path the platform
 * named. `checkBackfillRequest` cannot help: it approved the request that was sent,
 * not the one the platform redirected it to.
 *
 * `lib/platform-auth.ts` already treats that as a token leak for DeepSeek and
 * fetches those two paths with `redirect: 'manual'` (`w61b-refusal-review.test.ts`,
 * W61b-4). Kimi had no equivalent. This file pins the same two properties for Kimi,
 * because "the same rule, applied to one platform and not the other" is exactly the
 * shape of defect a second review finds.
 *
 * ## 2 · A logged-out Gemini was permanent
 * Gemini answers **HTTP 400** to a `batchexecute` request whose `at` is missing or
 * stale — the measured shape of "not signed in", and the whole reason
 * `createGeminiAuthorizedFetch` exists rather than "try cookies and see". That 400
 * reached `haltReasonForStatus`, which classified every 400 as `shape-changed`:
 * **permanent**, so the leg never asked again and nothing in the product clears such
 * a record. A user who was signed out when the leg ran stayed stopped after signing
 * back in.
 *
 * The evidence that this 400 is an auth refusal is this platform's own, not a
 * general claim about 400s: the wrapper's header records the measurement, and the
 * wrapper's own retry is built on it (it re-reads `at` and retries **on 400**, which
 * would be pointless if a 400 meant "malformed request"). So the mapping is scoped
 * to Gemini — every other platform keeps 400 ⇒ `shape-changed`, because there is no
 * measurement separating "auth" from "malformed" on them, and inventing one would
 * swallow every genuinely malformed request into a "sign in" message.
 *
 * 🔴 All fixtures are **synthetic**: invented tokens, invented ids, invented bodies.
 *    No request goes to kimi.com or gemini.google.com, there is no logged-in state,
 *    and no real conversation, account, token or id appears anywhere below. The http
 *    port is always injected and throws on a path it was not given, so "no request
 *    was sent" is proven by the run rather than asserted about it.
 */

import { describe, expect, it } from 'vitest';
import { PLATFORMS } from '../lib/contract';
import { runBackfill, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { KIMI_DETAIL_PATH, KIMI_LIST_PATH } from '../lib/backfill/enumerate';
import { haltClassOf } from '../lib/backfill/types';
import {
  createKimiAuthorizedFetch,
  type MinimalResponse,
} from '../lib/platform-auth';
import type { Clock } from '../lib/backfill/pace';

const KIMI_ORIGIN = 'https://www.kimi.com';
const KIMI_LIST_URL = `${KIMI_ORIGIN}${KIMI_LIST_PATH}`;
const KIMI_DETAIL_URL = `${KIMI_ORIGIN}${KIMI_DETAIL_PATH}`;
/** A path on the same origin that this wrapper does not own. */
const KIMI_OTHER_URL = `${KIMI_ORIGIN}/apiv2/kimi.gateway.chat.v1.ChatService/SendMessage`;

/** Synthetic. Invented here; it names no account and is not a real token. */
const TOKEN = 'synthetic.kimi.jwt.token';

/** What a Kimi list/detail `fetch` resolves to when the platform redirects, under `redirect: 'manual'`. */
function opaqueRedirect(): MinimalResponse {
  return { status: 0, text: async () => '' };
}

function ok(status = 200): MinimalResponse {
  return { status, text: async () => '{"synthetic":true}' };
}

function headersOf(call: { init: RequestInit }): Record<string, string> {
  return (call.init.headers ?? {}) as Record<string, string>;
}

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
// 1 · The Kimi bearer is never re-sent by a redirect
// ---------------------------------------------------------------------------

describe('W64b-1 · Kimi\'s bearer is not re-sent to a path the allowlist never approved', () => {
  it('🔴 Kimi\'s two paths are fetched with redirect: manual, and other paths are untouched', async () => {
    const calls: { url: string; init: RequestInit }[] = [];
    const rawFetch = async (url: string, init: RequestInit): Promise<MinimalResponse> => {
      calls.push({ url, init });
      return ok();
    };
    const fetchWithToken = createKimiAuthorizedFetch(KIMI_ORIGIN, rawFetch, {
      readToken: () => TOKEN,
      language: null,
    });
    const other = { method: 'POST' } as RequestInit;

    await fetchWithToken(KIMI_LIST_URL, { method: 'POST', body: '{}' });
    await fetchWithToken(KIMI_DETAIL_URL, { method: 'POST', body: '{}' });
    await fetchWithToken(KIMI_OTHER_URL, other);

    // 🔴 Before this fix `redirect` was left at the browser's default (`follow`), so a
    //    same-origin 30x from either endpoint re-sent the page's own bearer to
    //    wherever it pointed.
    expect(calls[0]!.init.redirect).toBe('manual');
    expect(calls[1]!.init.redirect).toBe('manual');
    // A path this wrapper does not own is not touched at all — not even to add a
    // redirect policy: the caller's own init object is passed through.
    expect(calls[2]!.init).toBe(other);
    expect(calls[2]!.init.redirect).toBeUndefined();
    // The token still rides on the one request that was sent, and only there.
    expect(headersOf(calls[0]!).authorization).toBe(`Bearer ${TOKEN}`);
    expect(headersOf(calls[2]!).authorization).toBeUndefined();
  });

  it('🔴 an unfollowed redirect is a failure with a trace, not a response that gets read', async () => {
    let calls = 0;
    const rawFetch = async (): Promise<MinimalResponse> => {
      calls += 1;
      return opaqueRedirect();
    };
    const fetchWithToken = createKimiAuthorizedFetch(KIMI_ORIGIN, rawFetch, {
      readToken: () => TOKEN,
      language: null,
    });

    // 🔴 Status 0 handed back as a response is read by the engine as `HTTP 0`, which
    //    its status branch calls a wire-shape change — permanent, over a redirect
    //    that may be a login page today and absent tomorrow. It is a transport fact:
    //    said out loud, and retried like one.
    await expect(fetchWithToken(KIMI_LIST_URL, { method: 'POST', body: '{}' }))
      .rejects.toThrow(/redirect/i);
    // Not retried: re-reading the token cannot change an answer that was never read.
    expect(calls).toBe(1);
  });

  it('the refusal names the path and never the token or a query value', async () => {
    const rawFetch = async (): Promise<MinimalResponse> => opaqueRedirect();
    const fetchWithToken = createKimiAuthorizedFetch(KIMI_ORIGIN, rawFetch, {
      readToken: () => TOKEN,
      language: null,
    });
    const error = await fetchWithToken(KIMI_LIST_URL, { method: 'POST', body: '{}' })
      .then(() => null, (err: Error) => err);

    expect(error).toBeInstanceOf(Error);
    expect(error!.message).toContain(KIMI_LIST_PATH);
    // 🔴 A halt detail is persisted and shown. No credential, and no JWT, in it.
    expect(error!.message).not.toContain(TOKEN);
    expect(error!.message.split(' ').some((word) => word.startsWith('eyJ'))).toBe(false);
  });

  it('every other answer is passed through untouched, so no existing status handling moved', async () => {
    // A 200 is still a 200 …
    const accepted = createKimiAuthorizedFetch(KIMI_ORIGIN, async () => ok(200), {
      readToken: () => TOKEN,
      language: null,
    });
    expect((await accepted(KIMI_LIST_URL, { method: 'POST', body: '{}' })).status).toBe(200);

    // … and a 401 is still the platform's own answer: read once more and retried once,
    // which is the behaviour the guard must not have swallowed.
    const seen: number[] = [];
    const queue = createKimiAuthorizedFetch(KIMI_ORIGIN, async () => {
      seen.push(seen.length);
      return seen.length === 1 ? ok(401) : ok(200);
    }, { readToken: () => TOKEN, language: null });
    expect((await queue(KIMI_DETAIL_URL, { method: 'POST', body: '{}' })).status).toBe(200);
    expect(seen).toHaveLength(2);
  });

  it('the redirect policy is set whether or not a token was found', async () => {
    // 🔴 Same rule as DeepSeek's: the request is built the same way either way, so a
    //    trace cannot read as two different requests depending on whether the user
    //    happened to be signed in.
    for (const token of [TOKEN, null]) {
      const calls: { init: RequestInit }[] = [];
      const fetchWithToken = createKimiAuthorizedFetch(KIMI_ORIGIN, async (_url, init) => {
        calls.push({ init });
        return ok();
      }, { readToken: () => token, language: null });
      await fetchWithToken(KIMI_LIST_URL, { method: 'POST', body: '{}' });
      expect(calls[0]!.init.redirect).toBe('manual');
    }
  });
});

// ---------------------------------------------------------------------------
// 2 · A logged-out Gemini comes back by itself
// ---------------------------------------------------------------------------

/**
 * A synthetic backend over one status. Any path it was not given throws.
 *
 * 🔴 W64c · For a **refusal** it answers the way Gemini's wrapper answers — with the
 *    credential fact attached, and only for the two statuses the wrapper marks (400
 *    and 401). That is not decoration: since W64c the classifier requires the fact
 *    before it will call a 400 a refused login, so a stub that omitted it would be
 *    modelling a response no page produces for these paths and would be testing the
 *    "no evidence" answer instead of this suite's subject. Every assertion below is
 *    unchanged; what changed is that the stub stands for the transport it replaces.
 */
function geminiBackend(status: number) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(new URL(url).pathname);
    return status === 400 || status === 401
      ? { status, text: 'synthetic refusal', survivedCredentialReread: true }
      : { status, text: 'synthetic refusal' };
  };
  return { calls, http };
}

function runGemini(
  http: (url: string) => Promise<HttpResponse>,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  const row = PLATFORMS.find((p) => p.id === 'gemini')!;
  return runBackfill({
    platform: 'gemini',
    origin: row.origins[0]!,
    scope: 'w64b',
    store: memoryStore(),
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink: (captured): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
    ...extra,
  });
}

describe('W64b-2 · a Gemini 400 is a login refusal, and is transient', () => {
  it('🔴 the measured logged-out 400 halts auth-refused, not shape-changed', async () => {
    const be = geminiBackend(400);
    const report = await runGemini(be.http);

    // 🔴 `waiting-retry`, not `halted`: the leg says it will come back by itself,
    //    which is the half of this fix a permanent record cannot say. A user who
    //    signs in afterwards is picked up on the next rung without touching storage.
    expect(report.stopped).toBe('waiting-retry');
    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.halted?.reason).not.toBe('shape-changed');
    // The trace keeps the status, so "which HTTP answer was this" survives.
    expect(report.halted?.detail).toContain('400');
    expect(haltClassOf(report.halted!.reason)).toBe('transient');
    expect(report.halted?.retryAt).toBeGreaterThan(Date.parse('2026-09-23T00:00:00.000Z'));

    // A refusal is not a result: nothing was counted, settled, or written off.
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.pending).toEqual([]);
    expect(report.state.enumCursor.complete).toBe(false);
  });

  it('🔴 the fix is scoped to Gemini: every other platform keeps 400 ⇒ shape-changed', async () => {
    // 🔴 Stated as a decision rather than left implicit. On Gemini the 400 is measured
    //    and documented as "the token was missing or stale", and the wrapper's own
    //    retry is built on that reading. No other platform this leg drives has such a
    //    measurement, and a blanket `400 ⇒ auth-refused` would swallow every genuinely
    //    malformed request into a "sign in" message that names no real remedy.
    const DRIVABLE = ['chatgpt', 'perplexity', 'kimi', 'grok'] as const;
    for (const platform of DRIVABLE) {
      const row = PLATFORMS.find((p) => p.id === platform)!;
      const report = await runBackfill({
        platform,
        origin: row.origins[0]!,
        scope: 'w64b',
        store: memoryStore(),
        http: (async () => ({ status: 400, text: 'synthetic refusal' })) as never,
        clock: fakeClock(),
        pace: NO_WAIT,
        sink: (captured): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
      });
      expect(report.halted?.reason, platform).toBe('shape-changed');
      expect(haltClassOf(report.halted!.reason), platform).toBe('permanent');
    }
  });

  it('what deliberately did not move on Gemini either', async () => {
    // 401 is already an auth refusal; 403/429/500 are the "not now" ladder; a 404 is a
    // wire fact. None of them is touched by this change.
    expect((await runGemini(geminiBackend(401).http)).halted?.reason).toBe('auth-refused');
    expect((await runGemini(geminiBackend(403).http)).halted?.reason).toBe('rate-limited');
    expect((await runGemini(geminiBackend(429).http)).halted?.reason).toBe('rate-limited');
    expect((await runGemini(geminiBackend(500).http)).halted?.reason).toBe('rate-limited');
    const notFound = await runGemini(geminiBackend(404).http);
    expect(notFound.halted?.reason).toBe('shape-changed');
    expect(haltClassOf(notFound.halted!.reason)).toBe('permanent');
  });

  it('the leg stops at the refusal instead of paging on through it', async () => {
    const be = geminiBackend(400);
    await runGemini(be.http);
    expect(be.calls).toHaveLength(1);
  });
});
