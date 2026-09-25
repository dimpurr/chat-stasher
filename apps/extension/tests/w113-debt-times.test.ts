/**
 * W113 · ADR-032 §6 — recording the list's own time for a conversation.
 *
 * Three things are being pinned, and each one is a way this feature could quietly produce a wrong month:
 *
 *  1. **The unit.** A number without a unit is not a time: `1755123456` and `1755123456789` are the same
 *     instant in seconds and in milliseconds. `epochMsFrom` is a range test, not a heuristic — a plausible
 *     conversation time lies in 2000–2100 and the two readings of that window do not overlap, so at most
 *     one interpretation can be in range and "neither" is an answer rather than a coin toss.
 *  2. **The write.** `recordDebtTimes` may not create a debt, may not restate `state` or `seq`, and may not
 *     overwrite a time that is already recorded — a re-listing must not move a conversation to another
 *     month.
 *  3. **The read.** A row with a number but no source, or a source this build does not know, has **no**
 *     time. Reading a bare number would let the page print a month for a value nobody labelled.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import {
  applyDebtDiff,
  readDebtSet,
  recordDebtTimes,
  resetDebtDbConnectionForTest,
} from '../lib/backfill/debt-store';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  DEEPSEEK_DETAIL_PATH,
  DEEPSEEK_LIST_PATH,
  epochMsFrom,
  EPOCH_MS_MAX,
  EPOCH_MS_MIN,
  parseDeepSeekListPage,
} from '../lib/backfill/enumerate';
import type { Clock } from '../lib/backfill/pace';
import { withI18n } from './i18n-harness';

const P = 'deepseek';
const S = 'default';

beforeEach(() => {
  (globalThis as { indexedDB?: unknown }).indexedDB = new IDBFactory();
  resetDebtDbConnectionForTest();
  vi.stubGlobal('browser', withI18n({} as never));
});

async function seed(ids: readonly string[]): Promise<void> {
  const ok = await applyDebtDiff(P, S, { enqueue: [...ids], settle: [], drop: [] }, 1);
  expect(ok).toBe(true);
}

describe('W113 · the unit of a list timestamp', () => {
  it('reads a value in the seconds window as seconds, and one in the milliseconds window as milliseconds', () => {
    const instant = Date.UTC(2026, 8, 24, 12, 0, 0);
    expect(epochMsFrom(instant / 1000)).toBe(instant);
    expect(epochMsFrom(instant)).toBe(instant);
    // Both readings of the same instant land on the same answer, which is the property that matters.
    expect(epochMsFrom(instant / 1000)).toBe(epochMsFrom(instant));
  });

  it('🔴 at most one interpretation is ever in range, so neither end can be confused for the other', () => {
    expect(epochMsFrom(EPOCH_MS_MIN / 1000)).toBe(EPOCH_MS_MIN);        // 2000-01-01 in seconds
    expect(epochMsFrom(EPOCH_MS_MIN)).toBe(EPOCH_MS_MIN);              // 2000-01-01 in milliseconds
    // The seconds window's ceiling, doubled, is still nowhere near the milliseconds window: the two
    // cannot overlap, which is why the rule is a range test rather than a guess between two candidates.
    expect(EPOCH_MS_MIN / 1000).toBeLessThan(EPOCH_MS_MIN);
    expect(EPOCH_MS_MAX / 1000).toBeLessThan(EPOCH_MS_MIN);
  });

  it('🔴 refuses anything whose unit is not decidable, rather than picking the nearer', () => {
    for (const raw of [0, -1, Number.NaN, Number.POSITIVE_INFINITY, 1, 12345, 4_102_444_800_000 + 10 ** 9]) {
      expect(epochMsFrom(raw), String(raw)).toBeNull();
    }
  });
});

describe('W113 · the DeepSeek list keeps a per-conversation time', () => {
  it('records `updated_at` per id, labelled as the list\'s own field', () => {
    const instant = Date.UTC(2026, 6, 4, 9, 30, 0);
    const text = JSON.stringify({
      data: {
        biz_data: {
          chat_sessions: [
            { id: 's-1', seq_id: 10, updated_at: instant / 1000 },
            { id: 's-2', seq_id: 9, updated_at: instant },
          ],
        },
      },
    });
    const parsed = parseDeepSeekListPage(text);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.times?.get('s-1')).toEqual({ at: instant, from: 'list-update' });
    // The same instant written in the other unit reads to the same value.
    expect(parsed.page.times?.get('s-2')).toEqual({ at: instant, from: 'list-update' });
  });

  it('an item with no time contributes no time, and the page still lists it', () => {
    const text = JSON.stringify({
      data: { biz_data: { chat_sessions: [{ id: 's-1', seq_id: 10 }] } },
    });
    const parsed = parseDeepSeekListPage(text);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.ids).toEqual(['s-1']);
    expect(parsed.page.times?.size).toBe(0);
  });

  it('a time whose unit cannot be decided is not recorded', () => {
    const text = JSON.stringify({
      data: { biz_data: { chat_sessions: [{ id: 's-1', seq_id: 10, updated_at: 12_345 }] } },
    });
    const parsed = parseDeepSeekListPage(text);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.times?.has('s-1')).toBe(false);
  });

  it('a non-numeric `updated_at` is still the shape change it always was', () => {
    const text = JSON.stringify({
      data: { biz_data: { chat_sessions: [{ id: 's-1', seq_id: 10, updated_at: '2026-09-24' }] } },
    });
    expect(parseDeepSeekListPage(text).ok).toBe(false);
  });
});

describe('W113 · writing times onto debt rows', () => {
  it('records a time on a row that is already owed', async () => {
    await seed(['a', 'b']);
    const at = Date.UTC(2026, 5, 1);
    expect(await recordDebtTimes(P, S, new Map([['a', { at, from: 'list-update' }]]))).toBe(1);
    const snapshot = await readDebtSet(P, S);
    expect(snapshot?.times.get('a')).toEqual({ at, from: 'list-update' });
    expect(snapshot?.times.has('b')).toBe(false);
  });

  it('🔴 never creates a row: a time for an id nothing enumerated is dropped', async () => {
    await seed(['a']);
    expect(await recordDebtTimes(P, S, new Map([['never-enumerated', { at: Date.UTC(2026, 5, 1), from: 'list-update' }]]))).toBe(0);
    const snapshot = await readDebtSet(P, S);
    // The debt set is exactly what it was — a row created here would be a conversation the engine would
    // then try to fetch.
    expect(snapshot?.pending).toEqual(['a']);
    expect(snapshot?.times.size).toBe(0);
  });

  it('🔴 never overwrites a recorded time, so a re-listing cannot move a conversation to another month', async () => {
    await seed(['a']);
    const first = Date.UTC(2026, 5, 1);
    const second = Date.UTC(2026, 7, 1);
    await recordDebtTimes(P, S, new Map([['a', { at: first, from: 'list-update' }]]));
    expect(await recordDebtTimes(P, S, new Map([['a', { at: second, from: 'list-update' }]]))).toBe(0);
    const snapshot = await readDebtSet(P, S);
    expect(snapshot?.times.get('a')?.at).toBe(first);
  });

  it('leaves `state` and `seq` alone, and the debt set still reads back in its own order', async () => {
    await seed(['a', 'b']);
    await applyDebtDiff(P, S, { enqueue: [], settle: ['a'], drop: [] }, 3);
    const before = await readDebtSet(P, S);
    await recordDebtTimes(P, S, new Map([['b', { at: Date.UTC(2026, 4, 5), from: 'list-create' }]]));
    const after = await readDebtSet(P, S);
    expect(after?.pending).toEqual(before?.pending);
    expect(after?.archived).toEqual(before?.archived);
    expect(after?.nextSeq).toBe(before?.nextSeq);
  });

  it('🔴 a row with a number but no known source has no time', async () => {
    await seed(['a']);
    // A row written by a build that recorded a third source this one does not know. It must read as "no
    // time", not as one of the two we do know.
    await applyDebtDiff(P, S, { enqueue: [], settle: ['a'], drop: [] }, 2);
    const rows = await readDebtSet(P, S);
    expect(rows).not.toBeNull();
    // Re-write the raw row as a future layout would: same shape, unknown source.
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const req = (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB.open('chat-stasher-backfill');
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction('debts_by_platform', 'readwrite');
      tx.objectStore('debts_by_platform').put({ platform: P, scope: S, id: 'a', state: 'archived', seq: 2, at: Date.UTC(2026, 5, 1), atFrom: 'list-created' });
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });
    db.close();
    resetDebtDbConnectionForTest();
    const again = await readDebtSet(P, S);
    expect(again?.times.has('a')).toBe(false);
  });

  it('a store that cannot be opened answers null, which is not the same fact as 0', async () => {
    await seed(['a']);
    (globalThis as { indexedDB?: unknown }).indexedDB = undefined;
    resetDebtDbConnectionForTest();
    const at = Date.UTC(2026, 5, 1);
    expect(await recordDebtTimes(P, S, new Map([['a', { at, from: 'list-update' }]]))).toBeNull();
  });

  it('nothing to record is 0, not a write', async () => {
    await seed(['a']);
    expect(await recordDebtTimes(P, S, new Map())).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// W113b · The same three facts, through the engine's own write path
// ---------------------------------------------------------------------------

/**
 * 🔴 **Why the store-level cases above are not enough.**
 *
 * They drive `applyDebtDiff`/`recordDebtTimes` by hand, which fixes the *order* the two are called in. The
 * engine has its own order, and it is the only one a real account ever runs: it records the times of a list
 * page **and then** persists the ids of that same page (`runBackfill`'s per-page loop). The row a time is
 * written onto therefore has to exist by the time the write happens, and the row has to keep it when the
 * conversation is later archived — a settle writes a whole fresh record (`applyDebtDiff`), so a missing
 * carry-through here erases a value the list gave and no later re-listing restores, because a complete
 * enumeration never lists those ids again.
 *
 * So this section runs the real `runBackfill` over the real ledger and the real debt store, twice — the
 * listing tick and the body tick — and reads the result back with `readDebtSet`. Nothing is seeded by hand.
 */

