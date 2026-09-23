/**
 * W64 · An HTTP **401** on a backfill request is a credential refusal, and it must
 * never be recorded as `shape-changed`.
 *
 * ## The measured defect
 * A `www.kimi.com` tab that was open and logged in held a `localStorage.access_token`
 * whose JWT `exp` had passed **about 15 hours** earlier. The list request
 * `KIMI_PLAN` builds — its own body, every header `createKimiAuthorizedFetch`
 * produces, `authorization: Bearer <that token>` — answered **HTTP 401** with
 * `{ code: "unauthenticated", message: "invalid user token: token has invalid claims:
 * token is expired" }`; the cookie-only form answered 401 with `code` and no
 * `message`. Measured from the page's own context in a logged-in Chrome, 2026-09-23 —
 * the probe table is in the W64 report. No key had been renamed and no header was
 * missing.
 *
 * The engine turned that 401 into **`shape-changed`**: "the platform changed its
 * format, wait for a fix". That is a **permanent** record, so the leg never asked
 * again, and the platform had said in as many words that the credential it was
 * handed had expired.
 *
 * ## What this file exists to stop changing back
 *  1. **A 401 halts `auth-refused`, on every segment and every plan** — never
 *     `shape-changed`, and never `rate-limited`.
 *  2. **It is transient**, so the leg comes back by itself and a token the page has
 *     since refreshed is picked up without a human editing storage.
 *  3. **A refusal still records nothing.** No debt archived, nothing settled, and the
 *     ids the list had already produced stay pending exactly as they were.
 *  4. **What deliberately did not move**: 403 is still `rate-limited` (the ladder the
 *     evidence supports) and a 404/500 is still what it was. The fix is scoped to the
 *     one status that is unambiguously a credential statement.
 *
 * 🔴 All fixtures are **synthetic**, written from the key names the probe measured.
 *    No request goes to kimi.com, there is no logged-in state, and no real
 *    conversation, account, token or id appears anywhere below. The http port is
 *    always injected and throws on any path or call it was not given, so "no further
 *    request was sent" is proven by the run rather than asserted about it.
 */

import { describe, expect, it } from 'vitest';
import { PLATFORMS } from '../lib/contract';
import { runBackfill, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  KIMI_CHAT_FEED_TYPE,
  KIMI_DETAIL_PATH,
  KIMI_LIST_PATH,
} from '../lib/backfill/enumerate';
import { haltClassOf, TRANSIENT_RETRY_BASE_MS } from '../lib/backfill/types';
import { t } from '../lib/i18n';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://www.kimi.com';
const ID = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
const PAGE_URL = `${ORIGIN}/chat/${ID}`;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

/** A fake clock that advances on sleep, so a run's timing is assertable rather than sampled. */
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

/** One page of the list; `nextPageToken` absent ⇒ last page, as the probe measured. */
function feedPage(items: unknown[]): string {
  return JSON.stringify({ items });
}

/** The measured **cookie-only** refusal body: `code` present, **no `message` field at all**. */
const COOKIE_ONLY_401 = JSON.stringify({ code: 'unauthenticated', details: ['synthetic'] });

/** The measured **expired-token** refusal body: the same code, plus the platform's own sentence. */
const EXPIRED_TOKEN_401 = JSON.stringify({
  code: 'unauthenticated',
  message: 'invalid user token: token has invalid claims: token is expired',
  details: ['synthetic'],
});

interface Recorder {
  calls: { url: string; init: unknown; at: number }[];
}

/**
 * A synthetic backend. Any path it was not given **throws**, so "this leg sent no
 * further request" is proven by the run rather than claimed.
 *
 * 🔴 `HttpResponse.text` is the body **as a string**, not a reader: the engine is
 *    handed bytes it has already read (see the interface in engine.ts). Returning a
 *    function here compiles only because the http port is cast at the `runBackfill`
 *    call, which is exactly why this file now asserts against the real shape.
 */
