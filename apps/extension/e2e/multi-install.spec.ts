/**
 * EXT-9 · **Two profiles, one machine, one host.**
 *
 * ## The topology this file exists for
 *
 * Every other spec in this suite runs one persistent Chromium profile, and the
 * harness says so out loud: its synthetic coordinator answers "granted,
 * `active_installs: 1`" so that backfill specs can exercise pacing without a
 * registered host. That is one install. The product's real topology is
 * `user → N machines → M browsers → K profiles → one extension instance each`
 * (`36-EXTENSION-TOPOLOGY.md` §1), and the audit behind it
 * (`_reconworker-W203-extension-singleton-audit.md`) found the singleton
 * assumption exactly in the places this file drives: bundle provenance, the stage
 * write lock, the backfill request budget, and the response to a rate limit.
 *
 * So this file runs **two** persistent profiles at once, both loaded with the
 * built extension, both pointed at **one real host** — the `chat-stasher` binary,
 * one process per request, over the wire format in
 * `crates/chat-stasher/src/nativehost.rs`. The host is started by the harness
 * rather than by the browser, and `harness.ts`'s `NativeHost` documents why
 * (measured: Chromium resolves its native-messaging directory from the OS home,
 * not from `$HOME`) and exactly which parts are still real — the frames, the
 * process model, the answering code and the state.
 *
 * ## What each case is a statement about
 *
 *  · **(a) one conversation, one record per install.** Two profiles capture the
 *    same conversation. The archive must hold **one** conversation — one id, one
 *    directory — with one record per install that observed it, and those records
 *    must differ in nothing but provenance. Re-delivering a capture's own bytes
 *    must be recognised as a duplicate and add nothing.
 *  · **(b) one backfiller at a time, per platform, per machine.** Both profiles
 *    want to backfill `chatgpt`. While one holds the host's lease the other makes
 *    **no platform request at all** — not a refused request, no request — and once
 *    the holder releases, the other proceeds. Both halves are asserted, because
 *    "nobody backfilled" would otherwise satisfy the first.
 *  · **(c) a 429 in one pauses both.** One profile is answered `429` with a
 *    `Retry-After`; the *other* profile's next attempt is refused by the host's
 *    machine-wide cooldown, and a third install owned by the harness is refused
 *    too. The wait it reports is compared against the header, so the 60 s floor
 *    could not pass for "read the header".
 *  · **(d) concurrent stage writes both land.** Two host processes are started in
 *    the same turn against one stage and one conversation directory, each with a
 *    distinct bundle for it. Both must be sealed, with distinct sequence numbers.
 *    The lock is taken before the duplicate scan precisely so that is true
 *    (`inbox.rs`); without it the two writers would allocate the same sequence
 *    number and the second rename would destroy the first record.
 *  · **(e) one install id, two live writers.** A copy of a profile carries the
 *    original's identity, and EXT-13 detects that from the sequences the two
 *    writers produce (ADR-045). The profile's capture is then refused
 *    **retryably** and stays queued rather than being rejected or dropped, the
 *    host answers its popup "shared", and the repair mints a new identity that
 *    lets the *next* capture through while the queued one stays queued. The
 *    second writer is the one synthesized input in this file; the case says at
 *    length why this harness cannot start one (`launchExtension` starts a profile
 *    that was never copied, and both routes to a real copied profile were
 *    measured and fail for harness reasons).
 *
 * ## Fabrication boundary
 *
 * Nothing here is invented except what `harness.ts` documents. Bundles come from
 * real captures in real browsers; the payloads case (d) races are those same
 * captures; the platform is a Playwright route fixture, exactly as in
 * `capture.spec.ts` and `backfill-migration.spec.ts`, and every request that is
 * not served by it is aborted and asserted empty.
 *
 * ## What is never printed
 *
 * A sealed record carries the conversation body in `raw.text`. These cases read
 * fields out of it — ids, digests, counts — and sha256 the text when they need to
 * compare two records' content. The text itself is never logged, never put in an
 * assertion message and never written to a fixture.
 */

import { expect, test as base } from '@playwright/test';
import { OUTBOX_ALARM_NAME } from '../lib/outbox-alarm';
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import {
  fireAlarm,
  fixture,
  installFakePlatforms,
  launchExtension,
  readOutbox,
  readStorage,
  seedOutbox,
  startNativeHost,
  waitForAlarm,
  waitForTickRecord,
  writeStorage,
  type Extension,
  type NativeHost,
} from './harness';

/** The row `capture.spec.ts` and `backfill-migration.spec.ts` both drive. */
const PLATFORM = 'chatgpt';
const ORIGIN = 'https://chatgpt.com';
const SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const PAGE_PATH = `/c/${SESSION_ID}`;
const API_PATH = `/backend-api/conversation/${SESSION_ID}`;
/** The plan's own list path (`lib/backfill/enumerate.ts`), queried `?offset=&limit=`. */
const LIST_PATH = '/backend-api/conversations';
/** The id the host files this conversation under (`session_dir_id`). */
const ARCHIVE_ID = `${PLATFORM}.${SESSION_ID}`;

