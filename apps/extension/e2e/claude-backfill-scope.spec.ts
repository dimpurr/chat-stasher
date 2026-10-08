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
  // 🔴 The instant the fire is issued, not the instant the tick runs:
  //    the record below is only accepted from a wake newer than this
  //    (`waitForTickRecord`'s `since`), so an already-concluded record
  //    from an earlier wake can never be read as this tick's verdict.
  const firedAt = Date.now();
  await fireAlarm(ext, TICK_ALARM);

  // 🔴 The list request is observable the moment the tick issues it, but the
  //    tick writes its completion record only when the whole run has finished
  //    (`conclude()` in entrypoints/background.ts, after the fetch, the cursor
  //    save and the account observation). Reading storage here therefore raced
  //    the write and could catch the key still absent — a stale read, not a
  //    verdict. Wait for the record itself, the same waiter the other tick
  //    specs use: it polls until a record written after `firedAt` exists and
  //    its sweep has concluded, so neither an absent key nor the provisional
  //    pre-sweep record (`SWEEP_NOT_CONCLUDED`, lib/backfill/alarm.ts) can pass
  //    for the tick's outcome.
  //
  //    The request log is read only after this wait, and that ordering is the
  //    fix for the flake this spec used to carry: the route handler records the
  //    request before the fetch it belongs to resolves, and the record is
  //    written only after that fetch, so a concluded record proves the request
  //    was already observed. The previous `expect.poll(() => requests)` read the
  //    same measurement but under its 5 s default, and failed on a loaded runner
  //    when the tick was merely slow to issue the request. This waiter polls the
  //    product's own signal with its 20 s bound instead, and no assertion changes:
  //    the empty list is still an ordinary completed run, the only page fetch is
  //    still the list for the page-established organization (discovery is skipped
  //    because the cookie answered), and no request escaped the intercepted
  //    origins.
  const all = await waitForTickRecord(ext, { since: firedAt });
  expect(requests).toEqual([`GET ${LIST_PATH}`]);
  expect(escaped).toEqual([]);
  expect(all.cs_backfill_lasttick_v1).toMatchObject({ ran: true, reason: 'ran', stopped: 'queue-empty' });
});
