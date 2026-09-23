/**
 * W28 · Perplexity (www.perplexity.ai): the row registers the route that carries
 * a conversation, and the id a live capture names the file by is the same id the
 * backfill list gives that same thread.
 *
 * ## What this file exists to stop changing back
 *  1. **The registered route is the content route, not the conversation list.**
 *     C27 registered `/rest/thread/list_ask_threads` because the body path had no
 *     source. It has one now, so the row moves to it — and the list route, the
 *     mark-viewed / set-title / delete siblings, all fall outside. The list is a
 *     summary of conversations; capturing it would file a summary as the data.
 *  2. **The shape gate is `entries`, and it is strict.** A content response
 *     without `entries` is the drift case and must be warned about, not stored as
 *     an empty-looking conversation. An empty `entries` array is a measurement
 *     and passes.
 *  3. **One thread, one id, both legs.** The live leg reads the slug the page URL
 *     carries (`/search/<slug>`); the backfill list reads the id off each record
 *     of its response body. For the same thread those two must be the same
 *     string — otherwise the same conversation is archived under two names, and a
 *     debt does not settle on its own id. This file asserts the equality on the
 *     same fixture rather than asserting each side against a literal.
 *  4. **A URL that is not a thread URL yields nothing.** The list route's own
 *     path segment is the most dangerous one: read as an id it would be the
 *     string 'list_ask_threads'.
 *
 * 🔴 All fixtures are **synthetic**, hand-written from the field names the
 *    sources record. No request goes to perplexity.ai, there is no logged-in
 *    state, and no real conversation, account or id appears anywhere below.
 */

