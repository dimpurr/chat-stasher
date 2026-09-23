/**
 * 🔴 W64d · **Gemini’s credential fact rides on the response, not on a copy of it.**
 *
 * W64c attached `survivedCredentialReread` by spreading the response —
 * `{ ...first, survivedCredentialReread: true }` (`lib/platform-auth.ts`). What the
 * wrapper is handed in production is a real `Response`: `status` is a prototype
 * accessor and `text` a prototype method, so neither is an own enumerable property and
 * the spread produced `{ survivedCredentialReread: true }` and nothing else.
 *
 * The damage was not a wrong classification, it was the loss of the response. The page
 * side hands the wrapper’s answer to `serveBackfillFetch`, which calls `res.text()`
 * (`lib/backfill/tab-port.ts`); on the copy that is `undefined`, the call throws, the
 * `catch` turns it into `{ok: false}`, `tabHttpPort` throws on that, and the engine
 * records `transport-error`. **Every** Gemini answer on the credential path collapsed
 * into that one halt:
 *
 *   · the 400 that should be `auth-refused`;
 *   · the 401, which states the credential itself and needs no mark at all;
 *   · and the retry that came back **200** — a healthy response recorded as a broken
 *     transport, with the page it carried thrown away.
 *
 * W64c’s tests stayed green because every stub was a plain `{ status, text }` object —
 * a shape no browser produces for `fetch`. **Every response in this file is a real
 * `Response`**, which is what the wrapper receives, and the chain below is assembled
 * the way `entrypoints/dw-bridge.content.ts` assembles it.
 *
 * What the fix changed is the *shape of the wrapper’s answer*, not the fact it carries:
 * the response travels on untouched, beside the fact, and no copy exists that could
 * drop what a `Response` keeps on its prototype.
 */

import { describe, it, expect } from 'vitest';
import { PLATFORMS } from '../lib/contract';
import { runBackfill } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  GEMINI_LIST_PAGE_SIZE,
  GEMINI_PLAN,
  listRequestInit,
} from '../lib/backfill/enumerate';
import {
  createGeminiAuthorizedFetch,
  type MinimalResponse,
} from '../lib/platform-auth';
import {
  serveBackfillFetch,
  tabHttpPort,
  type FetchLike,
} from '../lib/backfill/tab-port';
import type { GeminiBootstrapTokens } from '../lib/contract';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://gemini.google.com';
const AT_TOKEN = 'synthetic-at-token-value';
const REFUSAL = 'synthetic refusal body';
const LIST_BODY = listRequestInit(GEMINI_PLAN, ORIGIN, 0, GEMINI_LIST_PAGE_SIZE).body!;
const LIST_URL = GEMINI_PLAN.listUrl(ORIGIN, 0, GEMINI_LIST_PAGE_SIZE);

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

/**
 * 🔴 **The page-side fetch, assembled the way `dw-bridge.content.ts` assembles it.**
 *
 * Those three lines live in an entrypoint, which no suite can import (it boots against
 * a real page), so they are written down here rather than exported. What this test
 * proves about them is that the wrapper’s answer projects onto the message-shaped reply
 * without losing the response — the one thing that copy can get wrong, and the thing it
 * did get wrong.
 */
function pageFetchOver(
  authorized: (
    url: string,
    init: { method: 'POST'; body: string; headers?: Record<string, string> } | { headers?: Record<string, string> },
  ) => Promise<{ response: MinimalResponse; survivedCredentialReread?: true }>,
): FetchLike {
  return async (url, init) => {
    const answer = init && init.method === 'POST'
      ? await authorized(url, {
        method: 'POST',
        body: init.body!,
        headers: { ...(init.contentType ? { 'content-type': init.contentType } : {}) },
      })
      : await authorized(url, {});
    // 🔴 The response is handed on **by reference**: `text()` is called later, by
    //    `serveBackfillFetch`, and only a live `Response` can answer it.
    return answer.survivedCredentialReread === true
      ? { status: answer.response.status, text: () => answer.response.text(), survivedCredentialReread: true }
      : { status: answer.response.status, text: () => answer.response.text() };
  };
}

/**
 * The whole credential path, in the order production puts it in: wrapper → page side
 * (`serveBackfillFetch`) → message → background (`tabHttpPort`) → engine.
 */
async function runChain(
  rawFetch: (url: string, init: RequestInit) => Promise<MinimalResponse>,
  readTokens: () => Promise<GeminiBootstrapTokens | null> = async () => tokens(),
) {
  const authorized = createGeminiAuthorizedFetch(ORIGIN, rawFetch, { readTokens, language: null });
  const pageFetch = pageFetchOver(authorized);
  const spec = { url: LIST_URL, method: 'POST', body: LIST_BODY, contentType: 'application/x-www-form-urlencoded' };
  const reply = await serveBackfillFetch(spec, ORIGIN, pageFetch);
  const port = tabHttpPort(11, async () => reply as never);
  const row = PLATFORMS.find((p) => p.id === 'gemini')!;
  const report = await runBackfill({
    platform: 'gemini',
    origin: row.origins[0]!,
    scope: 'w64d',
    store: memoryStore(),
    http: port,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink,
  });
  return { reply, port, report };
}