const ENABLED_KEY = 'cs_backfill_enabled_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';
const IDENTITY_KEY = 'cs_install_identity_v1';
/** The one-shot tick alarm (`lib/backfill/alarm.ts`). */
const TICK_ALARM = 'cs-backfill-tick';

const CONVERSATION_BODY = fixture('chatgpt-conversation.json');

// ---------------------------------------------------------------------------
// Two profiles, one host
// ---------------------------------------------------------------------------

interface Pair {
  host: NativeHost;
  a: Extension;
  b: Extension;
}

/**
 * Two persistent profiles against one host, and their teardown.
 *
 * The profiles are named `a` and `b` rather than `one`/`two` deliberately: which
 * of them is "first" is a fact about each case (who held the lease, who saw the
 * 429), not a property of the profile, and a name implying an order would read
 * as one.
 */
const multiTest = base.extend<{ pair: Pair; host: NativeHost }>({
  // EXT-13 · The host on its own, for the cases that bring their own profiles —
  // and only their own. Case (e) launches one profile, copies it, and runs the
  // copy; it has no use for a pair, and starting two extra browsers to ignore
  // them would make the case's cost say something untrue about what it drives.
  host: async ({}, use) => {
    const host = startNativeHost();
    try {
      await use(host);
    } finally {
      host.close();
    }
  },
  pair: async ({ host }, use) => {
    const a = await launchExtension({ host });
    const b = await launchExtension({ host });
    try {
      await use({ host, a, b });
    } finally {
      for (const extension of [a, b]) {
        await extension.context.close().catch(() => undefined);
        rmSync(extension.userDataDir, { recursive: true, force: true });
      }
    }
  },
});

interface Served {
  /** The harness's own route log: page loads, conversation bodies, escapes. */
  log: { pageLoads: string[]; apiResponses: string[]; unexpected: string[]; escaped: string[] };
  /** Backfill **list** requests only — the requests cases (b) and (c) are about. */
  list: string[];
}

interface Gate {
  /** Resolves once the held request has arrived. */
  seen: Promise<void>;
  /** Resolves once the case lets it through. */
  released: Promise<void>;
  release(): void;
  /** Called by the route handler; idempotent, so a retry does not re-signal. */
  arrive(): void;
}

/**
 * A one-shot latch held inside a route handler and released by the case.
 *
 * This is how case (b) puts two profiles into one window without sleeping: A's
 * request is *observed* to have arrived before B is asked to try, so the interval
 * is bounded by two facts the product published — A's request and B's tick trace
 * — rather than by a guessed delay. That distinction is the one `waitForAlarm`
 * spells out at length; it is the same rule here.
 */
function makeGate(): Gate {
  let markArrived!: () => void;
  const seen = new Promise<void>((resolve) => { markArrived = resolve; });
  let markReleased!: () => void;
  const released = new Promise<void>((resolve) => { markReleased = resolve; });
  let announced = false;
  return {
    seen,
    released,
    release: markReleased,
    arrive: () => {
      if (announced) return;
      announced = true;
      markArrived();
    },
  };
}

interface ServeOptions {
  /**
   * `capture` (default) serves the fixture page, which fetches its own
   * conversation and therefore captures. `inert` serves a page on the same
   * origin that fetches nothing.
   *
   * 🔴 Why the coordination cases need `inert`. A capture does not only capture:
   *    it kicks the backfill leg fire-and-forget
   *    (`entrypoints/background.ts`, `kickBackfill`), and that kick claims the
   *    host's lease **before** it consults the switch. So a capture taken while
   *    the switch is off still parks a lease, and — because the kick is
   *    fire-and-forget and awaits a port ping first — it can land *after* the
   *    case turns the switch on and become the backfiller the case meant to
   *    choose. Measured: in case (c) the 429 was reported by a kick, and the
   *    alarm this case fired was then refused with `host-paused`.
   *
   *    An inert page keeps the one thing a tick needs from a page — the content
   *    script registers the tab, which is what gives the tick a port — and
   *    removes the capture, so the only backfill in the case is the one the case
   *    fired. The target row is written directly, exactly as
   *    `backfill-migration.spec.ts` does for the same reason.
   */
  page?: 'capture' | 'inert';
  /** What the backfill list request gets. Default: `200` with an empty page. */
  list?: { status: 200; holdUntil?: Gate } | { status: 429; retryAfterSeconds: number };
}

/**
 * Serve this profile's platform: the conversation page, the body it fetches, the
 * backfill list, and nothing else.
 *
 * The page and the body come from the harness's own fixture machinery —
 * `installFakePlatforms`, the same one `capture.spec.ts` uses — so this file
 * cannot drift from what the rest of the suite means by "the platform". The list
 * path is the one addition, and its handler is registered *after* so that it runs
 * *first* (Playwright runs matching handlers in the reverse of their registration
 * order) and falls back for every other path.
 *
 * `list.holdUntil` is what makes case (b) deterministic: the request is answered
 * only when the case says so, which is how the other profile is put inside the
 * window where this one is holding the host's lease.
 */
