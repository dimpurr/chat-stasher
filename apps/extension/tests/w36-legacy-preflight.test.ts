/**
 * W36 · The migration preflight: the storage layout moves at the top of a tick,
 * before any gate decides whether that tick may fetch anything.
 *
 * ## Why this is a file of its own
 *
 * `tests/w18-state-split.test.ts` covers the migration itself, inside a run:
 * `runBackfill` opens the ledger and the ledger migrates. That path needs a tick
 * that got all the way to "a request is about to go out" — switch on, host
 * answering, a registered target **and an open platform tab**. The first
 * real-Chrome acceptance spent days in the other state (every tick
 * `no-http-port`, no page open) with its pre-W18 record untouched, and there was
 * nothing in the code that would ever have moved it there.
 *
 * `migrateLegacyScopes` is that missing trigger. This file pins the three
 * properties that make it safe to call on every alarm wake:
 *
 *  1. a scope with a pre-W18 record is migrated — and the record is *gone*
 *     afterwards, which can only happen once the ids are written and read back;
 *  2. a scope with **no** pre-W18 record costs one `storage.local` read and
 *     nothing else: `openLedger` is not called at all, because opening it reads
 *     the whole debt set out of IndexedDB — the per-tick cost W18 exists to
 *     remove;
 *  3. a record this build cannot read is refused, the refusal is **returned**
 *     (it becomes the tick trace's `halted` rather than vanishing), and not one
 *     byte is written.
 */

import { describe, expect, it, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import { migrateLegacyScopes } from '../lib/backfill/alarm';
import * as ledger from '../lib/backfill/ledger';
import { memoryStore } from '../lib/backfill/store';
import { readDebtSet } from '../lib/backfill/debt-store';
import {
  isHeader,
  legacyStateKey,
  stateKey,
  type LegacyBackfillState,
} from '../lib/backfill/types';
import type { BackfillTarget } from '../lib/backfill/alarm';

const PLATFORM = 'chatgpt';
const SCOPE = 'acct-w36';
const ORIGIN = 'https://chatgpt.com';
const PENDING = ['p-1', 'p-2', 'p-3'];
const ARCHIVED = ['a-1'];

function target(scope = SCOPE): BackfillTarget {
  return { platform: PLATFORM, origin: ORIGIN, scope, at: 1 };
}

/** A faithful pre-W18 record: the whole debt set at one key, ids and all. */
function legacyRecord(): LegacyBackfillState {
  return {
    v: 1,
    platform: PLATFORM,
    scope: SCOPE,
    totalKnown: 4,
    totalSource: 'contradicted',
    enumCursor: { offset: 4, complete: true },
    pending: [...PENDING],
    archived: [...ARCHIVED],
    detailOutcomes: [],
    detailToday: { day: '2026-09-19', count: 0 },
    lastFetchAt: { enumerate: null, detail: null },
    failures: [],
    failuresDropped: 0,
    halted: null,
  } as LegacyBackfillState;
}

describe('W36 · the migration preflight', () => {
  it('carries a pre-W18 record over, so the layout moves without waiting for a request', async () => {
    (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB = new IDBFactory();
    const store = memoryStore({ [legacyStateKey(PLATFORM, SCOPE)]: legacyRecord() });

    const refusal = await migrateLegacyScopes(store, [target()]);
    expect(refusal).toBeNull();

    // The header is a header (no id arrays), the ids are in the debt store in
    // order, and the old key is gone — the migration's own last step.
    const header = store.data[stateKey(PLATFORM, SCOPE)];
    expect(isHeader(header)).toBe(true);
    expect(Object.keys(store.data)).not.toContain(legacyStateKey(PLATFORM, SCOPE));
    const debts = await readDebtSet(SCOPE);
    expect(debts?.pending).toEqual(PENDING);
    expect(debts?.archived).toEqual(ARCHIVED);
  });

  it('a scope with no pre-W18 record does not open a ledger at all', async () => {
    (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB = new IDBFactory();
    const store = memoryStore();
    // The one call that would read the whole debt set back out of IndexedDB.
    const openLedger = vi.spyOn(ledger, 'openLedger');

    // Two registered targets, neither with a legacy record: this is the ordinary
    // state of an account that has already been migrated.
    const refusal = await migrateLegacyScopes(store, [target(), target('acct-other')]);

    expect(openLedger).not.toHaveBeenCalled();
    expect(refusal).toBeNull();
    // And nothing was written: a scope that has nothing does not acquire a record
    // just by being looked at.
    expect(store.data).toEqual({});
    expect(store.writes).toBe(0);
  });

  it('a record this build cannot read is refused by name, and nothing is written', async () => {
    (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB = new IDBFactory();
    const record = { v: 1, platform: PLATFORM, scope: SCOPE, pending: 'not an array' };
    const store = memoryStore({ [legacyStateKey(PLATFORM, SCOPE)]: record });

    const refusal = await migrateLegacyScopes(store, [target()]);

    expect(refusal?.reason).toBe('state-unreadable');
    // The detail names the key it found, which is what a human needs to act.
    expect(refusal?.detail).toContain(legacyStateKey(PLATFORM, SCOPE));
    // Byte-for-byte what was found, and no v2 record beside it.
    expect(store.data[legacyStateKey(PLATFORM, SCOPE)]).toEqual(record);
    expect(Object.keys(store.data)).toHaveLength(1);
  });

  it('reports the first refusal and keeps walking, so one bad scope cannot hide another', async () => {
    (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB = new IDBFactory();
    const store = memoryStore({
      [legacyStateKey(PLATFORM, 'acct-bad')]: { v: 1, platform: PLATFORM, scope: 'acct-bad' },
      // 🔴 A *good* legacy record behind the bad one: the walk must reach it.
      [legacyStateKey(PLATFORM, SCOPE)]: legacyRecord(),
    });

    const refusal = await migrateLegacyScopes(store, [target('acct-bad'), target()]);

    expect(refusal?.reason).toBe('state-unreadable');
    // The healthy scope still moved.
    expect(isHeader(store.data[stateKey(PLATFORM, SCOPE)])).toBe(true);
    expect(Object.keys(store.data)).not.toContain(legacyStateKey(PLATFORM, SCOPE));
  });
});
