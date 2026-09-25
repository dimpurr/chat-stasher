/**
 * W149 · Screenshot generation for the coverage page and the popup card.
 *
 * This is not an assertion spec. It seeds one deliberate fixture of four
 * platform records, opens the real built pages, and writes PNGs into the
 * directory named by `CS_SHOTS_OUT`. It exists because the W149 redesign was
 * asked for before/after screenshots at two widths and both colour schemes, and
 * a screenshot produced by the same fixture on both sides of the change is the
 * only honest comparison: hand-picked states on one side would flatter exactly
 * the half of the design that was picked.
 *
 * 🔴 It is **skipped unless `CS_SHOTS_OUT` is set** — `pnpm e2e` (the gate) must
 *    stay an assertion suite. Writing PNGs into a caller-chosen directory is a
 *    local, one-shot side effect, not something every clone running the gate
 *    should do. The phase in the file names comes from `CS_SHOTS_PHASE`
 *    (`before` / `after`), so the two runs cannot overwrite each other.
 *
 * The fixture is synthetic on purpose (CLAUDE.md's privacy rule): no real
 * conversation id, no real account scope, no conversation text. The months are
 * generated relative to the real clock so the distribution looks like a
 * history whatever day the shots are taken on.
 */

import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { expect } from '@playwright/test';
import { test, writeStorage, type Extension } from './harness';

const OUT = process.env.CS_SHOTS_OUT ?? '';
const PHASE = process.env.CS_SHOTS_PHASE ?? 'shot';
const SWITCH_KEY = 'cs_backfill_enabled_v1';
const PRESET_KEY = 'cs_backfill_speed_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';
const TICK_KEY = 'cs_backfill_lasttick_v1';
const DEBTS_DB = 'chat-stasher-backfill';
const DEBTS_STORE = 'debts_by_platform';
const DAY = new Date().toISOString().slice(0, 10);
const NOW = Date.now();



/**
 * Write the debt rows straight into the store the way the engine would, so the
 * page's "the store is the authority" path is the one the screenshots show.
 * (Copied in shape from coverage-page.spec.ts rather than shared through the
 * harness: the harness is an assertion scaffold and this generator must not
 * grow a dependency on it.)
 */
async function writeDebtRows(ext: Extension, rows: Array<Record<string, unknown>>): Promise<void> {
  const worker = ext.context.serviceWorkers()[0];
  expect(worker, 'shots: the service worker must be running to seed the fixture').toBeTruthy();
  await worker!.evaluate(
    async (args: { db: string; store: string; rows: Array<Record<string, unknown>> }) => {
      await new Promise<void>((resolve, reject) => {
        const open = indexedDB.open(args.db, 2);
        open.onerror = () => reject(open.error);
        open.onblocked = () => reject(new Error('shots: the database is blocked by another connection'));
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
          for (const row of args.rows) tx.objectStore(args.store).put(row as never);
          tx.oncomplete = () => { db.close(); resolve(); };
          tx.onerror = () => reject(tx.error);
          tx.onabort = () => reject(tx.error ?? new Error('shots: the write transaction aborted'));
        };
      });
    },
    { db: DEBTS_DB, store: DEBTS_STORE, rows },
  );
}

/** A batch of synthetic conversation ids with times spread over `months` back from now. */
function timedIds(count: number, monthsBack: number): Array<{ id: string; at: number }> {
  const out: Array<{ id: string; at: number }> = [];
  for (let i = 0; i < count; i += 1) {
    const month = monthsBack - Math.floor((i * monthsBack) / Math.max(1, count));
    const at = NOW - month * 30 * 24 * 3600_000 - (i % 27) * 3600_000 - 40 * i;
    out.push({ id: `c-${monthsBack}-${String(i).padStart(3, '0')}`, at });
  }
  return out;
}

function seededHeader(platform: string, scope: string, over: Record<string, unknown>): Record<string, unknown> {
  return {
    v: 2,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pendingCount: 0,
    archivedCount: 0,
    detailOutcomes: [],
    detailToday: { day: DAY, count: 0, cap: 400 },
    lastFetchAt: { enumerate: NOW - 26 * 60_000, detail: NOW - 4 * 60_000 },
    failures: [],
    failuresDropped: 0,
    halted: null,
    ...over,
  };
}