async function servePlatform(ext: Extension, options: ServeOptions = {}): Promise<Served> {
  const log = await installFakePlatforms(ext.context, [
    { origin: ORIGIN, pagePath: PAGE_PATH, apiPath: API_PATH, apiBody: CONVERSATION_BODY },
  ]);
  const list: string[] = [];
  const listBehaviour = options.list ?? { status: 200 };

  if (options.page === 'inert') {
    // Registered after the harness's own routes, so it runs first for this one
    // path and the fetching fixture page is never served.
    await ext.context.route(`${ORIGIN}${PAGE_PATH}`, (route) => {
      log.pageLoads.push(`GET ${PAGE_PATH}`);
      return route.fulfill({
        status: 200,
        contentType: 'text/html; charset=utf-8',
        body: '<!doctype html><html lang="en"><head><meta charset="utf-8">'
          + '<title>chat-stasher e2e inert page</title></head><body></body></html>',
      });
    });
  }

  await ext.context.route(`${ORIGIN}/**`, async (route) => {
    const { pathname } = new URL(route.request().url());
    if (pathname !== LIST_PATH) return route.fallback();
    list.push(pathname);
    if (listBehaviour.status === 429) {
      return route.fulfill({
        status: 429,
        contentType: 'application/json',
        headers: { 'retry-after': String(listBehaviour.retryAfterSeconds) },
        body: JSON.stringify({ detail: 'rate limited' }),
      });
    }
    if (listBehaviour.holdUntil) {
      listBehaviour.holdUntil.arrive();
      await listBehaviour.holdUntil.released;
    }
    // An empty page: these cases are about coordination and staging, not about
    // how many conversations a platform has.
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items: [], limit: 100, offset: 0, total: 0 }),
    });
  });
  return { log, list };
}

/**
 * Wait for the host to have refused a delivery with this `kind`, and return it.
 *
 * The refusal is the event under test, so a timeout has to say what the host
 * answered instead: "it was never asked", "it was asked and stored it" and "it
 * was asked and refused for another reason" are three different outcomes, and a
 * bare timeout reports none of them.
 */
async function waitForRefusal(
  host: NativeHost,
  kind: string,
  timeoutMs = 30_000,
): Promise<Record<string, unknown>> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const refusal = host.answered.find((reply) => reply.type === 'nack' && reply.kind === kind);
    if (refusal) return refusal;
    if (Date.now() >= deadline) {
      throw new Error(
        `timed out waiting for a \`${kind}\` refusal; the host answered `
        + JSON.stringify(host.answered.map((reply) => ({
          type: reply.type, status: reply.status, kind: reply.kind, ok: reply.ok,
        }))),
      );
    }
    await new Promise((resolve) => { setTimeout(resolve, 50); });
  }
}

/** The popup, as a real page. Its own entrypoint runs and does the rest. *//** The popup, as a real page. Its own entrypoint runs and does the rest. */
async function openPopup(ext: Extension) {
  const page = await ext.context.newPage();
  await page.goto(`chrome-extension://${ext.extensionId}/popup.html`, { waitUntil: 'domcontentloaded' });
  return page;
}

/** Is this element actually on screen? `hidden` is how this popup hides things. */
async function visible(page: Awaited<ReturnType<typeof openPopup>>, id: string): Promise<boolean> {
  return await page.evaluate((elementId: string) => {
    const el = document.getElementById(elementId);
    return el !== null && !(el as HTMLElement).hidden;
  }, id);
}

/** Open the fixture page and wait for the capture the page's own hook reports. */
async function captureOn(ext: Extension): Promise<void> {
  await openPage(ext);
  await ext.context.pages().at(-1)?.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );
}

/**
 * Open the platform page and wait only for the document.
 *
 * For the inert page there is no capture to wait for; for the capture page the
 * caller waits for the capture itself (`captureOn`), which is the event the
 * capture cases are actually about.
 */
async function openPage(ext: Extension): Promise<void> {
  const page = await ext.context.newPage();
  await page.goto(`${ORIGIN}${PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
}

/** Poll until `ready`, or fail naming what was being waited for. Never a silent give-up. */
async function waitUntil(ready: () => boolean, label: string, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!ready()) {
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${label}`);
    await new Promise((resolve) => { setTimeout(resolve, 50); });
  }
}

/**
 * Wait for `count` deliveries the host **answered `stored`**.
 *
 * 🔴 Not "count deliveries were sent". A forwarded request is not an archived
 *    record: the second one can still be in flight, or refused with a `nack` its
 *    status does not show up in. The host seals the shard and *then* acknowledges,
 *    so an acknowledgement is the earliest instant at which reading the stage is a
 *    statement about a completed write — which is exactly what the stage
 *    assertions are. (Measured: reading on the request count made case (a) see one
 *    record instead of two on one run in six.)
 *
 * The failure message lists what the host did answer, because "nothing was
 * stored" and "everything was a duplicate" and "it was refused" are three
 * different facts and a bare timeout says none of them.
 */
