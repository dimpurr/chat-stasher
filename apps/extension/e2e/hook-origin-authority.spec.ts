/**
 * W46 · **A healthy child frame must not speak for the origin.**
 *
 * ## The measured hole this reproduces
 *
 * One record is kept per origin (`cs_hook_v1:<origin>`). Until this branch,
 * `reason: null` from **any** document on that origin called `remove` on the
 * key, and the rule across frames was last-writer-wins. So a main document that
 * had just recorded a real failure (`hook-was-replaced`) could have that record
 * erased by a sibling iframe whose own hook happened to verify.
 *
 * That made an empty record observationally indistinguishable from the W43c
 * case (the handshake was taken back and nobody asked again). A suite whose
 * pages are a single document cannot see this class at all — which is why
 * `e2e/hook-half-install.spec.ts` stayed green while the hole stayed open.
 *
 * ## What the fixture does, and why the child is created late
 *
 * The main document is the W43c half-install: it lets the handshake finish,
 * then takes the XHR half back. The spec waits until that failure is **in
 * storage**, and only then builds a same-origin iframe whose hook stays whole.
 *
 * Creating the child after the record exists is the whole point. If the two
 * documents raced from `document_start`, a child that verified first would
 * clear nothing (there is not yet a record) and a child that verified second
 * is the wipe. Waiting for the record, then injecting the child, makes the
 * wipe the only thing that can happen next — so this spec is red on the tree
 * that lets any document speak, and green only when the child's observation
 * is dropped.
 *
 * ## What is asserted
 *
 * That both documents are the shapes they claim (main half-installed, child
 * whole, child not the top frame), that the origin's record is still
 * `hook-was-replaced` after the child has verified, and that the popup still
 * says so.
 */

import { expect } from '@playwright/test';
import { readStorage, test, type Extension } from './harness';

const ORIGIN = 'https://chatgpt.com';
const PAGE_PATH = '/c/w46-origin-authority';
const CHILD_PATH = '/c/w46-healthy-child';
const HOOK_STATUS_KEY = `cs_hook_v1:${ORIGIN}`;
const READY_MESSAGE = '__chat_stasher_main_ready__';

/**
 * The W43c half-install, as the top document: handshake first, then a Proxy
 * around the live XHR methods so the page's own reading of itself is
 * `xhrNative=true` without needing a pristine constructor.
 */
function mainPage(): string {
  return [
    '<!doctype html>',
    '<html lang="en"><head><meta charset="utf-8"><title>w46 half-install top</title></head>',
    '<body><p id="fixture">e2e fixture page</p>',
    '<script>',
    'window.__csHalf = { handshakeAnswered: false, replaced: false };',
    'window.addEventListener("message", (event) => {',
    '  const data = event.data;',
    `  if (!data || data.type !== '${READY_MESSAGE}' || !data.token) return;`,
    '  if (window.__csHalf.replaced) return;',
    '  window.__csHalf.handshakeAnswered = true;',
    '  XMLHttpRequest.prototype.open = new Proxy(XMLHttpRequest.prototype.open, {});',
    '  XMLHttpRequest.prototype.send = new Proxy(XMLHttpRequest.prototype.send, {});',
    '  window.__csHalf.replaced = true;',
    '  window.__csHalf.fetchNative = String(window.fetch).indexOf("[native code]") !== -1;',
    '  window.__csHalf.xhrOpenNative = String(XMLHttpRequest.prototype.open).indexOf("[native code]") !== -1;',
    '  window.__csHalf.isTop = window.top === window;',
    '});',
    '</scr' + 'ipt></body></html>',
  ].join('\n');
}

/**
 * A same-origin document that does nothing to the hook. Its own world reports
 * whether the wrappers are still ours, and whether this window is the top.
 */
function childPage(): string {
  return [
    '<!doctype html>',
    '<html lang="en"><head><meta charset="utf-8"><title>w46 healthy child</title></head>',
    '<body><p id="fixture">e2e fixture child</p>',
    '<script>',
    'window.__csChild = { handshakeAnswered: false };',
    'window.addEventListener("message", (event) => {',
    '  const data = event.data;',
    `  if (!data || data.type !== '${READY_MESSAGE}' || !data.token) return;`,
    '  window.__csChild.handshakeAnswered = true;',
    '  window.__csChild.fetchNative = String(window.fetch).indexOf("[native code]") !== -1;',
    '  window.__csChild.xhrOpenNative = String(XMLHttpRequest.prototype.open).indexOf("[native code]") !== -1;',
    '  window.__csChild.isTop = window.top === window;',
    '});',
    '</scr' + 'ipt></body></html>',
  ].join('\n');
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
      return route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: mainPage() });
    }
    if (pathname === CHILD_PATH) {
      return route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: childPage() });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  return { escaped };
}

