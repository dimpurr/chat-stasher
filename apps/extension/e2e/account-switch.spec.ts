/**
 * W199 · W128 step 2 — a switch under a run, in a real Chromium with the real service worker.
 *
 * Why this spec exists beside `tests/w199-account-lease.test.ts` and
 * `tests/w199-account-suspension.test.ts`: those run in Node with a stubbed `browser`, where
 * `storage.local` is an object literal and the engine is called directly. They can say the
 * *logic* is right. They cannot say that the built extension, woken by the alarm it really
 * arms, running the real engine against the real store, both **stops** and leaves a record a
 * reader can see.
 *
 * 🔴 **The fingerprint is computed here, independently.** The lease seeded below is built with
 *    Node's `crypto.createHmac` from a salt this spec generated; the extension recomputes the
 *    same value in its own service worker from the same stored salt. Two implementations of
 *    the construction agreeing across the process boundary is the evidence that the comparison
 *    is about the account and not about two copies of our own arithmetic.
 *
 * 🔴 **The two claims this spec holds are the two the issue names.** A recorded account's
 *    *list ids* must not enter that scope's ledger, and its *pending queue* must be untouched —
 *    so the fixture owes one id and the platform answers with a different account's list, and
 *    both the debt rows and the counts are read back afterwards.
 *
 * Zero network: grok.com is intercepted, every request that is not served by a fixture is
 * aborted and counted, and `escaped` must be empty. No real account id, conversation id or
 * email appears anywhere below.
 */

import { createHmac, randomBytes, randomUUID } from 'node:crypto';
import { expect } from '@playwright/test';
import { installFakePlatforms, readStorage, test, writeStorage, type Extension } from './harness';

const ORIGIN = 'https://grok.com';
const PLATFORM = 'grok';
const PAGE_PATH = '/c/fixture-page';
/** The account the scope names — a fixture, and the value the scope key is built from. */
const ACCOUNT_A = 'acct-fixture-1';
/** The account the platform answers as, once switched. */
const ACCOUNT_B = 'acct-fixture-2';
/** One conversation owed under A, and not one of B's. */
const A_DEBT = 'cc111111-0000-4000-8000-000000000001';
/** What B's list says it has. It must never reach A's ledger. */
const B_ID = 'cc999999-0000-4000-8000-000000000009';

const LIST_PATH = '/rest/app-chat/conversations';
const DETAIL_PATH = `/rest/app-chat/conversations/${A_DEBT}/load-responses`;
const PAGE_URL = `${ORIGIN}${PAGE_PATH}`;

/** A page that makes one same-origin POST — grok's content route is a POST. */
const PAGE_HTML = `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>fixture</title></head>
<body><p id="fixture">e2e fixture page</p>
<script>
window.__csCapture = (async () => {
  const response = await fetch(${JSON.stringify(DETAIL_PATH)}, {
    method: 'POST',
    headers: { accept: 'application/json', 'content-type': 'application/json' },
    body: '{}',
  });
  const text = await response.text();
  return { status: response.status, bytes: text.length };
})();
</script></body></html>`;

const SALT_KEY = 'cs_account_salt_v1';
const SWITCH_KEY = 'cs_backfill_enabled_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';
const HEADER_KEY = `cs_backfill_v2:${PLATFORM}:${ACCOUNT_A}`;
const DEBTS_DB = 'chat-stasher-backfill';
const DEBTS_STORE = 'debts_by_platform';
const TICK_ALARM = 'cs-backfill-tick';
/** Kept in step with `lib/account-fingerprint.ts`'s ACCOUNT_FINGERPRINT_DOMAIN. */
const DOMAIN = 'chat-stasher/account-fingerprint/v1';

/** An install's salt, generated here so the expected fingerprints are ours and not the code's. */
const SALT = { id: randomUUID(), key: randomBytes(32).toString('base64'), createdAt: Date.now() };

/** HMAC-SHA256 of `domain \0 platform \0 id`, keyed with the salt above. */
function fingerprint(accountId: string): string {
  return createHmac('sha256', Buffer.from(SALT.key, 'base64'))
    .update(`${DOMAIN}\0${PLATFORM}\0${accountId}`)
    .digest('hex');
}

