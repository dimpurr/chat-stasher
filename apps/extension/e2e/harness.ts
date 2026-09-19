/**
 * The scaffolding the end-to-end specs share: launching a real Chromium with the
 * built extension loaded, serving a fake platform on the real origins, and
 * reading the extension's outbox back out of the service worker.
 *
 * Two properties of this file are load-bearing, and both are the project's own
 * invariants rather than test convenience:
 *
 *  · **No request leaves the machine.** Every request the context makes is
 *    either served by a fixture on one of the intercepted platform origins, or
 *    aborted and recorded. The specs assert that the recorded list is empty.
 *    Nothing here resolves a real hostname, and no spec ever reaches a real
 *    chat website.
 *
 *  · **"Zero" is a measurement, never a fallback.** `readOutbox` keeps the three
 *    states apart the same way `lib/outbox.ts` does: the store does not exist
 *    (nothing can ever have been written ⇒ `[]` is certain), the store exists
 *    and was read (⇒ the rows are what they are), and the store could not be
 *    read (⇒ it throws, so a spec can never read "I do not know" as "empty").
 */

import { chromium, test as base } from '@playwright/test';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { BrowserContext, Worker } from '@playwright/test';

/** The unpacked build `pnpm e2e` produces (`wxt build -b chrome`). */
export const EXTENSION_DIR = fileURLToPath(new URL('../.output/chrome-mv3', import.meta.url));
/** The extension's manifest, read from the same build. */
const MANIFEST_PATH = join(EXTENSION_DIR, 'manifest.json');

const FIXTURE_DIR = fileURLToPath(new URL('./fixtures', import.meta.url));

export function fixture(name: string): string {
  return readFileSync(join(FIXTURE_DIR, name), 'utf8');
}

/**
 * The fixture page with this platform's API path substituted in.
 * `replaceAll`, not `replace`: the placeholder is also named in the page's own
 * comment explaining what it is, and replacing only the first occurrence left
 * the script fetching a literal `__CS_API_PATH__` — a 404 that looked exactly
 * like a routing bug.
 */
export function fixturePage(apiPath: string): string {
  return fixture('platform-page.html').replaceAll('__CS_API_PATH__', apiPath);
}

// ---------------------------------------------------------------------------
// Launching
// ---------------------------------------------------------------------------

export interface Extension {
  context: BrowserContext;
  /** Derived from the worker's own URL, never hard-coded. */
  extensionId: string;
  worker: Worker;
  /** The throwaway profile, so the caller can clean it up. */
  userDataDir: string;
}

/**
 * 🔴 `channel: 'chromium'` is not decoration.
 *
 * Playwright ships two builds. With no channel, `headless: true` launches
 * `chromium-headless-shell`, which **does not load extensions at all** —
 * measured on this branch: `context.serviceWorkers()` came back empty, so there
 * was no worker to read the outbox from. `channel: 'chromium'` selects the full
 * Chromium build with the new headless mode, and there the extension loads and
 * its MV3 service worker starts (measured in the same run).
 *
 * That is why these specs need no `xvfb` on a machine with no display: the
 * extension is exercised in a real headless browser, not with the capture path
 * faked out.
 */
export async function launchExtension(): Promise<Extension> {
  const userDataDir = mkdtempSync(join(tmpdir(), 'chat-stasher-e2e-'));
  const context = await chromium.launchPersistentContext(userDataDir, {
    headless: true,
    channel: 'chromium',
    args: [
      `--disable-extensions-except=${EXTENSION_DIR}`,
      `--load-extension=${EXTENSION_DIR}`,
    ],
  });
  const worker = await acquireWorker(context);
  if (!worker) {
    await context.close();
    rmSync(userDataDir, { recursive: true, force: true });
    throw new Error(
      'the extension did not load: no service worker appeared. If this is the only'
      + ' failure, check that the launch still passes channel: "chromium" — the'
      + ' default headless build silently loads no extensions.',
    );
  }
  return { context, extensionId: new URL(worker.url()).host, worker, userDataDir };
}

/**
 * The MV3 service worker, woken if it happens to be asleep.
 *
 * An extension worker is reclaimed when idle, and a reclaimed worker does not
 * appear in `context.serviceWorkers()` — which would make "the outbox is empty"
 * indistinguishable from "nobody was there to ask". So a missing worker is
 * woken deliberately (opening the popup makes it ask background for its status)
 * rather than read as "nothing to report".
 */
