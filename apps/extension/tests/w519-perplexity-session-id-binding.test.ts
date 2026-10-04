/**
 * W519 · A Perplexity live capture is named only by the declared URL rule.
 * Synthetic response bodies deliberately carry an unrelated identifier so a
 * body fallback cannot silently choose which conversation the capture names.
 */
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { CAPTURE_MESSAGE } from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

let resolveCaptureSessionId: typeof import('../entrypoints/background')['resolveCaptureSessionId'];

const ORIGIN = 'https://www.perplexity.ai';
const URL_ID = 'synthetic-url-thread';
const BODY_ID = 'synthetic-unrelated-body-id';

function installHook(pageUrl: string, responseBody: string) {
  const posted: unknown[] = [];
  const fakeWindow: any = {
    location: { origin: ORIGIN, href: pageUrl },
    fetch: async () => new Response(responseBody, { status: 200 }),
    addEventListener() {},
    postMessage(message: unknown) { posted.push(message); },
  };
  vi.stubGlobal('window', fakeWindow);
  installPageFetchHook(PAGE_HOOK_OPTIONS);
  return { fakeWindow, posted };
}

async function capture(pageUrl: string, requestUrl: string, responseBody: string) {
  const { fakeWindow, posted } = installHook(pageUrl, responseBody);
  await fakeWindow.fetch(requestUrl);
  await new Promise((resolve) => setTimeout(resolve, 0));
  return posted.find((message: any) => message?.type === CAPTURE_MESSAGE) as any;
}

describe('W519 · Perplexity live capture session ID binding', () => {
  beforeAll(async () => {
    // WXT supplies this entrypoint macro in a built extension. The test needs
    // only the exported preparation helper, so keep its startup callback inert.
    vi.stubGlobal('defineBackground', () => ({}));
    ({ resolveCaptureSessionId } = await import('../entrypoints/background'));
    vi.unstubAllGlobals();
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('uses the declared URL identity even when the body carries another identifier', async () => {
    const body = JSON.stringify({ entries: [], session_id: BODY_ID });
    const requestUrl = `${ORIGIN}/rest/thread/${URL_ID}`;
    const captured = await capture(`${ORIGIN}/`, requestUrl, body);

    expect(captured).toBeDefined();
    expect(resolveCaptureSessionId(captured.payload)).toBe(URL_ID);
  });

  it.each([
    ['missing thread path segment', `${ORIGIN}/rest/thread/`],
    ['named sibling route', `${ORIGIN}/rest/thread/list_ask_threads`],
  ])('keeps identity unknown for a %s despite a body identifier', async (_label, requestUrl) => {
    const body = JSON.stringify({ entries: [], session_id: BODY_ID });
    const captured = await capture(`${ORIGIN}/`, requestUrl, body);

    expect(captured).toBeDefined();
    expect(resolveCaptureSessionId(captured.payload)).toBeNull();
  });
});