// ---------------------------------------------------------------------------
// 1 · The wrapper hands back the response it was given
// ---------------------------------------------------------------------------

describe('W64d-1 · the wrapper’s answer holds the response itself', () => {
  it('🔴 a 400 that survived the re-read keeps its status and its body', async () => {
    const raw = new Response(REFUSAL, { status: 400 });
    const authorized = createGeminiAuthorizedFetch(ORIGIN, async () => raw, {
      readTokens: async () => tokens(),
      language: null,
    });
    const answer = await authorized(LIST_URL, { method: 'POST', body: LIST_BODY });

    // 🔴 The defect this pins: `status` and `text()` live on `Response.prototype`, so
    //    the spread that used to attach the mark kept neither. The object that comes
    //    back has to be the object that went in.
    expect(answer.response).toBe(raw);
    expect(answer.response.status).toBe(400);
    await expect(answer.response.text()).resolves.toBe(REFUSAL);
    // The fact still travels, and it is the only thing the wrapper adds.
    expect(answer.survivedCredentialReread).toBe(true);
  });

  it('a request it does not credential comes back as the very response it was given, unmarked', async () => {
    const raw = new Response('synthetic', { status: 200 });
    const authorized = createGeminiAuthorizedFetch(ORIGIN, async () => raw, {
      readTokens: async () => tokens(),
      language: null,
    });
    const other = `${ORIGIN}/_/BardChatUi/data/other?x=1`;
    const answer = await authorized(other, { method: 'POST', body: 'a=b' });
    expect(answer.response).toBe(raw);
    expect(answer.survivedCredentialReread).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// 2 · Every answer on the credential path reaches the engine as itself
// ---------------------------------------------------------------------------

describe('W64d-2 · a real Response survives the credential path', () => {
  it('🔴 the 400 that survived the re-read is auth-refused — not a broken transport', async () => {
    const sent: number[] = [];
    const { reply, report } = await runChain(async () => {
      sent.push(1);
      return new Response(REFUSAL, { status: 400 });
    });

    // Two requests: the credential was re-read and this refusal is the retried one’s.
    expect(sent).toHaveLength(2);
    // The reply the page side sends carries the status and the body it just read.
    expect(reply).toMatchObject({ ok: true, status: 400, text: REFUSAL, survivedCredentialReread: true });
    // 🔴 Before this fix the reply was `{ok:false, error:'…text is not a function'}` —
    //    the response had already been lost, so the classifier never saw a 400.
    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.stopped).toBe('waiting-retry');
    expect(report.halted?.detail).toContain('400');
  });

  it('🔴 the 401 that survived the re-read is auth-refused, and needs no mark to be one', async () => {
    const { reply, report } = await runChain(async () => new Response(REFUSAL, { status: 401 }));
    expect(reply).toMatchObject({ ok: true, status: 401, text: REFUSAL });
    expect(report.halted?.reason).toBe('auth-refused');
  });

  it('🔴 a retry that comes back 200 is read as a 200, body and all', async () => {
    let call = 0;
    const body = 'synthetic healthy page';
    const { reply, port, report } = await runChain(async () => {
      call += 1;
      // The first attempt is refused; the re-read finds a token, so the request goes
      // out again — and this time the platform answers.
      return call === 1 ? new Response(REFUSAL, { status: 400 }) : new Response(body, { status: 200 });
    });

    expect(call).toBe(2);
    // 🔴 The half that was worst: a *successful* fetch was recorded as a transport
    //    error, so the leg’s whole page was discarded and the run halted on nothing.
    expect(reply).toMatchObject({ ok: true, status: 200, text: body });
    const res = await port(LIST_URL, { method: 'POST', body: LIST_BODY, contentType: 'application/x-www-form-urlencoded' });
    expect(res.status).toBe(200);
    expect(res.text).toBe(body);
    // 🔴 W64c · The mark rides on the answer the credential path settled on, and the
    //    retried attempt is that answer whatever it turned out to be — W64c pinned
    //    this same `true` for a retried 200. It is inert here: a classifier that asks
    //    about the credential only when the status is a 400 never reads it.
    expect(reply).toMatchObject({ survivedCredentialReread: true });
    // And the leg is not told its transport broke: this page was read, whatever the
    // engine then makes of a body that is not a Gemini list.
    expect(report.halted?.reason).not.toBe('transport-error');
  });
});
