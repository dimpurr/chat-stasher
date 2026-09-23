/**
 * W36 · The W18 storage migration, driven by a real tick in a real Chromium.
 *
 * ## What this file is for
 *
 * The first real-Chrome acceptance found `cs_backfill_v1:chatgpt:<scope>` still
 * holding the whole debt set, **no** `cs_backfill_v2:*` key anywhere and no
 * `chat-stasher-backfill` database — while the alarm's own trace said a tick had
 * `ran`. Two different things can produce that reading, and only a browser can
 * tell them apart:
 *
 *  · the migration is unreachable from the tick (it would then be a code defect
 *    in where `openLedger` is called from), or
 *  · the tick ran and `openLedger` **refused**, which writes not one byte and —
 *    until this task — left no trace that it had refused.
 *
 * So the setup here is the acceptance's own: a pre-W18 record in
 * `storage.local`, the backfill switch on, and then the production path that
 * makes a tick happen without an alarm — a real capture on a platform page
 * (`kickBackfill`). What the spec then asserts is the whole migration, in the
 * browser: the ids in the debt database, the header at the v2 key, and the old
 * key gone.
 *
 * ## Why not `runBackfill` directly
 *
 * `tests/w18-state-split.test.ts` already calls `openLedger`/`runBackfill`
 * directly under Node with a fake IndexedDB, and it passes. The question this
 * file asks is the one that cannot be asked there: *does the shipped wiring, in
 * a real MV3 service worker, reach the migration at all, and does the real
 * IndexedDB accept the write?* Nothing here calls an internal function; the only
 * lever pulled is "the user has a platform page open", exactly as in
 * `capture.spec.ts`.
 */

import { expect, type Page } from '@playwright/test';
import {
  fireAlarm,
  fixture,
  listDatabases,
  readDebtRows,
  readStorage,
  test,
  waitForAlarm,
  waitForOutbox,
  writeStorage,
  BACKFILL_DB_NAME,
  type Extension,
} from './harness';

/** chatgpt · the row `capture.spec.ts` also drives, so the fixture body is the same one. */
const CHATGPT_SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const CHATGPT_PAGE_PATH = `/c/${CHATGPT_SESSION_ID}`;
const CHATGPT_API_PATH = `/backend-api/conversation/${CHATGPT_SESSION_ID}`;
/** The row's list path (`lib/backfill/enumerate.ts`), served so the tick is a real one. */
const CHATGPT_LIST_PATH = '/backend-api/conversations';

/**
 * The scope a capture on this fixture registers (`extractIdentity` finds no
 * account field in the body, so `identity.value || 'default'`).
 */
const PLATFORM = 'chatgpt';
const SCOPE = 'default';
const LEGACY_KEY = `cs_backfill_v1:${PLATFORM}:${SCOPE}`;
const HEADER_KEY = `cs_backfill_v2:${PLATFORM}:${SCOPE}`;
const ENABLED_KEY = 'cs_backfill_enabled_v1';

const CHATGPT_BODY = fixture('chatgpt-conversation.json');

function uuid(n: number): string {
  const hex = n.toString(16).padStart(8, '0');
  return `${hex}-1111-4222-8333-${String(n).padStart(12, '0')}`;
}

const PENDING = Array.from({ length: 40 }, (_, i) => uuid(i));
const ARCHIVED = Array.from({ length: 4 }, (_, i) => uuid(900 + i));

/**
 * The record a real user's storage holds before this build first runs: the whole
 * debt set at one key, enumeration long finished, and a halt record written
 * before W13 (no `retryAt`) — the same shape `tests/w18-state-split.test.ts`
 * uses, at a size a browser test can move in one transaction.
 */
function legacyRecord(): Record<string, unknown> {
  return {
    v: 1,
    platform: 'chatgpt',
    scope: SCOPE,
    totalKnown: 44,
    totalSource: 'contradicted',
    enumCursor: { offset: PENDING.length, complete: true },
    pending: PENDING,
    archived: ARCHIVED,
    detailOutcomes: [],
    detailToday: { day: '2026-09-19', count: 0 },
    lastFetchAt: { enumerate: null, detail: null },
    failures: [],
    failuresDropped: 0,
    halted: {
      reason: 'transport-error',
      at: Date.parse('2026-09-18T22:00:00.000Z'),
      detail: `list offset=${PENDING.length}: message channel closed before a response was received`,
    },
  };
}

/**
 * Serve the platform: the conversation page, the conversation body, the list
 * page, and nothing else. Every request to the origin is answered here, so
 * "which requests the leg made" is a fact this spec can assert on.
 */
