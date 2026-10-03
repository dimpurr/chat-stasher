import { readFileSync } from 'node:fs';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  extractSessionId,
  isCapturedFetchShape,
  matchesResponseShape,
  PLATFORMS,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

const ORIGIN = 'https://www.kimi.com';
const CHAT_ID = 'fixture-chat-opaque-01';
const PAGE_URL = `${ORIGIN}/chat/${CHAT_ID}`;
const API_URL = `${ORIGIN}/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages`;
const RESPONSE = readFileSync(
  new URL('./fixtures/kimi-list-messages.synthetic.json', import.meta.url),
  'utf8',
);

function makeFakeWindow(body: string) {
  const posted: unknown[] = [];
  const fakeWindow: any = {
    location: { origin: ORIGIN, href: PAGE_URL },
    async fetch() { return new Response(body, { status: 200 }); },
    addEventListener() { /* probe handshake is outside this capture contract */ },
    postMessage(message: unknown) { posted.push(message); },
  };
  return { fakeWindow, posted };
}

describe('W422 · Kimi live-capture response contract', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('accepts the established messages array and takes identity from the chat URL', async () => {
    const row = PLATFORMS.find((platform) => platform.id === 'kimi')!;
    expect(matchesResponseShape(row, RESPONSE)).toBe(true);
    expect(extractSessionId(API_URL, RESPONSE, PAGE_URL)).toBe(CHAT_ID);
    expect(extractSessionId(API_URL, RESPONSE)).toBeNull();

    const { fakeWindow, posted } = makeFakeWindow(RESPONSE);
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);
    await fakeWindow.fetch(API_URL, { method: 'POST' });
    await new Promise((resolve) => setTimeout(resolve, 0));

    const captures = posted.filter((message: any) => message?.type === PAGE_HOOK_OPTIONS.captureMessage) as any[];
    expect(captures).toHaveLength(1);
    expect(isCapturedFetchShape(captures[0].payload)).toBe(true);
    expect(extractSessionId(captures[0].payload.url, captures[0].payload.text, captures[0].payload.pageUrl))
      .toBe(CHAT_ID);
  });

  it('rejects malformed, empty-looking, and drifted response envelopes at the live hook', async () => {
    const row = PLATFORMS.find((platform) => platform.id === 'kimi')!;
    const invalidBodies = [
      '',
      'not-json',
      '{}',
      '{"messages":null}',
      '{"messages":""}',
      '{"messages":{}}',
      '{"data":{"messages":[]}}',
      '{"items":[]}',
    ];
    for (const body of invalidBodies) expect(matchesResponseShape(row, body)).toBe(false);
    // An empty array is still a measured messages envelope; absence and other
    // empty-looking containers are the drift that must fail closed.
    expect(matchesResponseShape(row, '{"messages":[]}')).toBe(true);

    for (const body of invalidBodies) {
      const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
      const { fakeWindow, posted } = makeFakeWindow(body);
      vi.stubGlobal('window', fakeWindow);
      installPageFetchHook(PAGE_HOOK_OPTIONS);
      await fakeWindow.fetch(API_URL, { method: 'POST' });
      await new Promise((resolve) => setTimeout(resolve, 0));

      expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
      expect(warn.mock.calls[0]).toHaveLength(1);
      expect(posted.some((message: any) => message?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
      vi.unstubAllGlobals();
      warn.mockRestore();
    }
  });
});
