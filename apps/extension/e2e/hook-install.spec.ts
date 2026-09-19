/**
 * W43 · **A document on a supported origin that made the user's own request, and
 * that neither copy of the hook had ever reached.**
 *
 * ## The measured shape this reproduces
 *
 * The 2026-09-19 acceptance found two supported origins where the hook was
 * absent in the document that made the request and **nothing anywhere said so**.
 * A fixture battery (in the W43 report) then showed that the page's own
 * configuration is not the reason — a declarative MAIN-world hook at
 * `document_start` runs before any page script, and it installs on every
 * Trusted Types shape tried, `require-trusted-types-for 'script'` included. What
 * was left is a document the browser never injected into at all.
 *
 * Two of those shapes are this extension's own to fix, and they are the ones
 * measured here, in a page that enforces Trusted Types throughout:
 *
 *  · an **`about:srcdoc` subframe** of a matched origin gets no content script
 *    unless `match_origin_as_fallback` is set — so a request issued from it was
 *    answered 200 and produced **zero** capture messages and **zero** outbox
 *    rows;
 *  · and once one *is* injected there, such a frame's `location.origin` is the
 *    string `"null"` while `window.origin` is the inherited `https://…`. With the
 *    URL-derived value, `postMessage(data, "null")` **throws** (measured in this
 *    browser), so the hook could not post a capture or answer a probe even
 *    though it was installed, and every request the frame made failed our own
 *    `parsed.origin !== pageOrigin` gate.
 *
 * ## Why Trusted Types is in the fixture at all
 *
 * Because it is the page shape the acceptance blamed, and a fixture that dropped
 * it would leave the question it raised unanswered. The page here carries a
 * nonce-only `script-src` and `require-trusted-types-for 'script'`, and the
 * frame's own script carries that nonce — so this spec also pins, in a real
 * browser, that a page enforcing Trusted Types is one the declarative hook
 * installs *and captures* on. If a future change made Trusted Types the cause
 * after all, this file goes red.
 */

import { expect } from '@playwright/test';
import { test, waitForOutbox, readStorage, writeStorage, type Extension } from './harness';

/** The row `capture.spec.ts` drives, so the body and the name are the same fixture. */
const SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const PAGE_PATH = `/c/${SESSION_ID}`;
const API_PATH = `/backend-api/conversation/${SESSION_ID}`;
const BODY = JSON.stringify({ mapping: { 'node-1': { id: 'node-1' } }, current_node: 'node-1' });
const NONCE = 'w43srcdoc';
const ORIGIN = 'https://chatgpt.com';
const HOOK_STATUS_KEY = `cs_hook_v1:${ORIGIN}`;

/**
 * The fixture page: a Trusted Types + nonce-only-CSP document whose own body
 * builds the subframe, and a subframe that fetches the conversation.
 *
 * The frame is `srcdoc`, and its script carries the same nonce as the page's —
 * both because a srcdoc document inherits this page's policy, and because the
 * point is that the *extension* is what is being measured here, not a page that
 * has been given an easier environment than the real one.
 */
