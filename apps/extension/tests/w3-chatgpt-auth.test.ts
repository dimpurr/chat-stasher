import { afterEach, describe, expect, it, vi } from 'vitest';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import { CAPTURE_MESSAGE } from '../lib/contract';
import {
  CHATGPT_PAGED_DETAIL_PATTERN,
  CHATGPT_SESSION_PATH,
  CONVERSATION_SEEN_MESSAGE,
  chatgptDetailUrlFor,
  createAuthorizedFetch,
  createSeenGate,
  isConversationSeenMessage,
  needsChatgptBearer,
  type RawFetch,
} from '../lib/platform-auth';

/**
 * Measured 2026-09-13 in a logged-in Chrome: ChatGPT's conversation body answers
 * 404 without the session's bearer token and the full `mapping` with it. These
 * tests pin where the token may go and where it may not.
 */
const ORIGIN = 'https://chatgpt.com';
const ID = '6a93e10f-86d4-83ed-8091-5890097ae6c2';
const DETAIL = `${ORIGIN}/backend-api/conversation/${ID}`;

interface Call { url: string; auth: string | undefined }

/** A fake network: the session endpoint hands out tokens in order; the body endpoint wants the current one. */
function fakeNetwork(tokens: Array<string | null>, validToken: () => string) {
  const calls: Call[] = [];
  let sessionReads = 0;
  const raw: RawFetch = async (url, init) => {
    const headers = (init.headers ?? {}) as Record<string, string>;
    calls.push({ url, auth: headers.authorization });
    if (url === `${ORIGIN}${CHATGPT_SESSION_PATH}`) {
      const token = tokens[Math.min(sessionReads, tokens.length - 1)];
      sessionReads += 1;
      return { status: 200, text: async () => JSON.stringify(token === null ? {} : { accessToken: token }) };
    }
    if (url.startsWith(`${ORIGIN}/backend-api/conversation/`)) {
      const ok = headers.authorization === `Bearer ${validToken()}`;
      return ok
        ? { status: 200, text: async () => '{"mapping":{},"current_node":"n"}' }
        : { status: headers.authorization ? 401 : 404, text: async () => '{"detail":"x"}' };
    }
    return { status: 200, text: async () => '{"items":[],"total":0}' };
  };
  return { raw, calls, sessionReads: () => sessionReads };
}

describe('which requests may carry the ChatGPT token', () => {
  it('only the same-origin ChatGPT list and conversation-body URLs', () => {
    expect(needsChatgptBearer(DETAIL, ORIGIN)).toBe(true);
    // Measured: without the token the list answers 200 with an empty list — a false "no conversations".
    expect(needsChatgptBearer(`${ORIGIN}/backend-api/conversations?offset=0&limit=20`, ORIGIN)).toBe(true);
    expect(needsChatgptBearer(`${ORIGIN}/backend-api/me`, ORIGIN)).toBe(false);
    expect(needsChatgptBearer(`${ORIGIN}/backend-api/conversations/${ID}`, ORIGIN)).toBe(false);
    expect(needsChatgptBearer(DETAIL, 'https://chat.openai.com')).toBe(false);
    expect(needsChatgptBearer('https://chat.deepseek.com/backend-api/conversation/x', 'https://chat.deepseek.com')).toBe(false);
    expect(needsChatgptBearer('not a url', ORIGIN)).toBe(false);
  });
});

describe('authorized fetch', () => {
  it('adds the session token to a body request, which then succeeds', async () => {
    const net = fakeNetwork(['tok-1'], () => 'tok-1');
    const res = await createAuthorizedFetch(ORIGIN, net.raw)(DETAIL, { headers: { accept: 'application/json' } });
    expect(res.status).toBe(200);
    expect(net.calls.at(-1)?.auth).toBe('Bearer tok-1');
  });

  it('leaves every other request untouched and never reads the session for it', async () => {
    const net = fakeNetwork(['tok-1'], () => 'tok-1');
    const other = `${ORIGIN}/backend-api/me`;
    await createAuthorizedFetch(ORIGIN, net.raw)(other, { headers: { accept: 'application/json' } });
    expect(net.calls).toEqual([{ url: other, auth: undefined }]);
    expect(net.sessionReads()).toBe(0);
  });

  it('reads the session once and reuses the token from memory', async () => {
    const net = fakeNetwork(['tok-1'], () => 'tok-1');
    const f = createAuthorizedFetch(ORIGIN, net.raw);
    await f(DETAIL, {});
    await f(DETAIL, {});
    expect(net.sessionReads()).toBe(1);
  });

  it('on 401 drops the token, reads a fresh one once, and retries', async () => {
    let valid = 'tok-1';
    const net = fakeNetwork(['tok-1', 'tok-2'], () => valid);
    const f = createAuthorizedFetch(ORIGIN, net.raw);
    await f(DETAIL, {});
    valid = 'tok-2';                       // the token expired server-side
    const res = await f(DETAIL, {});
    expect(res.status).toBe(200);
    expect(net.sessionReads()).toBe(2);
  });

  it('the conversation list carries the token (cookie-only answers a false empty list)', async () => {
    const net = fakeNetwork(['tok-1'], () => 'tok-1');
    const list = `${ORIGIN}/backend-api/conversations?offset=0&limit=20`;
    await createAuthorizedFetch(ORIGIN, net.raw)(list, { headers: { accept: 'application/json' } });
    expect(net.calls.at(-1)).toEqual({ url: list, auth: 'Bearer tok-1' });
  });

  it('with no token available, sends without one so the real status reaches the caller', async () => {
    const net = fakeNetwork([null], () => 'tok-1');
    const res = await createAuthorizedFetch(ORIGIN, net.raw)(DETAIL, {});
    expect(res.status).toBe(404);          // not invented: the platform's own answer
    expect(net.calls.filter((c) => c.url === DETAIL).every((c) => c.auth === undefined)).toBe(true);
  });
});

