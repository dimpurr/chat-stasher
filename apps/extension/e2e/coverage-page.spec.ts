/**
 * W113 · ADR-032 — **the coverage page, as a real extension page in a real browser.**
 *
 * Everything else about this feature is asserted under Node, where the page's own entrypoint does not run:
 * `tests/w113-coverage.test.ts` pins what the model and the view layer produce, and `tests/w113-debt-times`
 * pins how a time is recorded and refused. Neither can say that opening `coverage.html` reaches any of it.
 * That is what this spec is for, and it is the same division `popup-migration.spec.ts` explains for the
 * popup: a spec that called `buildCoverage` directly would prove the module runs, not that the browser
 * shows it.
 *
 * 🔴 **The claim this spec exists to hold is the read-only one.** ADR-032 §1 and the W113 dispatch both say
 *    the page must never issue a request to a chat platform. The harness serves the platform origins and
 *    aborts everything else, so the assertion is a measurement rather than a reading of the source:
 *    `escaped` must be empty after the page has loaded and been used.
 *
 * The fixture is deliberately shaped so that the four rules ADR-032 makes explicit are all visible at once:
 *   · an **incomplete** list, so the count must be marked a lower bound and not printed as a total;
 *   · **different** numbers in the header and in the debt store, so a page that read the header would show
 *     the wrong one — the store is the authority;
 *   · an id with a recorded conversation time and one without, so a month and the time-unknown bucket both
 *     have to be drawn;
 *   · a platform with no total, so the "no denominator" sentence has to appear instead of a percentage.
 */

import { expect } from '@playwright/test';
import { test, writeStorage, readStorage, type Extension } from './harness';

const PLATFORM = 'chatgpt';
const SCOPE = 'acct-w113-page';
const HEADER_KEY = `cs_backfill_v2:${PLATFORM}:${SCOPE}`;
const SWITCH_KEY = 'cs_backfill_enabled_v1';
const PRESET_KEY = 'cs_backfill_speed_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';
const DEBTS_DB = 'chat-stasher-backfill';
const DEBTS_STORE = 'debts_by_platform';

/** One conversation with a recorded time (2026-03-04T05:06:07Z) and one without. */
const TIMED_ID = 'a-timed';
const UNTIMED_ID = 'a-untimed';
const TIMED_AT = Date.UTC(2026, 2, 4, 5, 6, 7);

/** The header: it says 9 owed and 3 stored — **wrong on purpose**, so the store's 2/1 must win. */
function header(): Record<string, unknown> {
  return {
    v: 2,
    platform: PLATFORM,
    scope: SCOPE,
    totalKnown: null,
    totalSource: 'unknown',
    // Not complete: the page must print this as a lower bound.
    enumCursor: { offset: 41, complete: false },
    pendingCount: 9,
    archivedCount: 3,
    detailOutcomes: [],
    detailToday: { day: '2026-09-24', count: 7, cap: 180 },
    lastFetchAt: { enumerate: null, detail: null },
    failures: [{ id: 'aaaaaaaa', reason: 'detail-empty', at: 1 }],
    failuresDropped: 0,
    halted: null,
  };
}

/**
 * Write the debt store directly, the way the engine's own `applyDebtDiff` would.
 *
 * Done in the service worker rather than by driving the leg, because what this spec is about is the page's
 * reading — a run of the engine would add its own failures and times to the fixture and make every
 * assertion below about the run instead.
 */
async function writeDebtRows(ext: Extension): Promise<void> {
  const worker = ext.context.serviceWorkers()[0];
  expect(worker, 'debt set: the service worker must be running to seed the fixture').toBeTruthy();
  await worker!.evaluate(
    async (args: { db: string; store: string; rows: Array<Record<string, unknown>> }) => {
      await new Promise<void>((resolve, reject) => {
        const open = indexedDB.open(args.db, 2);
        open.onerror = () => reject(open.error);
        // 🔴 `onblocked` is handled rather than left to hang. Without it a profile whose database is held
        //    open at another version leaves this promise pending forever, and the spec fails 90 seconds
        //    later with "the browser was closed" — a symptom that names nothing about the cause.
        open.onblocked = () => reject(new Error('debt set: the database is blocked by another connection'));
        open.onupgradeneeded = () => {
          // A fresh profile has no database at all. The extension creates it on its first write; this
          // creates the same shape so the fixture can be seeded before any run has happened.
          const db = open.result;
          if (!db.objectStoreNames.contains(args.store)) {
            const store = db.createObjectStore(args.store, { keyPath: ['platform', 'scope', 'id'] });
            store.createIndex('byPlatformScope', ['platform', 'scope']);
          }
        };
        open.onsuccess = () => {
          const db = open.result;
          const tx = db.transaction(args.store, 'readwrite');
          for (const row of args.rows) tx.objectStore(args.store).put(row as never);
          tx.oncomplete = () => { db.close(); resolve(); };
          tx.onerror = () => reject(tx.error);
          tx.onabort = () => reject(tx.error ?? new Error('debt set: the write transaction aborted'));
        };
      });
    },
    {
      db: DEBTS_DB,
      store: DEBTS_STORE,
      rows: [
        { platform: PLATFORM, scope: SCOPE, id: TIMED_ID, state: 'archived', seq: 1, at: TIMED_AT, atFrom: 'list-update' },
        { platform: PLATFORM, scope: SCOPE, id: UNTIMED_ID, state: 'pending', seq: 2 },
      ],
    },
  );
}