async function acquireWorker(context: BrowserContext): Promise<Worker | null> {
  const running = context.serviceWorkers()[0];
  if (running) return running;

  const extensionId = pinnedExtensionId();
  if (!extensionId) return null;

  const appeared = context.waitForEvent('serviceworker', { timeout: 15_000 }).catch(() => null);
  const page = await context.newPage();
  try {
    await page.goto(`chrome-extension://${extensionId}/popup.html`).catch(() => undefined);
    const woken = await appeared;
    if (woken) return woken;
  } finally {
    await page.close().catch(() => undefined);
  }
  return context.serviceWorkers()[0] ?? null;
}

/**
 * The id Chrome derives from the manifest `key`: the first 128 bits of the
 * SHA-256 of the DER public key, each nibble remapped onto `a`-`p`
 * (Chrome's `crx_file::id_util::GenerateId`).
 *
 * wxt.config.ts pins that key precisely so the id is the same on every machine
 * and every unpacked install. It is re-derived from the built manifest here
 * rather than spelled out, so this file cannot drift from the key it depends on.
 */
export function pinnedExtensionId(): string | null {
  try {
    const manifest = JSON.parse(readFileSync(MANIFEST_PATH, 'utf8')) as { key?: string };
    if (!manifest.key) return null;
    const digest = createHash('sha256')
      .update(Buffer.from(manifest.key, 'base64'))
      .digest()
      .subarray(0, 16);
    let id = '';
    for (const byte of digest) {
      id += String.fromCharCode(97 + (byte >> 4));
      id += String.fromCharCode(97 + (byte & 0x0f));
    }
    return id;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// The fake platform
// ---------------------------------------------------------------------------

export interface FakePlatform {
  /** The exact origin from the platform table, e.g. https://chatgpt.com. */
  origin: string;
  /** Page path served as HTML, e.g. /c/<session id>. */
  pagePath: string;
  /** API path served the response body; must match the row's pathHints. */
  apiPath: string;
  /** The exact bytes the page's request will receive. */
  apiBody: string;
}

export interface RouteLog {
  /** One entry per page document served. */
  pageLoads: string[];
  /** One entry per conversation response served, e.g. "GET /backend-api/...". */
  apiResponses: string[];
  /**
   * Requests to an intercepted platform origin that are neither the fixture
   * page nor the fixture endpoint. Must stay empty: the point of serving the
   * origin ourselves is that every request to it is one we know about.
   */
  unexpected: string[];
  /**
   * 🔴 Requests that would have left the machine. Every one of these was
   *    aborted; the specs assert this list is empty.
   */
  escaped: string[];
}

/**
 * Intercept the platform origins and abort everything else.
 *
 * Playwright matches routes in reverse registration order, so the catch-all goes
 * on first and the specific origins after it: a request to a platform origin is
 * answered by that origin's fixture, and a request anywhere else is aborted and
 * counted. No name is ever resolved.
 */
export async function installFakePlatforms(
  context: BrowserContext,
  platforms: readonly FakePlatform[],
): Promise<RouteLog> {
  const log: RouteLog = { pageLoads: [], apiResponses: [], unexpected: [], escaped: [] };

  await context.route('**/*', (route) => {
    log.escaped.push(route.request().url());
    return route.abort();
  });

  for (const platform of platforms) {
    await context.route(`${platform.origin}/**`, (route) => {
      const request = route.request();
      const { pathname } = new URL(request.url());
      const label = `${request.method()} ${pathname}`;

      if (pathname === '/favicon.ico') {
        // A browser-generated request, not a data request, and one only the
        // browser's own UI makes. Answered so it cannot be mistaken for traffic
        // to the platform, and recorded in neither list on purpose.
        return route.fulfill({ status: 204, body: '' });
      }
      if (pathname === platform.pagePath) {
        log.pageLoads.push(label);
        return route.fulfill({
          status: 200,
          contentType: 'text/html; charset=utf-8',
          body: fixturePage(platform.apiPath),
        });
      }
      if (pathname === platform.apiPath) {
        log.apiResponses.push(label);
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: platform.apiBody,
        });
      }
      log.unexpected.push(label);
      return route.fulfill({ status: 404, body: '' });
    });
  }

  return log;
}

