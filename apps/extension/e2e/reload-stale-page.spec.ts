/**
 * W36 · A page that was open across an extension reload — the state the first
 * real-Chrome acceptance's Gemini and Kimi tabs were in, and the reason a
 * response that matches the contract row perfectly never reached the stage.
 *
 * ## What is measured here
 *
 * The extension is reloaded from inside its own service worker (the same event
 * as loading a new build by hand, which is how the acceptance's build 0.1.0.8 was
 * loaded), with the platform page left open. What the page looks like afterwards
 * is the whole point:
 *
 *  · `window.fetch` is **still wrapped** — the previous build's MAIN-world hook
 *    is page JavaScript and survives the reload. So the page is armed, and the
 *    acceptance's own reading ("the wrapper may be the site's own") cannot tell
 *    the two apart;
 *  · the page's request is served normally, so nothing about the response is what
 *    stops the capture;
 *  · the capture is dropped at **delivery**: the isolated bridge's
 *    `runtime.sendMessage` targets an extension context that no longer exists.
 *
 * Before this task that drop was silent: an empty `catch` whose comment is right
 * that the page must not be disturbed and wrong about the truth. The fix — the
 * drop is **named**, once per page, in the page's own console — is asserted in
 * `tests/w36-page-link.test.ts`, and the reason this spec stops at the mechanism
 * is in the last comment below (an e2e driver cannot see it, and cannot reach the
 * extension after the reload either).
 *
 * 🔴 Chrome cannot re-inject content scripts into an open document, so the
 *    capture itself cannot be recovered from inside the page — that is stated
 *    rather than papered over. What is not allowed is losing a real conversation
 *    without saying so.
 */

import { expect } from '@playwright/test';
import { fixture, test, waitForOutbox, type Extension } from './harness';

const SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const PAGE_PATH = `/c/${SESSION_ID}`;
const API_PATH = `/backend-api/conversation/${SESSION_ID}`;
const BODY = fixture('chatgpt-conversation.json');

async function serve(ext: Extension): Promise<void> {
  await ext.context.route('**/*', (route) => {
    // The extension's own pages are read below, not traffic to a chat site.
    if (route.request().url().startsWith('chrome-extension://')) return route.continue();
    return route.abort();
  });
  await ext.context.route('https://chatgpt.com/**', (route) => {
    const { pathname } = new URL(route.request().url());
    if (pathname === '/favicon.ico') return route.fulfill({ status: 204, body: '' });
    if (pathname === PAGE_PATH) {
      return route.fulfill({
        status: 200,
        contentType: 'text/html; charset=utf-8',
        body: fixture('platform-page.html').replaceAll('__CS_API_PATH__', API_PATH),
      });
    }
    return route.fulfill({ status: 200, contentType: 'application/json', body: BODY });
  });
}

test('a page left open across an extension reload keeps the old hook, and its captures have nowhere to go', async ({ ext }) => {
  await serve(ext);

  const page = await ext.context.newPage();
  await page.goto(`https://chatgpt.com${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  await page.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );

  // The link is alive: the first request is captured and queued.
  const before = await waitForOutbox(ext, (rows) => rows.length >= 1);
  expect(before).toHaveLength(1);

  // The user loads a new build. The page is not reloaded — which is exactly the
  // state a tab is in when it has been open for days.
  await ext.worker.evaluate(() => (globalThis as unknown as { chrome: { runtime: { reload(): void } } }).chrome.runtime.reload());
  await new Promise((resolve) => setTimeout(resolve, 3000));

  // The page is still armed: the *previous* build's hook is in the page world.
  const hookInstalled = await page.evaluate(
    () => !String(window.fetch).includes('[native code]'),
  );
  expect(hookInstalled).toBe(true);

  // And it still works as far as the page is concerned: the request is served.
  const again = await page.evaluate(async (apiPath: string) => {
    const response = await fetch(apiPath, { headers: { accept: 'application/json' } });
    return { status: response.status, bytes: (await response.text()).length };
  }, API_PATH);
  expect(again).toEqual({ status: 200, bytes: BODY.length });

  // 🔴 The drop itself cannot be read from here, and that is stated rather than
  //    worked around: reloading the extension from inside its own worker leaves
  //    this harness unable to reach the extension again — `chrome-extension://`
  //    navigations are refused afterwards (measured) — so the outbox cannot be
  //    counted a second time. What is pinned here is the mechanism the drop
  //    rests on; the naming of it is asserted in `tests/w36-page-link.test.ts`,
  //    where the isolated world's console IS observable (the e2e driver does not
  //    see isolated-world console output at all: a marker logged by the bridge at
  //    document_start never reaches `page.on('console')`).
  expect(page.isClosed()).toBe(false);
});