describe('live leg: paged window seen', () => {
  it('matches only the paged detail path, capturing the id', () => {
    expect(CHATGPT_PAGED_DETAIL_PATTERN.exec(`/backend-api/conversations/${ID}`)?.[1]).toBe(ID);
    expect(CHATGPT_PAGED_DETAIL_PATTERN.test('/backend-api/conversations')).toBe(false);
    expect(CHATGPT_PAGED_DETAIL_PATTERN.test('/backend-api/conversations/search')).toBe(false);
    expect(CHATGPT_PAGED_DETAIL_PATTERN.test(`/backend-api/conversation/${ID}`)).toBe(false);
  });

  it('accepts only a well-formed id-only message', () => {
    expect(isConversationSeenMessage({ type: CONVERSATION_SEEN_MESSAGE, platform: 'chatgpt', id: ID })).toBe(true);
    expect(isConversationSeenMessage({ type: CONVERSATION_SEEN_MESSAGE, platform: 'deepseek', id: ID })).toBe(false);
    expect(isConversationSeenMessage({ type: CONVERSATION_SEEN_MESSAGE, platform: 'chatgpt', id: '../x' })).toBe(false);
    expect(isConversationSeenMessage({ type: 'other', platform: 'chatgpt', id: ID })).toBe(false);
  });

  it('builds the full-conversation URL on the page origin', () => {
    expect(chatgptDetailUrlFor(ORIGIN, ID)).toBe(DETAIL);
  });

  describe('in the page hook', () => {
    afterEach(() => {
      vi.restoreAllMocks();
      vi.unstubAllGlobals();
    });

    /** Installs the real hook on a fake ChatGPT page whose fetch answers `body`. */
    function chatgptPage(body: string) {
      const posted: Array<Record<string, unknown>> = [];
      const fakeWindow: any = {
        location: { origin: ORIGIN, href: `${ORIGIN}/c/${ID}` },
        fetch: async () => new Response(body, { status: 200 }),
        addEventListener() { /* no handshake needed */ },
        postMessage(message: Record<string, unknown>) { posted.push(message); },
      };
      vi.stubGlobal('window', fakeWindow);
      installPageFetchHook(PAGE_HOOK_OPTIONS);
      return { fakeWindow, posted };
    }
    const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

    it('a paged window posts only the id, never a capture', async () => {
      const { fakeWindow, posted } = chatgptPage('{"messages":[],"current_node":"n","page_info":{}}');
      await fakeWindow.fetch(`${ORIGIN}/backend-api/conversations/${ID}?include_has_versions=true&num_turns=10`);
      await settle();
      expect(posted.filter((m) => m.type === CAPTURE_MESSAGE)).toHaveLength(0);
      expect(posted.filter((m) => m.type === CONVERSATION_SEEN_MESSAGE)).toEqual([
        { type: CONVERSATION_SEEN_MESSAGE, platform: 'chatgpt', id: ID },
      ]);
    });

    it('an over-cap conversation is skipped with a visible, metadata-only warning', async () => {
      const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
      const posted: Array<Record<string, unknown>> = [];
      const body = '{"mapping":{"a":{}},"current_node":"a"}';
      const fakeWindow: any = {
        location: { origin: ORIGIN, href: `${ORIGIN}/c/${ID}` },
        fetch: async () => new Response(body, { status: 200 }),
        addEventListener() { /* no handshake needed */ },
        postMessage(message: Record<string, unknown>) { posted.push(message); },
      };
      vi.stubGlobal('window', fakeWindow);
      installPageFetchHook({ ...PAGE_HOOK_OPTIONS, maxRawBytes: body.length - 1 });
      await fakeWindow.fetch(DETAIL);
      await settle();
      expect(posted.filter((m) => m.type === CAPTURE_MESSAGE)).toHaveLength(0);
      expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response exceeds the size cap');
    });

    it('the full conversation is still captured passively', async () => {
      const { fakeWindow, posted } = chatgptPage('{"mapping":{"a":{}},"current_node":"a"}');
      await fakeWindow.fetch(DETAIL);
      await settle();
      expect(posted.filter((m) => m.type === CAPTURE_MESSAGE)).toHaveLength(1);
      expect(posted.filter((m) => m.type === CONVERSATION_SEEN_MESSAGE)).toHaveLength(0);
    });
  });

  it('refetches an id at most once per window', () => {
    let t = 0;
    const gate = createSeenGate(15_000, () => t);
    expect(gate(ID)).toBe(true);
    t = 5_000;
    expect(gate(ID)).toBe(false);
    expect(gate('ffffffff-0000')).toBe(true);
    t = 20_000;
    expect(gate(ID)).toBe(true);
  });
});
