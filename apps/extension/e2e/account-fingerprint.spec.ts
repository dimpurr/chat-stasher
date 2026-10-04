/**
 * W165 · W128 step 1 — the account field, end to end, in a real Chromium.
 *
 * Why this spec exists beside `tests/w165-account-fingerprint.test.ts`: the unit
 * suite runs in Node with a stubbed `browser`, where `crypto.subtle` is Node's and
 * `storage.local` is an object literal. That cannot show what the extension's own
 * service worker, its own WebCrypto and its own persisted storage do — only that
 * the logic does. This spec loads the built extension unpacked, serves a synthetic
 * Claude page from claude.ai, lets the real capture path run, and reads the bundle
 * back out of the real outbox.
 *
 * 🔴 W337 adds a real Claude current-user lookup to this spec's capture path. The
 *    organization remains the conversation namespace, but the page's current-user
 *    response supplies the account id. The spec verifies the HMAC is persisted while
 *    the raw synthetic id is absent from the archived bundle.
 *
 * 🔴 The construction is still proven in a real browser, by
 *    `account-switch.spec.ts`: it drives a **grok** capture, where the account id is a
 *    person, and checks the bundle's value against an HMAC rebuilt with Node's
 *    `crypto.createHmac` from the salt that spec pre-seeded. Neither spec proves the
 *    other's half, which is why both exist.
 *
 * Zero network, zero logged-in state, no real account data: claude.ai is
 * intercepted, the org id and conversation id are synthetic fixtures, and every
 * request that is not served by a fixture is aborted and counted.
 */

import { expect } from '@playwright/test';
import {
  installFakePlatforms,
  readOutbox,
  readStorage,
  test,
  waitForOutbox,
  type FakePlatform,
} from './harness';

const ORIGIN = 'https://claude.ai';
const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const PAGE_PATH = `/chat/${SESSION_ID}`;
const API_PATH = `/api/organizations/${ORG}/chat_conversations/${SESSION_ID}`;
const SALT_KEY = 'cs_account_salt_v1';
const USER_ID = 'synthetic-claude-user-337';

/** Hand-written against the row's `requiredPaths: ['chat_messages']`, not generated from it. */
const CLAUDE_BODY = JSON.stringify({
  uuid: SESSION_ID,
  name: 'synthetic',
  model: 'synthetic',
  chat_messages: [
    { uuid: 'synthetic-m1', sender: 'human', content: [{ type: 'text', text: 'synthetic fixture' }] },
  ],
});

const CLAUDE: FakePlatform = {
  origin: ORIGIN,
  pagePath: PAGE_PATH,
  apiPath: API_PATH,
  apiBody: CLAUDE_BODY,
};

function bundleOf(payload: string): Record<string, unknown> {
  const parsed: unknown = JSON.parse(payload);
  if (!parsed || typeof parsed !== 'object') throw new Error('the payload is not an object');
  return parsed as Record<string, unknown>;
}

test('a Claude capture fingerprints the current user id and never archives the raw id', async ({ ext }) => {
  const log = await installFakePlatforms(ext.context, [CLAUDE]);
  let whoamiRequests = 0;
  await ext.context.route(`${ORIGIN}/api/account`, async (route) => {
    whoamiRequests += 1;
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ userId: USER_ID }) });
  });

  const page = await ext.context.newPage();
  await page.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  const served = await page.evaluate(() => (window as unknown as { __csCapture?: unknown }).__csCapture);
  expect(served).toBeTruthy();

  const entries = await waitForOutbox(ext, (rows) => rows.length >= 1 && rows[0]!.attempts >= 1);
  expect(entries).toHaveLength(1);
  const bundle = bundleOf(entries[0]!.payload);

  // The row's own identity, unchanged by this task.
  expect(bundle.platform).toBe('claude');
  expect(bundle.sessionId).toBe(SESSION_ID);
  expect(entries[0]!.name).toBe(`claude-${SESSION_ID}.json`);

  // 🔴 W337 · The per-user id comes from the page's cache-disabled who-am-I request,
  //    not from the organization path. The archived field contains only its HMAC.
  const account = bundle.account as Record<string, unknown>;
  expect(account).toMatchObject({ kind: 'fingerprint', source: 'response-body-claude-whoami' });
  expect(account.value).toMatch(/^[0-9a-f]{64}$/);
  expect(JSON.stringify(bundle)).not.toContain(USER_ID);

  // 🔴 The salt is install-local and contains no input id.
  const stored = (await readStorage(ext, [SALT_KEY]))[SALT_KEY];
  expect(stored).toBeTruthy();
  expect(JSON.stringify(stored)).not.toContain(USER_ID);

  // The organization stays the capture namespace and is not used as the account id.
  expect(bundle.url).toContain(ORG);

  // Nothing left the machine, and the only traffic was the fixture's own.
  expect(log.escaped).toEqual([]);
  expect(log.unexpected).toEqual([]);
  expect(log.pageLoads).toEqual([`GET ${PAGE_PATH}`]);
  expect(log.apiResponses).toEqual([`GET ${API_PATH}`]);
  expect(whoamiRequests).toBe(1);
});