/** The list body that makes the switch provable: it names account B. */
const LIST_AS_B = JSON.stringify({
  conversations: [{ conversationId: B_ID }],
  nextPageToken: null,
  user_id: ACCOUNT_B,
});

/**
 * The body a *capture* carries: enough for grok's shape gate, and it names **A**.
 *
 * 🔴 Case 1 is about the segment a *run* drives, so the page and the capture must agree with
 *    the recorded scope — the switch is proved by the list the run asks for, not by the page
 *    the fixture opens. If the capture named B, the capture path would suspend A before any
 *    run happened, and the case would be case 2 wearing a different name.
 */
const CAPTURE_AS_A = JSON.stringify({
  responses: [{ responseId: 'r1', message: 'synthetic fixture', sender: 'human' }],
  user_id: ACCOUNT_A,
});
/** The same body naming **B**: the capture itself is the observation that proves the switch. */
const CAPTURE_AS_B = JSON.stringify({
  responses: [{ responseId: 'r1', message: 'synthetic fixture', sender: 'human' }],
  user_id: ACCOUNT_B,
});

/**
 * The harness's platform row. Two of them, because the two cases need the *capture* to name a
 * different account: case 1 needs it to agree with the scope (so the run is what proves the
 * switch), and case 2 needs it to disagree (so the capture is what proves it).
 */
const grokRow = (apiBody: string) => ({
  origin: ORIGIN,
  pagePath: PAGE_PATH,
  apiPath: DETAIL_PATH,
  apiBody,
});

/** The header a recorded scope has: a lease for A, and a list that is not finished. */
function headerA(): Record<string, unknown> {
  return {
    v: 2,
    platform: PLATFORM,
    scope: ACCOUNT_A,
    totalKnown: null,
    totalSource: 'unknown',
    // Not complete, so the run really issues a list request — which is the segment this
    // spec is about.
    enumCursor: { offset: 0, complete: false },
    pendingCount: 1,
    archivedCount: 0,
    detailToday: { day: '2026-09-26', count: 0, cap: null },
    accountLease: {
      value: fingerprint(ACCOUNT_A),
      saltId: SALT.id,
      source: 'response-body-platform-uid',
      at: 1,
    },
    halted: null,
  };
}

/** Seed A's one owed id, exactly as the engine's own write would. */
async function seedDebtRow(ext: Extension): Promise<void> {
  const worker = ext.context.serviceWorkers()[0];
  expect(worker, 'debt set: the service worker must be running to seed the fixture').toBeTruthy();
  await worker!.evaluate(
    async (args: { db: string; store: string; row: Record<string, unknown> }) => {
      await new Promise<void>((resolve, reject) => {
        const open = indexedDB.open(args.db, 2);
        open.onerror = () => reject(open.error);
        open.onblocked = () => reject(new Error('debt set: the database is blocked by another connection'));
        open.onupgradeneeded = () => {
          const db = open.result;
          if (!db.objectStoreNames.contains(args.store)) {
            const store = db.createObjectStore(args.store, { keyPath: ['platform', 'scope', 'id'] });
            store.createIndex('byPlatformScope', ['platform', 'scope']);
          }
        };
        open.onsuccess = () => {
          const db = open.result;
          const tx = db.transaction(args.store, 'readwrite');
          tx.objectStore(args.store).put(args.row as never);
          tx.oncomplete = () => { db.close(); resolve(); };
          tx.onerror = () => reject(tx.error);
          tx.onabort = () => reject(tx.error ?? new Error('debt set: the write transaction aborted'));
        };
      });
    },
    {
      db: DEBTS_DB,
      store: DEBTS_STORE,
      row: { platform: PLATFORM, scope: ACCOUNT_A, id: A_DEBT, state: 'pending', seq: 1 },
    },
  );
}

