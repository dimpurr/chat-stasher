/**
 * W519 · A Perplexity live capture is named only by the declared URL rule.
 * Synthetic response bodies deliberately carry an unrelated identifier so a
 * body fallback cannot silently choose which conversation the capture names.
 *
 * The second half of the change is the half that is easy to get backwards: the
 * file name may be refused, but the C21 account guard still needs a witness for
 * what the body claims the capture is, or a per-conversation string gets
 * promoted onto the cross-machine account axis.
 */
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { CAPTURE_MESSAGE } from '../lib/contract';
import { accountIdFromCapture } from '../lib/account-fingerprint';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

let resolveCaptureSessionId: typeof import('../entrypoints/background')['resolveCaptureSessionId'];
let claimedSessionId: typeof import('../entrypoints/background')['claimedSessionId'];

const ORIGIN = 'https://www.perplexity.ai';
const URL_ID = 'synthetic-url-thread';
const BODY_ID = 'synthetic-unrelated-body-id';
// Not dashy, so `isSessionShaped` does not reject it: only C21 can keep this one
// string off the account axis, which is the whole point of the second half.
const SELF_ID = 'synthetic-account-01';

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
    ({ resolveCaptureSessionId, claimedSessionId } = await import('../entrypoints/background'));
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

  it('still refuses the capture\'s own id as an account id when the name is refused', async () => {
    // 🔴 The regression this split exists to prevent: a body carrying ONE string
    // under both `session_id` and `user_id`. That string is the conversation's own
    // id, so C21 must refuse it as an account id. Naming is refused here (the URL
    // names no thread), and if the guard were handed that refusal instead of the
    // body's claim it would have nothing to exclude — and the per-conversation
    // string would be read as an account id, i.e. keyed onto the axis that
    // deduplicates two machines' archives of one account.
    const body = JSON.stringify({ entries: [], session_id: SELF_ID, user_id: SELF_ID });
    const captured = await capture(`${ORIGIN}/`, `${ORIGIN}/rest/thread/`, body);

    expect(captured).toBeDefined();
    const payload = captured.payload;
    // The name is refused: nothing in the URL identifies a thread.
    expect(resolveCaptureSessionId(payload)).toBeNull();
    // The claim is not: the body still says what this capture is.
    expect(claimedSessionId(payload)).toBe(SELF_ID);
    // ⇒ and that is what keeps the account axis clean.
    expect(accountIdFromCapture(payload, claimedSessionId(payload)))
      .toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
    // 🔴 Pinned as the failure mode: with `null` in the guard's place the very
    // same capture reports an account id, which is the mis-attribution.
    expect(accountIdFromCapture(payload, null))
      .toEqual({ kind: 'id', id: SELF_ID, source: 'response-body-platform-uid' });
  });
});
