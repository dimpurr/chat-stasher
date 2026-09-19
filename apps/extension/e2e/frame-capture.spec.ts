/**
 * W36b · **The document that made the request is the document that must have the
 * hook — and until this branch, a same-origin subframe never did.**
 *
 * ## The measured shape this reproduces
 *
 * The first real-Chrome acceptance of 2026-09-19 found, on a logged-in
 * `www.kimi.com/chat/<id>` page, a page-context `POST …/ChatService/ListMessages`
 * answered **200** with a top-level `{messages}` — the contract row's exact
 * envelope — and **zero** `__chat_stasher_capture__` window messages. On
 * `gemini.google.com/app/<id>` it found `window.fetch` and
 * `XMLHttpRequest.prototype.open` both printing `[native code]`.
 *
 * The review's blocker is that "zero capture messages" cannot be explained by a
 * delivery failure: `captureCandidate` posts **before** delivery
 * (`lib/page-hook.ts`), so a hook that ran always posts, whatever happens to the
 * message afterwards. The only explanations left are that the hook was never
 * installed **in the frame that made the request**, or that the request did not
 * go through `window.fetch`/`XMLHttpRequest` at all.
 *
 * This spec serves that first explanation as a page shape, and it is not a
 * hypothetical one: `allFrames` defaults to false, so **every same-origin
 * subframe of every matched origin** is in exactly the measured state, on every
 * load, until W36b. Measured here in a real Chromium before the fix (the red run
 * is in the report):
 *
 * ```
 * top   fetchNative=false xhrNative=false   ← the hook IS installed in the top document
 * frame fetchNative=true  xhrNative=true    ← and is absent from the frame that made the request
 * frame __chat_stasher_capture__ messages: 0
 * outbox: 0
 * ```
 *
 * which is the acceptance's reading, character for character: a 200, a
 * `{messages}` body, and not one capture message.
 *
 * ## What this pins, and what it does not claim
 *
 * It pins the path: the frame that issues the request must have the hook, the
 * capture message must be posted **on that frame's own window**, and the bundle
 * must reach the outbox with the id from the page's own URL. It does **not**
 * claim that the Kimi or Gemini tab the acceptance measured was a subframe
 * request — nothing here can see that tab. See the report for the one
 * measurement that separates the two remaining explanations, and for the one that
 * cannot be fixed in code at all (a document that predates the extension).
 */

import { expect } from '@playwright/test';
import { test, waitForOutbox, type Extension } from './harness';

/** The row `kimi-capture.spec.ts` drives, so the envelope is the one it pins. */
const ORIGIN = 'https://www.kimi.com';
const SESSION_ID = 'w36bFrameProbe-01';
const PAGE_PATH = `/chat/${SESSION_ID}`;
/** 🔴 A URL of its own, matched by `https://www.kimi.com/*` like any other. */
const FRAME_PATH = `/chat/${SESSION_ID}/view`;
const API_PATH = '/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages';

/** The body the acceptance measured: `messages` at the top level. */
const BODY = JSON.stringify({
  messages: [
    { id: 'm1', parentId: '', role: 'user', status: 'finished', blocks: [{ text: 'synthetic question' }] },
    { id: 'm2', parentId: 'm1', role: 'assistant', status: 'finished', blocks: [{ text: 'synthetic answer' }] },
  ],
});

/**
 * What a page (or frame) records about itself: whether the browser's own `fetch`
 * and `XHR.open` are still there, and every capture message posted **on this
 * window**. The listener is installed by the page's own first script — before any
 * request it makes — so "zero" here is a measurement of this window, not of the
 * page's own later reading of it.
 */
const SELF_REPORT = `
  window.__msgs = [];
  window.addEventListener('message', (event) => {
    const data = event.data;
    if (data && data.type === '__chat_stasher_capture__') window.__msgs.push(data.type);
  });
  window.__report = {
    fetchNative: String(window.fetch).includes('[native code]'),
    xhrOpenNative: String(XMLHttpRequest.prototype.open).includes('[native code]'),
    isTop: window.top === window,
  };`;