/** Every debt row for A, as `id:state` pairs, sorted — the queue's own shape. */
async function debtRowsOfA(ext: Extension): Promise<string[]> {
  const worker = ext.context.serviceWorkers()[0];
  const rows = await worker!.evaluate(
    async (args: { db: string; store: string; platform: string; scope: string }) => {
      return await new Promise<string[]>((resolve, reject) => {
        const open = indexedDB.open(args.db, 2);
        open.onerror = () => reject(open.error);
        open.onsuccess = () => {
          const db = open.result;
          const tx = db.transaction(args.store, 'readonly');
          const index = tx.objectStore(args.store).index('byPlatformScope');
          const request = index.getAll([args.platform, args.scope]);
          request.onsuccess = () => {
            const found = (request.result as Array<{ id: string; state: string }>)
              .map((row) => `${row.id}:${row.state}`);
            db.close();
            resolve(found);
          };
          request.onerror = () => { db.close(); reject(request.error); };
        };
      });
    },
    { db: DEBTS_DB, store: DEBTS_STORE, platform: PLATFORM, scope: ACCOUNT_A },
  );
  return rows.sort();
}

test('a run whose responses come from another account stops, and the scope keeps everything it owes', async ({ ext }) => {
  // The harness's own interception (it aborts everything else and answers the favicon, so a
  // browser-generated request cannot be mistaken for escaped traffic) …
  const log = await installFakePlatforms(ext.context, [grokRow(CAPTURE_AS_A)]);
  // … with the page and the list route overridden: the fixture page does a GET, and grok's
  // content route is a **POST**; and the list is the segment this case is about. Registered
  // after the harness's routes, so these win.
  await ext.context.route(PAGE_URL, (route) =>
    route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: PAGE_HTML }));
  await ext.context.route(`${ORIGIN}${LIST_PATH}*`, (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: LIST_AS_B }));
  // 🔴 The harness's catch-all aborts everything that is not a fixture, and the coverage page
  //    is `chrome-extension://…`, so it is passed through explicitly. Registered last, so it is
  //    matched first — and it is deliberately not logged as escaped traffic, because it is the
  //    extension's own page and not a request that left the machine.
  await ext.context.route('chrome-extension://**', (route) => route.continue());

  // A recorded scope: its own account, one conversation owed, a list not finished, and the
  // switch on. The salt is this spec's, so the extension must recompute the same value.
  await writeStorage(ext, {
    [SALT_KEY]: SALT,
    [SWITCH_KEY]: true,
    [TARGETS_KEY]: [{ platform: PLATFORM, origin: ORIGIN, scope: ACCOUNT_A, at: 1 }],
    [HEADER_KEY]: headerA(),
  });
  await seedDebtRow(ext);

  // A live grok page, so the leg has a channel to make its requests through.
  const page = await ext.context.newPage();
  await page.goto(PAGE_URL, { waitUntil: 'domcontentloaded' });
  await page.evaluate(() => (window as unknown as { __csCapture: Promise<unknown> }).__csCapture);

  const before = await debtRowsOfA(ext);
  expect(before).toEqual([`${A_DEBT}:pending`]);

  // 🔴 The capture the page made agrees with the scope, so the scope is runnable and the tick
  //    below is what proves the switch. (This is also the cross-implementation check again, on
  //    the live leg: the bundle's own account value is the one this spec computed in Node.)
  const { readOutbox } = await import('./harness');
  const captured = await readOutbox(ext);
  expect(captured.length).toBeGreaterThan(0);
  const capturedBundle = JSON.parse(captured[0]!.payload) as { account: { value: string } };
  expect(capturedBundle.account.value).toBe(fingerprint(ACCOUNT_A));
  expect((await readStorage(ext, [HEADER_KEY]))[HEADER_KEY]).toMatchObject({ halted: null });

  // Wake the leg the way the browser does.
  const worker = ext.context.serviceWorkers()[0];
  expect(worker, 'the service worker must be running to fire the tick').toBeTruthy();
  await worker!.evaluate(async (name: string) => {
    await (globalThis as unknown as {
      chrome: { alarms: { create(n: string, info: { when: number }): Promise<void> } };
    }).chrome.alarms.create(name, { when: Date.now() + 50 });
  }, TICK_ALARM);
  await page.waitForTimeout(1500);

  // 🔴 The record: the run stopped for the account reason, and it says **which** account
  //    answered — a fingerprint this spec computes independently, not the id.
  const headers = await readStorage(ext, [HEADER_KEY]);
  const header = headers[HEADER_KEY] as Record<string, unknown>;
  expect(header).toBeTruthy();
  expect((header.halted as { reason?: string } | null)?.reason).toBe('account-changed');

  const suspended = header.suspended as Record<string, unknown>;
  expect(suspended, 'the run must leave a suspension, not only a halt').toBeTruthy();
  expect(suspended.reason).toBe('account-changed');
  expect((suspended.observed as { value: string }).value).toBe(fingerprint(ACCOUNT_B));
  expect((suspended.lease as { value: string }).value).toBe(fingerprint(ACCOUNT_A));
  // The record carries fingerprints only — never the raw ids it compared.
  expect(JSON.stringify(suspended)).not.toContain(ACCOUNT_A);
  expect(JSON.stringify(suspended)).not.toContain(ACCOUNT_B);

  // 🔴 Nothing of B's reached this scope's ledger, and nothing already owed moved.
  const after = await debtRowsOfA(ext);
  expect(after).toEqual(before);
  expect(after.some((row) => row.startsWith(B_ID))).toBe(false);
  expect(header.pendingCount).toBe(1);
  expect(header.archivedCount).toBe(0);

  // 🔴 And the coverage page says so, in the browser, from the record this run wrote.
  const coverage = await ext.context.newPage();
  await coverage.goto(`chrome-extension://${ext.extensionId}/coverage.html`, { waitUntil: 'domcontentloaded' });
  // Both scopes of this platform have a record (the suspended one and the capture's own), so
  // there are two sections and the first is enough to say the page painted.
  await expect(coverage.locator('#sections h2').first()).toHaveText(PLATFORM, { timeout: 20_000 });
  await coverage.evaluate(() => {
    document.querySelectorAll('details').forEach((node) => { node.setAttribute('open', ''); });
  });
  const text = await coverage.locator('body').innerText();
  expect(text).toContain('holding this account');
  expect(text).toContain('Account this scope belongs to');

  // Nothing left the machine.
  expect(log.escaped).toEqual([]);
});

