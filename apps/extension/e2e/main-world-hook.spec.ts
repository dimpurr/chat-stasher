/**
 * W36 · The MAIN-world hook on a page that enforces Trusted Types and serves a
 * strict `script-src` — the page shape the first real-Chrome acceptance blamed
 * for the hook being absent on gemini.google.com.
 *
 * ## What this pins
 *
 * The extension has **two** ways to get `installPageFetchHook` into the page
 * world, and they fail differently:
 *
 *  · the **declarative** registration (`entrypoints/dw-fetch-main.content.ts`,
 *    `world: 'MAIN'`, emitted as `"world": "MAIN"` in the built manifest) — a
 *    browser-injected script that a page's CSP has no say over;
 *  · the **injected `<script>`** fallback in the isolated bridge, for a page
 *    whose MAIN-world script never answered the probe.
 *
 * This spec serves a page with `require-trusted-types-for 'script'` and a
 * `script-src` that only allows a nonce, and asserts the first path works there:
 * the hook is in the **page's own world** before the page's script runs, and a
 * response the page fetches is captured end to end into the outbox. It also
 * records the page's own attempt to inject an inline script, which is exactly
 * what the second path does — so the reason the fallback cannot work on such a
 * page is measured here rather than asserted (see the W12 unit case for the
 * bridge's behaviour when it does fail).
 *
 * ## Why a capture, and not just `window.fetch`'s source
 *
 * `[native code]` is ambiguous — a bound native function also prints it, and a
 * patch installed in the *isolated* world is invisible to a page-world read. A
 * bundle in the outbox has no such ambiguity: it can only exist if the hook ran
 * in the page's world, matched the contract row, and the bridge relayed it.
 */

import { expect } from '@playwright/test';
import { fixture, waitForOutbox, test, type Extension } from './harness';

/** chatgpt · the same row `capture.spec.ts` drives, so the body is the same fixture. */
const SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const PAGE_PATH = `/c/${SESSION_ID}`;
const API_PATH = `/backend-api/conversation/${SESSION_ID}`;
const BODY = fixture('chatgpt-conversation.json');
const NONCE = 'w36mainhook';

/**
 * The fixture page, with two additions: its own script carries the CSP nonce (so
 * the page can run at all under a strict `script-src`), and it reports what the
 * page world sees — whether `fetch` is the extension's wrapper, and whether this
 * document refuses an inline `<script>` this page builds itself.
 */
function page(): string {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>chat-stasher e2e fixture</title></head>
<body><p id="fixture">e2e fixture page</p>
<script nonce="${NONCE}">
  window.__csPage = (() => {
    let inlineRefused = null;
    try {
      const probe = document.createElement('script');
      probe.textContent = 'window.__csInline = true;';
      (document.documentElement || document.head).appendChild(probe);
      probe.remove();
    } catch (err) { inlineRefused = String(err && err.name); }
    return {
      // The page's own reading of the page's own world.
      fetchNative: String(window.fetch).includes('[native code]'),
      xhrOpenNative: String(XMLHttpRequest.prototype.open).includes('[native code]'),
      inlineRefused,
    };
  })();
  window.__csCapture = (async () => {
    const response = await fetch('${API_PATH}', { headers: { accept: 'application/json' } });
    const text = await response.text();
    return { status: response.status, bytes: text.length };
  })();
</script></body></html>`;
}

interface PageReport {
  fetchNative: boolean;
  xhrOpenNative: boolean;
  inlineRefused: string | null;
}

async function serve(ext: Extension): Promise<{ escaped: string[] }> {
  const escaped: string[] = [];
  await ext.context.route('**/*', (route) => {
    escaped.push(route.request().url());
    return route.abort();
  });
  await ext.context.route('https://chatgpt.com/**', (route) => {
    const { pathname } = new URL(route.request().url());
    if (pathname === '/favicon.ico') return route.fulfill({ status: 204, body: '' });
    if (pathname === PAGE_PATH) {
      return route.fulfill({
        status: 200,
        headers: {
          'content-type': 'text/html; charset=utf-8',
          // 🔴 The page shape the acceptance blamed: no 'unsafe-inline', and
          //    Trusted Types enforcement on top of it.
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

test('the declarative MAIN-world hook installs on a Trusted Types + strict-CSP page, and captures there', async ({ ext }) => {
  const { escaped } = await serve(ext);

  const p = await ext.context.newPage();
  await p.goto(`https://chatgpt.com${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });

  const served = await p.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );
  expect(served).toEqual({ status: 200, bytes: BODY.length });

  const report = await p.evaluate(
    () => (window as unknown as { __csPage: PageReport }).__csPage,
  );
  // The environment this case exists for: this document refuses an inline script
  // the page builds itself — `HTMLScriptElement.textContent` is a TrustedScript
  // sink. So the bridge's `<script>` fallback could not install a hook here, and
  // anything that works below is the declarative registration doing it.
  expect(report.inlineRefused).toBe('TypeError');
  // The hook is in the page's OWN world, before the page's script ran: neither
  // `fetch` nor `XMLHttpRequest.prototype.open` is the browser's own function.
  expect(report.fetchNative).toBe(false);
  expect(report.xhrOpenNative).toBe(false);

  // And the capture really happened: page world → bridge → background → outbox.
  const entries = await waitForOutbox(ext, (rows) => rows.length >= 1);
  expect(entries).toHaveLength(1);
  expect(entries[0]!.name).toBe(`chatgpt-${SESSION_ID}.json`);

  expect(escaped).toEqual([]);
});
