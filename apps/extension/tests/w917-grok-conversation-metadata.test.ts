/**
 * W917 · Does the grok.com web capture already hold the conversation-level `title` / `created_at`?
 *
 * 🔴 All fixtures here are **synthetic**: no request goes to grok.com, there is no logged-in state,
 *    and no real conversation body, account, title or id appears anywhere below. Nothing leaves the
 *    machine. Only the field *names* are the measured ones (section 1); every value is invented.
 *
 * ## The question, and the answer this file pins
 *
 * The reference-corpus oracle row reports that our Grok capture drops the conversation-level
 * `title` and `created_at` on every matched pair, because the bundle we keep is
 * `{"responses": [...]}` only. `updated_at` is recoverable from it (max response time equals the
 * conversation's `modify_time` on 38/38); `created_at` is not. This file answers the question the
 * ticket asks — *is that metadata already in the response the page hands us?* — and pins it.
 *
 * 1. **No, and this is measured rather than argued.** The only grok.com response this extension
 *    captures is the content POST (`/rest/app-chat/conversations/<id>/load-responses`), and its
 *    body carries exactly **one** top-level key, `responses`. Measured on this machine 2026-10-07
 *    over every archived live Grok bundle we hold: **61/61 bundles** have the body key set
 *    `{responses}`, and their **438 response elements** each carry the per-message `createTime`
 *    and none of a conversation-level one. There is therefore nothing to carry through, and
 *    nothing is invented to fill the gap — no time is rebuilt from message order or from a display
 *    string (invariant 1, and the standing never-rebuild-times-from-display-strings rule).
 *
 * 2. **Which request and fields would be needed.** The conversation-level fields do exist on the
 *    wire, in the conversation **list** response — `GET /rest/app-chat/conversations` — as
 *    `conversations[].{conversationId, title, starred, createTime, modifyTime}`. That is a
 *    source-backed field map (`GROK_PLAN`'s provenance; the `grok` row in `lib/contract.ts`), not
 *    a measurement of ours. Carrying `title`/`created_at` means carrying *that request's* body,
 *    joined on `conversations[].conversationId`. Section 2 pins why it is not captured today, in
 *    two independent gates, and why the leg that *does* receive it reads only the ids.
 *
 * 3. **The join key is already in hand.** `conversations[].conversationId` is the same conversation
 *    id this leg derives from the content URL and writes the session under, so the two responses
 *    are attachable as soon as the list body is carried. Section 2 pins that too.
 *
 * 🔴 Scope note: this file **stops at the determination**. No capture is widened, no field is
 *    carried, and no title is read. If the list response is ever carried, this file is where that
 *    change has to come and say so — the assertions below are written to go red on exactly that.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  PLATFORMS,
  extractSessionId,
  isCapturedFetchShape,
  matchesResponseShape,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import {
  GROK_DETAIL2_PATH,
  GROK_LIST_PAGE_SIZE_PARAM,
  GROK_LIST_PATH,
  parseGrokListPage,
} from '../lib/backfill/enumerate';

const ORIGIN = 'https://grok.com';
/** A synthetic conversation id in the shape the sources show (an opaque UUID-like token). */
const ID = '00000000-0000-4000-8000-0000000000aa';
const R1 = '11111111-0000-4000-8000-000000000001';
const R2 = '11111111-0000-4000-8000-000000000002';
const PAGE_URL = `${ORIGIN}/c/${ID}`;
const LOAD_URL = `${ORIGIN}${GROK_DETAIL2_PATH.replace('{id}', ID)}`;
const LIST_URL = `${ORIGIN}${GROK_LIST_PATH}?${GROK_LIST_PAGE_SIZE_PARAM}=20`;

const GROK_ROW = PLATFORMS.find((platform) => platform.id === 'grok')!;

/**
 * The per-response field set, **measured**. These are the 45 names every element of every
 * archived live Grok bundle carries (438 elements over 61 bundles, 2026-10-07). The values
 * below are invented; only the names are the measurement, and they are here so that "the
 * content response carries no conversation-level field" is a list a reader can check rather
 * than a claim they have to take.
 */