test('a capture of another account suspends the scope that account no longer answers for', async ({ ext }) => {
  // The same recorded scope, and a page whose capture names B. The harness's row serves the
  // detail route (grok's content route) with B's body; only the page is overridden, because
  // the harness fixture does a GET and this route is a POST.
  const log = await installFakePlatforms(ext.context, [grokRow(CAPTURE_AS_B)]);
  await ext.context.route(PAGE_URL, (route) =>
    route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: PAGE_HTML }));

  await writeStorage(ext, {
    [SALT_KEY]: SALT,
    [SWITCH_KEY]: false,
    [TARGETS_KEY]: [{ platform: PLATFORM, origin: ORIGIN, scope: ACCOUNT_A, at: 1 }],
    [HEADER_KEY]: headerA(),
  });
  // The owed id really is in the store: a header that claims an id the store does not hold is
  // W45's `ledger-mismatch`, and the scope would refuse to run (and refuse to be written to)
  // for a reason that has nothing to do with this spec.
  await seedDebtRow(ext);

  const page = await ext.context.newPage();
  await page.goto(PAGE_URL, { waitUntil: 'domcontentloaded' });
  await page.evaluate(() => (window as unknown as { __csCapture: Promise<unknown> }).__csCapture);
  await page.waitForTimeout(1500);

  const state = await readStorage(ext, [HEADER_KEY, TARGETS_KEY]);
  const header = state[HEADER_KEY] as Record<string, unknown>;
  const suspended = header.suspended as Record<string, unknown>;
  expect(suspended, 'a capture naming another account must suspend the old scope').toBeTruthy();
  expect((suspended.observed as { value: string }).value).toBe(fingerprint(ACCOUNT_B));

  // The switch's own scope was started, which is the "resume the scope for the new account"
  // half: a target exists for B, so the next wake enumerates B rather than nobody.
  const targets = state[TARGETS_KEY] as Array<{ platform: string; scope: string }>;
  expect(targets.some((t) => t.platform === PLATFORM && t.scope === ACCOUNT_B)).toBe(true);

  // 🔴 The old scope's own work is untouched: a suspension is not a cleanup.
  expect(header.pendingCount).toBe(1);
  expect(header.archivedCount).toBe(0);

  expect(log.escaped).toEqual([]);
});
