/** W100b · A stored Claude scope must be authorized by a freshly loaded page. */
import { expect } from '@playwright/test';
import {
  fireAlarm,
  test,
  waitForAlarm,
  waitForTickRecord,
  writeStorage,
  type Extension,
} from './harness';

const ORIGIN = 'https://claude.ai';
const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const LIST_PATH = `/api/organizations/${ORG}/chat_conversations`;
const ENABLED_KEY = 'cs_backfill_enabled_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';
const TICK_ALARM = 'cs-backfill-tick';

async function serveClaudeNewPage(ext: Extension): Promise<{ requests: string[]; escaped: string[] }> {
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
      return route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
    }
    if (path === '/api/organizations') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify([{ uuid: ORG }]) });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  return { requests, escaped };
}

test('a stored Claude scope is allowed by the cookie on a fresh /new page', async ({ ext }) => {
  const { requests, escaped } = await serveClaudeNewPage(ext);
  await ext.context.addCookies([{ name: 'lastActiveOrg', value: ORG, url: ORIGIN }]);
  await writeStorage(ext, {
    [ENABLED_KEY]: true,
    [TARGETS_KEY]: [{ platform: 'claude', origin: ORIGIN, scope: ORG, at: Date.now() }],
  });

  const page = await ext.context.newPage();
  await page.goto(`${ORIGIN}/new`, { waitUntil: 'domcontentloaded' });
  await waitForAlarm(ext, TICK_ALARM);
  // 🔴 The instant the fire is issued, so that only a record this tick writes can
  //    satisfy `since` — an already-concluded record from an earlier wake can
  //    never be read as this tick's verdict (`waitForTickRecord`'s own rule).
  const firedAt = Date.now();
  await fireAlarm(ext, TICK_ALARM);

  // The empty list is an ordinary completed run. The only page fetch is the
  // list for the page-established organization; discovery is skipped because
  // the cookie answered, and no request escaped the intercepted origins.
  await expect.poll(() => requests).toEqual([`GET ${LIST_PATH}`]);
  expect(requests).toEqual([`GET ${LIST_PATH}`]);
  expect(escaped).toEqual([]);
  // 🔴 The list request is observable the instant the tick issues it, but the
  //    trace is written only when the whole run has finished (`conclude()` in
  //    entrypoints/background.ts, after the fetch, the cursor save and the
  //    account observation). Reading storage here therefore raced that write and
  //    could find the key still absent — a stale snapshot, not a verdict. Waiting
  //    for the record instead reads this tick's outcome, and the waiter also
  //    rejects the provisional pre-sweep record, which is a tick still in flight.
  //    No assertion above or below changes.
  const all = await waitForTickRecord(ext, { since: firedAt });
  expect(all.cs_backfill_lasttick_v1).toMatchObject({ ran: true, reason: 'ran', stopped: 'queue-empty' });
});