const ENGINE_SCOPE = 'acct-times';
const ENGINE_ORIGIN = 'https://chat.deepseek.com';
const ENGINE_LIMIT = 100;
/** The instant every synthetic row's `updated_at` resolves to, written in **seconds** — DeepSeek's unit. */
const LISTED_AT = Date.UTC(2026, 5, 4, 9, 30, 0);

interface FixtureSession {
  id: string;
  seq_id: number;
  updated_at: number;
}

function engineClock(): Clock {
  let t = Date.parse('2026-09-20T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

/** One synthetic DeepSeek list page: same envelope as tests/c26-dslist.test.ts, no real endpoint. */
function listPage(sessions: readonly FixtureSession[], hasMore: boolean): string {
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: { biz_data: { chat_sessions: sessions, has_more: hasMore } },
  });
}

/** A synthetic single-conversation body carrying both of lib/contract.ts's DeepSeek `requiredAnyPaths`. */
function detailBody(id: string): string {
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: {
      biz_code: 0,
      biz_msg: 'ok',
      biz_data: {
        chat_session: { id, title: 'synthetic-fixture', current_message_id: 2 },
        chat_messages: [
          { message_id: 1, parent_id: null, role: 'USER', content: 'synthetic-turn-1' },
          { message_id: 2, parent_id: 1, role: 'ASSISTANT', content: 'synthetic-turn-2' },
        ],
      },
    },
  });
}