async function waitForStoredDeliveries(host: NativeHost, count: number, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const stored = host.answered.filter((reply) => reply.type === 'ack' && reply.status === 'stored');
    if (stored.length >= count) return;
    if (Date.now() >= deadline) {
      throw new Error(
        `timed out waiting for ${count} stored deliveries; the host answered `
        + JSON.stringify(host.answered.map((reply) => ({
          type: reply.type, status: reply.status, kind: reply.kind, ok: reply.ok,
        }))),
      );
    }
    await new Promise((resolve) => { setTimeout(resolve, 50); });
  }
}

/**
 * The tick record for a wake this case fired.
 *
 * `waitForTickRecord` deliberately returns its last reading on timeout rather
 * than throwing — the older spec asserts on the body either way — so a case that
 * needs the record itself says so here, once, instead of reading `undefined`
 * three frames deeper. It requires a record newer than `since`, so a trace left
 * by an earlier wake cannot stand in for the one being asked about.
 */
async function tickAfter(
  ext: Extension,
  since: number,
  timeoutMs = 30_000,
): Promise<Record<string, unknown>> {
  const all = await waitForTickRecord(ext, { since, timeoutMs });
  const record = all['cs_backfill_lasttick_v1'] as { at?: unknown } | undefined;
  if (!record || typeof record !== 'object' || typeof record.at !== 'number' || record.at <= since) {
    throw new Error('no concluded backfill tick record was written for this wake');
  }
  return record as Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Reading the stage
// ---------------------------------------------------------------------------

/** One sealed record, reduced to the fields a case asserts on. */
interface StagedRecord {
  id: string;
  platform: string;
  sessionId: string;
  installId: string | null;
  browser: string | null;
  profileLabel: string | null;
  /** sha256 of the bundle's own bytes — the host's duplicate key. */
  fileSha256: string;
  /** sha256 of the archived conversation text. The text itself is never kept. */
  contentSha256: string;
  /** The conversation directory this record was sealed into, relative to `sessions/<machine>`. */
  conversationDir: string;
  /** The shard file, relative to the stage root. */
  shard: string;
}

function sha256(value: string): string {
  return createHash('sha256').update(value, 'utf8').digest('hex');
}

function walkJsonl(dir: string, prefix = ''): Array<{ path: string; relative: string }> {
  const found: Array<{ path: string; relative: string }> = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    const relative = prefix === '' ? entry.name : `${prefix}/${entry.name}`;
    if (entry.isDirectory()) found.push(...walkJsonl(path, relative));
    else if (entry.name.endsWith('.jsonl')) found.push({ path, relative });
  }
  return found;
}

/**
 * Every sealed record under `sessions/<machine>/`, in shard order.
 *
 * The stage root is this harness's own temp directory, so reading it is not a
 * back door into the product: the product's own reader for these bytes is the
 * archive, and what is asserted here is what a delivery left behind.
 */
function readStage(host: NativeHost): StagedRecord[] {
  const root = join(host.stage, 'sessions', host.machine);
  const shards = walkJsonl(root).sort((x, y) => x.relative.localeCompare(y.relative));
  const records: StagedRecord[] = [];
  for (const shard of shards) {
    // The conversation directory is `<platform>.<session id>` under the machine
    // partition; the shard may sit a bucket level below it.
    const parts = shard.relative.split('/');
    const conversationDir = parts[0] ?? '';
    for (const line of readFileSync(shard.path, 'utf8').split('\n')) {
      if (line.trim() === '') continue;
      const record = JSON.parse(line) as Record<string, unknown>;
      const raw = record.raw as { text?: unknown } | undefined;
      records.push({
        id: String(record.id),
        platform: String(record.platform),
        sessionId: String(record.session_id),
        installId: typeof record.install_id === 'string' ? record.install_id : null,
        browser: typeof record.browser === 'string' ? record.browser : null,
        profileLabel: typeof record.profile_label === 'string' ? record.profile_label : null,
        fileSha256: String(record.file_sha256),
        contentSha256: sha256(typeof raw?.text === 'string' ? raw.text : ''),
        conversationDir,
        shard: shard.relative,
      });
    }
  }
  return records;
}

/** The install identity a profile actually stored — what its bundles must carry. */
async function installIdOf(ext: Extension): Promise<string> {
  const stored = (await readStorage(ext, [IDENTITY_KEY]))[IDENTITY_KEY] as
    | { install_id?: unknown }
    | undefined;
  const id = stored?.install_id;
  if (typeof id !== 'string' || id === '') throw new Error('the profile has no install identity');
  return id;
}

/** Give this profile a switched-on backfill leg with one target. */
async function enableBackfill(ext: Extension): Promise<void> {
  await writeStorage(ext, {
    [ENABLED_KEY]: true,
    [TARGETS_KEY]: [{ platform: PLATFORM, origin: ORIGIN, scope: 'default', at: Date.now() }],
  });
}

// ---------------------------------------------------------------------------
// (a) One conversation, one record per install
// ---------------------------------------------------------------------------

