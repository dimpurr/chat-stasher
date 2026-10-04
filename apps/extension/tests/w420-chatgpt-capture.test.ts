import { readFileSync } from 'node:fs';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  CAPTURE_MESSAGE,
  extractSessionId,
  isPageCaptureMessage,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

const ORIGIN = 'https://chatgpt.com';
const SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const DETAIL_URL = `${ORIGIN}/backend-api/conversation/${SESSION_ID}`;
const PAGE_URL = `${ORIGIN}/c/${SESSION_ID}`;
const CONVERSATION_TEXT = readFileSync(
  new URL('../e2e/fixtures/chatgpt-conversation.json', import.meta.url),
  'utf8',
);

function chatgptPage(responseText: string) {
  const posted: unknown[] = [];
  const fakeWindow: any = {
    location: { origin: ORIGIN, href: PAGE_URL },
    fetch: async () => new Response(responseText, { status: 200 }),
    addEventListener() { /* No handshake is needed to observe fetch capture. */ },
    postMessage(message: unknown) { posted.push(message); },
  };
  vi.stubGlobal('window', fakeWindow);
  installPageFetchHook(PAGE_HOOK_OPTIONS);
  return { fakeWindow, posted };
}

async function settlePageHook(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

describe('W420 · fixture-backed ChatGPT live capture probe', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('captures the bounded full-conversation fixture and resolves its session id', async () => {
    const { fakeWindow, posted } = chatgptPage(CONVERSATION_TEXT);

    await fakeWindow.fetch(DETAIL_URL);
    await settlePageHook();

    const captures = posted.filter((message: any) => message?.type === CAPTURE_MESSAGE);
    expect(captures).toHaveLength(1);
    const event = {
      source: fakeWindow,
      origin: ORIGIN,
      data: captures[0],
    };
    expect(isPageCaptureMessage(event, fakeWindow, ORIGIN)).toBe(true);
    expect(CONVERSATION_TEXT.length).toBeLessThan(16 * 1024);
    expect(extractSessionId(DETAIL_URL, (captures[0] as any).payload.text, PAGE_URL)).toBe(SESSION_ID);
  });

  it.each([
    ['malformed JSON', '{"mapping":'],
    ['missing current node', '{"mapping":{"node-0":{}}}'],
    ['paged window without a full mapping', '{"messages":[],"current_node":"node-1","page_info":{}}'],
  ])('does not capture a %s candidate as a full conversation', async (_label, responseText) => {
    vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = chatgptPage(responseText);

    await fakeWindow.fetch(DETAIL_URL);
    await settlePageHook();

    expect(posted.filter((message: any) => message?.type === CAPTURE_MESSAGE)).toHaveLength(0);
  });
});
