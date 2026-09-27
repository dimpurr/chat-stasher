/**
 * W228 - Perplexity, end to end: the content response the page fetches on a
 * `/search/<slug>` page, driven through the whole live-capture path in a real
 * Chromium, without a live account.
 *
 * This is the Perplexity half of Kimi's W36 spec, and it closes the one
 * platform-shaped capture gap the unit suite cannot reach: a real browser, a
 * real content script, the real outbox. It serves one page at
 * `/search/<slug>`, makes the page GET `/rest/thread/<slug>?<the pinned
 * five-key set>` (the route and query the contract row registers -
 * `apps/extension/lib/contract.ts` `pathHints: ['/rest/thread/']`, `methods:
 * ['GET']`), answers with an `entries` envelope, and requires the bundle to
 * reach the outbox. Every check the row's shape gate can go wrong on - method,
 * origin, body shape, session id, file name - is asserted between those two
 * ends, exactly as the row that this platform is measured against is asserted.
 *
 * No real Perplexity account is needed and none is used: the `www.perplexity.ai`
 * origin is served entirely by Playwright route interception (the fixture
 * page and the API response), and the every-request catch-all proves nothing
 * leaves the machine. This spec is a statement about our capture path, not
 * about a server that may or may not be reachable.
 */

import { expect } from '@playwright/test';
import { readOutbox, waitForOutbox, test, type Extension } from './harness';

/** lib/contract.ts's perplexity row: one origin, GET, `/rest/thread/`. */
const ORIGIN = 'https://www.perplexity.ai';
/**
 * The thread's identity on the wire and in the page URL, exactly as the W28/W65
 * suites use it: the slug. The page URL's `/search/<segment>` is what the live
 * leg names a file by, and the list leg reads the same value off each record.
 */
const SLUG = 'how-do-i-rotate-a-secret-1aBcDeFg';
const PAGE_PATH = `/search/${SLUG}`;
/**
 * The content request the page makes - GET /rest/thread/<slug> with the pinned
 * query the backfill plan builds (`PERPLEXITY_DETAIL_QUERY`). The path segment
 * and the method are the parts the capture row depends on; carrying the full
 * pinned set keeps the request byte-identical to the plan's own builder's
 * output, the same discipline W84 holds the allowlist to.
 */
const API_PATH =
  `/rest/thread/${SLUG}?with_parent_info=true`
  + '&with_schematized_response=true&version=2.18&source=default&from_first=true';

/**
 * A complete thread body: the `entries` envelope the row's shape gate is keyed
 * by, together with the completeness signal the backfill detail parser reads
 * (`has_next_page: false` + `next_cursor: null` - a confirmed no-more). The
 * shape gate only requires `entries`; the signal is included so the fixture is
 * the shape a real whole thread answers with, not a minimal stand-in.
 */
const BODY = JSON.stringify({
  background_entries: [],
  entries: [
    {
      uuid: 'entry-uuid-for-w228',
      query_str: 'synthetic perceptual question',
      blocks: [
        {
          intended_usage: 'ask_text',
          markdown_block: { answer: 'synthetic perceptual answer' },
        },
      ],
      updated_datetime: '2026-09-01T10:00:00.000Z',
      thread_title: 'synthetic thread',
    },
  ],
  first_entry: null,
  has_next_page: false,
  latest_entry: null,
  next_cursor: null,
  status: 'success',
  thread_metadata: {},
});

function page(): string {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>chat-stasher e2e fixture</title></head>
<body><p id="fixture">perplexity fixture page</p>
<script>
  window.__csCapture = (async () => {
    // The page's own request, made the way the app makes it: GET to the content
    // route with the page's own credentials. No auth header, no token - Perplexity
    // reads the logged-in state from its cookies, and this spec asserts the body
    // that comes back, exactly as the live page would receive it.
    const response = await fetch('${API_PATH}', {
      method: 'GET',
      credentials: 'same-origin',
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
    // Record the FULL request URL, not just the path — the query is part of what
    // Perplexity's content request carries, and the assertion below pins it.
    api.push(request.url());
    return route.fulfill({ status: 200, contentType: 'application/json', body: BODY });
  });
  return { escaped, api };
}

test('a Perplexity content response on a /search/<slug> page becomes one bundle', async ({ ext }) => {
  const { escaped, api } = await serve(ext);

  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  const served = await p.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );
  // The page really did receive this response - so "one bundle" is a statement
  // about our capture path, not about a request that never happened.
  expect(served).toEqual({ status: 200, bytes: BODY.length });

  const entries = await waitForOutbox(ext, (rows) => rows.length >= 1);
  expect(entries).toHaveLength(1);
  const entry = entries[0]!;

  // The session id came from the page's own URL - the row's first
  // `sessionIdPattern` (`/search/([^/?#]+)`) - and it is already a safe file-name
  // fragment (contract.ts's identity map, so nothing is renamed on the way to disk).
  expect(entry.name).toBe(`perplexity-${SLUG}.json`);

  const bundle = JSON.parse(entry.payload) as Record<string, unknown>;
  expect(bundle.platform).toBe('perplexity');
  expect(bundle.sessionId).toBe(SLUG);
  expect(bundle.url).toBe(`${ORIGIN}${API_PATH}`);
  expect(bundle.method).toBe('GET');
  expect(bundle.status).toBe(200);
  // The body is passed through untouched: the archive holds the response, not our
  // reading of it.
  expect(bundle.raw).toEqual({ text: BODY, bytes: Buffer.byteLength(BODY, 'utf8') });

  // The page's request was the only one to that origin, and the one it made was
  // byte-for-byte the content URL with the pinned five-key query. Nothing left the
  // machine.
  expect(api).toEqual([`${ORIGIN}${API_PATH}`]);
  expect(escaped).toEqual([]);
});

test('a response that is not the entries envelope is refused rather than captured', async ({ ext }) => {
  // The row's shape gate is `requiredPaths: ['entries']`. This body is valid
  // JSON, 2xx, GET, right path - and carries the envelope under a different key.
  // A capture here would file what is not a Perplexity conversation as one.
  const { escaped } = await serve(ext);
  await ext.context.route(`${ORIGIN}${API_PATH}`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ data: { entries: [] } }),
  }));

  const p = await ext.context.newPage();
  await p.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  const served = await p.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );
  // The page really did receive that response - so "nothing captured" is a
  // statement about the gate, not about a request that never happened.
  expect(served).toEqual({ status: 200, bytes: JSON.stringify({ data: { entries: [] } }).length });

  expect(await readOutbox(ext)).toEqual([]);
  expect(escaped).toEqual([]);
});