async function serveChatgpt(ext: Extension): Promise<{ api: string[]; list: string[]; escaped: string[] }> {
  const api: string[] = [];
  const list: string[] = [];
  const escaped: string[] = [];
  // 🔴 The catch-all goes on first (Playwright matches in reverse registration
  //    order), so every request that is not to the intercepted origin is aborted
  //    and counted: "nothing left the machine" is a measurement here, not a hope.
  await ext.context.route('**/*', (route) => {
    escaped.push(route.request().url());
    return route.abort();
  });
  await ext.context.route('https://chatgpt.com/**', (route) => {
    const { pathname } = new URL(route.request().url());
    if (pathname === '/favicon.ico') return route.fulfill({ status: 204, body: '' });
    if (pathname === CHATGPT_PAGE_PATH) {
      return route.fulfill({
        status: 200,
        contentType: 'text/html; charset=utf-8',
        body: fixture('platform-page.html').replaceAll('__CS_API_PATH__', CHATGPT_API_PATH),
      });
    }
    if (pathname === CHATGPT_API_PATH) {
      api.push(pathname);
      return route.fulfill({ status: 200, contentType: 'application/json', body: CHATGPT_BODY });
    }
    if (pathname === CHATGPT_LIST_PATH) {
      list.push(pathname);
      // An empty list page: this spec is about the migration, and the fixture's
      // 44 ids are already the whole debt set.
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ items: [], limit: 100, offset: 0, total: 0 }),
      });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  return { api, list, escaped };
}

/** The extension's own one-shot tick alarm name (`lib/backfill/alarm.ts`). */
const TICK_ALARM = 'cs-backfill-tick';

/** Poll `storage.local` until the alarm has recorded its tick. */
async function waitForTickRecord(
  ext: Extension,
  timeoutMs = 20_000,
): Promise<Record<string, unknown>> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const all = await readStorage(ext, null);
    if (all['cs_backfill_lasttick_v1']) return all;
    if (Date.now() >= deadline) return all;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

/** Poll `storage.local` until the v2 key holds something, or give up. */
async function waitForHeader(ext: Extension, timeoutMs = 20_000): Promise<Record<string, unknown>> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const all = await readStorage(ext, null);
    const header = all[HEADER_KEY];
    if (header && typeof header === 'object') return all;
    if (Date.now() >= deadline) return all;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

async function loadFixturePage(ext: Extension): Promise<Page> {
  const page = await ext.context.newPage();
  await page.goto(`https://chatgpt.com${CHATGPT_PAGE_PATH}`, { waitUntil: 'domcontentloaded' });
  await page.evaluate(
    () => (window as unknown as { __csCapture?: Promise<unknown> }).__csCapture,
  );
  return page;
}

test('a real tick carries a pre-W18 record over: ids in the debt database, header at v2, old key gone', async ({ ext }) => {
  const { api, list } = await serveChatgpt(ext);

  // The user's storage, before anything of ours runs: the whole v1 debt set and
  // the switch the user turned on.
  await writeStorage(ext, {
    [ENABLED_KEY]: true,
    [LEGACY_KEY]: legacyRecord(),
  });

  // A real capture on a real platform page. The live leg stores it in the outbox
  // and, fire-and-forget, kicks the backfill leg — which is the tick under test.
  await loadFixturePage(ext);
  await waitForOutbox(ext, (rows) => rows.length >= 1);

  const all = await waitForHeader(ext);

  // 1 · The old key is gone — and it could only be removed by the migration's
  //     last step, which runs after the ids were written *and read back*.
  expect(Object.keys(all)).not.toContain(LEGACY_KEY);

  // 2 · The header is at the v2 key, is a header (no id arrays), and carries the
  //     old record's own numbers: the migration must not invent progress.
  const header = all[HEADER_KEY] as Record<string, unknown>;
  expect(header).toBeTruthy();
  expect(header.v).toBe(2);
  expect(header.platform).toBe('chatgpt');
  expect(header.scope).toBe(SCOPE);
  expect(header.pendingCount).toBe(PENDING.length);
  expect(header.archivedCount).toBe(ARCHIVED.length);
  expect(Array.isArray(header.pending)).toBe(false);
  expect(Array.isArray(header.archived)).toBe(false);

  // 3 · The ids really are in the debt database, exactly once each, in order.
  const rows = await readDebtRows(ext);
  const mine = rows.filter((row) => row.platform === PLATFORM && row.scope === SCOPE);
  expect(mine.filter((row) => row.state === 'pending').map((row) => row.id)).toEqual(PENDING);
  expect(new Set(mine.filter((row) => row.state === 'archived').map((row) => row.id)))
    .toEqual(new Set(ARCHIVED));

  // 4 · The database the acceptance found missing is the one the migration wrote.
  expect(await listDatabases(ext)).toContain(BACKFILL_DB_NAME);

  // 5 · The tick was a real one: the page's own request went out and was served.
  //     (The leg's own requests are paced by design — `DEFAULT_PACE` — so this
  //     spec asserts the migration, not the fetch schedule.)
  expect(api).toEqual([CHATGPT_API_PATH]);
});