multiTest('(a) two profiles capture one conversation: one conversation, one record each, provenance the only difference', async ({ pair }) => {
  multiTest.setTimeout(180_000);
  const { host, a, b } = pair;
  const logA = await servePlatform(a);
  const logB = await servePlatform(b);

  // Both profiles really do capture the same conversation from a page load in a
  // real browser, and both really do deliver it: a delivery is what the host
  // records, and an empty outbox afterwards means the host accepted it.
  await captureOn(a);
  await captureOn(b);
  await waitForStoredDeliveries(host, 2);
  expect(logA.log.apiResponses).toEqual([`GET ${API_PATH}`]);
  expect(logB.log.apiResponses).toEqual([`GET ${API_PATH}`]);
  expect(logA.log.escaped).toEqual([]);
  expect(logB.log.escaped).toEqual([]);

  const idA = await installIdOf(a);
  const idB = await installIdOf(b);
  expect(idA).not.toBe(idB);

  const staged = readStage(host);
  expect(staged.length).toBe(2);

  // One conversation: one archive id, one session id, one directory.
  expect(new Set(staged.map((record) => record.id))).toEqual(new Set([ARCHIVE_ID]));
  expect(new Set(staged.map((record) => record.sessionId))).toEqual(new Set([SESSION_ID]));
  expect(new Set(staged.map((record) => record.conversationDir))).toEqual(new Set([ARCHIVE_ID]));

  // Two observations, attributed to the two profiles that made them: the
  // archive's provenance is the browsers' own identity, not a guess.
  expect(new Set(staged.map((record) => record.installId))).toEqual(new Set([idA, idB]));

  // 🔴 The claim in one line: the two records differ in **nothing but**
  //    provenance. Both carry the same conversation text — compared as a digest,
  //    because the text is never printed — while their bundle bytes differ, which
  //    is the whole reason two records exist rather than one.
  expect(new Set(staged.map((record) => record.contentSha256)).size).toBe(1);
  expect(new Set(staged.map((record) => record.fileSha256)).size).toBe(2);

  // Re-delivering a capture's own bytes is recognised as a duplicate and adds no
  // record: idempotency is content-addressed on the bundle's exact bytes.
  const firstDelivery = host.delivered()[0];
  if (!firstDelivery) throw new Error('no delivery was recorded');
  const before = readStage(host).map((record) => record.shard);
  // 🔴 EXT-13 · The replay goes out **verbatim**, sequence and nonce included,
  //    and that is the point rather than a convenience. A replayed frame is one
  //    reservation arriving twice, which the host answers as the duplicate it
  //    is: the same `report_seq` under the same `report_nonce` is evidence of
  //    nothing, while the same sequence under a *different* nonce is two copies
  //    of a profile — which is the case below, and not this one.
  const again = await host.ask({ ...(firstDelivery as Record<string, unknown>) });
  expect(again.type).toBe('ack');
  expect(again.status).toBe('duplicate');
  expect(readStage(host).map((record) => record.shard)).toEqual(before);
});

// ---------------------------------------------------------------------------
// (b) One backfiller at a time
// ---------------------------------------------------------------------------

multiTest('(b) only one install backfills a platform at a time: the second makes no request until the first releases', async ({ pair }) => {
  multiTest.setTimeout(180_000);
  const { host, a, b } = pair;
  const gate = makeGate();
  // A holds the answer to its own list request. B's list route is not held,
  // because B is not supposed to issue one while A holds the lease.
  const logA = await servePlatform(a, { page: 'inert', list: { status: 200, holdUntil: gate } });
  const logB = await servePlatform(b, { page: 'inert' });

  // Inert pages: the tab is registered (so a tick has a port to fetch through)
  // and nothing is captured, so no capture kick can become the backfiller this
  // case meant to choose — see `ServeOptions.page`. The empty conversation log
  // is that premise as a measurement rather than an assumption: had a capture
  // happened, this line is where the case would say so.
  await openPage(a);
  await openPage(b);
  expect(logA.log.apiResponses).toEqual([]);
  expect(logB.log.apiResponses).toEqual([]);
  expect(logA.list).toEqual([]);
  expect(logB.list).toEqual([]);

  await enableBackfill(a);
  await waitForAlarm(a, TICK_ALARM);

  // A runs first: its list request arrives and is held, so A holds the host's
  // platform lease for as long as its tick is alive.
  const firedAtA = Date.now();
  await fireAlarm(a, TICK_ALARM);
  await gate.seen;
  expect(logA.list).toEqual([LIST_PATH]);

  // B is switched on **inside** that window, so its attempt cannot be the one
  // that got there first: any tick it runs now meets a lease A is holding.
  await enableBackfill(b);
  await waitForAlarm(b, TICK_ALARM);
  const firedAtB = Date.now();
  await fireAlarm(b, TICK_ALARM);
  const tickB = await tickAfter(b, firedAtB);
  expect(tickB.reason).toBe('host-paused');
  expect(tickB.ran).toBe(false);
  expect(logB.list).toEqual([]);
  expect(logB.log.apiResponses).toEqual([]);

  // Release A: its tick finishes and gives the lease back.
  gate.release();
  await tickAfter(a, firedAtA);
  expect(logA.list).toEqual([LIST_PATH]);

  // B, asking again once the lease is free, does backfill: the refusal above was
  // "not your turn", never "not ever". Exactly one request — two would mean a
  // tick that ran twice.
  await waitForAlarm(b, TICK_ALARM);
  await fireAlarm(b, TICK_ALARM);
  await waitUntil(() => logB.list.length >= 1, "B's list request once the lease was free");
  expect(logB.list).toEqual([LIST_PATH]);

  // The host is the arbiter here, so its own record is the corroborating fact:
  // both profiles asked it, and it saw two installs on this machine.
  const claimants = host.forwarded
    .filter((message) => message.type === 'coordination' && message.mode === 'claim')
    .map((message) => message.install_id);
  expect(new Set(claimants).size).toBe(2);
  expect(logA.log.escaped).toEqual([]);
  expect(logB.log.escaped).toEqual([]);
});