function backend(
  clock: Clock,
  routes: Partial<Record<string, string | ((u: URL) => { status: number; text: string })>>,
): Recorder & { http: (url: string, init?: unknown) => Promise<HttpResponse> } {
  const calls: { url: string; init: unknown; at: number }[] = [];
  const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
    calls.push({ url, init, at: clock.now() });
    const u = new URL(url);
    const route = routes[u.pathname];
    if (route === undefined) throw new Error(`unexpected path ${u.pathname}`);
    if (typeof route === 'function') return route(u);
    return { status: 200, text: route };
  };
  return { calls, http };
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: (url: string, init?: unknown) => Promise<HttpResponse>,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'kimi',
    origin: ORIGIN,
    scope: 'w64',
    store,
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink: (captured): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
    ...extra,
  });
}

// ---------------------------------------------------------------------------
// 1 · The defect itself: a 401 is an auth refusal
// ---------------------------------------------------------------------------

describe('W64-1 · a 401 halts auth-refused on the list segment', () => {
  it('the measured kimi 401: auth-refused, transient, nothing read, nothing archived', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      // The refusal arrives as a status, which is how the leg sees it in a browser.
      [KIMI_LIST_PATH]: () => ({ status: 401, text: EXPIRED_TOKEN_401 }),
      // Given on purpose: a leg that read the refusal as an empty list WOULD go on to
      // fetch bodies, and this route would let it. That is the mistake being excluded.
      [KIMI_DETAIL_PATH]: () => ({ status: 200, text: '{"messages":[]}' }),
    });
    const report = await run(memoryStore(), be.http, { clock });

    // 🔴 `waiting-retry`, not `halted`: a transient stop is the leg saying it will
    //    come back by itself, and that is the half of this fix a permanent record
    //    could not say.
    expect(report.stopped).toBe('waiting-retry');
    // 🔴 The whole of this file, in one line: not `shape-changed`.
    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.halted?.reason).not.toBe('shape-changed');
    // The trace keeps the status, so "which HTTP answer was this" survives.
    expect(report.halted?.detail).toContain('401');
    // 🔴 Transient: the leg comes back by itself. A permanent record would never ask
    //    again, which is how a temporary logout froze the platform.
    expect(haltClassOf(report.halted!.reason)).toBe('transient');
    expect(report.halted?.retryAt).toBeGreaterThan(clock.now());
    expect(report.halted?.attempts).toBe(1);

    // A refusal is not a result: nothing was counted, settled, or written off.
    expect(report.state.pending).toEqual([]);
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.enumTruncated).toBeNull();
    expect(report.archivedThisRun).toEqual([]);
    // Exactly one request went out, and it was the list.
    expect(be.calls).toHaveLength(1);
    expect(new URL(be.calls[0]!.url).pathname).toBe(KIMI_LIST_PATH);
  });

  it('the refusal body is not read for the reason: a 401 with no `message` halts the same way', async () => {
    // 🔴 The cookie-only answer the probe measured carries `code` and `details` and
    //    **no `message` field at all**, and it must land on the same reason. The
    //    classification is the status's, not the body's: this build cannot tell
    //    "you sent no token" from "you sent an expired one" out of the envelope, and
    //    it must not guess between them.
    const clock = fakeClock();
    const be = backend(clock, { [KIMI_LIST_PATH]: () => ({ status: 401, text: COOKIE_ONLY_401 }) });
    const report = await run(memoryStore(), be.http, { clock });

    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.halted?.detail).toContain('401');
  });
});

describe('W64-2 · a 401 on the body segment is the same refusal', () => {
  it('halts auth-refused, and the ids the list produced stay pending', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [KIMI_LIST_PATH]: feedPage([chatItem(ID)]),
      [KIMI_DETAIL_PATH]: () => ({ status: 401, text: EXPIRED_TOKEN_401 }),
    });
    const report = await run(memoryStore(), be.http, { clock });

    expect(report.stopped).toBe('waiting-retry');
    expect(report.halted?.reason).toBe('auth-refused');
    // The detail names the segment, which is the one thing the popup sentence
    // deliberately does not (it is printed for both segments).
    expect(report.halted?.detail).toContain('detail');
    expect(report.halted?.detail).toContain('401');
    // 🔴 A refused body is not a stored body and not a settled debt: the list had
    //    already been read, and every id it produced is still owed.
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.pending).toEqual([ID]);
  });
});