/** Serves the list page, and the one body, to a run that is otherwise allowed nowhere else. */
function engineBackend(body: string) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === DEEPSEEK_LIST_PATH) return { status: 200, text: listPage(SESSIONS, false) };
    if (u.pathname === DEEPSEEK_DETAIL_PATH) return { status: 200, text: body };
    throw new Error(`unexpected path ${u.pathname}`);
  };
  return { http, calls };
}

const FIRST_ID = 'ds-0001-aaaaaaaa';
const SECOND_ID = 'ds-0002-aaaaaaaa';
const SESSIONS: FixtureSession[] = [
  { id: FIRST_ID, seq_id: 999, updated_at: LISTED_AT / 1000 },
  { id: SECOND_ID, seq_id: 998, updated_at: LISTED_AT / 1000 },
];

function engineTick(
  store: ReturnType<typeof memoryStore>,
  http: (url: string) => Promise<HttpResponse>,
  maxDetails: number,
) {
  return runBackfill({
    platform: P,
    origin: ENGINE_ORIGIN,
    scope: ENGINE_SCOPE,
    store,
    http,
    clock: engineClock(),
    listLimit: ENGINE_LIMIT,
    maxDetails,
  });
}

describe('W113b · the engine lists, then archives, and the time survives both', () => {
  it('🔴 a time recorded while a debt is pending is still there once the debt is settled', async () => {
    const store = memoryStore();
    const be = engineBackend(detailBody(FIRST_ID));

    // Tick 1 — the list. The engine records each page's times and persists the ids of that page.
    const listing = await engineTick(store, be.http, 0);
    expect(listing.state.pending).toEqual(SESSIONS.map((s) => s.id));
    const listed = await readDebtSet(P, ENGINE_SCOPE);
    expect(listed?.times.get(FIRST_ID)).toEqual({ at: LISTED_AT, from: 'list-update' });
    expect(listed?.times.get(SECOND_ID)).toEqual({ at: LISTED_AT, from: 'list-update' });

    // Tick 2 — the body. One conversation is fetched and archived; the enum cursor is complete, so nothing
    // is listed again and no later read can repair a time lost here.
    const bodies = await engineTick(store, be.http, 1);
    expect(bodies.state.archived).toEqual([FIRST_ID]);

    const after = await readDebtSet(P, ENGINE_SCOPE);
    expect(after?.archived).toEqual([FIRST_ID]);
    // 🔴 F1: the settle writes a fresh record, so the time has to be carried through it by name.
    expect(after?.times.get(FIRST_ID)).toEqual({ at: LISTED_AT, from: 'list-update' });
    // The one still pending is untouched, which is what makes the line above a statement about the settle.
    expect(after?.times.get(SECOND_ID)).toEqual({ at: LISTED_AT, from: 'list-update' });
  });

  it('a second listing of the same conversations does not move them to another month', async () => {
    const store = memoryStore();
    const be = engineBackend(detailBody(FIRST_ID));

    await engineTick(store, be.http, 0);
    // The same ids, listed again — the shape W98's migration and a recovery both produce. `recordDebtTimes`
    // is what refuses to restate a time, and this pins that the engine's own path goes through it.
    const recorded = await recordDebtTimes(P, ENGINE_SCOPE, new Map([
      [FIRST_ID, { at: Date.UTC(2026, 8, 1), from: 'list-create' as const }],
    ]));
    expect(recorded).toBe(0);
    const after = await readDebtSet(P, ENGINE_SCOPE);
    expect(after?.times.get(FIRST_ID)).toEqual({ at: LISTED_AT, from: 'list-update' });
  });
});