function topPage(): string {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>chat-stasher e2e fixture</title></head>
<body><p id="fixture">kimi fixture page</p>
<script>${SELF_REPORT}
  // The app's own chat view, in a frame of its own on the same origin.
  const frame = document.createElement('iframe');
  frame.src = '${FRAME_PATH}';
  document.body.appendChild(frame);
</script></body></html>`;
}

function framePage(): string {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>chat-stasher e2e fixture frame</title></head>
<body><p id="fixture">kimi fixture frame</p>
<script>${SELF_REPORT}
  window.__csCapture = (async () => {
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

async function serve(ext: Extension): Promise<{ escaped: string[] }> {
  const escaped: string[] = [];
  await ext.context.route('**/*', (route) => {
    escaped.push(route.request().url());
    return route.abort();
  });
  await ext.context.route(`${ORIGIN}/**`, (route) => {
    const { pathname } = new URL(route.request().url());
    if (pathname === '/favicon.ico') return route.fulfill({ status: 204, body: '' });
    if (pathname === PAGE_PATH) {
      return route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: topPage() });
    }
    if (pathname === FRAME_PATH) {
      return route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: framePage() });
    }
    return route.fulfill({ status: 200, contentType: 'application/json', body: BODY });
  });
  return { escaped };
}

interface FrameReport {
  fetchNative: boolean;
  xhrOpenNative: boolean;
  isTop: boolean;
}

test('a conversation fetched from a same-origin frame is captured, with the hook installed in that frame', async ({ ext }) => {
  const { escaped } = await serve(ext);

  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  // The frame is created by the top document's script, so it appears a tick later.
  await expect.poll(() => p.frames().length).toBe(2);
  const frame = p.frames()[1]!;

  const served = await frame.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );
  // The request really happened and really was answered with the measured body —
  // so nothing below is a statement about a request that never went out.
  expect(served).toEqual({ status: 200, bytes: BODY.length });

  const top = await p.evaluate(() => (window as unknown as { __report: FrameReport }).__report);
  const inFrame = await frame.evaluate(() => (window as unknown as { __report: FrameReport }).__report);
  // The premise of the whole case: this is a subframe, on a matched origin, and
  // its document is its own — not the top document.
  expect(top.isTop).toBe(true);
  expect(inFrame.isTop).toBe(false);

  // 🔴 Neither document has the browser's own functions: the hook is installed in
  //    the frame that made the request, which is the fact the acceptance's
  //    `[native code]` reading said was missing.
  expect(inFrame.fetchNative).toBe(false);
  expect(inFrame.xhrOpenNative).toBe(false);

  // 🔴 And the capture message is posted on **that frame's own window**, before
  //    delivery — so this is a statement about the hook, not about the outbox.
  //
  //    🔴 Polled, not read once. `hookedFetch` does not await `maybeCapture`
  //    (`lib/page-hook.ts:475-476`), so the message is posted in a task of its
  //    own: a single read straight after the request settles can legitimately see
  //    the 200 served above and an empty `__msgs`, and would then report a hook
  //    that is present as a hook that is missing. This is the same wait the
  //    outbox gets (`waitForOutbox`), in the polling idiom this file already uses
  //    for the frame appearing — and `toEqual` still pins **exactly one**, so a
  //    retry that found a second message would fail rather than pass.
  await expect
    .poll(
      () => frame.evaluate(() => (window as unknown as { __msgs: string[] }).__msgs),
      { timeout: 20_000 },
    )
    .toEqual(['__chat_stasher_capture__']);

  // The whole path, end to end: page world → bridge in the frame → background.
  // 🔴 Exactly one entry, not one per frame: the top document makes no request of
  //    its own, and a second copy would mean one conversation filed twice.
  const entries = await waitForOutbox(ext, (rows) => rows.length >= 1);
  expect(entries).toHaveLength(1);
  expect(entries[0]!.name).toBe(`kimi-${SESSION_ID}.json`);
  const bundle = JSON.parse(entries[0]!.payload) as Record<string, unknown>;
  expect(bundle.platform).toBe('kimi');
  expect(bundle.url).toBe(`${ORIGIN}${API_PATH}`);
  expect(bundle.method).toBe('POST');
  expect(bundle.status).toBe(200);

  expect(escaped).toEqual([]);
});