describe('W64-3 · the status is classified the same way whichever plan is driving', () => {
  /**
   * Every platform whose plan can build a list request without a resolved account
   * scope. claude.ai is excluded for that structural reason, not to hide it: its
   * plans carry `{org}` and the leg stops at `org-unresolved` **before** a request,
   * so no status ever reaches this function for it.
   */
  const DRIVABLE = ['chatgpt', 'perplexity', 'kimi', 'gemini', 'grok'] as const;

  for (const platform of DRIVABLE) {
    it(`${platform}: a 401 on the list is auth-refused, never shape-changed`, async () => {
      const clock = fakeClock();
      const row = PLATFORMS.find((p) => p.id === platform)!;
      const origin = row.origins[0]!;
      const calls: string[] = [];
      const http = async (url: string): Promise<HttpResponse> => {
        calls.push(url);
        return { status: 401, text: COOKIE_ONLY_401 };
      };
      const report = await runBackfill({
        platform,
        origin,
        scope: 'w64',
        store: memoryStore(),
        http: http as never,
        clock,
        pace: NO_WAIT,
        sink: (captured): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
      });

      expect(report.stopped).toBe('waiting-retry');
      expect(report.halted?.reason).toBe('auth-refused');
      expect(report.state.pending).toEqual([]);
      // The leg stopped at the first request; it did not page on through the refusal.
      expect(calls).toHaveLength(1);
    });
  }
});

describe('W64-4 · what deliberately did not move', () => {
  /** Kimi's list, answering with one status. */
  async function listStatus(status: number) {
    const clock = fakeClock();
    const be = backend(clock, {
      [KIMI_LIST_PATH]: () => ({ status, text: COOKIE_ONLY_401 }),
    });
    return run(memoryStore(), be.http, { clock });
  }

  it('403 is still rate-limited — the ladder the evidence supports, and 401 is not folded into it', async () => {
    // 🔴 Stated as a decision, not left implicit. 429/403/5xx-as-"not now" is the
    //    reading this ladder was built on and no platform this leg drives has been
    //    measured answering 403 for a credential reason. The two are different facts
    //    about why the platform said no, so they keep two reasons and two rungs.
    const report = await listStatus(403);
    expect(report.halted?.reason).toBe('rate-limited');
    expect(report.halted?.reason).not.toBe('auth-refused');
    expect(haltClassOf(report.halted!.reason)).toBe('transient');
    // The two rungs are genuinely different, which is the point of not folding them.
    expect(TRANSIENT_RETRY_BASE_MS['rate-limited']).not.toBe(TRANSIENT_RETRY_BASE_MS['auth-refused']);
  });

  it('429 and 500 are still rate-limited', async () => {
    expect((await listStatus(429)).halted?.reason).toBe('rate-limited');
    expect((await listStatus(500)).halted?.reason).toBe('rate-limited');
  });

  it('a 404 is still shape-changed — a wrong route really is a wire fact', async () => {
    // The fix is scoped to the one status that is unambiguously a credential
    // statement. A 404 is the case this whole table was written for: the plan's
    // route does not exist, and a human has to read it.
    const report = await listStatus(404);
    expect(report.halted?.reason).toBe('shape-changed');
    expect(haltClassOf(report.halted!.reason)).toBe('permanent');
  });
});

describe('W64-5 · the user is told to sign in, not that the API changed', () => {
  it('auth-refused has its own sentence, and it names the login', () => {
    const sentence = t('popup.notes.halted.authRefused', {
      platform: 'kimi',
      detail: 'kimi list token=first-page returned HTTP 401',
      attempts: 1,
      minutes: 30,
    });
    expect(sentence).toContain('kimi');
    // The two sentences it must not be: the record-describing fallback, and the one
    // that promises a wait without naming the action.
    expect(sentence).not.toBe(t('popup.notes.halted.other', {
      reason: 'auth-refused', detail: 'kimi list returned HTTP 401',
    }));
    expect(sentence).not.toBe(t('popup.notes.halted.waitingRetry', {
      reason: 'auth-refused', attempts: 1, minutes: 30, detail: 'kimi list returned HTTP 401',
    }));
    // "the fix is to open kimi and sign in again" — the action a 401 has.
    expect(sentence.toLowerCase()).toContain('sign in');
    // And it does not claim the platform changed, which is what the old reason said.
    expect(sentence.toLowerCase()).not.toContain('api changed');
  });
});
