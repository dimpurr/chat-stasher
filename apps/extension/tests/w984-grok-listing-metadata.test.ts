/**
 * W984 · Grok's conversation-level `title` / `createTime` reach the bundle.
 *
 * 🔴 All fixtures here are **synthetic**: no request goes to grok.com, there is
 *    no logged-in state, and no real conversation body, account, title or id
 *    appears anywhere below. Nothing leaves the machine. Only the field *names*
 *    are the measured/source-backed ones; every value is invented.
 *
 * ## The gap this file pins closed
 *
 * The reference-corpus oracle (27-ORACLE §5.1 finding 3) measured that on all
 * 38 matched Grok pairs our capture bundle is `{"responses": [...]}` only: the
 * conversation-level `title` and `createTime` (which the official export calls
 * `title` / `create_time`) were dropped, and `created_at` is **not derivable**
 * from per-message times (min(response.createTime) == create_time on 0/38 — the
 * conversation exists before its first message). W917 measured where those
 * fields do live: the conversation **list** response
 * (`GET /rest/app-chat/conversations`), as
 * `conversations[].{conversationId, title, starred, createTime, modifyTime}` —
 * the response the backfill's own enumeration already fetches and, until this
 * change, read only the `conversationId` out of.
 *
 * This change carries that metadata to the delivered bundle:
 *
 * 1. `parseGrokListPage` reads the four fields per row, verbatim, into
 *    `EnumPage.listings` (section 1). Absent stays absent; a value whose type
 *    is not readable is not carried — never resynthesised, never defaulted.
 * 2. The engine records them against the debt rows (the W113 `DebtTime`
 *    pattern) and attaches them to the `CapturedFetch` it hands the sink (sections
 *    2 and 3 — the cross-tick case is the normal one: bodies drain across days,
 *    so the metadata has to survive the debt store, not just the run).
 * 3. `buildBundle` copies them onto the `InboxBundle` under one additive key,
 *    `listing` — present when the enumeration knew them, absent when it did not
 *    (section 4). Older bundles and metadata-less captures therefore keep their
 *    exact bytes; no key appears where no observation existed.
 * 4. The delivery gate keeps authorship with the extension: a page payload
 *    carrying `listing` is rejected on sight, exactly like `sessionId` (section 5)
 *    — a page choosing the bundle's conversation metadata is a page choosing
 *    facts the archive will trust.
 *
 * 🔴 The **live leg** (a conversation opened in a tab) still captures no
 *    metadata: the list route is outside the grok row's `pathHints` (two
 *    independent gates, pinned by W917-2 and unchanged by this change), so a
 *    live capture of a conversation the enumeration has not listed yet carries
 *    no `listing`, and that is the accepted state: the field appears once the
 *    conversation is enumerated and its body fetched by the backfill leg.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import {
  isCapturedFetchShape,
  type CapturedFetch,
} from '../lib/contract';
import { runBackfill, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  GROK_DETAIL2_PATH,
  GROK_DETAIL_PATH,
  GROK_LIST_PATH,
  parseGrokListPage,
} from '../lib/backfill/enumerate';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://grok.com';
const ID = '00000000-0000-4000-8000-0000000000aa';
const ID2 = '00000000-0000-4000-8000-0000000000bb';
const R1 = '11111111-0000-4000-8000-000000000001';
const PAGE_URL = `${ORIGIN}/c/${ID}`;
const NODE_PATH = GROK_DETAIL_PATH.replace('{id}', ID);
const LOAD_PATH = GROK_DETAIL2_PATH.replace('{id}', ID);
const LOAD_URL = `${ORIGIN}${LOAD_PATH}`;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

/** A fake clock that advances on sleep; the waits are irrelevant to these assertions. */
function fakeClock(): Clock {
  let t = Date.parse('2026-10-12T00:00:00.000Z');
  return {
    now: () => t,
    async sleep(ms: number) {
      t += ms;
    },
  };
}

// ---------------------------------------------------------------------------
// Synthetic fixtures (field names source-backed; values invented)
// ---------------------------------------------------------------------------

/** One row of the conversation list. Every metadata field is optional here. */
function listRow(conversationId: string, meta: Record<string, unknown> = {}): Record<string, unknown> {
  return { conversationId, ...meta };
}

/** A list page: ids always, metadata only where the test provides it. */
function listPage(rows: Record<string, unknown>[], token?: string | null): string {
  const body: Record<string, unknown> = { conversations: rows, textSearchMatches: [] };
  if (token !== undefined) body.nextPageToken = token === null ? '' : token;
  return JSON.stringify(body);
}

