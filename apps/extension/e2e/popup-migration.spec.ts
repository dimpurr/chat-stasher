/**
 * W36b · **The pre-W18 layout moves when the popup is opened — no tick needed.**
 *
 * ## The state this exists for
 *
 * The alarm tick is not the only occasion the backfill state is loaded; opening
 * the popup is the other one, and it is the one a user actually reaches for when
 * they want to know what is going on. Until W36b that page read
 * `storage.local`, found no `cs_backfill_v2:*` header, and drew "Progress: not
 * started yet" — while the `cs_backfill_v1:*` record holding every id sat
 * untouched in the same storage it had just read. A user in the 5-10 minutes
 * before the next jittered tick, or with the switch off so no alarm fires at
 * all, stayed in that state indefinitely.
 *
 * ## What this spec drives, and why it is a browser test
 *
 * It writes a real pre-W18 record into a real extension's `storage.local`, opens
 * the real `popup.html`, and asserts on what the user sees and on what storage
 * holds afterwards. Nothing here calls `migrateLegacyScopes` directly: the claim
 * is about the *wiring* — that opening the popup reaches the migration at all —
 * and that claim cannot be tested under Node, where the popup's own entrypoint
 * does not run.
 *
 * 🔴 The negative half is as important as the positive one: the progress line
 *    must stop saying "not started yet". "The key moved" alone would still allow
 *    a popup that moved it and then drew the stale snapshot it had already read.
 */

import { expect } from '@playwright/test';
import { readDebtRows, readStorage, test, writeStorage, type Extension } from './harness';

/**
 * A scope no tick in this spec will ever visit: no target is registered, no
 * platform page is opened, and the alarm is never fired. That is the point —
 * W36's preflight walked the target registry, so a record like this one was
 * reachable from nothing at all.
 */
const PLATFORM = 'chatgpt';
const SCOPE = 'acct-w36b-popup';
const LEGACY_KEY = `cs_backfill_v1:${PLATFORM}:${SCOPE}`;
const HEADER_KEY = `cs_backfill_v2:${PLATFORM}:${SCOPE}`;

const PENDING = ['p-1', 'p-2', 'p-3'];
const ARCHIVED = ['a-1'];

/** The record a user's storage holds before this build first runs: the whole debt set at one key. */
function legacyRecord(): Record<string, unknown> {
  return {
    v: 1,
    platform: 'chatgpt',
    scope: SCOPE,
    totalKnown: 4,
    totalSource: 'contradicted',
    enumCursor: { offset: 4, complete: true },
    pending: PENDING,
    archived: ARCHIVED,
    detailOutcomes: [],
    detailToday: { day: '2026-09-19', count: 0 },
    lastFetchAt: { enumerate: null, detail: null },
    failures: [],
    failuresDropped: 0,
    halted: null,
  };
}

/** The popup, as a real page. Its own entrypoint runs and does the rest. */
async function openPopup(ext: Extension) {
  const page = await ext.context.newPage();
  await page.goto(`chrome-extension://${ext.extensionId}/popup.html`, { waitUntil: 'domcontentloaded' });
  return page;
}

/** Poll a reading of `storage.local` until `settled` accepts it, and return it. */
async function waitForStorage(
  ext: Extension,
  settled: (all: Record<string, unknown>) => boolean,
  timeoutMs = 20_000,
): Promise<Record<string, unknown>> {
  const deadline = Date.now() + timeoutMs;
  let all = await readStorage(ext, null);
  while (!settled(all) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    all = await readStorage(ext, null);
  }
  return all;
}

test('opening the popup moves a pre-W18 record on its own, and says so', async ({ ext }) => {
  await writeStorage(ext, { [LEGACY_KEY]: legacyRecord() });
  // The premise, measured rather than assumed: the record is really there and no
  // new-layout key exists yet.
  const before = await readStorage(ext, null);
  expect(Object.keys(before)).toContain(LEGACY_KEY);
  expect(Object.keys(before)).not.toContain(HEADER_KEY);

  const popup = await openPopup(ext);

  // 🔴 No alarm is fired and no platform page is opened in this spec. The popup
  //    is the only thing that runs.
  const after = await waitForStorage(ext, (all) => !(LEGACY_KEY in all));

  // The whole move: the header in the new layout, the ids in the debt database,
  // and the old key gone — which can only happen once they have been written and
  // read back (lib/backfill/ledger.ts's `migrate`).
  const header = after[HEADER_KEY] as Record<string, unknown> | undefined;
  expect(header).toBeTruthy();
  expect(header?.v).toBe(2);
  expect(header?.pendingCount).toBe(PENDING.length);
  expect(header?.archivedCount).toBe(ARCHIVED.length);
  expect(Object.keys(after)).not.toContain(LEGACY_KEY);

  const debts = (await readDebtRows(ext))
    .filter((row) => row.platform === PLATFORM && row.scope === SCOPE);
  expect(debts.map((row) => row.id).sort()).toEqual([...PENDING, ...ARCHIVED].sort());
  expect(debts.filter((row) => row.state === 'pending').map((row) => row.id)).toEqual(PENDING);

  // The user-visible half: the popup stopped saying "not started yet", and it
  // names the move it made instead of doing it silently.
  await expect.poll(async () => (await popup.textContent('#progress')) ?? '').not.toContain('not started yet');
  const progress = (await popup.textContent('#progress')) ?? '';
  expect(progress).toContain('Archived 1');
  const notes = (await popup.textContent('#notes')) ?? '';
  expect(notes).toContain('Storage layout');

  await popup.close();
});
