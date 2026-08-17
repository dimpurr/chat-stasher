import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  CONTENT_MATCHES,
  extractSessionId,
  isCapturedFetchShape,
  matchesResponseShape,
  PLATFORMS,
  platformForTraffic,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

/**
 * SYNTHETIC ONLY. Every body below is hand-written fake data; no real
 * conversation, account, token or chat id was used, and no request was ever
 * made to kimi.com. The route shape is SOURCE-BACKED but NOT live-verified:
 * it comes from public open-source exporters (see lib/contract.ts comments),
 * never from a logged-in Kimi session.
 */
const SYNTHETIC_CHAT_ID = 'd0000000000000000001';
const KIMI_PAGE_URL = `https://www.kimi.com/chat/${SYNTHETIC_CHAT_ID}`;
/** The chat id travels in the POST body, so the API URL carries no id at all. */
const KIMI_API_URL = 'https://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages';

/** Envelope as read by the MIT exporter: a top-level `messages` array. */
const goodBody = JSON.stringify({
  messages: [
    { id: 'm1', role: 'user', content: [{ type: 'text', text: 'synthetic prompt' }] },
    { id: 'm2', role: 'assistant', content: [{ type: 'text', text: 'synthetic reply' }] },
    { id: 'm3', role: 'user', content: [{ type: 'text', text: 'synthetic follow-up' }] },
  ],
  nextPageToken: '',
});

/**
 * "Looks like conversation data, but the fields are not the ones we expect."
 * Same route, same 200, plainly a conversation envelope — but the message list
 * moved under a different key. This MUST NOT be skipped silently.
 */
const driftedBody = JSON.stringify({
  data: {
    items: [{ id: 'm1', role: 'user', content: [{ type: 'text', text: 'synthetic prompt' }] }],
  },
  nextPageToken: '',
});

function makeFakeWindow(responseBody: string) {
  const posted: unknown[] = [];
  const fakeWindow: any = {
    location: { origin: 'https://www.kimi.com', href: KIMI_PAGE_URL },
    fetch: async () => new Response(responseBody, { status: 200 }),
    addEventListener() {
      // The hook's probe listener is irrelevant to these assertions.
    },
    postMessage(message: unknown) {
      posted.push(message);
    },
  };
  return { fakeWindow, posted };
}

describe('C16 · www.kimi.com platform row', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('1. correct shape → extracts, session id and message count line up', () => {
    const kimi = PLATFORMS.find((platform) => platform.id === 'kimi');
    expect(kimi).toBeDefined();
    expect(kimi!.credibility).toBe('from-source');
    // The closed origin set: exactly one origin, no wildcard of any kind.
    expect(kimi!.origins).toEqual(['https://www.kimi.com']);

    expect(platformForTraffic(KIMI_API_URL, 'POST')?.id).toBe('kimi');
    expect(matchesResponseShape(kimi!, goodBody)).toBe(true);
    // The id is only in the page URL, which is exactly why pageUrl is carried.
    expect(extractSessionId(KIMI_API_URL, goodBody, KIMI_PAGE_URL)).toBe(SYNTHETIC_CHAT_ID);

    expect(JSON.parse(goodBody).messages).toHaveLength(3);
    expect(isCapturedFetchShape({
      url: KIMI_API_URL,
      method: 'POST',
      status: 200,
      text: goodBody,
      pageUrl: KIMI_PAGE_URL,
      capturedAt: Date.now(),
    })).toBe(true);

    // The conversation-LIST route is a different service and stays out.
    expect(platformForTraffic(
      'https://www.kimi.com/apiv2/kimi.chat.v1.ChatService/ListChats',
      'POST',
    )).toBeNull();
  });

  it('2. origin outside the closed set → no match at all', () => {
    expect(CONTENT_MATCHES).toContain('https://www.kimi.com/*');
    expect(CONTENT_MATCHES).not.toContain('<all_urls>');
    expect(CONTENT_MATCHES.some((match) => match.startsWith('*'))).toBe(false);

    for (const hostile of [
      // Look-alike host, bare apex, the legacy domain we deliberately did NOT
      // onboard, and plain http — none of them may match.
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
        text: goodBody,
        pageUrl: KIMI_PAGE_URL,
        capturedAt: Date.now(),
      })).toBe(false);
    }
  });

  it('3. looks-like-but-wrong shape → halts loudly; unrelated route stays silent', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(driftedBody);
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(KIMI_API_URL, { method: 'POST' });
    await new Promise((resolve) => setTimeout(resolve, 0));

    // (a) Recognised as a conversation-data candidate, rejected on shape, and
    //     the rejection is on the record — NOT silently dropped.
    expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
    // Metadata-only: the warning carries no URL, body, token or chat id.
    expect(warn.mock.calls[0]).toHaveLength(1);
    // Nothing was handed to the bridge, so nothing can be mistaken for a backup.
    expect(posted.some((message: any) => message?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);

    // (b) Same origin, but not a conversation-data route → normal skip, no noise.
    warn.mockClear();
    await fakeWindow.fetch('https://www.kimi.com/apiv2/kimi.chat.v1.ChatService/ListChats', { method: 'POST' });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(warn).not.toHaveBeenCalled();
  });
});