/** The row the oracle measured with everything readable: the common source-backed shape. */
const FULL_META: Record<string, unknown> = {
  title: 'synthetic-conversation-title',
  starred: false,
  createTime: '2026-07-01T09:59:00.000000Z',
  modifyTime: '2026-07-01T10:01:10.000Z',
};

/** A readable content body for one conversation. */
function responseBody(message: string): string {
  return JSON.stringify({
    responses: [
      {
        responseId: R1,
        message,
        sender: 'human',
        createTime: '2026-07-01T10:00:05.000Z',
        model: 'synthetic-model',
      },
    ],
  });
}

function responseNodeBody(): string {
  return JSON.stringify({
    responseNodes: [{ responseId: R1, sender: 'human' }],
    inflightResponses: [],
  });
}

/**
 * A synthetic backend. Any path it was not given **throws**, so "this leg sent
 * no further request" is proven by the run rather than asserted about it.
 */
function backend(
  clock: Clock,
  routes: Partial<Record<string, string | ((url: URL) => string)>>,
): { calls: string[]; http: (url: string, init?: unknown) => Promise<HttpResponse> } {
  const calls: string[] = [];
  const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    const route = routes[u.pathname];
    if (route === undefined) throw new Error(`unexpected path ${u.pathname}`);
    return { status: 200, text: typeof route === 'string' ? route : route(u) };
  };
  return { calls, http };
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: (url: string, init?: unknown) => Promise<HttpResponse>,
  scope: string,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'grok',
    origin: ORIGIN,
    scope,
    store,
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    ...extra,
  });
}

