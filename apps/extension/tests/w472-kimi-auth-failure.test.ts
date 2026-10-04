/**
 * W472 · Kimi auth failures stay inside the auth boundary. Unreadable page storage
 * must not prevent the plan's request, invent a bearer value, or replace Kimi's
 * response; a 401 still gets exactly one fresh-token read and retry.
 *
 * All tokens, storage, and responses here are synthetic. The fake fetch rejects
 * every route except the one the Kimi plan builds.
 */

import { describe, expect, it, vi } from 'vitest';
import { createKimiAuthorizedFetch, KIMI_PLATFORM_HEADER } from '../lib/platform-auth';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { KIMI_LIST_PATH } from '../lib/backfill/enumerate';
import { memoryStore } from '../lib/backfill/store';
import type { CapturedFetch } from '../lib/contract';
import type { MinimalResponse } from '../lib/platform-auth';

const ORIGIN = 'https://www.kimi.com';
const TOKEN_OLD = 'opaque.synthetic.token.old';
const TOKEN_FRESH = 'opaque.synthetic.token.fresh';
const PLATFORM_REFUSAL = JSON.stringify({ code: 401, details: 'synthetic auth refusal' });

type PagePort = { readonly localStorage: { readonly access_token: string | null } };

function unreadablePage(): PagePort {
  return Object.defineProperty({}, 'localStorage', {
    get() {
      throw new Error(`synthetic storage accessor failed ${TOKEN_OLD}`);
    },
  }) as PagePort;
}

function throwingTokenPage(): PagePort {
  const storage = Object.defineProperty({}, 'access_token', {
    get() {
      throw new Error(`synthetic token accessor failed ${TOKEN_OLD}`);
    },
  });
  return { localStorage: storage as PagePort['localStorage'] };
}

interface FetchCall {
  url: string;
  init: RequestInit;
}

function fakeFetch(replies: MinimalResponse[]) {
  const calls: FetchCall[] = [];
  const fetch = async (url: string, init: RequestInit): Promise<MinimalResponse> => {
    calls.push({ url, init });
    if (new URL(url).origin !== ORIGIN || new URL(url).pathname !== KIMI_LIST_PATH) {
      throw new Error(`unexpected synthetic route ${new URL(url).pathname}`);
    }
    const response = replies.shift();
    if (!response) throw new Error('unexpected synthetic fetch');
    return response;
  };
  return { calls, fetch };
}

function response(status: number, body: string): MinimalResponse {
  return { status, text: async () => body };
}

function initOf(call: FetchCall): RequestInit {
  return call.init;
}

function headersOf(call: FetchCall): Record<string, string> {
  return (call.init.headers ?? {}) as Record<string, string>;
}

async function runKimi(readToken: () => string | null, fake: ReturnType<typeof fakeFetch>) {
  const authorizedFetch = createKimiAuthorizedFetch(ORIGIN, fake.fetch, {
    readToken,
    language: null,
  });
  const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
    const res = await authorizedFetch(url, init as RequestInit);
    return { status: res.status, text: await res.text() };
  };
  let now = Date.parse('2026-10-04T00:00:00.000Z');
  return runBackfill({
    platform: 'kimi',
    origin: ORIGIN,
    scope: 'w472-synthetic-scope',
    store: memoryStore(),
    http,
    clock: {
      now: () => now,
      async sleep(ms: number) { now += ms; },
    },
    pace: {
      enumerate: { minIntervalMs: 0, maxPerDay: null },
      detail: { minIntervalMs: 0, maxPerDay: null },
    },
    sink: (captured: CapturedFetch) => ({ saved: true, sessionId: captured.sessionId }),
  });
}

