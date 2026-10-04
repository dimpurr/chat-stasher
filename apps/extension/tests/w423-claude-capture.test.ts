/**
 * W423 · Pin the current Claude page-capture gate against synthetic response
 * fixtures. The layout mirrors the response shape measured for W92 and carried
 * in the W31 fixtures; every fixture value is invented and this probe makes no
 * new live request.
 *
 * This probes the page hook and the isolated-side payload validator together:
 * a valid detail response is posted, a candidate response with the expected
 * conversation envelope but no `chat_messages` is warned and refused, and
 * representative non-candidates remain silent. Account identity and
 * organization-scope behavior are outside this probe.
 */

import { readFileSync } from 'node:fs';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  isCaptureMessage,
  matchesResponseShape,
  PLATFORMS,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

const ORIGIN = 'https://claude.ai';
const DETAIL_URL = `${ORIGIN}/api/organizations/synthetic-org/chat_conversations/11111111-1111-4111-8111-111111111111?tree=true&rendering_mode=messages`;
const GOOD_BODY = readFileSync(new URL('../e2e/fixtures/claude-conversation.json', import.meta.url), 'utf8');
const WRONG_SHAPE_BODY = readFileSync(new URL('../e2e/fixtures/claude-wrong-shape.json', import.meta.url), 'utf8');

function installHook(responseBody: string) {
  const posted: unknown[] = [];
  const fakeWindow: any = {
    location: { origin: ORIGIN, href: `${ORIGIN}/chat/11111111-1111-4111-8111-111111111111` },
    fetch: async () => new Response(responseBody, { status: 200 }),
    addEventListener() {},
    postMessage(message: unknown) { posted.push(message); },
  };
  vi.stubGlobal('window', fakeWindow);
  installPageFetchHook(PAGE_HOOK_OPTIONS);
  return { fakeWindow, posted };
}

async function settleHook(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

describe('W423 · Claude live capture fixture probe', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('posts the fixture response that satisfies the current conversation shape', async () => {
    const claude = PLATFORMS.find((platform) => platform.id === 'claude');
    expect(claude).toBeDefined();
    expect(matchesResponseShape(claude!, GOOD_BODY)).toBe(true);
    const body = JSON.parse(GOOD_BODY);
    expect(body.chat_messages).toHaveLength(2);
    expect(body.chat_messages[0]).toMatchObject({
      uuid: expect.any(String),
      index: 0,
      sender: 'human',
      parent_message_uuid: expect.any(String),
      truncated: false,
      content: [{ type: 'text', text: 'synthetic prompt' }],
    });

    const { fakeWindow, posted } = installHook(GOOD_BODY);
    const response = await fakeWindow.fetch(DETAIL_URL);
    expect(response.status).toBe(200);
    await settleHook();

    const capture = posted.find((message: any) => message?.type === PAGE_HOOK_OPTIONS.captureMessage);
    expect(isCaptureMessage(capture)).toBe(true);
    expect((capture as any).payload.text).toBe(GOOD_BODY);
  });

  it('warns and refuses a candidate response whose conversation field was renamed', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = installHook(WRONG_SHAPE_BODY);

    await fakeWindow.fetch(DETAIL_URL);
    await settleHook();

    expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
    expect(warn.mock.calls[0]).toHaveLength(1);
    expect(posted.some((message: any) => message?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
  });

  it('silently skips representative paths and methods outside the capture candidate', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = installHook(GOOD_BODY);

    await fakeWindow.fetch(`${ORIGIN}/api/organizations/synthetic-org/chat_conversations?limit=50`);
    await fakeWindow.fetch(`${ORIGIN}/api/organizations/synthetic-org/settings`);
    await fakeWindow.fetch(DETAIL_URL, { method: 'POST' });
    await settleHook();

    expect(warn).not.toHaveBeenCalled();
    expect(posted.some((message: any) => message?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
  });
});