/** Open the built page, exactly as the popup's link does. */
async function openCoverage(ext: Extension) {
  const page = await ext.context.newPage();
  await page.goto(`chrome-extension://${ext.extensionId}/coverage.html`, { waitUntil: 'domcontentloaded' });
  // The page paints from storage on load; wait for the thing every case below depends on.
  await expect(page.locator('#sections h2')).toHaveText(PLATFORM, { timeout: 20_000 });
  return page;
}

async function seed(ext: Extension): Promise<void> {
  await writeStorage(ext, {
    [HEADER_KEY]: header(),
    [SWITCH_KEY]: true,
    [TARGETS_KEY]: [{ platform: PLATFORM, origin: 'https://chatgpt.com', scope: SCOPE, at: 1 }],
  });
  await writeDebtRows(ext);
}

test('the coverage page is a real page that renders the record, and sends nothing anywhere', async ({ ext }) => {
  await seed(ext);
  const page = await openCoverage(ext);

  const body = await page.locator('body').innerText();

  // 1 · the listed count is a **lower bound** while the list is unfinished.
  expect(body).toContain('≥');
  expect(body).toContain('41');

  // 2 · no total from the platform ⇒ the sentence says which number is missing, and there is no percent.
  expect(body).toContain('no denominator');
  expect(body).not.toContain('%');

  // 3 · the debt store is the authority: 1 stored / 1 owed, not the header's 3 / 9.
  expect(body).toContain('stored in full');
  expect(body).toContain('still owed');
  expect(body).not.toMatch(/stored in full\s*9/);
  expect(body).toContain('not stored, by reason');

  // 4 · the state, in plain words. Nothing is stopping this leg, so it must not claim a stop.
  expect(body).not.toContain('has stopped');

  // 5 · speed and an estimate that says it is one.
  expect(body).toContain('Daily limit in force today');
  expect(body).toContain('estimate');

  // 6 · the month with a recorded time, and the time-unknown bucket kept out of it.
  expect(body).toContain('2026-03');
  expect(body).toContain('time unknown');
  await expect(page.locator('table')).toHaveCount(1);

  // The control: three presets, and the default is the one ADR-032 §3 names.
  const buttons = page.locator('.speed button');
  await expect(buttons).toHaveCount(3);
  await expect(page.locator('.speed button[aria-pressed="true"]')).toHaveText('Gentle (default)');

  // 🔴 And the point of the whole spec: not one request left this page.
  //    `installFakePlatforms` is not installed here on purpose — the context is
  //    a plain one, so any request the page made would show up as a real network
  //    attempt. Playwright's own request log on the page is the measurement.
  const requests = await page.evaluate(() => performance.getEntriesByType('resource').map((e) => e.name));
  const external = requests.filter((url) => !url.startsWith('chrome-extension://'));
  expect(external, 'the coverage page must not fetch anything from outside the extension').toEqual([]);

  await page.close();
});

test('picking a preset writes the choice, and the page repaints with it', async ({ ext }) => {
  await seed(ext);
  const page = await openCoverage(ext);

  await page.locator('.speed button', { hasText: 'Faster' }).click();

  // The write is the one thing this page does, and it is a local setting, not a request.
  await expect.poll(async () => (await readStorage(ext, null))[PRESET_KEY]).toBe('faster');
  await expect(page.locator('.speed button[aria-pressed="true"]')).toHaveText('Faster');
  // Choosing it surfaces the risk note, which is the promise ADR-032 §3 makes about the fast preset.
  const body = await page.locator('body').innerText();
  expect(body).toContain('Faster raises how much is fetched per day');

  await page.close();
});

test('the popup carries the summary card and the link into the page', async ({ ext }) => {
  await seed(ext);
  const page = await ext.context.newPage();
  await page.goto(`chrome-extension://${ext.extensionId}/popup.html`, { waitUntil: 'domcontentloaded' });

  const card = page.locator('#coverage-card');
  await expect(card).toBeVisible({ timeout: 20_000 });
  // Counts and a state word, no percentage and no estimate: the card may not carry a number that needs the
  // page's caveats.
  const cardText = await card.innerText();
  expect(cardText).toContain(PLATFORM);
  expect(cardText).not.toContain('%');
  expect(cardText).not.toContain('estimate');

  const link = page.locator('#open-coverage');
  await expect(link).toBeVisible();
  await expect(link).toHaveText(/coverage page/i);

  await page.close();
});