describe('W472 · Kimi auth failure probe', () => {
  it.each([
    ['localStorage is unreadable', () => {
      const page = unreadablePage();
      return () => page.localStorage.access_token;
    }],
    ['the access_token getter throws', () => {
      const page = throwingTokenPage();
      return () => page.localStorage.access_token;
    }],
  ])('%s sends the plan request without a bearer and preserves the platform refusal', async (_label, reader) => {
    const platformResponse = response(401, PLATFORM_REFUSAL);
    const fake = fakeFetch([platformResponse]);
    const diagnostics: string[] = [];
    const warn = vi.spyOn(console, 'warn').mockImplementation((...args) => {
      diagnostics.push(args.map(String).join(' '));
    });
    const error = vi.spyOn(console, 'error').mockImplementation((...args) => {
      diagnostics.push(args.map(String).join(' '));
    });
    let report: Awaited<ReturnType<typeof runBackfill>>;
    try {
      report = await runKimi(reader(), fake);
    } finally {
      warn.mockRestore();
      error.mockRestore();
    }

    expect(fake.calls).toHaveLength(1);
    expect(fake.calls[0]!.url).toBe(`${ORIGIN}${KIMI_LIST_PATH}`);
    expect(initOf(fake.calls[0]!)).toMatchObject({
      method: 'POST',
      body: JSON.stringify({ page_size: 100, page_token: '' }),
      redirect: 'manual',
    });
    expect(headersOf(fake.calls[0]!).authorization).toBeUndefined();
    expect(headersOf(fake.calls[0]!)[KIMI_PLATFORM_HEADER]).toBe('web');
    expect(report.stopped).toBe('waiting-retry');
    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.halted?.detail).toContain('401');
    expect(report.halted?.detail).not.toContain(TOKEN_OLD);
    expect(report.halted?.detail).not.toContain(TOKEN_FRESH);
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.state.pending).toEqual([]);
    const reportText = JSON.stringify(report);
    expect(reportText).not.toContain(TOKEN_OLD);
    expect(reportText).not.toContain(TOKEN_FRESH);
    expect(diagnostics.join('\n')).not.toContain(TOKEN_OLD);
    expect(diagnostics.join('\n')).not.toContain(TOKEN_FRESH);
    expect(PLATFORM_REFUSAL).not.toContain(TOKEN_OLD);
    expect(PLATFORM_REFUSAL).not.toContain(TOKEN_FRESH);
  });

  it('re-reads once after a 401 and retries with the newly readable opaque token', async () => {
    const reads: Array<string | null> = [TOKEN_OLD, TOKEN_FRESH];
    let readCount = 0;
    const fake = fakeFetch([
      response(401, PLATFORM_REFUSAL),
      response(401, PLATFORM_REFUSAL),
    ]);
    const authorizedFetch = createKimiAuthorizedFetch(ORIGIN, fake.fetch, {
      readToken: () => {
        readCount += 1;
        return reads.shift() ?? null;
      },
      language: null,
    });

    const firstInit = {
      method: 'POST',
      body: JSON.stringify({ page_size: 100, page_token: '' }),
    };
    const result = await authorizedFetch(`${ORIGIN}${KIMI_LIST_PATH}`, firstInit);

    expect(fake.calls).toHaveLength(2);
    expect(fake.calls.map((call) => call.url)).toEqual([
      `${ORIGIN}${KIMI_LIST_PATH}`,
      `${ORIGIN}${KIMI_LIST_PATH}`,
    ]);
    expect(fake.calls.map((call) => call.init.body)).toEqual([firstInit.body, firstInit.body]);
    expect(fake.calls.map((call) => call.init.method)).toEqual(['POST', 'POST']);
    expect(fake.calls.map((call) => call.init.redirect)).toEqual(['manual', 'manual']);
    expect(headersOf(fake.calls[0]!).authorization).toBe(`Bearer ${TOKEN_OLD}`);
    expect(headersOf(fake.calls[1]!).authorization).toBe(`Bearer ${TOKEN_FRESH}`);
    expect(readCount).toBe(2);
    expect(reads).toEqual([]);
    expect(await result.text()).toBe(PLATFORM_REFUSAL);
    expect(result.status).toBe(401);
    expect(JSON.stringify(result)).not.toContain(TOKEN_OLD);
    expect(JSON.stringify(result)).not.toContain(TOKEN_FRESH);
  });
});