test('a healthy child frame does not withdraw a half-install the main document recorded', async ({ ext }) => {
  const { escaped } = await serve(ext);

  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  await p.waitForFunction(() => (window as unknown as { __csHalf: { replaced: boolean } }).__csHalf.replaced);

  const half = await p.evaluate(() => (window as unknown as { __csHalf: Record<string, unknown> }).__csHalf);
  // The measured half-install, in the document the user is looking at.
  expect(half.fetchNative).toBe(false);
  expect(half.xhrOpenNative).toBe(true);
  expect(half.isTop).toBe(true);

  // 🔴 The failure is in storage *before* the child exists, so what happens
  //    next can only be a wipe or a keep — not a race about which document
  //    spoke first.
  const before = await waitForHookRecord(ext);
  expect(before.origin).toBe(ORIGIN);
  expect(before.platform).toBe('chatgpt');
  expect((before.reasons as Array<{ reason: string }>).map((row) => row.reason))
    .toEqual(['hook-was-replaced']);

  // Now a sibling document on the same origin, whose hook is whole.
  await p.evaluate((src) => {
    const frame = document.createElement('iframe');
    frame.setAttribute('title', 'w46 healthy child');
    frame.src = src;
    document.body.appendChild(frame);
  }, `${ORIGIN}${CHILD_PATH}`);

  // The iframe element appears before it has navigated, so waiting for a
  // second frame is not waiting for this document. Wait for the URL.
  await expect.poll(
    () => p.frames().some((frame) => frame.url() === `${ORIGIN}${CHILD_PATH}`),
  ).toBe(true);
  const child = p.frames().find((frame) => frame.url() === `${ORIGIN}${CHILD_PATH}`);
  expect(child, 'the healthy child frame was not created').toBeTruthy();

  await child!.waitForFunction(
    () => (window as unknown as { __csChild: { handshakeAnswered: boolean } }).__csChild.handshakeAnswered,
  );
  const childReport = await child!.evaluate(
    () => (window as unknown as { __csChild: Record<string, unknown> }).__csChild,
  );
  // 🔴 The child's hook really did verify. On the unfixed tree that answer is
  //    what removes the origin's record. If this were not true, a green result
  //    could mean "the child never spoke" rather than "the child was not
  //    allowed to speak".
  expect(childReport.fetchNative).toBe(false);
  expect(childReport.xhrOpenNative).toBe(false);
  expect(childReport.isTop).toBe(false);

  // The child's positive observation has been sent (or dropped). Give the
  // storage write a moment to land if it was going to; the main document's
  // next self-check is 5 s away, so a short poll cannot be rescued by it.
  await expectRecordToSurviveChild(ext);

  const after = await readStorage(ext, null);
  const kept = after[HOOK_STATUS_KEY] as Record<string, unknown> | undefined;
  expect(kept, 'the origin\'s hook record was cleared by a healthy child frame').toBeTruthy();
  expect((kept!.reasons as Array<{ reason: string }>).map((row) => row.reason))
    .toEqual(['hook-was-replaced']);

  await ext.context.unroute('**/*');
  const popup = await openPopup(ext);
  await expect(popup.locator('#notes')).toContainText(ORIGIN);
  const notes = await popup.locator('#notes').innerText();
  expect(notes).toContain('something replaced it afterwards');
  await popup.close();

  expect(escaped).toEqual([]);
});

async function openPopup(ext: Extension) {
  const page_ = await ext.context.newPage();
  await page_.goto(`chrome-extension://${ext.extensionId}/popup.html`, { waitUntil: 'domcontentloaded' });
  return page_;
}

async function waitForHookRecord(
  ext: Extension,
  timeoutMs = 20_000,
): Promise<Record<string, unknown>> {
  const deadline = Date.now() + timeoutMs;
  let last: Record<string, unknown> | undefined;
  while (Date.now() < deadline) {
    const storage = await readStorage(ext, null);
    const found = storage[HOOK_STATUS_KEY];
    if (found && typeof found === 'object') return found as Record<string, unknown>;
    last = found as Record<string, unknown> | undefined;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(
    `no hook record was written for ${ORIGIN} (the key held ${JSON.stringify(last ?? null)});`
    + ' the half-installed top document must be recorded before the child is created',
  );
}

/**
 * Poll storage for long enough that a child's `reason: null` would have
 * removed the key, and fail the moment it does.
 *
 * Two seconds is well under the hook's 5 s self-check, so a later re-assert
 * from the top document cannot put the record back and turn a wipe into a
 * pass.
 */
async function expectRecordToSurviveChild(ext: Extension, windowMs = 2_000): Promise<void> {
  const deadline = Date.now() + windowMs;
  while (Date.now() < deadline) {
    const storage = await readStorage(ext, null);
    if (!(HOOK_STATUS_KEY in storage)) {
      throw new Error(
        `the origin's hook record was cleared after a healthy child frame verified;`
        + ' a child frame\'s observation is a statement about that frame, not about the origin',
      );
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}