// ---------------------------------------------------------------------------
// Reading the outbox
// ---------------------------------------------------------------------------

export interface OutboxEntry {
  sha256: string;
  name: string;
  payload: string;
  bytes: number;
  enqueuedAt: number;
  attempts: number;
  lastError: string | null;
  lastAttemptAt: number | null;
  state: 'pending' | 'rejected';
  rejectKind?: string;
}

export const OUTBOX_DB_NAME = 'chat-stasher-outbox';
export const OUTBOX_STORE = 'entries';

/** The backfill's debt set (`lib/backfill/debt-store.ts`). The migration's destination. */
export const BACKFILL_DB_NAME = 'chat-stasher-backfill';
export const DEBTS_STORE = 'debts';

/**
 * The extension APIs the specs reach for **from inside the worker**.
 *
 * Declared here rather than pulled from `@types/chrome`: this repository does not
 * depend on that package, and a spec that reaches for an API the extension does
 * not have should fail on the browser's own error, not on a type.
 */
declare const chrome: {
  storage: {
    local: {
      get(query: null | string[] | Record<string, unknown>): Promise<Record<string, unknown>>;
      set(values: Record<string, unknown>): Promise<void>;
    };
  };
  alarms: {
    create(name: string, info: { when?: number; delayInMinutes?: number }): Promise<void>;
  };
};

interface ReadRequest {
  dbName: string;
  storeName: string;
  /** Prefix for the thrown messages, so a failure names which store it was. */
  label: string;
}

/**
 * Runs **inside the service worker**. Self-contained by necessity: Playwright
 * serialises the function source, so it may not close over anything.
 *
 * `indexedDB.databases()` comes first on purpose. Opening the database to look
 * inside would create it if it did not exist — at version 1 with none of the
 * object stores the extension's own `onupgradeneeded` creates — and a database
 * created that way is never upgraded afterwards, so the reader would have broken
 * the thing it was inspecting. Asking whether it exists is also what keeps "no
 * record was ever written" a certainty rather than a guess.
 */
const READ_OUTBOX = async (request: ReadRequest): Promise<unknown[]> => {
  const databases = await indexedDB.databases();
  if (!databases.some((entry) => entry.name === request.dbName)) return [];

  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const open = indexedDB.open(request.dbName);
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(new Error(`${request.label}: open failed`));
    open.onblocked = () => reject(new Error(`${request.label}: open blocked`));
  });
  try {
    return await new Promise<unknown[]>((resolve, reject) => {
      const tx = db.transaction(request.storeName, 'readonly');
      const read = tx.objectStore(request.storeName).getAll();
      read.onsuccess = () => resolve(read.result as unknown[]);
      read.onerror = () => reject(new Error(`${request.label}: read failed`));
    });
  } finally {
    db.close();
  }
};

/**
 * Every IndexedDB database this extension's service worker can see, by name.
 *
 * 🔴 The list is the **measurement**, not a convenience: "the debt database does
 *    not exist" and "the debt database exists and is empty" are different facts
 *    (CLAUDE.md invariant 1), and a reader that opened the database to look
 *    inside would create it if it were absent — the failure `READ_OUTBOX`'s own
 *    comment warns about. Asking which databases exist answers the first
 *    question without touching anything.
 */
export async function listDatabases(extension: Extension): Promise<string[]> {
  const worker = extension.context.serviceWorkers()[0];
  if (!worker) throw new Error('databases: no service worker is running, so nothing was listed');
  const names = await worker.evaluate(async () => {
    const databases = await indexedDB.databases();
    return databases.map((entry) => String(entry.name));
  });
  return names.sort();
}

function asEntry(row: unknown): OutboxEntry {
  if (!row || typeof row !== 'object') throw new Error('outbox: row is not an object');
  const record = row as Record<string, unknown>;
  for (const key of ['sha256', 'name', 'payload', 'state']) {
    if (typeof record[key] !== 'string') throw new Error(`outbox: row has no ${key}`);
  }
  return row as OutboxEntry;
}

/**
 * Every entry in the outbox, oldest first, read through the service worker.
 * 🔴 Throws — never returns `[]` — when the worker is gone or the store cannot
 *    be read.
 */
