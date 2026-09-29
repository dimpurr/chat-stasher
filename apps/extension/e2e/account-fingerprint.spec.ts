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
 * 🔴 W239 changed what there is to assert here, and this spec asserts the new fact
 *    rather than a weaker version of the old one. W128 step 3: claude.ai files
 *    conversations under an **organization**, two accounts can be members of one, so a
 *    fingerprint over the organization is equal for both — and this spec used to
 *    recompute exactly that value. A real capture now records
 *    `organization-is-not-an-account`, creates no salt to key a value with, and leaves
 *    the organization where it is a fact (the bundle's own `url`).
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

test('a Claude capture records that an organization is not an account, and keys nothing', async ({ ext }) => {
  const log = await installFakePlatforms(ext.context, [CLAUDE]);

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

  // 🔴 W239 · **The account field carries no fingerprint, and says why.** This spec
  //    used to recompute the HMAC the Claude capture produced. That value was derived
  //    from the organization alone, and W128's step 3 is the fact that two accounts can
  //    share an organization — so the value was equal for both and the archive would
  //    have read one account's conversation as the other's. The construction itself is
  //    still pinned in a real browser, on a platform whose account id is a person:
  //    `account-switch.spec.ts` recomputes a grok fingerprint from the persisted salt
  //    the same way this spec used to. What is asserted here is the refusal.
  const account = bundle.account as Record<string, unknown>;
  expect(account).toEqual({ kind: 'unknown', reason: 'organization-is-not-an-account' });
  // No value, no salt id, no source: the field invents nothing to fill the gap.
  expect(account).not.toHaveProperty('value');

  // 🔴 Nothing was keyed, so nothing was created to key it with: the install's secret
  //    is not made for a value this build does not produce.
  const stored = (await readStorage(ext, [SALT_KEY]))[SALT_KEY];
  expect(stored, 'no fingerprint is produced here, so no salt may have been written').toBeUndefined();

  // 🔴 The organization is not *lost* by this — it is simply not called an account. It is
  //    still on the record verbatim, in the URL the capture was taken from (and in the
  //    backfill scope), which is where a reader who needs it looks.
  expect(JSON.stringify(account)).not.toContain(ORG);
  expect(bundle.url).toContain(ORG);

  // Nothing left the machine, and the only traffic was the fixture's own.
  expect(log.escaped).toEqual([]);
  expect(log.unexpected).toEqual([]);
  expect(log.pageLoads).toEqual([`GET ${PAGE_PATH}`]);
  expect(log.apiResponses).toEqual([`GET ${API_PATH}`]);
});
