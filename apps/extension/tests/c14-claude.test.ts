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
 * conversation, account, org id or session was used, and no request was ever
 * made to claude.ai. The route shape is a SECOND-HAND assumption taken from
 * public open-source exporters (see lib/contract.ts comments) and has NOT been
 * verified against a logged-in page.
 */
const SYNTHETIC_ORG = '00000000-0000-4000-8000-000000000001';
const SYNTHETIC_CONVERSATION = '11111111-1111-4111-8111-111111111111';
const CLAUDE_API_URL =
  `https://claude.ai/api/organizations/${SYNTHETIC_ORG}` +
  `/chat_conversations/${SYNTHETIC_CONVERSATION}?tree=true&rendering_mode=messages`;

/** Shape as documented by the MIT exporter's own API contract notes. */
const goodBody = JSON.stringify({
  name: 'synthetic conversation',
  model: 'synthetic-model',
  current_leaf_message_uuid: '22222222-2222-4222-8222-222222222222',
  chat_messages: [
    { uuid: 'aaaa1111', sender: 'human', index: 0, content: [{ type: 'text', text: 'synthetic prompt' }] },
    { uuid: 'aaaa2222', sender: 'assistant', index: 1, content: [{ type: 'text', text: 'synthetic reply' }] },
    { uuid: 'aaaa3333', sender: 'human', index: 2, content: [{ type: 'text', text: 'synthetic follow-up' }] },
  ],
});

/**
 * "Looks like conversation data, but the fields are not the ones we expect."
 * Same route, same 200, plainly a conversation envelope — but `chat_messages`
 * has been renamed. This MUST NOT be skipped silently.
 */
const driftedBody = JSON.stringify({
  name: 'synthetic conversation',
  current_leaf_message_uuid: '22222222-2222-4222-8222-222222222222',
  messages: [
    { uuid: 'aaaa1111', sender: 'human', content: [{ type: 'text', text: 'synthetic prompt' }] },
  ],
});

function makeFakeWindow(responseBody: string) {
  const posted: unknown[] = [];
  const fakeWindow: any = {
    location: {
      origin: 'https://claude.ai',
      href: `https://claude.ai/chat/${SYNTHETIC_CONVERSATION}`,
    },
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

describe('C14 · claude.ai platform row', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('1. correct shape → extracts, session id and message count line up', () => {
    const claude = PLATFORMS.find((platform) => platform.id === 'claude');
    expect(claude).toBeDefined();
    expect(claude!.credibility).toBe('from-source');
    // The closed origin set: exactly one origin, no wildcard of any kind.
    expect(claude!.origins).toEqual(['https://claude.ai']);

    expect(platformForTraffic(CLAUDE_API_URL, 'GET')?.id).toBe('claude');
    expect(matchesResponseShape(claude!, goodBody)).toBe(true);
    expect(extractSessionId(CLAUDE_API_URL, goodBody)).toBe(SYNTHETIC_CONVERSATION);
    // Session id is also recoverable from the page URL alone.
    expect(extractSessionId(
      'https://claude.ai/api/organizations/x/chat_conversations',
      goodBody,
      `https://claude.ai/chat/${SYNTHETIC_CONVERSATION}`,
    )).toBe(SYNTHETIC_CONVERSATION);

    expect(JSON.parse(goodBody).chat_messages).toHaveLength(3);
    expect(isCapturedFetchShape({
      url: CLAUDE_API_URL,
      method: 'GET',
      status: 200,
      text: goodBody,
      pageUrl: `https://claude.ai/chat/${SYNTHETIC_CONVERSATION}`,
      capturedAt: Date.now(),
    })).toBe(true);
  });

  it('2. origin outside the closed set → no match at all', () => {
    expect(CONTENT_MATCHES).toContain('https://claude.ai/*');
    expect(CONTENT_MATCHES).not.toContain('<all_urls>');
    expect(CONTENT_MATCHES.some((match) => match.startsWith('*'))).toBe(false);

    for (const hostile of [
      `https://claude.ai.attacker.example/api/organizations/o/chat_conversations/${SYNTHETIC_CONVERSATION}`,
      `https://www.claude.ai/api/organizations/o/chat_conversations/${SYNTHETIC_CONVERSATION}`,
      `https://claude.com/api/organizations/o/chat_conversations/${SYNTHETIC_CONVERSATION}`,
      `http://claude.ai/api/organizations/o/chat_conversations/${SYNTHETIC_CONVERSATION}`,
    ]) {
      expect(platformForTraffic(hostile, 'GET')).toBeNull();
      expect(isCapturedFetchShape({
        url: hostile,
        method: 'GET',
        status: 200,
        text: goodBody,
        capturedAt: Date.now(),
      })).toBe(false);
    }
  });

  it('3. looks-like-but-wrong shape → halts loudly; unrelated route stays silent', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(driftedBody);
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(CLAUDE_API_URL);
    await new Promise((resolve) => setTimeout(resolve, 0));

    // (a) Recognised as a conversation-data candidate, rejected on shape, and
    //     the rejection is on the record — NOT silently dropped.
    expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
    // Metadata-only: the warning carries no URL, body, org or conversation id.
    expect(warn.mock.calls[0]).toHaveLength(1);
    // Nothing was handed to the bridge, so nothing can be mistaken for a backup.
    expect(posted.some((message: any) => message?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);

    // (b) Same origin, but not a conversation-data route → normal skip, no noise.
    warn.mockClear();
    await fakeWindow.fetch('https://claude.ai/api/organizations/o/settings');
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(warn).not.toHaveBeenCalled();
  });
});