function responseElement(responseId: string, message: string, sender: string): Record<string, unknown> {
  return {
    responseId,
    message,
    sender,
    // 🔴 The only time on a response element, and it is the message's own — not the
    //    conversation's creation time (that is not derivable from it; see the header).
    createTime: '2026-07-01T10:00:05.000Z',
    parentResponseId: R1,
    model: 'synthetic-model',
    metadata: {},
    requestMetadata: {},
    partial: false,
    manual: false,
    shared: false,
    isControl: false,
    query: '',
    queryType: '',
    steps: [],
    streamErrors: [],
    toolResponses: [],
    inputChunks: [],
    outputChunks: [],
    webSearchResults: [],
    citedWebSearchResults: [],
    searchProductResults: [],
    ragResults: [],
    citedRagResults: [],
    connectorSearchResults: [],
    citedConnectorSearchResults: [],
    collectionSearchResults: [],
    citedCollectionSearchResults: [],
    xposts: [],
    xpostIds: [],
    citedXposts: [],
    mediaTypes: [],
    webpageUrls: [],
    generatedImageUrls: [],
    imageAttachments: [],
    imageEditUris: [],
    fileAttachments: [],
    fileAttachmentAssetMetadata: [],
    fileAttachmentsMetadata: [],
    fileIds: [],
    fileUris: [],
    cardAttachmentsJson: [],
    uiLayout: null,
    thinkingStartTime: null,
    thinkingEndTime: null,
  };
}

/** The content response: one top-level key, `responses`. */
const CONTENT_BODY = JSON.stringify({
  responses: [
    responseElement(R1, 'synthetic-question', 'human'),
    responseElement(R2, 'synthetic-answer', 'ASSISTANT'),
  ],
});

/**
 * The conversation list response: the body that **does** carry the conversation-level fields.
 * Field names are the source-backed ones (`createTime`, not the export's `create_time`); the
 * values are invented.
 */
const LIST_BODY = JSON.stringify({
  conversations: [
    {
      conversationId: ID,
      title: 'synthetic-conversation-title',
      starred: false,
      createTime: '2026-07-01T09:59:00.000000Z',
      modifyTime: '2026-07-01T10:01:10.000Z',
    },
  ],
  textSearchMatches: [],
  nextPageToken: '',
});

interface FakeWindow {
  location: { origin: string; href: string };
  fetch(url: string, init?: unknown): Promise<Response>;
  addEventListener(): void;
  postMessage(message: unknown): void;
}

function makeFakeWindow(responseBody: string): { fakeWindow: FakeWindow; posted: unknown[] } {
  const posted: unknown[] = [];
  const fakeWindow: FakeWindow = {
    location: { origin: ORIGIN, href: PAGE_URL },
    async fetch() { return new Response(responseBody, { status: 200 }); },
    addEventListener() { /* the probe listener is irrelevant to these assertions */ },
    postMessage(message: unknown) { posted.push(message); },
  };
  return { fakeWindow, posted };
}

interface CapturePayload {
  url: string;
  method: string;
  status: number;
  text: string;
  pageUrl?: string;
}

/** The capture payloads the hook posted. */
function capturesOf(posted: unknown[]): CapturePayload[] {
  return (posted as { type?: string; payload?: CapturePayload }[])
    .filter((message) => message?.type === PAGE_HOOK_OPTIONS.captureMessage)
    .map((message) => message.payload as CapturePayload);
}

/** Let the hook's own microtasks land. */
const settle = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

// ---------------------------------------------------------------------------
// 1 · The response the page hands us carries no conversation-level metadata
// ---------------------------------------------------------------------------
describe('W917-1 · the content response carries no conversation-level field', () => {
  it('the captured body has exactly one top-level key, `responses`, and no title or conversation time', async () => {
    const { fakeWindow, posted } = makeFakeWindow(CONTENT_BODY);
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(LOAD_URL, { method: 'POST' });
    await settle();

    const captures = capturesOf(posted);
    expect(captures).toHaveLength(1);
    const capture = captures[0]!;
    // The payload is the raw body, untouched: the hook adds nothing of its own, so what the
    // archive holds is exactly what the page was handed.
    expect(capture.text).toBe(CONTENT_BODY);
    expect(isCapturedFetchShape(capture)).toBe(true);

    // 🔴 The measured shape, pinned. One top-level key, and it is not one of the four names the
    //    oracle counts as missing — so a future capture that starts carrying `title` or a
    //    conversation-level creation time has to change this line rather than slip past it.
    const body = JSON.parse(capture.text) as Record<string, unknown>;
    expect(Object.keys(body)).toEqual(['responses']);
    for (const absent of ['title', 'createdAt', 'created_at', 'createTime', 'modifyTime', 'conversation']) {
      expect(Object.prototype.hasOwnProperty.call(body, absent)).toBe(false);
    }
  });

  it("the only time on a response element is the message's own `createTime`", async () => {
    const { fakeWindow, posted } = makeFakeWindow(CONTENT_BODY);
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(LOAD_URL, { method: 'POST' });
    await settle();

    const captures = capturesOf(posted);
    expect(captures).toHaveLength(1);
    const body = JSON.parse(captures[0]!.text) as { responses: Record<string, unknown>[] };
    const element = body.responses[0]!;
    expect(Object.prototype.hasOwnProperty.call(element, 'createTime')).toBe(true);
    // 🔴 `created_at` is NOT derivable from this element, and nothing here tries to derive it: the
    //    conversation exists before its first message, so min(createTime) is not the creation time
    //    (measured 0/38 on the oracle's own pairs). Reporting the field as missing is the honest
    //    outcome; inventing one is not on the table. The element does not even name its own
    //    conversation — `conversationId` is in the URL this leg captures, not on the element — so
    //    the join key has to come from the request, which is where it already does.
    for (const absent of ['createdAt', 'created_at', 'title', 'modifyTime', 'conversationId']) {
      expect(Object.prototype.hasOwnProperty.call(element, absent)).toBe(false);
    }
  });
});

