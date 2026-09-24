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
import { epochMsFrom, EPOCH_MS_MAX, EPOCH_MS_MIN, parseDeepSeekListPage } from '../lib/backfill/enumerate';
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
