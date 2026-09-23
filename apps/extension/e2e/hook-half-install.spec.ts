/**
 * W43c · **The page that takes its transport back after the handshake.**
 *
 * ## The measured state this reproduces
 *
 * Build 0.1.0.11, loaded in a real, logged-in Chrome, on the one supported
 * origin whose hook does not survive the page load — with the tab **fully
 * reloaded** after the extension was updated, so "this document predates the
 * extension" is not an explanation:
 *
 * ```
 * after reload:  fetchNative=false   xhrNative=true   hookWhole=false
 * cs_hook_v1:*   {}        (no record at all, for any origin)
 * popup #notes   contains no hook sentence
 * ```
 *
 * `window.fetch` is the extension's wrapper, `XMLHttpRequest.prototype.open` is
 * **not** ours, and nothing anywhere records that. The two facts that make the
 * page silent are measured rather than assumed:
 *
 *  · W43's own two checks run **once** — the read-back at install, and the
 *    answer to the bridge's probe a moment later. A page that replaces the
 *    transport *after* that answer is never asked again, so the hook's last
 *    observation stays "whole" while the page's capture path is gone.
 *  · A page that replaces it *before* the answer is already recorded
 *    (`e2e/scratch` measurement in the W43c report: the probe returns
 *    `hook-was-replaced`, and the unanswered fallback adds `hook-did-not-run`).
 *    So the silence cannot be an install that never took: the install-time
 *    read-back would have reported `hook-did-not-take`.
 *
 * ## What the fixture page does, and why it does it that way
 *
 * The page waits for the hook's own answer to the bridge's probe — the only
 * moment that proves the handshake finished — and *then* wraps
 * `XMLHttpRequest.prototype.open`/`send` in a `Proxy`.
 *
 * A `Proxy` is deliberate. `Function.prototype.toString` on a proxy (or a bound
 * function) returns `"function () { [native code] }"`, so the page's own
 * reading of itself matches the real measurement's `xhrNative=true` **without**
 * the page having to obtain a pristine constructor from somewhere — which, in
 * this browser, it cannot: a frame this page creates is injected by the
 * extension synchronously with its creation (measured, `SCRATCH 6` in the W43c
 * report), so every realm this page can reach is already wrapped. What the
 * measurement's `[native code]` reading establishes is therefore narrower than
 * it looks: it separates "our wrapper is gone" from "our wrapper is there", and
 * that is exactly the fact the hook re-reads.
 *
 * ## What is asserted
 *
 * That the half-install is **recorded** (`chrome.storage.local`, under
 * `cs_hook_v1:<origin>`, reason `hook-was-replaced`) and **shown** (the popup's
 * note). Both are checked against the state the page's own world reports, so a
 * fixture that quietly stopped being the measured shape fails here rather than
 * passing for the wrong reason.
 */

import { expect } from '@playwright/test';
import { readStorage, test, type Extension } from './harness';

const ORIGIN = 'https://chatgpt.com';
const PAGE_PATH = '/c/w43c-half-install';
const HOOK_STATUS_KEY = `cs_hook_v1:${ORIGIN}`;
const READY_MESSAGE = '__chat_stasher_main_ready__';

/**
 * The fixture page: it lets the handshake finish, then takes the XHR half back.
 *
 * The replacement wraps what is *there* rather than restoring a browser
 * function the page cannot reach (see the file header), which keeps the page's
 * own XHR working — so what the hook loses is capture, not the page's traffic.
 */
function page(): string {
  return [
    '<!doctype html>',
    '<html lang="en"><head><meta charset="utf-8"><title>w43c half-install fixture</title></head>',
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
      return route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: page() });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  return { escaped };
}

test('a page that takes the XHR half back after the handshake is recorded, and the popup says so', async ({ ext }) => {
  const { escaped } = await serve(ext);

  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  await p.waitForFunction(() => (window as unknown as { __csHalf: { replaced: boolean } }).__csHalf.replaced);

  // The measured shape, stated by the page's own world rather than assumed: our
  // fetch wrapper is still installed, and the live XHR prototype is no longer
  // the function this hook installed.
  const half = await p.evaluate(() => (window as unknown as { __csHalf: Record<string, unknown> }).__csHalf);
  expect(half.fetchNative).toBe(false);
  expect(half.xhrOpenNative).toBe(true);

  // 🔴 The record the extension owes the user for this page. It is written by
  //    the hook's own re-check, which is why this assertion is red on the tree
  //    that only checked at install and at the probe.
  const record = await waitForHookRecord(ext);
  expect(record.origin).toBe(ORIGIN);
  expect(record.platform).toBe('chatgpt');
  expect((record.reasons as Array<{ reason: string }>).map((row) => row.reason))
    .toEqual(['hook-was-replaced']);

  // And it reaches the surface the user has: the popup's notes.
  // 🔴 The catch-all that proves no request left the machine aborts *every*
  //    request, the popup's own scripts included — so it comes off first. An
  //    empty `#notes` from a popup that never ran is exactly the state this test
  //    exists to tell apart from a popup with nothing to say.
  await ext.context.unroute('**/*');
  const popup = await openPopup(ext);
  await expect(popup.locator('#notes')).toContainText(ORIGIN);
  const notes = await popup.locator('#notes').innerText();
  expect(notes).toContain('the page took that global back afterwards');
  // 🔴 W69 · And it must not read as breakage. This page took the XHR half back,
  //    which is an identity change and not a measurement that captures stopped —
  //    whether the replacement calls through to ours is not measured here — so the
  //    verdict the popup owes the user is unknown, in so many words. On the tree
  //    this change is for, the same record is reported as the hook not working.
  expect(notes).toContain('Whether capture still works there is unknown.');
  expect(notes).not.toContain('is not being archived');
  await popup.close();

  expect(escaped).toEqual([]);
});

async function openPopup(ext: Extension) {
  const page_ = await ext.context.newPage();
  await page_.goto(`chrome-extension://${ext.extensionId}/popup.html`, { waitUntil: 'domcontentloaded' });
  return page_;
}

/** Poll `chrome.storage.local` until the origin's record appears, then return it. */
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
    + ' a page whose live XHR half is no longer the hook\'s wrapper must be recorded',
  );
}
