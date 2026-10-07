/**
 * W931 · A same-origin ChatGPT request that never carries the `ChatGPT-Account-Id`
 * header must not wipe the account identity the page already observed.
 *
 * The backfill leg sends its list and body requests with the account header the
 * page last reported, and the engine binds the reply to that exact header. The
 * page's own session refresh (`/api/auth/session`) and its other non-backfill
 * same-origin calls are headerless by construction — they say nothing about which
 * account is signed in. Treating each of them as "the account is now unknown"
 * cleared the observation between the page's own backend calls and the backfill's
 * next request, so the wrapper refused before sending and the leg halted with
 * `chatgpt-account-header-unavailable` while thousands of debts stayed pending.
 *
 * Fixtures only: opaque synthetic ids, no logged-in session.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';
import { CHATGPT_WORKSPACE_OBSERVED_MESSAGE } from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

const ORIGIN = 'https://chatgpt.com';
const ID = 'aaaaaaaa-1111-2222-3333-444444444444';
const LIST = `${ORIGIN}/backend-api/conversations?offset=0&limit=100`;
const DETAIL = `${ORIGIN}/backend-api/conversation/${ID}`;
const SESSION = `${ORIGIN}/api/auth/session`;
const BODY = JSON.stringify({ items: [], total: 0, mapping: {}, current_node: 'n0' });
const HEADER_A = 'acct-fixture-alpha';

function pageWindow(origin: string, body: string): any {
  const posted: any[] = [];
  const win: any = {
    location: { origin, href: `${origin}/` },
    fetch: async () => new Response(body, { status: 200 }),
    addEventListener() { /* the hook's other listeners are not what this file tests */ },
    postMessage(message: unknown) { posted.push(message); },
  };
  win.posted = posted;
  return win;
}

async function flush(times = 6): Promise<void> {
  for (let i = 0; i < times; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
}

function install(win: any): void {
  vi.stubGlobal('window', win);
  installPageFetchHook(PAGE_HOOK_OPTIONS);
}

const observations = (win: any): any[] =>
  win.posted.filter((m: any) => m?.type === CHATGPT_WORKSPACE_OBSERVED_MESSAGE);

describe('W931 · the observed ChatGPT account survives a headerless non-backfill request', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('a headerless session refresh does not wipe the account the page reported', async () => {
    const win = pageWindow(ORIGIN, BODY);
    install(win);
    await win.fetch(LIST, { headers: { 'ChatGPT-Account-Id': HEADER_A } });
    await flush();
    await win.fetch(SESSION);
    await flush();

    expect(observations(win).map((m) => m.accountId)).toEqual([HEADER_A]);
  });

  it('a headerless backfill route still invalidates the observation', async () => {
    const win = pageWindow(ORIGIN, BODY);
    install(win);
    await win.fetch(LIST, { headers: { 'ChatGPT-Account-Id': HEADER_A } });
    await flush();
    await win.fetch(DETAIL);
    await flush();

    expect(observations(win).map((m) => m.accountId)).toEqual([HEADER_A, null]);
  });
});