export async function readOutbox(extension: Extension): Promise<OutboxEntry[]> {
  const worker = extension.context.serviceWorkers()[0];
  if (!worker) {
    throw new Error('outbox: no service worker is running, so the outbox was not read');
  }
  const rows = await worker.evaluate(READ_OUTBOX, {
    dbName: OUTBOX_DB_NAME,
    storeName: OUTBOX_STORE,
    label: 'outbox',
  });
  return rows.map(asEntry).sort((a, b) => a.enqueuedAt - b.enqueuedAt);
}

/** One row of the backfill debt set (`lib/backfill/debt-store.ts`'s `DebtRecord`). */
export interface DebtRow {
  scope: string;
  id: string;
  state: 'pending' | 'archived';
  seq: number;
}

/**
 * Every debt row in the backfill database, or `[]` when the database does not
 * exist. Throws when a service worker is not running — the same three-state rule
 * as `readOutbox`, so a spec can never read "I could not ask" as "there is none".
 */
export async function readDebtRows(extension: Extension): Promise<DebtRow[]> {
  const worker = extension.context.serviceWorkers()[0];
  if (!worker) {
    throw new Error('debt set: no service worker is running, so the debt set was not read');
  }
  const rows = await worker.evaluate(READ_OUTBOX, {
    dbName: BACKFILL_DB_NAME,
    storeName: DEBTS_STORE,
    label: 'debt set',
  });
  return rows.map((row) => {
    const record = row as Record<string, unknown>;
    if (typeof record.scope !== 'string' || typeof record.id !== 'string') {
      throw new Error('debt set: row has no scope/id');
    }
    return record as unknown as DebtRow;
  });
}

/**
 * Read keys out of the extension's own `storage.local`, from inside the worker.
 *
 * `get(null)` for everything (which is what the acceptance's own dump did), or a
 * list of keys. Never a page: `chrome.storage` is not exposed to pages at all.
 */
export async function readStorage(
  extension: Extension,
  keys: string[] | null,
): Promise<Record<string, unknown>> {
  const worker = extension.context.serviceWorkers()[0];
  if (!worker) throw new Error('storage: no service worker is running, so storage was not read');
  return await worker.evaluate(
    async (query: string[] | null) => chrome.storage.local.get(query as never) as Promise<
      Record<string, unknown>
    >,
    keys,
  );
}

/**
 * Wake one of the extension's own alarms now.
 *
 * The alarm is a production event, not a test hook: `cs-backfill-tick` is the
 * one-shot the leg re-arms after every tick, and firing it here is the same wake
 * the browser would deliver — just without waiting out the jittered 5-10 minute
 * draw. It is also the only path that writes the tick trace the acceptance read.
 */
export async function fireAlarm(extension: Extension, name: string): Promise<void> {
  const worker = extension.context.serviceWorkers()[0];
  if (!worker) throw new Error('alarm: no service worker is running, so nothing was fired');
  await worker.evaluate(async (alarmName: string) => {
    await chrome.alarms.create(alarmName, { when: Date.now() + 100 });
  }, name);
}

/** Write into the extension's own `storage.local` — how a spec sets up "a user's storage". */
export async function writeStorage(
  extension: Extension,
  values: Record<string, unknown>,
): Promise<void> {
  const worker = extension.context.serviceWorkers()[0];
  if (!worker) throw new Error('storage: no service worker is running, so nothing was written');
  await worker.evaluate(
    async (entries: Record<string, unknown>) => chrome.storage.local.set(entries as never),
    values,
  );
}

/**
 * Poll the outbox until `settled` accepts what it sees, and return that reading.
 *
 * Returning the last reading rather than throwing on timeout is deliberate: the
 * caller decides what the number means. A spec that expects one record asserts
 * on it; a spec that expects none reads the outbox only after the page has
 * confirmed the response arrived, and an exception there would look exactly like
 * the failure it is looking for.
 */
export async function waitForOutbox(
  extension: Extension,
  settled: (entries: readonly OutboxEntry[]) => boolean,
  timeoutMs = 20_000,
): Promise<OutboxEntry[]> {
  const deadline = Date.now() + timeoutMs;
  let last = await readOutbox(extension);
  while (!settled(last) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    last = await readOutbox(extension);
  }
  return last;
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

export const test = base.extend<{ ext: Extension }>({
  ext: async ({}, use) => {
    const extension = await launchExtension();
    try {
      await use(extension);
    } finally {
      await extension.context.close().catch(() => undefined);
      rmSync(extension.userDataDir, { recursive: true, force: true });
    }
  },
});