import { describe, expect, it, vi } from 'vitest';
import {
  CONTENT_MATCHES,
  extractSessionId,
  isCapturedFetchShape,
  matchesResponseShape,
  pathSafeSessionId,
  PLATFORMS,
  platformForTraffic,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import { PERPLEXITY_LIST_PATH, parsePerplexityListPage } from '../lib/backfill/enumerate';

const ORIGIN = 'https://www.perplexity.ai';

/**
 * The thread's identity as every source names it: the slug. It appears in the
 * page URL, in the content route's path, and on each record of the list
 * response — that is the whole point of the equality asserted in W28-3.
 */
const SLUG = 'how-do-i-rotate-a-secret-1aBcDeFg';
/** The other shape the content route accepts (`entry_uuid_or_slug`). Never used as the session id. */
const THREAD_UUID = 'b7c2f0e3-4a51-4d8f-9c31-0f2a6e5d7b90';
/** A second thread, for the page-URL fallback test. */
const SLUG2 = 'why-is-the-sky-blue-9zYx8WvU';

const PAGE_URL = `${ORIGIN}/search/${SLUG}`;
const PAGE_URL2 = `${ORIGIN}/search/${SLUG2}`;
const LIST_URL = `${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`;
/**
 * The content route: GET /rest/thread/<entry_uuid_or_slug> with the query the
 * page adds. The parameters are the union of what the sources show; the path
 * segment and the method are the parts this row depends on.
 */
const CONTENT_URL = `${ORIGIN}/rest/thread/${SLUG}?with_parent_info=true`
  + '&with_schematized_response=true&version=2.18&source=default&limit=100&offset=0'
  + '&from_first=true&supported_block_use_cases=ask_text&supported_block_use_cases=web_results';

// ---------------------------------------------------------------------------
// Synthetic fixtures — field names from the sources, values invented here
// ---------------------------------------------------------------------------

/** One turn of a thread, in the shape the content response carries them. */
function entry(uuid: string, query: string, answer: string): Record<string, unknown> {
  return {
    uuid,
    query_str: query,
    updated_datetime: '2026-09-01T10:00:00.000Z',
    thread_title: 'synthetic thread title',
    blocks: [{ intended_usage: 'ask_text', markdown_block: { answer } }],
  };
}

/** The content envelope: a top-level `entries` array. */
function contentBody(entries: Array<Record<string, unknown>>): string {
  return JSON.stringify({ entries });
}

const GOOD_CONTENT = contentBody([
  entry('synthetic-entry-1', 'synthetic question one', 'synthetic answer one'),
  entry('synthetic-entry-2', 'synthetic question two', 'synthetic answer two'),
]);

/**
 * One record of the list response.
 *
 * 🔴 W65 (2026-09-23) · **the live probe this comment asked for has now run, and
 * it changed this fixture.** The comment that stood here said: "`thread_id` is
 * what this repository's list parser consumes, `slug` is what the reference
 * implementations read ... If a live probe ever shows the two keys holding
 * different strings, this fixture is the first thing to change." The probe
 * showed something stronger — the endpoint's items carry **36 keys and no
 * `thread_id` at all**, so the parser was reading a name the API never had and
 * every list run halted on it. `thread_id` is gone from this fixture; `slug` is
 * what the parser now consumes, which is also what the reference reads.
 *
 * `uuid` stays, and stays a DIFFERENT string on purpose: it is the other id the
 * reference carries, and `slug` being the one the parser takes (not `uuid`) is
 * exactly the property this fixture now pins. Probe + reasoning:
 * `w65-pplx-list-shape.test.ts`.
 */
function threadRecord(slug: string): Record<string, unknown> {
  return {
    slug,
    uuid: THREAD_UUID,
    title: `synthetic-${slug}`,
    collection: { title: 'synthetic space' },
  };
}

/** The list envelope: a top-level array, no `total`, no `has_more`. */
function listBody(slugs: string[]): string {
  return JSON.stringify(slugs.map(threadRecord));
}

function perplexityRow() {
  const row = PLATFORMS.find((platform) => platform.id === 'perplexity');
  expect(row).toBeDefined();
  return row!;
}

describe('W28-1 · what the row registers', () => {
  it('one origin, the content route, the method that carries it, and the measured trust level', () => {
    const row = perplexityRow();
    expect(row.origins).toEqual(['https://www.perplexity.ai']);
    // 🔴 The content route family. The conversation-LIST route is deliberately
    //    inside the prefix but outside the row: the method gate below excludes it.
    expect(row.pathHints).toEqual(['/rest/thread/']);
    expect(row.methods).toEqual(['GET']);
    expect(row.credibility).toBe('from-source');
    expect(row.webSocketCapture).toBe(false);
  });

  it('the content request is this platform, and the conversation list is not', () => {
    expect(platformForTraffic(CONTENT_URL, 'GET')?.id).toBe('perplexity');
    // 🔴 A list response is a summary of conversations, not one of them. It is
    //    skipped by the method gate, silently — never captured, never warned about.
    expect(platformForTraffic(LIST_URL, 'POST')).toBeNull();
    // The record's own named sub-routes are POSTs too, so they are outside as well.
    expect(platformForTraffic(`${ORIGIN}/rest/thread/mark_viewed`, 'POST')).toBeNull();
    expect(platformForTraffic(`${ORIGIN}/rest/thread/set_thread_title`, 'POST')).toBeNull();
    // And the page URL itself is not traffic on this origin.
    expect(platformForTraffic(PAGE_URL, 'GET')).toBeNull();
  });

  it('the origin set is still closed, and a hostile neighbour is not this platform', () => {
    expect(CONTENT_MATCHES).toContain('https://www.perplexity.ai/*');
    expect(CONTENT_MATCHES).not.toContain('<all_urls>');
    expect(CONTENT_MATCHES.some((match) => match.startsWith('*'))).toBe(false);
    for (const hostile of [
      'https://www.perplexity.ai.attacker.example/rest/thread/aaaaaaaa',
      'https://perplexity.ai/rest/thread/aaaaaaaa',
      'http://www.perplexity.ai/rest/thread/aaaaaaaa',
    ]) {
      expect(platformForTraffic(hostile, 'GET')).toBeNull();
      expect(isCapturedFetchShape({
        url: hostile,
        method: 'GET',
        status: 200,
        text: GOOD_CONTENT,
        pageUrl: PAGE_URL,
        capturedAt: Date.now(),
      })).toBe(false);
    }
  });

  it('the shape gate takes the entries envelope and refuses everything that is not it', () => {
    const row = perplexityRow();
    expect(matchesResponseShape(row, GOOD_CONTENT)).toBe(true);
    // An EMPTY array passes: `[]` is a measurement, not a missing field.
    expect(matchesResponseShape(row, contentBody([]))).toBe(true);
    // Drift: same route, same 200, the envelope moved under a different key.
    expect(matchesResponseShape(row, JSON.stringify({ data: { entries: [] } }))).toBe(false);
    // The list envelope is plainly not the content envelope.
    expect(matchesResponseShape(row, listBody([SLUG]))).toBe(false);
    expect(matchesResponseShape(row, 'not json')).toBe(false);
  });
});

describe('W28-2 · which conversation this capture is', () => {
  it('the id comes out of the page URL, and a new thread falls back to the request path', () => {
    // The ordinary case: the page is on the thread, so the address bar names it.
    expect(extractSessionId(CONTENT_URL, GOOD_CONTENT, PAGE_URL)).toBe(SLUG);
    expect(extractSessionId(CONTENT_URL, GOOD_CONTENT, PAGE_URL2)).toBe(SLUG2);
    // 🔴 A brand-new thread's address bar has no slug yet; the request path is
    //    the fallback, and the reference implementations feed it the slug too.
    expect(extractSessionId(CONTENT_URL, GOOD_CONTENT)).toBe(SLUG);
    expect(extractSessionId(CONTENT_URL, GOOD_CONTENT, `${ORIGIN}/`)).toBe(SLUG);
    // The other shape the route accepts is read as it is: an id, not a slug.
    expect(extractSessionId(
      `${ORIGIN}/rest/thread/${THREAD_UUID}?version=2.18&source=default`,
      GOOD_CONTENT,
    )).toBe(THREAD_UUID);
  });

  it('a URL that is not a thread URL yields no id', () => {
    // 🔴 The dangerous one: read as an id, the list route's own path segment is
    //    the string 'list_ask_threads', and a list response would then be filed
    //    as if it were that conversation.
    expect(extractSessionId(LIST_URL, listBody([SLUG]), `${ORIGIN}/`)).toBeNull();
    expect(extractSessionId(`${ORIGIN}/rest/thread/mark_viewed`, '{}', `${ORIGIN}/`)).toBeNull();
    expect(extractSessionId(`${ORIGIN}/rest/thread/set_thread_title`, '{}')).toBeNull();
    expect(extractSessionId(`${ORIGIN}/rest/thread/delete_thread_by_entry_uuid`, '{}')).toBeNull();
    // The library page and the home page are not conversations either.
    expect(extractSessionId(LIST_URL, listBody([SLUG]), `${ORIGIN}/library`)).toBeNull();
    expect(extractSessionId(LIST_URL, listBody([SLUG]), `${ORIGIN}/`)).toBeNull();
  });

  it('the id from the URL and the id from the list body are the same value', () => {
    const list = parsePerplexityListPage(listBody([SLUG, SLUG2]));
    expect(list.ok).toBe(true);
    const listedId: string = list.ok ? list.page.ids[0]! : '';

    // 🔴 The property that makes a debt settle on its own id: the live leg and
    //    the list leg name the SAME thread with the SAME string.
    const fromUrl = extractSessionId(CONTENT_URL, GOOD_CONTENT, PAGE_URL);
    expect(listedId).toBe(SLUG);
    expect(fromUrl).toBe(listedId);
    // And it survives the trip to a file name, unchanged: the identity map
    // (contract.ts pathSafeSessionId) is one-to-one, so nothing renames it.
    expect(pathSafeSessionId(fromUrl!)).toBe(pathSafeSessionId(listedId));
    expect(pathSafeSessionId(fromUrl!)).toBe(SLUG);
  });

  it('a page payload cannot name the file it is written to', () => {
    expect(isCapturedFetchShape({
      url: CONTENT_URL,
      method: 'GET',
      status: 200,
      text: GOOD_CONTENT,
      pageUrl: PAGE_URL,
      capturedAt: Date.now(),
      sessionId: SLUG,
    })).toBe(false);
  });
});

describe('W28-3 · the passive hook', () => {
  function makeFakeWindow(responseBody: string) {
    const posted: unknown[] = [];
    const fakeWindow: any = {
      location: { origin: ORIGIN, href: PAGE_URL },
      async fetch() { return new Response(responseBody, { status: 200 }); },
      addEventListener() { /* the probe listener is irrelevant to these assertions */ },
      postMessage(msg: unknown) { posted.push(msg); },
    };
    return { fakeWindow, posted };
  }

  it('a content response on the /search/<slug> page becomes one bundle with that session id', async () => {
    const { fakeWindow, posted } = makeFakeWindow(GOOD_CONTENT);
    vi.stubGlobal('window', fakeWindow);
    try {
      installPageFetchHook(PAGE_HOOK_OPTIONS);
      await fakeWindow.fetch(CONTENT_URL, { method: 'GET' });
      await new Promise((resolve) => setTimeout(resolve, 0));

      const captures = posted.filter((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage) as any[];
      expect(captures).toHaveLength(1);
      const payload = captures[0].payload;
      expect(payload.url).toBe(CONTENT_URL);
      expect(payload.method).toBe('GET');
      expect(payload.status).toBe(200);
      expect(payload.pageUrl).toBe(PAGE_URL);
      // The payload is the raw body, untouched.
      expect(payload.text).toBe(GOOD_CONTENT);
      // It passes the bridge's own gate, which is what makes it deliverable.
      expect(isCapturedFetchShape(payload)).toBe(true);
      expect(extractSessionId(payload.url, payload.text, payload.pageUrl)).toBe(SLUG);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it('the conversation list passes by without a warning and without a capture', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(listBody([SLUG]));
    vi.stubGlobal('window', fakeWindow);
    try {
      installPageFetchHook(PAGE_HOOK_OPTIONS);
      await fakeWindow.fetch(LIST_URL, { method: 'POST' });
      await new Promise((resolve) => setTimeout(resolve, 0));

      // 🔴 This is why the row registers the content route: a list is not the
      //    conversation, so it is skipped silently rather than warned about on
      //    every visit to the library.
      expect(warn).not.toHaveBeenCalled();
      expect(posted.some((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
    } finally {
      vi.restoreAllMocks();
      vi.unstubAllGlobals();
    }
  });

  it('a content response whose shape drifted is refused and said out loud', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(JSON.stringify({ data: { entries: [] } }));
    vi.stubGlobal('window', fakeWindow);
    try {
      installPageFetchHook(PAGE_HOOK_OPTIONS);
      await fakeWindow.fetch(CONTENT_URL, { method: 'GET' });
      await new Promise((resolve) => setTimeout(resolve, 0));

      expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
      // Metadata-only: no URL, body or id in the warning.
      expect(warn.mock.calls[0]).toHaveLength(1);
      expect(posted.some((m: any) => m?.type === PAGE_HOOK_OPTIONS.captureMessage)).toBe(false);
    } finally {
      vi.restoreAllMocks();
      vi.unstubAllGlobals();
    }
  });
});