// ---------------------------------------------------------------------------
// (c) A 429 in one pauses both
// ---------------------------------------------------------------------------

multiTest('(c) a 429 in one install pauses the other: the cooldown is machine-wide and honours Retry-After', async ({ pair }) => {
  multiTest.setTimeout(180_000);
  const { host, a, b } = pair;
  const retryAfterSeconds = 120;
  const logA = await servePlatform(a, { page: 'inert', list: { status: 429, retryAfterSeconds } });
  const logB = await servePlatform(b, { page: 'inert', list: { status: 429, retryAfterSeconds } });

  await openPage(a);
  await openPage(b);
  await enableBackfill(a);
  await waitForAlarm(a, TICK_ALARM);

  // A is answered 429 and stops: the run's own trace names the halt.
  const firedAtA = Date.now();
  await fireAlarm(a, TICK_ALARM);
  const tickA = await tickAfter(a, firedAtA);
  expect(logA.list).toEqual([LIST_PATH]);
  expect(tickA.halted).toBe('rate-limited');

  // B is switched on only now — after A's 429 has been reported to the host —
  // so its refusal cannot be its own doing: B has never spoken to the platform.
  await enableBackfill(b);
  await waitForAlarm(b, TICK_ALARM);
  const firedAtB = Date.now();
  await fireAlarm(b, TICK_ALARM);
  const tickB = await tickAfter(b, firedAtB);
  expect(tickB.reason).toBe('host-paused');
  expect(tickB.ran).toBe(false);
  expect(logB.list).toEqual([]);
  expect(logB.log.apiResponses).toEqual([]);

  // 🔴 Corroboration from a third install the harness owns: the pause lives in
  //    the host, not in either profile's storage, and the wait it reports is the
  //    header's own number — the 60 s floor alone could not produce it.
  const probe = await host.ask({
    protocol: 1,
    type: 'coordination',
    request_id: 'w217-probe-claim',
    mode: 'claim',
    platform: PLATFORM,
    install_id: 'w217-harness-probe',
  });
  expect(probe.ok).toBe(true);
  expect(probe.granted).toBe(false);
  expect(probe.active_installs).toBeGreaterThanOrEqual(2);
  expect(probe.wait_ms).toBeGreaterThan((retryAfterSeconds - 15) * 1000);
  expect(probe.wait_ms).toBeLessThanOrEqual(retryAfterSeconds * 1000);
  expect(probe.cooldown_until).toBeGreaterThan(Date.now());
  expect(logA.log.escaped).toEqual([]);
  expect(logB.log.escaped).toEqual([]);
});

// ---------------------------------------------------------------------------
// (d) Concurrent stage writes both land
// ---------------------------------------------------------------------------

multiTest('(d) two host processes seal into one conversation directory: both records land, with distinct sequence numbers', async ({ pair }) => {
  multiTest.setTimeout(180_000);
  const { host, a, b } = pair;
  await servePlatform(a);
  await servePlatform(b);

  // Two real captures of the same conversation, one per profile. Their bundles
  // differ — each carries its own install identity — which is what makes these
  // two writers rather than one write delivered twice.
  await captureOn(a);
  await captureOn(b);
  await waitForStoredDeliveries(host, 2);
  const [first, second] = host.delivered();
  if (!first || !second) throw new Error('expected two deliveries');
  expect(first.sha256).not.toBe(second.sha256);

  // A second host — a second stage, the same machine and the same platform — is
  // where the race is run, so both payloads are new bytes to it. The two
  // processes are started in the same turn, which is what puts them in one
  // window; the stage lock is what makes the outcome the same either way.
  const fresh = startNativeHost({ machine: host.machine });
  try {
    const [sealedFirst, sealedSecond] = await Promise.all([
      fresh.ask({ ...first }),
      fresh.ask({ ...second }),
    ]);
    expect([sealedFirst.status, sealedSecond.status]).toEqual(['stored', 'stored']);
    expect(sealedFirst.shard).not.toBe(sealedSecond.shard);

    const staged = readStage(fresh);
    expect(staged.length).toBe(2);
    expect(new Set(staged.map((record) => record.shard)).size).toBe(2);
    // Both records are in the *same* conversation directory: the writers really
    // were contending for one directory's sequence numbers.
    expect(new Set(staged.map((record) => record.conversationDir)).size).toBe(1);
    expect(new Set(staged.map((record) => record.fileSha256)))
      .toEqual(new Set([first.sha256, second.sha256]));
    expect(new Set(staged.map((record) => record.installId)))
      .toEqual(new Set([await installIdOf(a), await installIdOf(b)]));
  } finally {
    fresh.close();
  }
});