// ---------------------------------------------------------------------------
// 2 · The fields live in the list response, which this leg does not carry
// ---------------------------------------------------------------------------
describe('W917-2 · the conversation-level fields live in the list response, which is not carried', () => {
  it('the list body carries `title` and `createTime`, and the grok row refuses it on two gates', () => {
    // The request that would be needed, named from the plan's own constant rather than as prose.
    expect(GROK_LIST_PATH).toBe('/rest/app-chat/conversations');
    const body = JSON.parse(LIST_BODY) as { conversations: Record<string, unknown>[] };
    const conversation = body.conversations[0]!;
    // The two fields the oracle reports missing, on the conversation-level object.
    expect(typeof conversation.title).toBe('string');
    expect(typeof conversation.createTime).toBe('string');

    // 🔴 Gate 1: the row names the content path only, so a list request is never a candidate.
    expect(GROK_ROW.pathHints).toEqual(['/load-responses']);
    expect(GROK_ROW.methods).toEqual(['POST']);
    // 🔴 Gate 2: the row's shape gate requires `responses`, so a list body is a shape mismatch
    //    even if it were a candidate — a request carrying the metadata cannot be captured today
    //    without changing the row, and changing the row is what this file is here to notice.
    expect(matchesResponseShape(GROK_ROW, LIST_BODY)).toBe(false);
    expect(matchesResponseShape(GROK_ROW, CONTENT_BODY)).toBe(true);
  });

  it('the page hook stores no capture for the list request', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { fakeWindow, posted } = makeFakeWindow(LIST_BODY);
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(LIST_URL, { method: 'GET' });
    await settle();

    // Nothing captured, and no warning either: the list route is not a candidate at all, which is
    // why widening the row (gate 1 above) would also have to decide what a list body is worth.
    expect(capturesOf(posted)).toHaveLength(0);
    expect(warn).not.toHaveBeenCalled();
  });

  it('the leg that already receives the list body reads its ids, not its metadata', () => {
    const parsed = parseGrokListPage(LIST_BODY);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.ids).toEqual([ID]);
    // 🔴 The metadata is dropped on the floor here today: the returned page names ids, a total and
    //    a cursor, and no per-conversation title or time. `EnumPage.times` is the slot for a list's
    //    per-conversation time (`DebtTimeSource` already names a `list-create` source) — and the
    //    grok plan fills neither, because the field names above are source-backed rather than
    //    measured on a live list response, and a field name we have not measured is a field name
    //    this repository does not read.
    expect(Object.keys(parsed.page).sort()).toEqual(['ids', 'nextToken', 'total']);
    expect(parsed.page.times).toBeUndefined();
  });

  it('the join key the two responses share is the one this leg already holds', () => {
    // The session id comes out of the content URL we already capture...
    const fromContent = extractSessionId(LOAD_URL, CONTENT_BODY, PAGE_URL);
    expect(fromContent).toBe(ID);
    // ...and it is the same value the list body keys the conversation-level fields under.
    const body = JSON.parse(LIST_BODY) as { conversations: Record<string, unknown>[] };
    expect(body.conversations[0]!.conversationId).toBe(fromContent);
  });
});