// ---------------------------------------------------------------------------
// 1 · The list parser carries the row's metadata, field by field
// ---------------------------------------------------------------------------
describe('W984-1 · parseGrokListPage carries the conversation-level fields', () => {
  it('the four source-backed fields arrive in page.listings keyed by conversation id, verbatim', () => {
    const parsed = parseGrokListPage(listPage([listRow(ID, FULL_META)]));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    // 🔴 Verbatim, not re-typed or re-formatted: what the list said is what the
    //    bundle will say. createTime stays the platform's own string — it is
    //    never converted into a timestamp, so nothing can drift in conversion.
    expect(parsed.page.listings?.get(ID)).toEqual(FULL_META);
  });

  it('absent stays absent: a field the row does not name is not invented', () => {
    const partial = { title: 'synthetic-title-only' };
    const parsed = parseGrokListPage(listPage([listRow(ID, partial)]));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    const listing = parsed.page.listings?.get(ID) as Record<string, unknown> | undefined;
    expect(listing).toBeDefined();
    // Only the one readable field is carried; `createTime` is absent, which is
    // exactly the oracle's honest "missing" — never a placeholder.
    expect(listing).toEqual(partial);
    expect(Object.prototype.hasOwnProperty.call(listing, 'createTime')).toBe(false);
  });

  it('a value of the wrong type is not carried, and does not halt the leg', () => {
    const parsed = parseGrokListPage(listPage([listRow(ID, { ...FULL_META, title: 123 })]));
    // 🔴 The ids are the enumeration's own requirement and still pass; metadata
    //    is carried best-effort. A drifted metadata type costs one field, never
    //    the whole account (halting here would trade content for decoration).
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    const listing = parsed.page.listings?.get(ID) as Record<string, unknown> | undefined;
    expect(listing).toBeDefined();
    // The wrong-typed title was dropped; the readable fields survive it.
    expect(Object.prototype.hasOwnProperty.call(listing, 'title')).toBe(false);
    expect(listing?.createTime).toBe(FULL_META.createTime);
  });

  it('a page whose rows carry no readable metadata omits listings entirely', () => {
    const parsed = parseGrokListPage(listPage([listRow(ID)]));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    // Absent key, not an empty map: "this page named nothing we can read" and
    // "the page carried-metadata" must stay two distinguishable facts.
    expect(parsed.page.listings).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// 2 · The engine attaches the enumeration's metadata to the delivered body
// ---------------------------------------------------------------------------
describe('W984-2 · the backfill leg carries the listing to the sink', () => {
  it('a body fetched in the same run as its enumeration carries the listing', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([listRow(ID, FULL_META)]),
      [NODE_PATH]: responseNodeBody(),
      [LOAD_PATH]: responseBody('synthetic-question'),
    });
    const captured: CapturedFetch[] = [];
    const report = await run(memoryStore(), be.http, 'w984-same-tick', {
      clock,
      sink: (c: CapturedFetch): SinkOutcome => { captured.push(c); return { saved: true, sessionId: c.sessionId }; },
    });

    expect(report.state.archived).toEqual([ID]);
    expect(captured).toHaveLength(1);
    // 🔴 The metadata read off this run's list page reaches the delivered
    //    artefact, keyed by the debt id the enumeration filed it under.
    expect(captured[0]!.sessionId).toBe(ID);
    expect((captured[0] as { listing?: unknown }).listing).toEqual(FULL_META);
  });

  it('a body fetched in a LATER run carries the listing read back from the debt store', async () => {
    const clock = fakeClock();
    // Two list pages: the first carries the id and a token, the second is empty
    // (the API's own ending). One page per tick (W10), so tick 1 lists only.
    const be = {
      page: backend(clock, {
        [GROK_LIST_PATH]: listPage([listRow(ID, FULL_META)], 'token-1'),
      }),
      empty: backend(clock, {
        // Page two is empty and carries no token ⇒ the API's own ending.
        [GROK_LIST_PATH]: listPage([]),
        [NODE_PATH]: responseNodeBody(),
        [LOAD_PATH]: responseBody('synthetic-question'),
      }),
    };
    const store = memoryStore();
    // Tick 1: the page is read, the debt recorded — and no body is fetched.
    const first = await run(store, be.page.http, 'w984-cross-tick', {
      clock,
      maxDetails: 0,
      sink: (): SinkOutcome => { throw new Error('no sink call may happen in tick 1'); },
    });
    expect(first.state.pending).toEqual([ID]);

    // Tick 2: a fresh run over the same store. The listing must now come from
    // the debt store (recorded against the row in tick 1), not from any
    // in-memory state — the body's own run never saw a metadata-bearing page.
    const captured: CapturedFetch[] = [];
    const second = await run(store, be.empty.http, 'w984-cross-tick', {
      clock,
      sink: (c: CapturedFetch): SinkOutcome => { captured.push(c); return { saved: true, sessionId: c.sessionId }; },
    });
    expect(second.state.archived).toEqual([ID]);
    expect(captured).toHaveLength(1);
    expect((captured[0] as { listing?: unknown }).listing).toEqual(FULL_META);
  });

  it('a list without readable metadata delivers a body with NO listing key', async () => {
    const clock = fakeClock();
    const be = backend(clock, {
      [GROK_LIST_PATH]: listPage([
        listRow(ID, { title: 'synthetic-has-metadata' }),
        listRow(ID2),
      ]),
      [GROK_DETAIL_PATH.replace('{id}', ID)]: responseNodeBody(),
      [GROK_DETAIL2_PATH.replace('{id}', ID)]: responseBody('synthetic-first'),
      [GROK_DETAIL_PATH.replace('{id}', ID2)]: responseNodeBody(),
      [GROK_DETAIL2_PATH.replace('{id}', ID2)]: responseBody('synthetic-second'),
    });
    const captured: CapturedFetch[] = [];
    const byId = (sid: string): CapturedFetch | undefined =>
      captured.find((c) => c.sessionId === sid);
    await run(memoryStore(), be.http, 'w984-mixed-page', {
      clock,
      sink: (c: CapturedFetch): SinkOutcome => { captured.push(c); return { saved: true, sessionId: c.sessionId }; },
    });

    expect(captured).toHaveLength(2);
    // The row that named a title gets one per delivery…
    expect((byId(ID) as { listing?: unknown }).listing).toEqual({ title: 'synthetic-has-metadata' });
    // …and the row that named none is delivered without the key at all:
    // 🔴 absent stays absent (invariant 1, on the delivery artefact).
    expect('listing' in (byId(ID2) as object)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 3 · The bundle the extension sends: additive key, verbatim value, absent absent
// ---------------------------------------------------------------------------
describe('W984-3 · the delivered bundle carries `listing` additively', () => {
  /**
   * The real background's backfill exit (`deliverBackfillItem`), against a fake
   * host that records the payload strings; the payload is the bundle the host
   * would seal, which is the bundle shape the archive holds.
   */
  let deliveries: Array<{ name: string; payload: string }> = [];

  /**
   * The fake extension surface. The storage area is a real in-memory map (the
   * install-identity reader does read-modify-write against it; a store whose
   * `set` is a no-op makes it report its own identity as unstable).
   */
  let backing: Record<string, unknown> = {};

  const fakeBrowser: Record<string, unknown> = {
    runtime: {
      id: 'w984-listing',
      onStartup: { addListener() { /* unused here */ } },
      onMessage: { addListener() { /* no live dispatch in this suite */ } },
      async sendNativeMessage(_host: string, message: Record<string, unknown>) {
        if (message.type === 'deliver') {
          deliveries.push({ name: message.name as string, payload: message.payload as string });
          return {
            protocol: 1,
            type: 'ack',
            request_id: message.request_id,
            status: 'stored',
            sha256: message.sha256,
            shard: '000001.jsonl',
          };
        }
        throw new Error(`unexpected ${String(message.type)}`);
      },
    },
    storage: {
      local: {
        async get(query: Record<string, unknown> | null) {
          if (query === null) return { ...backing };
          const out: Record<string, unknown> = {};
          for (const key of Object.keys(query)) out[key] = backing[key] ?? (query as Record<string, unknown>)[key];
          return out;
        },
        async set(values: Record<string, unknown>) { Object.assign(backing, values); },
        async remove(keys: string[] | string) { for (const k of Array.isArray(keys) ? keys : [keys]) delete backing[k]; },
      },
    },
    action: {
      async setBadgeText() { /* decorative */ },
      async setBadgeBackgroundColor() { /* decorative */ },
      async setTitle() { /* decorative */ },
    },
  };

  beforeEach(() => {
    deliveries = [];
    backing = {};
    vi.resetModules();
    (globalThis as Record<string, unknown>).indexedDB = new IDBFactory();
    vi.stubGlobal('browser', withI18n(fakeBrowser as never));
    vi.stubGlobal('chrome', fakeBrowser);
    // The background module's default export needs WXT's defineBackground to
    // exist at import time; it is only defined, never invoked, in this suite.
    vi.stubGlobal('defineBackground', (cb: unknown) => cb);
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  /** The engine-shaped capture: the backfill leg names the session itself. */
  function grokCapture(withListing: boolean): CapturedFetch {
    return {
      url: LOAD_URL,
      method: 'POST',
      status: 200,
      text: responseBody('synthetic-question'),
      pageUrl: PAGE_URL,
      capturedAt: Date.parse('2026-10-12T01:00:00.000Z'),
      sessionId: ID,
      ...(withListing ? { listing: FULL_META as never } : {}),
    };
  }

  it('a capture carrying a listing delivers a bundle whose `listing` is that value verbatim', async () => {
    const mod = await import('../entrypoints/background');
    const result = await mod.deliverBackfillItem(grokCapture(true));
    expect(result.saved).toBe(true);

    expect(deliveries).toHaveLength(1);
    const bundle = JSON.parse(deliveries[0]!.payload) as Record<string, unknown>;
    // 👉 The oracle's two missing fields, present and untouched:
    //    `listing.title` and `listing.createTime` (the export's title / create_time).
    expect(bundle.listing).toEqual(FULL_META);
    // The content itself is unchanged: the key is additive, the body authoritative.
    expect(JSON.parse((bundle.raw as { text: string }).text)).toEqual(JSON.parse(responseBody('synthetic-question')));
  });

  it('a capture with no listing delivers a bundle with no `listing` key — byte-compat with older bundles', async () => {
    const mod = await import('../entrypoints/background');
    const result = await mod.deliverBackfillItem(grokCapture(false));
    expect(result.saved).toBe(true);

    expect(deliveries).toHaveLength(1);
    const bundle = JSON.parse(deliveries[0]!.payload) as Record<string, unknown>;
    expect('listing' in bundle).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 4 · The gate: a page may never author this field
// ---------------------------------------------------------------------------
describe('W984-4 · page payloads cannot supply a listing', () => {
  it('isCapturedFetchShape rejects a hook payload carrying `listing`, like `sessionId`', () => {
    const base = {
      url: LOAD_URL,
      method: 'POST',
      status: 200,
      text: responseBody('synthetic-question'),
      pageUrl: PAGE_URL,
      capturedAt: Date.parse('2026-10-12T01:00:00.000Z'),
    };
    // The same payload is accepted without the field…
    expect(isCapturedFetchShape(base)).toBe(true);
    // …and rejected with it: conversation metadata is an extension-authored
    // fact (the enumeration read it), and a page naming it is choosing what the
    // archive will record about a conversation, not reporting its own traffic.
    expect(isCapturedFetchShape({ ...base, listing: FULL_META })).toBe(false);
  });
});