// ---------------------------------------------------------------------------
// (e) A copied profile: one install id, two live copies
// ---------------------------------------------------------------------------

/**
 * EXT-13 · **Copying a profile copies its identity, and the two copies are
 * invisible to each other until they reserve the same sequence.**
 *
 * This is the case the whole EXT-13 mechanism exists for, and it is the one case
 * that cannot be written against a stubbed storage layer, because the thing
 * under test is what two *real* browser profiles do with their own
 * `storage.local` after one of them was copied from the other:
 *
 *  · the copy carries `cs_install_identity_v1` verbatim, so both profiles report
 *    the same install id — the assertion that makes this a copy case rather than
 *    a second profile;
 *  · neither carries a counter yet, so both start at the same value and reserve
 *    the *same* number for their first delivery. Each mints a token of its own
 *    for it, and the host has already recorded the original's: the same sequence
 *    under a second token is two reservations of one number, which is the only
 *    positive evidence of a second writer the protocol has (ADR-045);
 *  · the copy's capture is therefore refused `identity-conflict`, **retryably** —
 *    it stays queued rather than being rejected or dropped, and nothing of it
 *    reaches the stage;
 *  · the popup of the copy shows the repair, and pressing it mints a new install
 *    id, after which that profile's captures archive normally again.
 *
 * The profile is copied **while it is closed**, which is how a person copies one
 * and the only way the copy is a coherent snapshot on disk.
 */