async function seed(ext: Extension): Promise<void> {
  // chatgpt · default: the big running leg. Still listing (offset < end), a
  // healthy quota draw, months of history and a few time-unknown ids.
  const gptArchived = timedIds(184, 5);
  const gptPending = timedIds(27, 1);
  const rows: Array<Record<string, unknown>> = [];
  for (const { id, at } of gptArchived) {
    rows.push({ platform: 'chatgpt', scope: 'default', id, state: 'archived', seq: rows.length, at, atFrom: 'list-update' });
  }
  for (const { id, at } of gptPending.slice(0, 21)) {
    rows.push({ platform: 'chatgpt', scope: 'default', id, state: 'pending', seq: rows.length, at, atFrom: 'list-create' });
  }
  // Six with no recorded time at all: the time-unknown bucket has to be visible.
  for (let i = 0; i < 6; i += 1) {
    rows.push({ platform: 'chatgpt', scope: 'default', id: `c-no-time-${i}`, state: 'pending', seq: rows.length });
  }

  // claude · default: waiting, because the last wake found no page of it open.
  const claudeArchived = timedIds(305, 6);
  for (const { id, at } of claudeArchived) {
    rows.push({ platform: 'claude', scope: 'default', id, state: 'archived', seq: rows.length, at, atFrom: 'list-update' });
  }
  for (let i = 0; i < 64; i += 1) {
    const at = NOW - (i % 5) * 30 * 24 * 3600_000;
    rows.push({ platform: 'claude', scope: 'default', id: `c-cl-p-${String(i).padStart(3, '0')}`, state: 'pending', seq: rows.length, at, atFrom: 'list-update' });
  }

  // chatgpt · ws:ab12: finished. A response total, a complete list, nothing owed.
  const wsArchived = timedIds(46, 3);
  for (const { id, at } of wsArchived) {
    rows.push({ platform: 'chatgpt', scope: 'ws:ab12', id, state: 'archived', seq: rows.length, at, atFrom: 'list-update' });
  }

  // deepseek · default: held by a transient stop, with failures and a dropped overflow.
  const dsArchived = timedIds(24, 4);
  for (const { id, at } of dsArchived) {
    rows.push({ platform: 'deepseek', scope: 'default', id, state: 'archived', seq: rows.length, at, atFrom: 'list-update' });
  }
  for (const { id, at } of timedIds(118, 2)) {
    rows.push({ platform: 'deepseek', scope: 'default', id, state: 'pending', seq: rows.length, at, atFrom: 'list-update' });
  }

  const targets = [
    { platform: 'chatgpt', scope: 'default', origin: 'https://chatgpt.com', at: 1 },
    { platform: 'chatgpt', scope: 'ws:ab12', origin: 'https://chatgpt.com', at: 1 },
    { platform: 'claude', scope: 'default', origin: 'https://claude.ai', at: 1 },
    { platform: 'deepseek', scope: 'default', origin: 'https://chat.deepseek.com', at: 1 },
  ];

  await writeStorage(ext, {
    ...Object.fromEntries([
      [`cs_backfill_v2:chatgpt:default`, seededHeader('chatgpt', 'default', {
        enumCursor: { offset: 250, complete: false },
        pendingCount: 27,
        archivedCount: 184,
        detailToday: { day: DAY, count: 42, cap: 400 },
      })],
      [`cs_backfill_v2:chatgpt:ws:ab12`, seededHeader('chatgpt', 'ws:ab12', {
        totalKnown: 46,
        totalSource: 'response-total',
        enumCursor: { offset: 46, complete: true },
        pendingCount: 0,
        archivedCount: 46,
        detailToday: { day: DAY, count: 6, cap: 400 },
      })],
      [`cs_backfill_v2:claude:default`, seededHeader('claude', 'default', {
        enumCursor: { offset: 369, complete: false },
        pendingCount: 64,
        archivedCount: 305,
        detailToday: { day: DAY, count: 88, cap: 400 },
        parkedEmpty: ['c-parked-1', 'c-parked-2'],
        emptyStreak: 1,
      })],
      [`cs_backfill_v2:deepseek:default`, seededHeader('deepseek', 'default', {
        enumCursor: { offset: 142, complete: false, truncated: 'has-more-missing' },
        pendingCount: 118,
        archivedCount: 24,
        detailToday: { day: DAY, count: 400, cap: 400 },
        failures: [
          { id: 'bbbbbbbb', reason: 'detail-too-long', at: NOW - 20 * 3600_000 },
          { id: 'cccccccc', reason: 'detail-too-long', at: NOW - 19 * 3600_000 },
          { id: 'dddddddd', reason: 'detail-empty', at: NOW - 18 * 3600_000 },
        ],
        failuresDropped: 5,
        halted: {
          reason: 'rate-limited',
          detail: 'HTTP 429 from the history endpoint, after 3 requests in the streak',
          at: NOW - 47 * 60_000,
          retryAt: NOW + 33 * 60_000,
          attempts: 3,
        },
      })],
      [SWITCH_KEY, true],
      [PRESET_KEY, 'standard'],
      [TARGETS_KEY, targets],
      [TICK_KEY, {
        at: NOW - 18 * 60_000,
        ran: true,
        reason: 'ran',
        targets: 4,
        schedule: { served: 'chatgpt', skipped: [{ platform: 'claude', reason: 'no-http-port' }] },
      }],
    ] satisfies [string, unknown][] as Array<[string, unknown]>),
  });
  await writeDebtRows(ext, rows);
}

const PAGES: Array<{ name: string; width: number }> = [
  { name: 'page', width: 1280 },
  { name: 'page', width: 420 },
  { name: 'popup', width: 1280 },
  { name: 'popup', width: 420 },
];
const SCHEMES: Array<'light' | 'dark'> = ['light', 'dark'];

test('generate the W149 fixture screenshots', async ({ ext }) => {
  test.skip(!OUT, 'CS_SHOTS_OUT sets the directory the PNGs are written to');
  mkdirSync(OUT, { recursive: true });
  await seed(ext);

  for (const { name, width } of PAGES) {
    const page = await ext.context.newPage();
    await page.setViewportSize({ width, height: width >= 1000 ? 960 : 980 });
    await page.goto(`chrome-extension://${ext.extensionId}/${name === 'page' ? 'coverage' : 'popup'}.html`, {
      waitUntil: 'domcontentloaded',
    });
    for (const scheme of SCHEMES) {
      await page.emulateMedia({ colorScheme: scheme });
      // The old and the new DOM both agree on these two contracts: a platform
      // heading inside #sections, and the always-present-or-hidden card.
      if (name === 'page') {
        await page.waitForSelector('#sections h2', { timeout: 20_000 });
      } else {
        await page.waitForSelector('#coverage-card:not([hidden])', { timeout: 20_000 });
      }
      await page.waitForTimeout(350);
      const target = page.locator('body');
      await target.screenshot({
        path: join(OUT, `${PHASE}-${name}-${width}-${scheme}.png`),
      });
    }
    await page.close();
  }
});
