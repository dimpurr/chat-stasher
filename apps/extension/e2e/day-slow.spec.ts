/**
 * W296 · **A 429 on a real tick, in a real MV3 worker, arms the rest-of-day brake.**
 *
 * ## Why this is a browser test and not another Node one
 *
 * `tests/w296-day-slow.test.ts` pins the predicate, the window, the record and the
 * plan transform as pure functions; `tests/w296-day-slow-flow.test.ts` drives the
 * real `entrypoints/background.ts` against a fake browser and pins the whole
 * flow, including that the *next* round really fetches fewer bodies. Neither can
 * answer the one question here: **does the shipped extension, in Chromium, on the
 * page-fetch path it actually uses, write that record at all?** The gateway that
 * arms it sits between the engine and the page whose fetch answers for it, and a
 * stub that answered in place of that chain would prove nothing about the chain.
 *
 * ## What this spec does *not* claim
 *
 * It does **not** restart the service worker. Reloading the extension from
 * inside its own worker leaves this harness unable to reach it again —
 * `chrome-extension://` navigations are refused afterwards (measured, and
 * written down in `reload-stale-page.spec.ts`) — so a post-reload storage read
 * would be an assertion about the harness rather than about the product. What
 * makes the brake survive a restart is that it is a record in `storage.local`
 * read fresh by every tick, with nothing kept in memory; that shape is pinned in
 * the Node suite, where "a new reader over the same snapshot" can be built
 * exactly.
 *
 * Everything here is synthetic: an invented organization id, a fixture page, and
 * a 429 the route handler invents. No request leaves the machine.
 */

import { expect } from '@playwright/test';
import {
  fireAlarm,
  readStorage,
  test,
  waitForAlarm,
  waitForTickRecord,
  writeStorage,
  type Extension,
} from './harness';
import { localMidnightAfter } from '../lib/backfill/day-slow';

const ORIGIN = 'https://claude.ai';
/** A synthetic organization id — the shape the row's scope axis expects, nothing more. */
const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const LIST_PATH = `/api/organizations/${ORG}/chat_conversations`;
const ENABLED_KEY = 'cs_backfill_enabled_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';
const TICK_ALARM = 'cs-backfill-tick';
const DAY_SLOW_KEY = 'cs_backfill_day_slow_v1';

/**
 * Serve Claude: the page the spec opens, and a **429 with a `Retry-After`** for the
 * one list request the tick makes. Everything else is aborted and counted, so
 * "nothing left the machine" stays a measurement.
 */
async function serveRateLimitedClaude(ext: Extension): Promise<{ requests: string[]; escaped: string[] }> {
  const requests: string[] = [];
  const escaped: string[] = [];
  await ext.context.route('**/*', (route) => {
    escaped.push(route.request().url());
    return route.abort();
  });
  await ext.context.route(`${ORIGIN}/**`, (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/new') {
      return route.fulfill({
        status: 200,
        contentType: 'text/html; charset=utf-8',
        body: '<!doctype html><html><head><title>synthetic Claude page</title></head><body></body></html>',
      });
    }
    if (path === '/favicon.ico') return route.fulfill({ status: 204, body: '' });
    requests.push(`${route.request().method()} ${path}`);
    if (path === LIST_PATH) {
      return route.fulfill({
        status: 429,
        contentType: 'application/json',
        headers: { 'retry-after': '120' },
        body: '{}',
      });
    }
    if (path === '/api/organizations') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify([{ uuid: ORG }]) });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  return { requests, escaped };
}

