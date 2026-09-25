/**
 * W25 · The capture path, end to end, in a real Chromium.
 *
 * What is real here: the built extension is loaded unpacked into the browser,
 * the content scripts are injected because the page is served from the
 * platform's own origin, and the service worker's outbox is read back out of
 * IndexedDB. What is fake: the platform. Both origins are intercepted, a tiny
 * page and a hand-written response are served for them, and **every other
 * request is aborted and counted** — no name is ever resolved, and nothing in
 * this suite can reach a real chat website.
 *
 * The response bodies are hand-written against the field names the contract row
 * declares (`apps/extension/lib/contract.ts`), not generated from it: a body
 * generated from `requiredPaths` would keep passing after the row changed, which
 * is the one thing a fixture must not do.
 *
 * The assertions these specs carry, in the order the task states them:
 *  1. a conversation response makes the extension queue exactly one record,
 *     with the right platform and session id, and — with no native host
 *     registered — the record is still queued after the delivery attempt;
 *  2. a response whose shape does not match the contract row is not captured
 *     at all (0 records), while the page has proof it received that response;
 *  3. loading the same conversation again does not queue a second copy.
 */

import { expect, type Page } from '@playwright/test';
import {
  fixture,
  installFakePlatforms,
  readOutbox,
  test,
  waitForOutbox,
  type Extension,
  type FakePlatform,
} from './harness';

// ---------------------------------------------------------------------------
// The fixtures, spelled against the contract rows they exercise.
// ---------------------------------------------------------------------------

/** chatgpt · origins ['https://chatgpt.com', ...], pathHints ['/backend-api/conversation/'], GET. */
const CHATGPT_SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const CHATGPT_PAGE_PATH = `/c/${CHATGPT_SESSION_ID}`;
const CHATGPT_API_PATH = `/backend-api/conversation/${CHATGPT_SESSION_ID}`;

/** deepseek · origins ['https://chat.deepseek.com'], pathHints ['/api/v0/chat', '/chat/session'], GET. */
const DEEPSEEK_SESSION_ID = '9f8e7d6c-5b4a-4938-8271-0a1b2c3d4e5f';
const DEEPSEEK_PAGE_PATH = `/chat/session/${DEEPSEEK_SESSION_ID}`;
const DEEPSEEK_API_PATH = '/api/v0/chat/history_messages';

const CHATGPT_BODY = fixture('chatgpt-conversation.json');
const WRONG_SHAPE_BODY = fixture('chatgpt-wrong-shape.json');
const DEEPSEEK_BODY = fixture('deepseek-history.json');

function chatgpt(body: string): FakePlatform {
  return {
    origin: 'https://chatgpt.com',
    pagePath: CHATGPT_PAGE_PATH,
    apiPath: CHATGPT_API_PATH,
    apiBody: body,
  };
}

const DEEPSEEK: FakePlatform = {
  origin: 'https://chat.deepseek.com',
  pagePath: DEEPSEEK_PAGE_PATH,
  apiPath: DEEPSEEK_API_PATH,
  apiBody: DEEPSEEK_BODY,
};

interface CaptureOutcome {
  status: number;
  bytes: number;
}

type PageWithCapture = Window & { __csCapture?: Promise<CaptureOutcome> };

/**
 * Load the fixture page and wait for the request it makes to come back, so that
 * every later assertion is about a request that really happened.
 */
async function loadFixturePage(
  ext: Extension,
  url: string,
): Promise<{ page: Page; served: CaptureOutcome }> {
  const page = await ext.context.newPage();
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  const served = await page.evaluate(() => (window as PageWithCapture).__csCapture);
  if (!served) throw new Error('the fixture page did not run its request');
  return { page, served };
}

/** A record's payload is the inbox bundle; parse it rather than pattern-match it. */
function bundleOf(payload: string): Record<string, unknown> {
  const parsed: unknown = JSON.parse(payload);
  if (!parsed || typeof parsed !== 'object') throw new Error('the payload is not an object');
  return parsed as Record<string, unknown>;
}

