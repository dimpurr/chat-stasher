/** W100b · A stored Claude scope must be authorized by a freshly loaded page. */
import { expect } from '@playwright/test';
import {
  fireAlarm,
  readStorage,
  test,
  waitForAlarm,
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
  await fireAlarm(ext, TICK_ALARM);

  // The empty list is an ordinary completed run. The only page fetch is the
  // list for the page-established organization; discovery is skipped because
  // the cookie answered, and no request escaped the intercepted origins.
  await expect.poll(() => requests).toEqual([`GET ${LIST_PATH}`]);
  expect(requests).toEqual([`GET ${LIST_PATH}`]);
  expect(escaped).toEqual([]);
  const all = await readStorage(ext, ['cs_backfill_lasttick_v1']);
  expect(all.cs_backfill_lasttick_v1).toMatchObject({ ran: true, reason: 'ran', stopped: 'queue-empty' });
});