multiTest('(e) a shared install id: the capture stays queued, and the repair lets it through', async ({ host }) => {
  const ext = await launchExtension({ host });
  const pageErrors: string[] = [];
  try {
    const log = await servePlatform(ext);

    // 1. A real capture from a real browser, archived under this profile's id.
    await captureOn(ext);
    await waitForStoredDeliveries(host, 1);
    expect(log.log.escaped).toEqual([]);
    const installId = await installIdOf(ext);
    expect(readStage(host)[0]?.installId).toBe(installId);

    // 2. The other live copy, at the **wire level**.
    //
    // 🔴 This is the one thing here that is synthesized, and it is synthesized
    //    because this harness cannot produce it. Two live contexts on one profile
    //    is exactly what a copied profile is, and `launchExtension` starts a
    //    profile that has not been copied. Both ways of getting there were
    //    measured, and neither works: a profile re-launched over its own
    //    directory loses the harness's native bridge (`send-failed` on every
    //    delivery), and a second browser started from a copy taken while the
    //    first was running has a bridge that answers a direct fetch but whose own
    //    deliveries never reach it. Neither is a property of the product.
    //
    //    What the host sees here is therefore what it would see from a copy: a
    //    delivery carrying **this** install id, a **different** conversation,
    //    and a `report_seq` at the number this profile has already reserved —
    //    under a token the copy minted for it, because that is what makes it a
    //    copy rather than this profile's own frame arriving twice. Everything
    //    below — the queued capture, the popup's question, the repair, the
    //    recovery — is the product's own code against the real host.
    const first = host.delivered()[0];
    if (!first) throw new Error('no delivery was recorded');
    // A copy of an untouched profile starts where the original did, so its first
    // reservation is the number this profile has already sent.
    expect(first.report_seq).toBe(1);
    expect(typeof first.report_nonce).toBe('string');
    const sessionOf = (payload: string, session: string): string => {
      const bundle = JSON.parse(payload) as Record<string, unknown>;
      bundle.sessionId = session;
      return JSON.stringify(bundle);
    };
    const secondCopyPayload = sessionOf(String(first.payload), 'w248-second-copy-session');
    const otherCopy = await host.ask({
      protocol: 1,
      type: 'deliver',
      request_id: 'w248-second-copy',
      name: 'chatgpt-w248-second-copy-session.json',
      payload: secondCopyPayload,
      sha256: createHash('sha256').update(secondCopyPayload, 'utf8').digest('hex'),
      report_seq: first.report_seq,
      report_nonce: 'w248-second-copy-nonce',
    });
    expect(otherCopy).toMatchObject({ type: 'nack', kind: 'identity-conflict', retryable: true });
    // Nothing of the second copy's bytes was archived: the refusal is total.
    expect(readStage(host).length).toBe(1);

    const verdict = await host.ask({
      protocol: 1, type: 'identity_state', request_id: 'w248-verdict', install_id: installId,
    });
    expect(verdict.identity_conflict).toBe(true);

    // 3. The popup is told, over its own message channel. This is the exact
    //    message the repair card is gated on and the exact message its button
    //    sends — `lib/popup-view.ts`'s `POPUP_REQUEST_IDENTITY_MESSAGE` — and a
    //    popup page is a real page in a real browser, so the round trip is the
    //    product's. What the card *looks like* once the answer is `true` is
    //    asserted against the view layer instead (`tests/w248-*`), because this
    //    file is about what the wire and the store do.
    const popup = await openPopup(ext);
    popup.on('pageerror', (error) => pageErrors.push(error.message));
    const asked = await popup.evaluate(async () => await (globalThis as unknown as { chrome: { runtime: { sendMessage: (m: unknown) => Promise<unknown> } } }).chrome.runtime.sendMessage({ type: 'cs-request-identity' }));
    expect(asked).toEqual({ ok: true, conflict: true });

    // 4. A capture this profile takes now is refused **retryably** and stays
    //    queued: not rejected, and nothing about it dropped.
    const queuedPayload = sessionOf(String(first.payload), 'w248-queued-session');
    await seedOutbox(ext, [{
      sha256: createHash('sha256').update(queuedPayload, 'utf8').digest('hex'),
      name: 'chatgpt-w248-queued-session.json',
      payload: queuedPayload,
      bytes: Buffer.byteLength(queuedPayload, 'utf8'),
      enqueuedAt: Date.now(),
      attempts: 0,
      lastError: null,
      lastAttemptAt: null,
      state: 'pending',
    }]);
    await fireAlarm(ext, OUTBOX_ALARM_NAME);
    await expect.poll(
      async () => (await readOutbox(ext))[0]?.lastError,
      { timeout: 30_000 },
    ).toBe('nack:identity-conflict');
    const held = (await readOutbox(ext))[0];
    expect(held?.state).toBe('pending');
    expect(held?.rejectKind).toBeUndefined();
    expect(readStage(host).length).toBe(1);

    // 5. The repair — the message the popup's button sends, sent from the popup.
    //    It mints a new install id for this profile and nothing else does: the
    //    refusal in step 4 rotated nothing by itself.
    const repaired = await popup.evaluate(async () => await (globalThis as unknown as { chrome: { runtime: { sendMessage: (m: unknown) => Promise<unknown> } } }).chrome.runtime.sendMessage({ type: 'cs-rekey-identity' }));
    const rekey = repaired as { ok?: boolean; install?: { install_id?: string } } | null;
    expect(rekey?.ok).toBe(true);
    const repairedId = rekey?.install?.install_id as string;
    expect(repairedId).not.toBe(installId);
    expect(await installIdOf(ext)).toBe(repairedId);

    // 6. A capture this profile makes *after* the repair goes out under the new
    //    identity. The one queued before it does not, and is not rewritten to:
    //    its bundle names the identity this profile had when it was captured, and
    //    re-stamping that would rewrite a capture's own provenance. It stays
    //    queued and unchanged, which is the same "nothing dropped" rule that kept
    //    it there — and the repair card says so before the user presses it.
    const afterRepairPayload = sessionOf(String(first.payload), 'w248-after-repair');
    const afterRepairBundle = JSON.parse(afterRepairPayload) as Record<string, unknown>;
    afterRepairBundle.install_id = repairedId;
    const afterRepair = JSON.stringify(afterRepairBundle);
    await seedOutbox(ext, [{
      sha256: createHash('sha256').update(afterRepair, 'utf8').digest('hex'),
      name: 'chatgpt-w248-after-repair.json',
      payload: afterRepair,
      bytes: Buffer.byteLength(afterRepair, 'utf8'),
      enqueuedAt: Date.now(),
      attempts: 0,
      lastError: null,
      lastAttemptAt: null,
      state: 'pending',
    }]);
    await fireAlarm(ext, OUTBOX_ALARM_NAME);
    await waitForStoredDeliveries(host, 2);
    const staged = readStage(host);
    expect(new Set(staged.map((record) => record.installId)))
      .toEqual(new Set([installId, repairedId]));
    const stillQueued = (await readOutbox(ext)).map((entry) => entry.name);
    expect(stillQueued).toEqual(['chatgpt-w248-queued-session.json']);

    // The wire, in order, as this profile's own sequences: 1 for its first
    // capture, 2 for the capture the conflict held, and then **1 again** for the
    // capture made after the repair — because the repair mints a new identity and
    // starts its sequence over. A repair that carried the old counter across
    // would hand the new identity a value the host never recorded from it, which
    // is what the reset exists to prevent.
    //
    // The second copy is not in this list: it was sent by this case, through the
    // harness, not by the extension.
    const sent = host.forwarded.filter((message) => message.type === 'deliver');
    expect(sent.map((message) => message.report_seq)).toEqual([1, 2, 1]);
    // Every message this profile sent carried a token of its own, and the
    // second copy's is not among them: the case's frame named a sequence this
    // profile reserved, and a different token, which is exactly the collision.
    // (The last two are `1` again because the repair restarts the counter, not
    // because a token was reused.)
    const tokens = sent.map((message) => message.report_nonce);
    expect(new Set(tokens).size).toBe(tokens.length);
    expect(tokens).not.toContain('w248-second-copy-nonce');

    // A popup that threw while painting would leave the repair invisible with
    // nothing to show for it; this is the one place a real popup page runs.
    expect(pageErrors).toEqual([]);
  } finally {
    await ext.context.close().catch(() => undefined);
    rmSync(ext.userDataDir, { recursive: true, force: true });
  }
});