// ---------------------------------------------------------------------------

test('1 · a conversation response is captured, and stays queued with no native host', async ({ ext }) => {
  const log = await installFakePlatforms(ext.context, [chatgpt(CHATGPT_BODY)]);

  const { served } = await loadFixturePage(ext, `https://chatgpt.com${CHATGPT_PAGE_PATH}`);
  expect(served.status).toBe(200);
  expect(served.bytes).toBe(CHATGPT_BODY.length);

  // Wait for the delivery attempt, not merely for the record: the outbox is
  // written *before* the delivery is attempted (§10), so a record with
  // `attempts: 0` is a capture whose write-ahead landed a moment ago. Asserting
  // on it here would be asserting on a race.
  const entries = await waitForOutbox(ext, (rows) => rows.length === 1 && rows[0]!.attempts >= 1);
  expect(entries).toHaveLength(1);
  const entry = entries[0]!;

  // The name is the shard's `source_file` on the host side (§6.2): platform + session id.
  expect(entry.name).toBe(`chatgpt-${CHATGPT_SESSION_ID}.json`);
  expect(entry.bytes).toBe(Buffer.byteLength(entry.payload, 'utf8'));

  const bundle = bundleOf(entry.payload);
  expect(bundle.schema).toBe('chat-stasher/inbox@2');
  expect(bundle.platform).toBe('chatgpt');
  expect(bundle.sessionId).toBe(CHATGPT_SESSION_ID);
  expect(bundle.url).toBe(`https://chatgpt.com${CHATGPT_API_PATH}`);
  expect(bundle.method).toBe('GET');
  expect(bundle.status).toBe(200);
  // Byte-for-byte what the fixture served: the capture passes the body through
  // untouched, so an archive built from it holds the response, not our reading of it.
  expect(bundle.raw).toMatchObject({ text: CHATGPT_BODY, bytes: Buffer.byteLength(CHATGPT_BODY, 'utf8') });

  // 🔴 W128 step 1 · In a real browser, through the real service worker and the
  //    real `storage.local`, every bundle carries an account field — and it is
  //    never absent, because `unknown` is a value with a reason rather than a
  //    missing key.
  //
  //    The kind asserted here is a fact about **this fixture**, not a claim about
  //    ChatGPT's real wire shape: `chatgpt-conversation.json` spells the
  //    conversation id and the mapping and nothing account-shaped, exactly as the
  //    file's own header says its bodies are hand-written against declared field
  //    names. So the honest answer for it is a named unknown, and a fingerprint
  //    appearing here would mean the scan had matched something that is not an
  //    account id. The fingerprint half — and every per-platform source — is
  //    pinned in `tests/w165-account-fingerprint.test.ts`, where a body can be
  //    stated exactly.
  expect(bundle.account).toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
  // 🔴 And irreversible in the observable sense: nothing in the field is the one
  //    id the capture certainly saw — the conversation's own.
  expect(JSON.stringify(bundle.account)).not.toContain(CHATGPT_SESSION_ID);

  // 🔴 No host is registered in this context, so the delivery attempt cannot
  //    succeed. The record must still be there afterwards: the outbox is
  //    write-ahead, and only a matching `ack` ever removes an entry (§1, §10).
  expect(entry.state).toBe('pending');
  expect(entry.attempts).toBe(1);
  expect(entry.lastAttemptAt).not.toBeNull();
  expect(entry.lastError).toBe('send-failed');

  // The whole point of intercepting the origins: nothing left the machine.
  expect(log.escaped).toEqual([]);
  expect(log.unexpected).toEqual([]);
  expect(log.pageLoads).toEqual([`GET ${CHATGPT_PAGE_PATH}`]);
  expect(log.apiResponses).toEqual([`GET ${CHATGPT_API_PATH}`]);
});