function page(): string {
  const inner = [
    '<!doctype html><html><body>',
    `<script nonce="${NONCE}">`,
    'window.__csSeen = [];',
    "window.addEventListener('message', (event) => {",
    '  const type = event.data && event.data.type;',
    "  if (typeof type === 'string' && type.indexOf('__chat_stasher') === 0) window.__csSeen.push(type);",
    '});',
    'window.__csFrame = {',
    "  href: location.href,",
    "  locationOrigin: location.origin,",
    "  environmentOrigin: String(window.origin),",
    "  fetchNative: String(window.fetch).indexOf('[native code]') !== -1,",
    "  xhrOpenNative: String(XMLHttpRequest.prototype.open).indexOf('[native code]') !== -1,",
    '};',
    'window.__csServed = fetch(' + JSON.stringify(API_PATH) + ')',
    '  .then((response) => response.text())',
    '  .then((text) => ({ status: 200, bytes: text.length }))',
    "  .catch((err) => String(err).slice(0, 80));",
    '</scr' + 'ipt></body></html>',
  ].join('\n');

  return [
    '<!doctype html>',
    '<html lang="en"><head><meta charset="utf-8"><title>chat-stasher e2e fixture</title></head>',
    '<body><p id="fixture">e2e fixture page</p>',
    `<script nonce="${NONCE}">`,
    'window.__csTop = {',
    "  fetchNative: String(window.fetch).indexOf('[native code]') !== -1,",
    "  xhrOpenNative: String(XMLHttpRequest.prototype.open).indexOf('[native code]') !== -1,",
    '};',
    'const frame = document.createElement("iframe");',
    'frame.setAttribute("title", "w43 subframe");',
    // 🔴 `HTMLIFrameElement.srcdoc` is itself a TrustedHTML sink, so on this page
    //    the assignment below is refused unless it goes through a policy — which
    //    is exactly what a real page under this policy does. It is worth being
    //    explicit about: the page is not weakened for the fixture, the fixture
    //    does what the site does.
    'const policy = window.trustedTypes && trustedTypes.createPolicy',
    "  ? trustedTypes.createPolicy('w43-fixture', { createHTML: (html) => html, createScript: (src) => src })",
    '  : null;',
    'const trusted = (html) => (policy ? policy.createHTML(html) : html);',
    // 🔴 The inner document is a *string inside* this script element, and the
    //    HTML parser does not read JavaScript: a literal `</script>` in it would
    //    close this block early and the rest of the page would be text. The JSON
    //    escape `\/` is the same character to the JS engine and invisible to the
    //    parser.
    'frame.srcdoc = trusted(' + JSON.stringify(inner).replace(/<\//g, '<\\/') + ');',
    'document.body.appendChild(frame);',
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
      return route.fulfill({
        status: 200,
        headers: {
          'content-type': 'text/html; charset=utf-8',
          // The page shape the acceptance blamed: no 'unsafe-inline', a nonce
          // only, and Trusted Types enforcement on top of it.
          'content-security-policy':
            `default-src 'self'; script-src 'self' 'nonce-${NONCE}'; require-trusted-types-for 'script'`,
        },
        body: page(),
      });
    }
    if (pathname === API_PATH) {
      return route.fulfill({ status: 200, contentType: 'application/json', body: BODY });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  return { escaped };
}

/** The subframe, found by the URL the browser gives a srcdoc document. */
function subframe(page_: Awaited<ReturnType<Extension['context']['newPage']>>) {
  const frame = page_.frames().find((candidate) => candidate.url() === 'about:srcdoc');
  expect(frame, 'the fixture subframe was not created').toBeTruthy();
  return frame!;
}

test('the hook reaches a srcdoc subframe on a Trusted Types page, and its request is captured', async ({ ext }) => {
  const { escaped } = await serve(ext);

  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  // The frame is built by the page's own script; wait for that script to run.
  await p.waitForFunction(() => document.querySelector('iframe') !== null);

  const frame = subframe(p);
  const served = await frame.evaluate(
    () => (window as unknown as { __csServed?: Promise<unknown> }).__csServed,
  );
  // The page's own request is answered — and, before the fix, that was all that
  // happened: a 200 with a readable body and not one capture message anywhere.
  expect(served).toEqual({ status: 200, bytes: BODY.length });

  const report = await frame.evaluate(
    () => (window as unknown as {
      __csFrame: {
        href: string;
        locationOrigin: string;
        environmentOrigin: string;
        fetchNative: boolean;
        xhrOpenNative: boolean;
      };
    }).__csFrame,
  );
  // 🔴 The document shape the whole fix is about, stated rather than assumed.
  expect(report.href).toBe('about:srcdoc');
  expect(report.locationOrigin).toBe('null');
  expect(report.environmentOrigin).toBe(ORIGIN);

  // 🔴 The hook is in the frame that made the request. On the unfixed tree both
  //    of these are `true`: no content script is injected into such a frame at
  //    all, and the page world still holds the browser's own functions.
  expect(report.fetchNative).toBe(false);
  expect(report.xhrOpenNative).toBe(false);

  // 🔴 And the capture reaches the stage, from the frame's own window.
  const entries = await waitForOutbox(ext, (rows) => rows.length >= 1);
  expect(entries).toHaveLength(1);
  expect(entries[0]!.name).toBe(`chatgpt-${SESSION_ID}.json`);
  const seen = await frame.evaluate(
    () => (window as unknown as { __csSeen: string[] }).__csSeen,
  );
  expect(seen.filter((type) => type === '__chat_stasher_capture__')).toHaveLength(1);

  // The page verified its own hook, so nothing is recorded against this origin.
  const storage = await readStorage(ext, null);
  expect(Object.keys(storage)).not.toContain(HOOK_STATUS_KEY);

  expect(escaped).toEqual([]);
});

test('a page whose hook did not install is visible in the popup, and a verifying page clears it', async ({ ext }) => {
  // 🔴 The state this half exists for: the extension knows, from a page's own
  //    report, that live capture is not installed on an origin — and until W43
  //    there was no surface anywhere that could say so. Writing the record
  //    directly is the point: the popup's job is to *show* it, and the writing
  //    itself is covered by the unit suite and by the capture test above.
  await writeStorage(ext, { [HOOK_STATUS_KEY]: hookRecord() });

  // 🔴 Before `serve`, which installs a catch-all that aborts every request: the
  //    popup is not network traffic, and a context-wide route would take its own
  //    script down with it — leaving a `#notes` element that is empty because
  //    nothing ran, which is exactly what this test is trying to tell apart.
  const popup = await openPopup(ext);
  // The popup paints its notes after it has read storage, so this waits for the
  // painting rather than for the navigation.
  await expect(popup.locator('#notes')).toContainText(ORIGIN);
  const notes = await popup.locator('#notes').innerText();
  expect(notes).toContain('Live capture is not installed');
  expect(notes).toContain('chatgpt');
  await popup.close();

  const { escaped } = await serve(ext);

  // Now a real page on that origin, whose hook verifies — the only thing that
  // withdraws the record, and the reason there is no button for it.
  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  await p.waitForFunction(() => document.querySelector('iframe') !== null);
  await waitForRecordCleared(ext);
  // The same reason as above, in reverse: the popup reads the record back out of
  // storage, and it must not be the abort route that keeps it from running.
  await ext.context.unroute('**/*');

  const after = await openPopup(ext);
  // Wait for the popup to have painted something before reading the absence —
  // an empty `#notes` also does not contain the sentence.
  await expect(after.locator('#notes')).not.toBeEmpty();
  expect(await after.locator('#notes').innerText()).not.toContain('Live capture is not installed');
  await after.close();

  expect(escaped).toEqual([]);
});

/** One observation, in the shape `lib/hook-status.ts` stores. */
function hookRecord(): Record<string, unknown> {
  return {
    origin: ORIGIN,
    platform: 'chatgpt',
    reasons: [{ reason: 'hook-did-not-run', at: Date.now() }],
    at: Date.now(),
  };
}

async function openPopup(ext: Extension) {
  const page = await ext.context.newPage();
  await page.goto(`chrome-extension://${ext.extensionId}/popup.html`, { waitUntil: 'domcontentloaded' });
  return page;
}

async function waitForRecordCleared(ext: Extension, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const storage = await readStorage(ext, null);
    if (!(HOOK_STATUS_KEY in storage)) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`the hook record for ${ORIGIN} was never cleared by a verifying page`);
}
