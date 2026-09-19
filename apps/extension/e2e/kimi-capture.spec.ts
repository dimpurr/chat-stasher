/**
 * W36 · Kimi, end to end: the response the first real-Chrome acceptance measured,
 * driven through the whole capture path in a real Chromium.
 *
 * The acceptance's reading was "the response is exactly the declared shape and
 * nothing appeared in the stage", so the question is which check in
 * `captureCandidate` drops it — method, path hint, shape, session id,
 * dedupe/fingerprint, or delivery. This spec answers it by construction: it
 * serves one page at `/chat/<id>`, makes the page POST to
 * `/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages`, answers with
 * `{messages: […]}`, and requires the bundle to reach the outbox. Every check
 * the acceptance listed is between those two ends, and the assertions on the
 * bundle name the ones that could have gone wrong (the platform and session id
 * in the file name; the URL, method and status in the payload).
 *
 * 🔴 A zero here would be a real zero. `waitForOutbox` returns what it read, and
 *    `readOutbox` throws rather than returning `[]` when the worker is gone — so
 *    "nothing was captured" cannot be read as "I could not ask".
 */

import { expect } from '@playwright/test';
import { readOutbox, waitForOutbox, test, type Extension } from './harness';

/** lib/contract.ts's kimi row: one origin, POST, `ChatService/ListMessages`. */
const ORIGIN = 'https://www.kimi.com';
/** The id shape the row's own comment describes: alphanumeric, `-`/`_` allowed. */
const SESSION_ID = 'w36KimiProbe-01';
const PAGE_PATH = `/chat/${SESSION_ID}`;
const API_PATH = '/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages';

/**
 * The body the acceptance measured: `messages` at the top level, three of them.
 * Field names are the ones the row's comment records as measured
 * (`id, parentId, role, status, blocks, …`); none of them is required by the
 * shape gate, which is the point of writing a realistic body rather than a
 * minimal one.
 */
const BODY = JSON.stringify({
  messages: [
    { id: 'm1', parentId: '', role: 'user', status: 'finished', blocks: [{ text: 'synthetic question' }], createTime: '2026-09-19T00:00:00Z' },
    { id: 'm2', parentId: 'm1', role: 'assistant', status: 'finished', blocks: [{ text: 'synthetic answer' }], createTime: '2026-09-19T00:00:01Z' },
    { id: 'm3', parentId: 'm2', role: 'assistant', status: 'finished', blocks: [{ text: 'synthetic follow-up' }], createTime: '2026-09-19T00:00:02Z' },
  ],
});

function page(): string {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>chat-stasher e2e fixture</title></head>
<body><p id="fixture">kimi fixture page</p>
<script>
  window.__csCapture = (async () => {
    // The page's own request, made the way the app makes it: POST, JSON body
    // carrying the chat id (the row's comment: the id is in the body, not the
    // URL), and the page's own credentials.
    const response = await fetch('${API_PATH}', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ chat_id: '${SESSION_ID}' }),
    });
    const text = await response.text();
    return { status: response.status, bytes: text.length };
  })();
</script></body></html>`;
}

async function serve(ext: Extension): Promise<{ escaped: string[]; api: string[] }> {
  const escaped: string[] = [];
  const api: string[] = [];
  await ext.context.route('**/*', (route) => {
    escaped.push(route.request().url());
    return route.abort();
  });
  await ext.context.route(`${ORIGIN}/**`, (route) => {
    const request = route.request();
    const { pathname } = new URL(request.url());
    if (pathname === '/favicon.ico') return route.fulfill({ status: 204, body: '' });
    if (pathname === PAGE_PATH) {
      return route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: page() });
    }
    api.push(`${request.method()} ${pathname}`);
    return route.fulfill({ status: 200, contentType: 'application/json', body: BODY });
  });
  return { escaped, api };
}

test('a Kimi ListMessages response on a /chat/<id> page becomes one bundle', async ({ ext }) => {
  const { escaped, api } = await serve(ext);

  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  const served = await p.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );
  expect(served).toEqual({ status: 200, bytes: BODY.length });

  const entries = await waitForOutbox(ext, (rows) => rows.length >= 1);
  expect(entries).toHaveLength(1);
  const entry = entries[0]!;

  // The session id came from the page's own URL — the row's `sessionIdPatterns`
  // — and it is already a safe file-name fragment (contract.ts's identity map,
  // so nothing is renamed on the way to disk).
  expect(entry.name).toBe(`kimi-${SESSION_ID}.json`);

  const bundle = JSON.parse(entry.payload) as Record<string, unknown>;
  expect(bundle.platform).toBe('kimi');
  expect(bundle.sessionId).toBe(SESSION_ID);
  expect(bundle.url).toBe(`${ORIGIN}${API_PATH}`);
  expect(bundle.method).toBe('POST');
  expect(bundle.status).toBe(200);
  // The body is passed through untouched: the archive holds the response, not our
  // reading of it.
  expect(bundle.raw).toEqual({ text: BODY, bytes: Buffer.byteLength(BODY, 'utf8') });

  // The page's request was the only one to that origin, and nothing left the machine.
  expect(api).toEqual([`POST ${API_PATH}`]);
  expect(escaped).toEqual([]);
});

test('a response that is not the messages envelope is refused rather than captured', async ({ ext }) => {
  // The row's shape gate is `requiredPaths: ['messages']`. This body is valid
  // JSON, 2xx, POST, right path — and carries the *feed* envelope instead. A
  // capture here would file a conversation-list preview as the conversation.
  const { escaped } = await serve(ext);
  await ext.context.route(`${ORIGIN}${API_PATH}`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ data: { messages: [] } }),
  }));

  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  const served = await p.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );
  // The page really did receive that response — so "nothing captured" is a
  // statement about the gate, not about a request that never happened.
  expect(served).toEqual({ status: 200, bytes: JSON.stringify({ data: { messages: [] } }).length });

  expect(await readOutbox(ext)).toEqual([]);
  expect(escaped).toEqual([]);
});