test('a 429 on a real tick writes the platform brake, and the tick stops on it', async ({ ext }) => {
  const { requests, escaped } = await serveRateLimitedClaude(ext);
  await ext.context.addCookies([{ name: 'lastActiveOrg', value: ORG, url: ORIGIN }]);
  await writeStorage(ext, {
    [ENABLED_KEY]: true,
    [TARGETS_KEY]: [{ platform: 'claude', origin: ORIGIN, scope: ORG, at: Date.now() }],
  });

  const page = await ext.context.newPage();
  await page.goto(`${ORIGIN}/new`, { waitUntil: 'domcontentloaded' });
  await waitForAlarm(ext, TICK_ALARM);
  // 🔴 The instant the fire is issued, not the instant the tick runs:
  //    the record below is only accepted from a wake newer than this
  //    (`waitForTickRecord`'s `since`), so an already-concluded record
  //    from an earlier wake can never be read as this tick's verdict.
  const firedAt = Date.now();
  await fireAlarm(ext, TICK_ALARM);

  // 🔴 The list request is observable the moment the tick issues it, but the
  //    tick writes its completion record only when the whole run has finished
  //    (`conclude()` in entrypoints/background.ts, after the fetch, its 429
  //    verdict and the brake that verdict arms). Reading the log here therefore
  //    raced the tick and could catch it before the request was issued — the
  //    stale reading, not a refuted run. Wait for the record itself, the same
  //    waiter the other tick specs use: it polls until a record written after
  //    `firedAt` exists and its sweep has concluded, so neither an absent key
  //    nor the provisional pre-sweep record (`SWEEP_NOT_CONCLUDED`,
  //    lib/backfill/alarm.ts) can pass for this tick's outcome.
  //
  //    The request log is read only after this wait, and that ordering is the
  //    fix for the flake this spec carried — the same pre-fix pattern its
  //    sibling spec already shed (`claude-backfill-scope.spec.ts`, whose own
  //    comment tells the same story): the route handler records the request
  //    before the fetch it belongs to resolves, and the record is written
  //    only after that fetch, so a concluded record proves the request was
  //    already observed. The previous `expect.poll(() => requests)` read the
  //    same measurement but under its 5 s default, and failed on a loaded
  //    runner when the tick was merely slow to issue the request. This waiter
  //    polls the product's own signal with its 20 s bound instead, and no
  //    assertion changes: the tick really made one list request, and nothing
  //    escaped the intercepted origins.
  //
  //    The wait also settles the brake reads below: the report the tick makes
  //    for a 429 arms the local brake *inside the request gateway*
  //    (`reportPlatformRateLimit`, entrypoints/background.ts) — before the
  //    response reaches the engine, so long before `conclude()` — which means
  //    the concluded record this wait returned on already proves the brake was
  //    written, and the polls and reads below cannot race a write either.
  await waitForTickRecord(ext, { since: firedAt });

  // The tick really asked the platform and really got the refusal: one list
  // request, and nothing escaped the intercepted origin.
  expect(requests).toEqual([`GET ${LIST_PATH}`]);
  expect(escaped).toEqual([]);

  // …and the same observation left the brake behind, in the extension's own storage.
  await expect.poll(async () => (await readStorage(ext, [DAY_SLOW_KEY]))[DAY_SLOW_KEY]).toBeTruthy();
  const all = await readStorage(ext, [DAY_SLOW_KEY, 'cs_backfill_lasttick_v1']);
  const record = all[DAY_SLOW_KEY] as {
    v: number;
    platforms: Record<string, { until: number; at: number }>;
  };
  expect(record.v).toBe(1);
  expect(Object.keys(record.platforms)).toEqual(['claude']);
  const brake = record.platforms.claude!;
  // 🔴 The window is the rest of the **local** day, and the timestamps are the
  //    real clock of a real worker rather than anything this spec supplied.
  expect(brake.until).toBe(localMidnightAfter(brake.at));
  expect(brake.until).toBeGreaterThan(brake.at);
  expect(brake.at).toBeLessThanOrEqual(Date.now() + 60_000);
  expect(brake.until - brake.at).toBeLessThanOrEqual(25 * 60 * 60 * 1000);

  // The stop the run recorded and the brake come from the same response: a
  // rate-limited stop, not a completed run.
  expect(all.cs_backfill_lasttick_v1).toMatchObject({
    ran: true,
    stopped: 'waiting-retry',
    halted: 'rate-limited',
  });
});
