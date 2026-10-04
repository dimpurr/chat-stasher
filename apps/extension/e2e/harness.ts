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
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { endianness, tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { isSweepNotConcluded, type TabSweepTrace } from '../lib/backfill/alarm';
// 🔴 W230 · Both read from the product rather than restated here: the settling
// step below waits on the key `checkHost` writes and the timeout that bounds it.
// A second copy would keep passing after either moved.
import { HOST_STATUS_KEY } from '../lib/host-status';
import { HELLO_PROBE_TIMEOUT_MS } from '../lib/native-host';
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
 * Chromium build. The default launch pins `--headless=new` explicitly, which
 * loads the extension and starts its MV3 service worker without opening a
 * browser window. Set `CS_E2E_HEADED=1` to launch a visible browser while
 * debugging.
 *
 * That is why these specs need no `xvfb` on a machine with no display: the
 * extension is exercised in a real headless browser, not with the capture path
 * faked out.
 */
export async function launchExtension(options: { host?: NativeHost } = {}): Promise<Extension> {
  const headed = process.env.CS_E2E_HEADED === '1';
  const userDataDir = mkdtempSync(join(tmpdir(), 'chat-stasher-e2e-'));
  const context = await chromium.launchPersistentContext(userDataDir, {
    headless: !headed,
    channel: 'chromium',
    // Playwright's generic `--headless` is omitted so Chromium receives the
    // explicit new-headless switch. Headed debugging gets neither switch.
    ignoreDefaultArgs: headed ? undefined : ['--headless'],
    args: [
      ...(!headed ? ['--headless=new', '--disable-gpu'] : []),
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
  const extension = { context, extensionId: new URL(worker.url()).host, worker, userDataDir };
  // 🔴 Exactly one of the two, and never both: a spec either drives the real
  //    host binary or it does not. Installing the synthetic coordinator on top
  //    of a real host would answer coordination from a stub while delivery went
  //    to the binary — a mix no deployment has, and one whose failures would be
  //    attributed to the product.
  if (options.host) await installNativeHostBridge(extension, options.host);
  else await installSyntheticCoordinator(extension);
  return extension;
}

/**
 * Backfill E2E specs exercise platform pacing, not a registered native host.
 * Model a responsive single-install arbiter for coordination messages only;
 * all delivery and other native-host messages still take Chromium's real path.
 */
async function installSyntheticCoordinator(extension: Extension): Promise<void> {
  await extension.worker.evaluate(() => {
    const globals = globalThis as unknown as { browser?: any; chrome?: any };
    const runtime = globals.browser?.runtime?.id ? globals.browser.runtime : globals.chrome?.runtime;
    if (!runtime || runtime.__chatStasherCoordinationStub) return;
    const original = runtime.sendNativeMessage?.bind(runtime);
    runtime.sendNativeMessage = (host: string, message: Record<string, unknown>, callback?: (reply: unknown) => void) => {
      if (message?.type !== 'coordination') return original?.(host, message, callback);
      const reply = {
        protocol: 1, type: 'coordination', ok: true,
        request_id: String(message.request_id), granted: true,
        active_installs: 1, gentle: false, cooldown_until: 0, wait_ms: 0,
      };
      if (callback) { callback(reply); return undefined; }
      return Promise.resolve(reply);
    };
    runtime.__chatStasherCoordinationStub = true;
  });
}

/**
 * The MV3 service worker, woken if it happens to be asleep.
 *
 * An extension worker is reclaimed when idle, and a reclaimed worker does not
 * appear in `context.serviceWorkers()` — which would make "the outbox is empty"
 * indistinguishable from "nobody was there to ask". So a missing worker is
 * woken deliberately (opening the popup makes it ask background for its status)
 * rather than read as "nothing to report".
 *
 * 🔴 W230 · **Waking it this way writes, and the write outlives the page.** The
 *    popup's boot asks background for `POPUP_STATUS_MESSAGE`, whose only
 *    implementation is `hostStatusForPopup()` — and that is the one call site of
 *    `checkHost`, which runs a `hello` probe and then records the conclusion in
 *    `cs_native_host_status_v1` (`lib/host-status.ts:77-86`). `page.close()` below
 *    does not cancel it: the handler is already running in the worker, and it
 *    finishes after the page is gone.
 *
 *    So `launchExtension` used to return with a probe still in flight, free to
 *    write `storage.local` at any later moment — including *after* a spec had
 *    seeded that very key. The failure this caused was measured, not theorised:
 *    `extension-only.spec.ts`'s "was connected, now broken" seeds a record whose
 *    `lastKnownStage` is the only evidence a CLI exists on the machine, the
 *    setup probe (which started when nothing was stored yet) landed its
 *    stage-less failure record on top, and the popup then told a user with a
 *    working CLI to install it. It reproduced only as a rare red in CI — the same
 *    commit of `main` passed this job at 20:55 and failed it at 21:13 — because
 *    whether the probe's write beats the spec's seed is a race, not a property.
 *
 *    `settleWokenPopup` closes it by the only means that is a property: the
 *    fixture does not return until the probe has concluded, so no setup-time
 *    write can still land afterwards. Nothing here is a sleep or a retry — the
 *    signal is the product's own record of having concluded.
 *
 * What each step may fail with is part of the contract, not an
 * implementation detail:
 *
 *  · **The event wait's timeout is the one documented fallback.** It
 *    means the popup did not wake the worker, and the list below is
 *    then the answer. Any other error the wait raises (a closed
 *    context, a protocol failure) rethrows — a broken run must not
 *    read as "no worker yet".
 *  · **The wake-up page is this harness's own mechanism.** A `goto`
 *    that cannot open it (a bad extension id, a dead popup URL) means
 *    the run is broken, not merely unwoken, so the navigation's own
 *    error propagates.
 *  · **A settling `evaluate` error rethrows** rather than being
 *    retried as "not recorded yet"; only the deadline proceeds, as
 *    documented on `settleWokenPopup`.
 */
async function acquireWorker(context: BrowserContext): Promise<Worker | null> {
  const running = context.serviceWorkers()[0];
  if (running) return running;

  const extensionId = pinnedExtensionId();
  if (!extensionId) return null;

  const appeared = context
    .waitForEvent('serviceworker', { timeout: 15_000 })
    .catch((error: unknown) => {
      if (!isPlaywrightTimeout(error)) throw error;
      return null;
    });
  const page = await context.newPage();
  try {
    await page.goto(`chrome-extension://${extensionId}/popup.html`);
    const woken = await appeared;
    const worker = woken ?? context.serviceWorkers()[0] ?? null;
    if (worker) await settleWokenPopup(worker);
    return worker;
  } finally {
    await page.close().catch(() => undefined);
  }
}

/**
 * Playwright's own timeout rejection (`TimeoutError`, message
 * `Timeout <ms>ms exceeded`) — the one error from an event wait that
 * means "the bound was reached", which on the wake-up path is the
 * documented signal that no worker appeared. Any other rejection is
 * a broken run and is not this harness's to interpret.
 */
function isPlaywrightTimeout(error: unknown): boolean {
  return error instanceof Error
    && (error.name === 'TimeoutError' || /Timeout \d+ms exceeded/.test(error.message));
}

/**
 * 🔴 W230 · Wait until the wake-up popup's host probe has recorded its
 * conclusion, so the fixture hands the spec a worker with nothing in flight.
 *
 * The signal is the product's own: `checkHost` is the only writer of
 * `HOST_STATUS_KEY` (`lib/host-status.ts:24`, and the only call site of
 * `checkHost` is `hostStatusForPopup`), and `hostStatusForPopup` awaits
 * `store.save` *before* it answers the popup — so the key appearing means the
 * probe wrote and can no longer overwrite anything a spec seeds afterwards.
 *
 * The bound is `HELLO_PROBE_TIMEOUT_MS` plus slack: a probe against no host
 * answers in milliseconds, but one against a host that accepts the connection
 * and then says nothing is cut off at that timeout (`lib/native-host.ts:47`).
 * Timing out proceeds rather than throws — a spec that needs the worker should
 * not be failed by the settling step, and this can only be reached on the path
 * that had to open a page at all. An error from the evaluate itself — the
 * worker gone, a serialization failure — is not "not recorded yet", so it
 * rethrows at once instead of burning the deadline while a broken run is
 * reported as a settling step.
 */
async function settleWokenPopup(worker: Worker): Promise<void> {
  const deadline = Date.now() + HELLO_PROBE_TIMEOUT_MS + 1_000;
  for (;;) {
    // Self-contained for the same reason `READ_OUTBOX` is: Playwright serialises
    // the function source, so it cannot close over `HOST_STATUS_KEY`.
    const recorded = await worker.evaluate(
      async (key: string) => {
        const got = await chrome.storage.local.get({ [key]: null } as never);
        const value = (got as Record<string, unknown>)[key];
        return value !== null && value !== undefined;
      },
      HOST_STATUS_KEY,
    );
    if (recorded) return;
    if (Date.now() >= deadline) return;
    await new Promise((resolve) => { setTimeout(resolve, 25); });
  }
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
  /**
   * API path served the response body; must match the row's pathHints. May
   * carry a query — W473's DeepSeek row binds the capture to the request's
   * `chat_session_id` query, so the fake route matches pathname and query.
   */
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

  await context.route('**/*', async (route) => {
    const url = route.request().url();
    // 🔴 The harness's own native-host bridge, and the one request that must not
    //    be counted as "escaped". A spec's catch-all is registered *after* the
    //    bridge's own route and therefore runs first (Playwright runs matching
    //    handlers in the reverse of their registration order), so this falls
    //    through to it rather than aborting the extension's own delivery. The
    //    pattern is narrow on purpose: everything else really is aborted and
    //    recorded, which is what makes `escaped` a measurement.
    if (url.startsWith(`${BRIDGE_ORIGIN}/`)) return route.fallback();
    log.escaped.push(url);
    return route.abort();
  });

  for (const platform of platforms) {
    await context.route(`${platform.origin}/**`, (route) => {
      const request = route.request();
      const { pathname, search } = new URL(request.url());
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
      // The path AND its query: an apiPath that names a session in the query
      // (DeepSeek, W473) must not be answerable to a query-less request.
      if (`${pathname}${search}` === platform.apiPath) {
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
export const DEBTS_STORE = 'debts_by_platform';

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
  platform: string;
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
    if (
      typeof record.platform !== 'string'
      || typeof record.scope !== 'string'
      || typeof record.id !== 'string'
    ) {
      // The platform is part of the key since W45. A row without it is not a row
      // this reader understands, and returning it as if it were would let a spec
      // count one platform's ids as another's.
      throw new Error('debt set: row has no platform/scope/id');
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
 * The one poll shape every waiter in this harness uses: read, judge, repeat
 * until `settled` accepts or the deadline passes, and return the last **real**
 * reading.
 *
 * 🔴 W618 · A worker restart mid-poll makes a reader throw (`readStorage`,
 *    `readOutbox`, `readDebtRows` and `listDatabases` all throw rather than
 *    return an empty result, precisely so "I could not ask" is never read as
 *    "there is none" — see the three-state rule on each of them). That throw is
 *    "not settled yet", not an answer, so the poll keeps the reading it already
 *    has and tries again instead of failing the wait outright. This is the
 *    flake W618 was written for: the restart is transient, and a restart must
 *    not decide a test.
 *
 * 🔴 But the throw is only swallowed **once there is a reading to fall back
 *    on**. If the deadline passes and *no* read ever succeeded, this re-throws
 *    the reader's own error instead of returning its empty seed value. That is
 *    the whole reason the readers throw: an extension that never installed, a
 *    worker that never came up, or a reader that genuinely broke would
 *    otherwise return `{}` / `[]` — indistinguishable at the call site from a
 *    storage that was read and was empty — and the caller's assertions would
 *    then report a *missing record* for a run that never read anything, after
 *    burning the whole timeout. With `retries: 0` that misleading failure is
 *    also final. So "unreadable" and "empty" stay two states here too, and the
 *    diagnostic that names the real cause survives.
 */
async function pollLastReading<T>(
  read: () => Promise<T>,
  settled: (reading: T) => boolean,
  timeoutMs: number,
  intervalMs: number,
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  // A `T` can legitimately be anything, so "have we ever read" is its own flag
  // rather than a null/undefined check on the value.
  let reading: { value: T } | null = null;
  let failure: unknown;
  for (;;) {
    try {
      reading = { value: await read() };
      failure = undefined;
    } catch (error) {
      failure = error;
    }
    if (reading && settled(reading.value)) return reading.value;
    if (Date.now() >= deadline) {
      if (!reading) throw failure;
      return reading.value;
    }
    await new Promise((resolve) => { setTimeout(resolve, intervalMs); });
  }
}

/**
 * Poll `storage.local` until `settled` accepts the reading, and return that reading.
 *
 * 🔴 W618 · The storage twin of `waitForOutbox`, and it exists for the same reason:
 *    a fixed `page.waitForTimeout` is the suite's known flake source, and this
 *    config has `retries: 0`, so one impatient sleep fails the run rather than
 *    being papered over. A spec that sleeps for the product to write a record is
 *    guessing how long the write takes; polling on the record's own shape is not.
 *
 *    The **shape the next assertion reads** is the only thing worth waiting on: a
 *    shorter wait cannot prove the state exists, and a longer one buys nothing the
 *    timeout below does not already bound.
 *
 * Returning the last reading on timeout is the same rule `waitForOutbox` and
 * `waitForTickRecord` follow, and for the same reason: the caller's own
 * assertions are the judge, so a missing record fails with the sentence that
 * names which field was absent rather than with "a poll ran out of time". That
 * holds only for a reading that was actually taken — `pollLastReading` throws
 * rather than inventing an empty one.
 */
export async function waitForStorage(
  extension: Extension,
  keys: string[] | null,
  settled: (state: Record<string, unknown>) => boolean,
  timeoutMs = 20_000,
): Promise<Record<string, unknown>> {
  return await pollLastReading(
    () => readStorage(extension, keys),
    settled,
    timeoutMs,
    50,
  );
}

/**
 * Wait until the extension has **armed** one of its own alarms.
 *
 * 🔴 W82 · `fireAlarm` below does not add a second alarm — `alarms.create` on a
 *    name that already exists **replaces** it (`lib/backfill/alarm.ts`'s note on
 *    the one-shot tick). So a fire that lands while the extension is still
 *    deciding re-arms the very alarm the spec meant to bring forward, and the
 *    spec's deadline is gone.
 *
 *    Measured in a real Chromium, in `backfill-migration.spec.ts`: the
 *    extension's own `create` — a fresh 5-10 minute draw — and the spec's are
 *    **2-4 ms apart**, so which of them lands second decides. Widening that
 *    window by 40 ms (the same code, nothing else changed) made the extension's
 *    draw land second and put the tick **6.29 minutes** out: no tick ran, and the
 *    test failed **10 times out of 10**, 20 s later, with
 *    `expect(tick).toBeTruthy()`.
 *
 *    What arms it is the switch going on (`syncBackfillAlarm`: "switch on ⇒ both
 *    alarms exist"), and the switch is a `storage.local` write the extension
 *    reacts to. So waiting for the alarm to exist *is* waiting for that reaction,
 *    and after it the extension's own syncs observe the alarm and leave it alone
 *    — the override below is then the last writer rather than one of two
 *    racing. This is a synchronisation on a fact the extension publishes, not
 *    patience: a longer timeout would leave the two writes racing for the whole
 *    of it.
 */
export async function waitForAlarm(
  extension: Extension,
  name: string,
  timeoutMs = 20_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const worker = extension.context.serviceWorkers()[0];
    if (!worker) throw new Error('alarm: no service worker is running, so arming was not observed');
    const armed = await worker.evaluate(async (alarmName: string) => {
      const alarms = (chrome as unknown as {
        alarms: { get(n: string): Promise<unknown> };
      }).alarms;
      return Boolean(await alarms.get(alarmName));
    }, name);
    if (armed) return;
    if (Date.now() >= deadline) {
      // Reported as the precondition it is, not as the failure it would otherwise
      // be mistaken for: "no tick ran" and "the arm this tick needed never
      // happened" are different facts, and collapsing them is what W82 was.
      throw new Error(
        `alarm: ${name} was never armed, so it could not be brought forward —`
        + ' the extension did not arm it, which is the thing the switch going on does',
      );
    }
    await new Promise((resolve) => { setTimeout(resolve, 20); });
  }
}

/**
 * Wake one of the extension's own alarms now.
 *
 * The alarm is a production event, not a test hook: `cs-backfill-tick` is the
 * one-shot the leg re-arms after every tick, and firing it here is the same wake
 * the browser would deliver — just without waiting out the jittered 5-10 minute
 * draw. It is also the only path that writes the tick trace the acceptance read.
 *
 * 🔴 W82 · It overrides the deadline of an alarm that is already armed, so a
 *    caller that means to bring a tick forward must `waitForAlarm` first — see
 *    the note there.
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
 * Runs **inside the service worker**. Self-contained for the same reason
 * `READ_OUTBOX` is: Playwright serialises the function source.
 *
 * 🔴 It creates the object stores on `onupgradeneeded` rather than opening the
 *    database bare. `READ_OUTBOX` warns about the opposite hazard — a reader that
 *    opens a database into existence and leaves it at version 1 with no stores,
 *    after which the product's own upgrade never runs and the outbox is broken
 *    for good. A *writer* has to open it read-write, so it must create exactly the
 *    stores the product would, or it would be that hazard itself. The names and
 *    the keyPath are the product's (`lib/outbox.ts`: `entries` keyed by `sha256`).
 */
const WRITE_OUTBOX = async (rows: unknown[]): Promise<void> => {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const open = indexedDB.open('chat-stasher-outbox', 1);
    open.onupgradeneeded = () => {
      const database = open.result;
      if (!database.objectStoreNames.contains('entries')) {
        database.createObjectStore('entries', { keyPath: 'sha256' });
      }
      if (!database.objectStoreNames.contains('meta')) {
        database.createObjectStore('meta');
      }
    };
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(new Error('outbox seed: open failed'));
    open.onblocked = () => reject(new Error('outbox seed: open blocked'));
  });
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction('entries', 'readwrite');
      const store = tx.objectStore('entries');
      for (const row of rows) store.put(row);
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(new Error('outbox seed: write failed'));
      tx.onabort = () => reject(new Error('outbox seed: write aborted'));
    });
  } finally {
    db.close();
  }
};

/**
 * Seed the outbox with entries, so a spec can put the spool into a state it would
 * otherwise take a real capture (and a real platform page) to reach.
 *
 * 🔴 `bytes` on each entry is the product's own byte accounting and the only thing
 *    `summary()` reads (lib/outbox.ts:321 sums the rows' `bytes`), so a spec that
 *    wants "a spool that is 82% full" seeds that number rather than megabytes of
 *    payload. The payload only has to be well-formed enough for the code under
 *    test; nothing here is delivered, because no host is installed.
 */
export async function seedOutbox(extension: Extension, rows: OutboxEntry[]): Promise<void> {
  const worker = extension.context.serviceWorkers()[0];
  if (!worker) throw new Error('outbox seed: no service worker is running, so nothing was seeded');
  await worker.evaluate(WRITE_OUTBOX, rows as unknown[]);
}
/**
 * Poll `storage.local` until **a concluded tick record written at or after
 * `since`** exists, and return that snapshot.
 *
 * 🔴 W70 · A truthy `cs_backfill_lasttick_v1` is not the completion signal.
 *    Since W62 the tick publishes a **provisional** record before its tab
 *    registry recovery sweep (`{tabSweep: {sweeping: true}}`, `SWEEP_NOT_CONCLUDED`
 *    in `lib/backfill/alarm.ts`) and replaces it with the verdict when the sweep
 *    concludes. Returning on the key alone read the tick *while it was still
 *    running* — a state the code is not required to be finished in — and the
 *    no-tab case's assertions happen to hold for the provisional record too, so
 *    it passed without proving what it claims. The predicate below is the
 *    product's own (`isSweepNotConcluded`), not a second spelling of the shape.
 *
 * 🔴 W67(b) · And the record is not necessarily **this** tick's: one written by
 *    an earlier wake is already concluded, so the first condition alone would
 *    return it. `since` rejects every record older than the caller's own fire —
 *    pass the instant just before firing the alarm.
 *
 * Returning the last reading on timeout (rather than throwing) is deliberate,
 * the same rule the other waiters follow: the caller decides what the snapshot
 * means, and the body's assertions are what fail on a missing or provisional
 * record — for a reading that was taken. A poll in which the reader threw every
 * time never got one, so `pollLastReading` re-throws rather than returning an
 * empty snapshot that reads as a storage with no record in it.
 */
export async function waitForTickRecord(
  ext: Extension,
  options: { since?: number; timeoutMs?: number } = {},
): Promise<Record<string, unknown>> {
  const { since = 0, timeoutMs = 20_000 } = options;
  return await pollLastReading(
    async () => await readStorage(ext, null),
    (all) => {
      const record = all['cs_backfill_lasttick_v1'];
      if (record && typeof record === 'object') {
        const fields = record as { at?: unknown; tabSweep?: TabSweepTrace | null };
        if (
          typeof fields.at === 'number'
          && fields.at > since
          && !isSweepNotConcluded(fields.tabSweep)
        ) {
          return true;
        }
      }
      return false;
    },
    timeoutMs,
    100,
  );
}

/**
 * Fire `cs-backfill-tick` until a tick actually runs (i.e. the trace is not the
 * single-flight refusal), and return that snapshot.
 *
 * 🔴 W67(a) · The fired alarm can lose to the capture kick's own tick. Loading
 *    the fixture page makes the page fetch; the capture leg stores it and then
 *    kicks the backfill leg fire-and-forget (`entrypoints/background.ts:2365-2373`),
 *    and that kick holds the single-flight lock (`lib/backfill/schedule.ts:303-304`).
 *    An alarm fired inside that window is refused with `already-running` — the
 *    product is right to serialise the two, and the trace says so — but the case
 *    under test is the tick that actually runs, so it must not be read as a
 *    failure. Measured on main: 7 of 100 repeats of test 2 failed at
 *    `expect(tick.ran)`. Only one failure specimen was captured with its record
 *    (a separate instrumented run), and it read
 *    `{ran:false, reason:'already-running', stopped:'already-running', halted:'state-unreadable'}`.
 *    Re-firing synchronises on that published outcome rather than sleeping a
 *    guessed interval; every assertion on the record is unchanged.
 */
export async function fireAlarmUntilRuns(
  ext: Extension,
  name: string,
  timeoutMs = 20_000,
): Promise<Record<string, unknown>> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const since = Date.now();
    await fireAlarm(ext, name);
    const all = await waitForTickRecord(ext, {
      since,
      timeoutMs: Math.max(1, deadline - Date.now()),
    });
    const record = all['cs_backfill_lasttick_v1'];
    const reason = record && typeof record === 'object'
      ? (record as { reason?: unknown }).reason
      : undefined;
    if (reason !== 'already-running') return all;
    if (Date.now() >= deadline) return all;
    // The capture kick releases the single-flight lock when it finishes; a tick
    // that runs then is the one this case is about.
    await new Promise((resolve) => { setTimeout(resolve, 150); });
  }
}

/**
 * Poll the outbox until `settled` accepts what it sees, and return that reading.
 *
 * Returning the last reading rather than throwing on timeout is deliberate: the
 * caller decides what the number means. A spec that expects one record asserts
 * on it; a spec that expects none reads the outbox only after the page has
 * confirmed the response arrived, and an exception there would look exactly like
 * the failure it is looking for. That again holds only for a reading that was
 * taken: `readOutbox` throws rather than returning `[]`, and a poll whose every
 * read threw has measured nothing at all — so `pollLastReading` re-throws
 * instead of returning the empty list, which at the call site would be
 * indistinguishable from an outbox that was read and was empty.
 */
export async function waitForOutbox(
  extension: Extension,
  settled: (entries: readonly OutboxEntry[]) => boolean,
  timeoutMs = 20_000,
): Promise<OutboxEntry[]> {
  return await pollLastReading(
    async () => await readOutbox(extension),
    (entries) => settled(entries),
    timeoutMs,
    100,
  );
}

// ---------------------------------------------------------------------------
// The real native host
// ---------------------------------------------------------------------------

/**
 * 🔴 **Why this file starts the host itself.**
 *
 * The browser will not. Chromium resolves its native-messaging manifest
 * directory from the *OS* home, not from `$HOME`: measured on macOS 2026-09-27,
 * a probe extension holding the `nativeMessaging` permission, launched under a
 * rewritten `HOME`/`XDG_CONFIG_HOME` with manifests planted in three candidate
 * directories (`…/Google/ChromeForTesting/NativeMessagingHosts`, `…/Google/Chrome/…`,
 * `…/Chromium/…`), answered `Specified native messaging host not found.` — the
 * probe is kept at `w217-probe.mjs`. Reaching the *real* directory
 * would mean writing into the machine's own browser configuration, which is
 * neither hermetic nor the same path on Linux, and a suite that only works on
 * one platform is not one this repository accepts (CLAUDE.md, "one-sided cfg").
 *
 * So the harness starts `chat-stasher native-host` itself and carries the bytes
 * to it. What is faked is the *carrier*; what is real is everything a spec here
 * asserts on:
 *
 *  · the **frames** — 4-byte native-endian length prefix + JSON body, which is
 *    the wire format `read_request_frame`/`encode_response_frame` implement and
 *    the one Chromium's own native messaging uses;
 *  · the **process model** — one request per process, which is what one-shot
 *    `sendNativeMessage` does (`serve_one` on real stdin/stdout);
 *  · the **answering code** — `respond()` in `crates/chat-stasher/src/nativehost.rs`,
 *    including the EXT-3 arbiter and `inbox::seal_payload`;
 *  · the **state** — one config, one stage and one `extension-coordination.sqlite3`
 *    per host instance, shared by every profile pointed at it, exactly as one
 *    machine's host is shared by its browsers.
 *
 * The isolation is the whole temp root: `HOME`, `XDG_CONFIG_HOME` and
 * `XDG_DATA_HOME` all point inside it, so `config_path()`, `default_state_dir()`
 * and `default_data_root()` resolve there on every platform (each reads its
 * `XDG_*` variable first — this is not a platform-dependent directory
 * convention). Nothing under the real home is read or written.
 */
export class NativeHost {
  /** The throwaway root: home, config, state and stage all live under it. */
  readonly root: string;
  readonly stage: string;
  /** The archive partition every shard from this host lands under. */
  readonly machine: string;
  /**
   * Every message a profile asked this host, in order.
   *
   * Kept because "the host was never asked" and "the host was asked and said no"
   * are different facts, and because a `deliver`'s own bytes are the only
   * honest input to a re-delivery or a concurrency case. Nothing here is ever
   * printed: payloads carry conversation text, and these specs print counts,
   * ids and digests only.
   */
  readonly forwarded: Array<Record<string, unknown>> = [];
  /**
   * What the host answered, **one entry per forwarded message and in the same
   * order** — a request that could not be put to the host at all is recorded here
   * too, as `{type: 'bridge-error'}`. Kept because a refused delivery and a
   * delivery that never happened produce the same empty stage, and only this
   * tells them apart: `status: 'duplicate'`, a `nack` with its `kind`, or nothing
   * at all. Index-aligned rather than append-on-success, so a reader can put the
   * two lists side by side without counting.
   */
  readonly answered: Array<Record<string, unknown>> = [];
  private readonly env: NodeJS.ProcessEnv;

  constructor(root: string, stage: string, machine: string, env: NodeJS.ProcessEnv) {
    this.root = root;
    this.stage = stage;
    this.machine = machine;
    this.env = env;
  }

  /** The binary this host runs. Throws rather than skipping when it is not built. */
  static binary(): string {
    const override = process.env.CS_E2E_BINARY;
    if (override) return override;
    return fileURLToPath(new URL('../../../target/debug/chat-stasher', import.meta.url));
  }

  /** One request in, one response out — the same turn `serve_one` performs. */
  async ask(message: Record<string, unknown>): Promise<Record<string, unknown>> {
    const binary = NativeHost.binary();
    const stdout = await new Promise<Buffer>((resolve, reject) => {
      const child = spawn(binary, ['native-host'], { env: this.env, stdio: ['pipe', 'pipe', 'pipe'] });
      const out: Buffer[] = [];
      const err: Buffer[] = [];
      child.stdout.on('data', (chunk: Buffer) => out.push(chunk));
      child.stderr.on('data', (chunk: Buffer) => err.push(chunk));
      child.on('error', (error) => reject(new Error(`native host could not start (${binary}): ${error.message}`)));
      child.on('close', (code) => {
        if (code !== 0) {
          // The host's own stderr is the diagnosis; without it a spawn failure,
          // a refused config and a panic all read as "no response".
          reject(new Error(
            `native host exited ${code} for ${String(message.type)}: `
            + `${Buffer.concat(err).toString('utf8').trim().slice(0, 800)}`,
          ));
          return;
        }
        resolve(Buffer.concat(out));
      });
      child.stdin.on('error', () => undefined);
      child.stdin.end(encodeFrame(message));
    });
    return decodeFrame(stdout, String(message.type));
  }

  /**
   * Answer one `sendNativeMessage` from a profile: record it, then put it to the
   * real binary. A rejection here reaches the extension as a `send-failed`
   * outcome — never as a plausible empty answer.
   */
  async forward(host: string, message: Record<string, unknown>): Promise<Record<string, unknown>> {
    this.forwarded.push({ host, ...message });
    let reply: Record<string, unknown>;
    try {
      reply = await this.ask(message);
    } catch (error) {
      this.answered.push({ type: 'bridge-error', detail: (error as Error).message });
      throw error;
    }
    this.answered.push(reply);
    return reply;
  }

  /**
   * The `deliver` messages this host was asked to archive, in order, as the
   * extension sent them.
   *
   * The native-messaging host name is the harness's own bookkeeping and is
   * stripped, so a case can put one of these straight back to a host —
   * re-delivering the same bytes, or racing two of them — without carrying a
   * field the protocol does not have.
   */
  delivered(): Array<Record<string, unknown>> {
    return this.forwarded
      .filter((message) => message.type === 'deliver')
      .map(({ host: _host, ...message }) => message);
  }

  close(): void {
    rmSync(this.root, { recursive: true, force: true });
  }
}

/**
 * Start a host over a fresh temp root.
 *
 * The config carries an explicit `machine`, and that is required rather than
 * tidy: `resolve_machine` refuses to mint an identity for a host, because a
 * browser-spawned host does not see a shell's environment and a second identity
 * would silently split the archive. A test that left it out would exercise the
 * refusal instead of the delivery.
 *
 * The binary is *not* built here. `pnpm e2e:multi` builds it first and says so;
 * a missing binary throws with the command, because "the host was not there" is
 * the one outcome these specs must never read as "nothing happened".
 */
export function startNativeHost(options: { machine?: string } = {}): NativeHost {
  const binary = NativeHost.binary();
  if (!existsSync(binary)) {
    throw new Error(
      `the native host binary is missing at ${binary}; these specs drive the real`
      + ' host (`crates/chat-stasher`), so build it first:'
      + ' `cargo build -p chat-stasher --bin chat-stasher` (or run `pnpm e2e:multi`,'
      + ' which does it for you)',
    );
  }
  const root = mkdtempSync(join(tmpdir(), 'chat-stasher-host-'));
  const home = join(root, 'home');
  const configDir = join(home, '.config', 'chat-stasher');
  const dataDir = join(home, '.local', 'share');
  const stage = join(root, 'stage');
  mkdirSync(configDir, { recursive: true });
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(stage, { recursive: true });

  const machine = options.machine ?? 'w217-e2e';
  writeFileSync(
    join(configDir, 'config.toml'),
    `machine = ${JSON.stringify(machine)}\n\n[native_host]\nstage = ${JSON.stringify(stage)}\n`,
  );

  const env: NodeJS.ProcessEnv = {
    ...process.env,
    HOME: home,
    // Windows resolves the product's home through `USERPROFILE`, not `HOME`
    // (W306): set both so the child's config/data/state fallbacks land in the
    // temp root on every platform the suite runs on.
    USERPROFILE: home,
    XDG_CONFIG_HOME: join(home, '.config'),
    XDG_DATA_HOME: dataDir,
    XDG_STATE_HOME: join(home, '.local', 'state'),
    // The host reads this config file, so its rustic metadata cache is pinned
    // through the product's own knob (W289): rustic resolves the default root
    // with `dirs::cache_dir()` — `%LOCALAPPDATA%` on Windows — which `HOME` does
    // not move. The `sessions` request reads the stage rather than a
    // repository, so nothing leaks today; the pin is here so a spec that adds a
    // repository-opening frame cannot start writing into the machine's cache.
    CHAT_STASHER_RUSTIC_CACHE_DIR: join(root, 'cache', 'rustic'),
  };
  return new NativeHost(root, stage, machine, env);
}

/** The crate's own request cap (`MAX_REQUEST_BYTES`); the frame never exceeds it. */
const NATIVE_ENDIAN = endianness() === 'LE' ? 'LE' : 'BE';

/**
 * One native-messaging frame: `u32` length prefix in **native** byte order, then
 * the JSON body — `read_request_frame` reads the prefix with
 * `u32::from_ne_bytes`, so a fixed-order prefix would be read as a huge length
 * on a big-endian host and refused as `too-large`.
 */
function encodeFrame(value: unknown): Buffer {
  const body = Buffer.from(JSON.stringify(value), 'utf8');
  const header = Buffer.alloc(4);
  if (NATIVE_ENDIAN === 'LE') header.writeUInt32LE(body.length, 0);
  else header.writeUInt32BE(body.length, 0);
  return Buffer.concat([header, body]);
}

function decodeFrame(bytes: Buffer, label: string): Record<string, unknown> {
  if (bytes.length < 4) {
    throw new Error(`${label}: the host wrote ${bytes.length} bytes, not even a length prefix`);
  }
  const declared = NATIVE_ENDIAN === 'LE' ? bytes.readUInt32LE(0) : bytes.readUInt32BE(0);
  if (bytes.length < 4 + declared) {
    throw new Error(
      `${label}: the host declared ${declared} bytes but wrote ${bytes.length - 4}`,
    );
  }
  const parsed = JSON.parse(bytes.subarray(4, 4 + declared).toString('utf8')) as Record<string, unknown>;
  return parsed;
}

// ---------------------------------------------------------------------------
// Carrying `sendNativeMessage` to that host
// ---------------------------------------------------------------------------

/**
 * The origin the worker's stub posts to.
 *
 * `.invalid` is reserved by RFC 2606 and can never resolve, so if interception
 * ever stopped working the request would still reach nothing — the same property
 * the platform fixtures have.
 */
export const BRIDGE_ORIGIN = 'https://cs-native-bridge.invalid';

/**
 * Runs **inside the service worker**. Self-contained by necessity: Playwright
 * serialises the function source, so it may not close over anything.
 *
 * Both halves of what `sendOnce` accepts are honoured: the stub always returns a
 * promise and never calls the callback, so a failure rejects instead of being
 * delivered as an empty response — `native-host.ts` reads the promise form and
 * reports its message verbatim as `send-failed`, where a fabricated `undefined`
 * would have read as "no response and no runtime.lastError".
 */
const INSTALL_BRIDGE = async (origin: string): Promise<string> => {
  const globals = globalThis as unknown as { browser?: any; chrome?: any };
  const runtime = globals.browser?.runtime?.id ? globals.browser.runtime : globals.chrome?.runtime;
  if (!runtime || typeof runtime.sendNativeMessage !== 'function') return 'no-runtime-api';
  if (runtime.__chatStasherHostBridge) return 'already-installed';
  runtime.sendNativeMessage = (host: string, message: unknown) => (async () => {
    const response = await fetch(`${origin}/native`, {
      method: 'POST',
      headers: { 'content-type': 'text/plain;charset=utf-8' },
      body: JSON.stringify({ host, message }),
      cache: 'no-store',
    });
    if (!response.ok) throw new Error(`native bridge answered HTTP ${response.status}`);
    const envelope = await response.json() as { reply?: unknown; error?: string };
    if (typeof envelope.error === 'string') throw new Error(envelope.error);
    return envelope.reply;
  })();
  runtime.__chatStasherHostBridge = true;
  return 'installed';
};

/**
 * Point one profile's `sendNativeMessage` at `host`.
 *
 * Every message type goes through — `hello`, `deliver`, `has`, `coordination`,
 * `summary` — so a spec cannot accidentally get a stubbed arbiter beside a real
 * stage. This route is registered *first*, which by Playwright's
 * reverse-registration rule makes it the **last** handler to run, so it is
 * reached only by a spec's catch-all falling back for [`BRIDGE_ORIGIN`].
 * `installFakePlatforms` does exactly that; a spec that registers its own
 * `'**\/*'` catch-all must do the same, or every delivery in it fails as
 * `send-failed` (loudly, at least — the extension's own error, not a silent
 * empty stage).
 */
async function installNativeHostBridge(extension: Extension, host: NativeHost): Promise<void> {
  await extension.context.route(`${BRIDGE_ORIGIN}/**`, async (route) => {
    const body = route.request().postData() ?? '';
    let forwarded: { host: string; message: Record<string, unknown> } | null = null;
    try {
      forwarded = JSON.parse(body) as { host: string; message: Record<string, unknown> };
    } catch {
      forwarded = null;
    }
    if (!forwarded || typeof forwarded.message !== 'object' || forwarded.message === null) {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        headers: { 'access-control-allow-origin': '*' },
        body: JSON.stringify({ error: 'the bridge was sent no message' }),
      });
      return;
    }
    let reply: Record<string, unknown> | null = null;
    let error: string | null = null;
    try {
      reply = await host.forward(forwarded.host, forwarded.message);
    } catch (e) {
      error = (e as Error).message;
    }
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      // The worker's fetch is cross-origin (its own origin is
      // `chrome-extension://<id>`) and nothing grants this host permission, so
      // the browser applies CORS to the fulfilled response too.
      headers: { 'access-control-allow-origin': '*' },
      body: JSON.stringify(error === null ? { reply } : { error }),
    });
  });

  const installed = await extension.worker.evaluate(INSTALL_BRIDGE, BRIDGE_ORIGIN);
  if (installed !== 'installed' && installed !== 'already-installed') {
    throw new Error(`the native host bridge was not installed: ${installed}`);
  }
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
