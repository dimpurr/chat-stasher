/**
 * W165 · W128 step 1 — the account fingerprint, end to end, in a real Chromium.
 *
 * Why this spec exists beside `tests/w165-account-fingerprint.test.ts`: the unit
 * suite runs in Node with a stubbed `browser`, where `crypto.subtle` is Node's and
 * `storage.local` is an object literal. That cannot show that the extension's own
 * service worker, its own WebCrypto and its own persisted salt produce a value —
 * only that the logic does. This spec loads the built extension unpacked, serves a
 * synthetic Claude page from claude.ai, lets the real capture path run, and reads
 * the bundle back out of the real outbox.
 *
 * 🔴 The strongest assertion here is the **recomputation**: the fingerprint is
 *    rebuilt with Node's `crypto.createHmac` from the salt the extension persisted,
 *    independently of the extension's own code path. A value that matched only
 *    itself would prove nothing about the construction being keyed, or stable, or
 *    derived from the id it claims to be derived from.
 *
 * Zero network, zero logged-in state, no real account data: claude.ai is
 * intercepted, the org id and conversation id are synthetic fixtures, and every
 * request that is not served by a fixture is aborted and counted.
 *
 * Claude is the platform this spec can make a positive claim about: it is the one
 * whose stable account/org id the extension already sees (it is a path segment of
 * the page's own request). The other five resolve to a named `unknown` on this
 * build, which is what the report's per-platform table records and what
 * `capture.spec.ts` case 1 pins for a ChatGPT body.
 */

import { createHash, createHmac } from 'node:crypto';
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
/** Kept in step with `lib/account-fingerprint.ts`'s ACCOUNT_FINGERPRINT_DOMAIN. */
const DOMAIN = 'chat-stasher/account-fingerprint/v1';

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

test('a Claude capture carries a fingerprint recomputable from the salt this install persisted', async ({ ext }) => {
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

  // 🔴 The field this task adds: a fingerprint, from the organization the page's
  //    own request named — not an unknown, because for Claude the id is visible.
  const account = bundle.account as Record<string, unknown>;
  expect(account.kind).toBe('fingerprint');
  expect(account.source).toBe('request-url-organization');
  expect(account.value).toMatch(/^[0-9a-f]{64}$/);

  // 🔴 The saltId on the bundle is the id of the salt this install really stored —
  //    not a second id invented at bundle time.
  const stored = (await readStorage(ext, [SALT_KEY]))[SALT_KEY] as { id: string; key: string };
  expect(stored, 'the capture path must have persisted a salt in storage.local').toBeTruthy();
  expect(account.saltId).toBe(stored.id);

  // 🔴 Recomputed from the persisted salt with an independent implementation. This
  //    is what pins the construction: keyed (not a bare digest), domain-separated,
  //    and derived from the platform id plus the organization.
  const expected = createHmac('sha256', Buffer.from(stored.key, 'base64'))
    .update(`${DOMAIN}\0claude\0${ORG}`)
    .digest('hex');
  expect(account.value).toBe(expected);

  // 🔴 Irreversible, in the two observable senses: the raw id is not in the field,
  //    and the value is not its unkeyed digest (which a small id space would let
  //    anyone brute-force straight back out of the archive).
  expect(JSON.stringify(account)).not.toContain(ORG);
  expect(account.value).not.toBe(createHash('sha256').update(ORG).digest('hex'));

  // Nothing left the machine, and the only traffic was the fixture's own.
  expect(log.escaped).toEqual([]);
  expect(log.unexpected).toEqual([]);
  expect(log.pageLoads).toEqual([`GET ${PAGE_PATH}`]);
  expect(log.apiResponses).toEqual([`GET ${API_PATH}`]);
});