test('2 · a response that does not match the contract row is not captured', async ({ ext }) => {
  // `requiredPaths: ['mapping', 'current_node']` for this row. This body is
  // valid JSON, arrives with status 200, and is missing both — the drift case
  // the shape gate exists to refuse. Serving real-looking drift is the only way
  // "0 records" can mean anything.
  const log = await installFakePlatforms(ext.context, [chatgpt(WRONG_SHAPE_BODY)]);

  const { served } = await loadFixturePage(ext, `https://chatgpt.com${CHATGPT_PAGE_PATH}`);
  expect(served.status).toBe(200);
  expect(served.bytes).toBe(WRONG_SHAPE_BODY.length);
  expect(log.apiResponses).toEqual([`GET ${CHATGPT_API_PATH}`]);

  const entries = await readOutbox(ext);
  expect(entries).toEqual([]);

  expect(log.escaped).toEqual([]);
  expect(log.unexpected).toEqual([]);
});

test('3 · loading the same conversation again does not queue a second copy', async ({ ext }) => {
  // 🔴 Stated plainly, because it bounds what this case proves: `capturedAt` is
  //    part of the bundle, so two views of one unchanged response are byte-
  //    identical only when they happen at the same instant. Pinning the page's
  //    clock makes that condition real instead of timing-dependent. What the
  //    case proves is the outbox's own rule — an identical payload is not queued
  //    twice — exercised through the whole path rather than at the module.
  //    The unpinned timing is measured and reported in the W25 report; it is a
  //    different number, for a reason that is in the payload.
  await ext.context.clock.setFixedTime(new Date('2026-09-14T12:00:00.000Z'));

  const log = await installFakePlatforms(ext.context, [chatgpt(CHATGPT_BODY)]);

  const { page } = await loadFixturePage(ext, `https://chatgpt.com${CHATGPT_PAGE_PATH}`);
  const first = await waitForOutbox(ext, (rows) => rows.length >= 1);
  expect(first).toHaveLength(1);

  await page.reload({ waitUntil: 'domcontentloaded' });
  const second = await page.evaluate(() => (window as PageWithCapture).__csCapture);
  expect(second?.status).toBe(200);

  // The second view really did re-deliver the response — twice served, once queued.
  expect(log.pageLoads).toHaveLength(2);
  expect(log.apiResponses).toHaveLength(2);

  const after = await readOutbox(ext);
  expect(after).toHaveLength(1);
  expect(after[0]!.sha256).toBe(first[0]!.sha256);
  expect(after[0]!.attempts).toBe(1);

  expect(log.escaped).toEqual([]);
  expect(log.unexpected).toEqual([]);
});

test('4 · the capture path is the platform table, not one hard-coded origin', async ({ ext }) => {
  // The same fixture and the same machinery, on the second origin the task
  // names. A hook that only ever worked on one origin would pass cases 1-3.
  const log = await installFakePlatforms(ext.context, [DEEPSEEK]);

  const { served } = await loadFixturePage(ext, `https://chat.deepseek.com${DEEPSEEK_PAGE_PATH}`);
  expect(served.status).toBe(200);
  expect(served.bytes).toBe(DEEPSEEK_BODY.length);

  const entries = await waitForOutbox(ext, (rows) => rows.length >= 1);
  expect(entries).toHaveLength(1);
  const bundle = bundleOf(entries[0]!.payload);
  expect(bundle.platform).toBe('deepseek');
  // The API URL carries no conversation id on this platform; the id comes from
  // the page URL, which is what the row's second pattern is for.
  expect(bundle.sessionId).toBe(DEEPSEEK_SESSION_ID);
  expect(entries[0]!.name).toBe(`deepseek-${DEEPSEEK_SESSION_ID}.json`);

  expect(log.escaped).toEqual([]);
  expect(log.unexpected).toEqual([]);
});