test('a record this build cannot read is left untouched, and the alarm tick says why it did nothing', async ({ ext }) => {
  // The other half of the same invariant, at the same level the acceptance was
  // reading: the alarm's own trace. `ran: true` there means only "runBackfill
  // returned" — a run that halted on a record it could not read looks identical
  // to a run that migrated everything, which is exactly what sent that
  // investigation after the migration itself.
  const { escaped } = await serveChatgpt(ext);
  const unreadable = { v: 1, platform: 'chatgpt', scope: SCOPE, pending: 'not an array' };

  await writeStorage(ext, {
    [ENABLED_KEY]: true,
    [LEGACY_KEY]: unreadable,
    // The registry row the alarm needs: a target is what makes a tick possible
    // without a capture, and it is written by the ordinary capture path too.
    cs_backfill_targets_v1: [
      { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: SCOPE, at: Date.now() },
    ],
  });

  // A platform page open ⇒ the tab registry has a port ⇒ the tick is not blocked
  // by 'no-http-port', which is the state the acceptance's ticks were stuck in.
  await loadFixturePage(ext);

  // 🔴 W82 · The extension arms this alarm in response to the switch going on
  //    above, and `fireAlarm` below overrides the deadline of an alarm that is
  //    already there. Waiting for its own arm is what makes this the last write to
  //    the alarm instead of a race with it (see `waitForAlarm`). This case is the
  //    one that never flaked, and only because the page load above is slow enough
  //    to outlast the extension's arm — luck, not ordering.
  await waitForAlarm(ext, TICK_ALARM);
  await fireAlarm(ext, TICK_ALARM);
  const all = await waitForTickRecord(ext);
  const tick = all['cs_backfill_lasttick_v1'] as Record<string, unknown>;

  // What the acceptance saw, and what it could not see past:
  expect(tick.ran).toBe(true);
  expect(tick.reason).toBe('ran');
  // 🔴 The fact that was missing: the tick's own trace names the refusal.
  expect(tick.halted).toBe('state-unreadable');
  expect(tick.stopped).toBe('halted');

  // And the refusal really refused: nothing written, nothing moved, nothing sent.
  expect(all[LEGACY_KEY]).toEqual(unreadable);
  expect(Object.keys(all)).not.toContain(HEADER_KEY);
  expect(await listDatabases(ext)).not.toContain(BACKFILL_DB_NAME);
  expect(escaped).toEqual([]);
});

test('with no platform tab open at all, the layout still moves: the migration does not wait for a fetch', async ({ ext }) => {
  // 🔴 The state the acceptance was in for days, and the reason this case exists:
  //    every tick blocked at 'no-http-port' because no platform page was open. The
  //    migration used to be reachable only from a run that was about to make a
  //    request, so in exactly this state the user's v1 record was never carried
  //    over — no v2 key, no debt database, and nothing anywhere saying so.
  //
  //    No page is opened in this case. The tick is expected to be *blocked*; what
  //    it must not be is silent about the storage layout.
  await serveChatgpt(ext);

  await writeStorage(ext, {
    [ENABLED_KEY]: true,
    [LEGACY_KEY]: legacyRecord(),
    cs_backfill_targets_v1: [
      { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: SCOPE, at: Date.now() },
    ],
  });

  // 🔴 W82 · The case that flaked, and the reason it did: nothing between the
  //    switch going on above and this fire, so the extension's own `create` and
  //    the one below are two writes to one alarm 2-4 ms apart. The extension's
  //    draw is 5-10 minutes out, so when it lands second the 100 ms deadline is
  //    gone, no tick runs, and `waitForTickRecord` returns nothing 20 s later.
  //    Fire only once the extension has armed it itself.
  await waitForAlarm(ext, TICK_ALARM);
  await fireAlarm(ext, TICK_ALARM);
  // Wait for the complete tick record, not just the header. The header is
  // written by the migration before any gate; the trace that names how the tick
  // ended (and that this body asserts on) is what makes the state consistent, so
  // reading on the header alone could catch the tick between the two. Since W62
  // the trace is recorded before the recovery sweep, so in this no-tab case it
  // lands in the same instant as the migration — never behind the sweep's pings.
  const all = await waitForTickRecord(ext);

  // The tick was blocked — that is the point — and the trace says which gate.
  const tick = all['cs_backfill_lasttick_v1'] as Record<string, unknown>;
  expect(tick).toBeTruthy();
  expect(tick.reason).toBe('no-http-port');
  expect(tick.halted).toBeNull();

  // And the storage layout moved anyway.
  expect(all[HEADER_KEY]).toBeTruthy();
  expect(Object.keys(all)).not.toContain(LEGACY_KEY);
  const rows = await readDebtRows(ext);
  expect(rows.filter((row) => row.platform === PLATFORM && row.scope === SCOPE && row.state === 'pending').map((r) => r.id))
    .toEqual(PENDING);
});
