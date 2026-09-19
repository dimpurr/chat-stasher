/**
 * W36 · The migration preflight: the storage layout moves before any gate decides
 * whether a tick may fetch anything — and it finds its input by scanning
 * `storage.local`, not the target registry.
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
 * `migrateLegacyScopes` is that missing trigger. W36 built it on
 * `loadTargets()` — the scopes the user is *currently registered for* — which
 * left the same hole one door over: a pre-W18 record whose scope is not in the
 * registry is visited by nothing at all, and neither is any of them on a machine
 * whose switch is off, or in the 5-10 minutes before the next tick. W36b's scan
 * enumerates the **keys**, so every caller — the tick preflight and the popup's
 * first state load — reaches every record.
 *
 * ## What this file pins
 *
 *  1. a pre-W18 record is moved even when **no target is registered at all**, and
 *     the record is *gone* afterwards, which can only happen once the ids are
 *     written and read back;
 *  2. a layout that has already moved costs the ledger **nothing**: `openLedger`
 *     is not called at all, because opening it reads the whole debt set out of
 *     IndexedDB — the per-tick cost W18 exists to remove;
 *  3. a record this build cannot read is refused, the refusal is **returned** (it
 *     becomes the tick trace's `halted` and a popup note rather than vanishing),
 *     and not one byte is written;
 *  4. one unreadable key does not hide the keys behind it — the walk continues;
 *  5. a store that cannot be **listed** is refused by name rather than answered
 *     with "there was nothing to find";
 *  6. 🔴 an orphaned pre-W18 key (a migration killed between its header write and
 *     its last step) is cleared only once its copy in the debt store has been
 *     read back and confirmed equal to it.
 */

import { describe, expect, it, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import { legacyScopeFromKey, migrateLegacyScopes } from '../lib/backfill/alarm';
import { openLedger, saveHeader } from '../lib/backfill/ledger';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { readDebtSet, resetDebtDbConnectionForTest } from '../lib/backfill/debt-store';
import {
  initialState,
  isHeader,
  legacyStateKey,
  stateKey,
  type LegacyBackfillState,
} from '../lib/backfill/types';

const PLATFORM = 'chatgpt';
const SCOPE = 'acct-w36';
const PENDING = ['p-1', 'p-2', 'p-3'];
const ARCHIVED = ['a-1'];

/** A faithful pre-W18 record: the whole debt set at one key, ids and all. */
function legacyRecord(scope = SCOPE): LegacyBackfillState {
  return {
    v: 1,
    platform: PLATFORM,
    scope,
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

function freshIdb(): void {
  (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB = new IDBFactory();
}

describe('W36 · the key a pre-W18 record names', () => {
  it('splits on the FIRST colon, because a scope may contain one and a platform id never does', () => {
    expect(legacyScopeFromKey('cs_backfill_v1:claude:org:2f1c')).toEqual({
      platform: 'claude',
      scope: 'org:2f1c',
    });
    expect(legacyScopeFromKey('cs_backfill_v2:chatgpt:acct')).toBeNull();
    expect(legacyScopeFromKey('cs_backfill_v1:chatgpt')).toBeNull();
    expect(legacyScopeFromKey('cs_backfill_v1:chatgpt:')).toBeNull();
    // A key that does not name a scope we ever wrote is not a migration input.
    expect(legacyScopeFromKey('cs_backfill_v1:')).toBeNull();
  });
});

describe('W36 · the migration preflight', () => {
  it('carries a pre-W18 record over with NO target registered, so the layout does not wait for the registry', async () => {
    freshIdb();
    const store = memoryStore({ [legacyStateKey(PLATFORM, SCOPE)]: legacyRecord() });

    // 🔴 No argument about scopes at all. W36's version took `loadTargets()` and
    //    walked it, so with an empty registry this record was visited by nothing.
    const report = await migrateLegacyScopes(store);

    expect(report.found).toBe(1);
    expect(report.moved).toBe(1);
    expect(report.refusal).toBeNull();
    // The header is a header (no id arrays), the ids are in the debt store in
    // order, and the old key is gone — the migration's own last step.
    const header = store.data[stateKey(PLATFORM, SCOPE)];
    expect(isHeader(header)).toBe(true);
    expect(Object.keys(store.data)).not.toContain(legacyStateKey(PLATFORM, SCOPE));
    const debts = await readDebtSet(SCOPE);
    expect(debts?.pending).toEqual(PENDING);
    expect(debts?.archived).toEqual(ARCHIVED);
  });

  it('a storage.local with no pre-W18 record does not open a ledger at all', async () => {
    freshIdb();
    const store = memoryStore();
    // The one call that would read the whole debt set back out of IndexedDB.
    const openLedgerSpy = vi.spyOn(await import('../lib/backfill/ledger'), 'openLedger');

    const report = await migrateLegacyScopes(store);

    expect(openLedgerSpy).not.toHaveBeenCalled();
    expect(report).toMatchObject({ found: 0, moved: 0, refusal: null });
    // And nothing was written: a scope that has nothing does not acquire a record
    // just by being looked at.
    expect(store.data).toEqual({});
    expect(store.writes).toBe(0);
  });

  it('a record this build cannot read is refused by name, and nothing is written', async () => {
    freshIdb();
    const record = { v: 1, platform: PLATFORM, scope: SCOPE, pending: 'not an array' };
    const store = memoryStore({ [legacyStateKey(PLATFORM, SCOPE)]: record });

    const report = await migrateLegacyScopes(store);

    expect(report.refusal?.reason).toBe('state-unreadable');
    // The detail names the key it found, which is what a human needs to act.
    expect(report.refusal?.detail).toContain(legacyStateKey(PLATFORM, SCOPE));
    // Byte-for-byte what was found, and no v2 record beside it.
    expect(store.data[legacyStateKey(PLATFORM, SCOPE)]).toEqual(record);
    expect(Object.keys(store.data)).toHaveLength(1);
  });

  it('reports the first refusal and keeps walking, so one bad scope cannot hide another', async () => {
    freshIdb();
    const store = memoryStore({
      [legacyStateKey(PLATFORM, 'acct-bad')]: { v: 1, platform: PLATFORM, scope: 'acct-bad' },
      // 🔴 A *good* legacy record behind the bad one: the walk must reach it.
      [legacyStateKey(PLATFORM, SCOPE)]: legacyRecord(),
    });

    const report = await migrateLegacyScopes(store);

    expect(report.found).toBe(2);
    expect(report.refusal?.reason).toBe('state-unreadable');
    // The healthy scope still moved.
    expect(report.moved).toBe(1);
    expect(isHeader(store.data[stateKey(PLATFORM, SCOPE)])).toBe(true);
    expect(Object.keys(store.data)).not.toContain(legacyStateKey(PLATFORM, SCOPE));
  });

  it('a key that cannot be read does not stop the walk at that key', async () => {
    freshIdb();
    const inner = memoryStore({
      [legacyStateKey(PLATFORM, 'acct-throws')]: legacyRecord('acct-throws'),
      [legacyStateKey(PLATFORM, SCOPE)]: legacyRecord(),
    });
    // 🔴 W36's version `return`ed here, so every scope after the throwing one was
    //    skipped — one bad read hid a healthy record. The refusal is still
    //    reported; it no longer ends the sweep.
    const flaky: BackfillStore = {
      load: async (key) => {
        if (key === legacyStateKey(PLATFORM, 'acct-throws')) throw new Error('storage read failed');
        return inner.load(key);
      },
      save: (key, value) => inner.save(key, value),
      remove: (key) => inner.remove(key),
      keys: () => inner.keys(),
    };

    const report = await migrateLegacyScopes(flaky);

    expect(report.found).toBe(2);
    expect(report.refusal?.reason).toBe('storage-unavailable');
    expect(report.moved).toBe(1);
    expect(isHeader(inner.data[stateKey(PLATFORM, SCOPE)])).toBe(true);
  });

  it('a store that cannot be listed is refused, not answered with "nothing was found"', async () => {
    freshIdb();
    const inner = memoryStore();
    const unlistable: BackfillStore = {
      load: (key) => inner.load(key),
      save: (key, value) => inner.save(key, value),
      remove: (key) => inner.remove(key),
      keys: async () => { throw new Error('storage.local is not available'); },
    };

    const report = await migrateLegacyScopes(unlistable);

    // 🔴 "I could not look" is not "there is nothing" (CLAUDE.md invariant 1).
    expect(report.refusal?.reason).toBe('storage-unavailable');
    expect(report.found).toBe(0);
  });

  it('🔴 a refused debt-store open is not remembered, so one transient failure does not block every later migration', async () => {
    const real = new IDBFactory();
    let opens = 0;
    /**
     * A factory whose very first `open` throws. The real case is an upgrade
     * blocked by another connection, or a browser that has not finished starting
     * IndexedDB; the shape that matters is "the first attempt refused, the next one
     * would have worked".
     */
    const flaky = {
      open: (name: string, version?: number) => {
        opens += 1;
        if (opens === 1) throw new Error('IndexedDB is not available just now');
        return real.open(name, version);
      },
    } as unknown as IDBFactory;
    (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB = flaky;
    resetDebtDbConnectionForTest();

    // The first read is an honest "could not be read".
    expect(await readDebtSet(SCOPE)).toBeNull();

    // 🔴 …and it is NOT the answer for the rest of this worker's life. Caching it
    //    would make every later `openLedger` refuse with 'storage-unavailable',
    //    which in the migration means the pre-W18 record never moves: the old key
    //    keeps every id and the trace blames the debt store forever, on the
    //    strength of one transient open.
    const second = await readDebtSet(SCOPE);
    expect(second).toEqual({ pending: [], archived: [], nextSeq: 1 });
  });

  it('clears an orphaned pre-W18 key once its copy in the debt store is confirmed equal', async () => {
    freshIdb();
    const legacyKey = legacyStateKey(PLATFORM, SCOPE);
    const store = memoryStore({ [legacyKey]: legacyRecord() });
    // An ordinary first migration, then the same record put back: this is byte
    // for byte the state a kill between the header write and `remove` leaves.
    expect((await openLedger(store, PLATFORM, SCOPE)).ok).toBe(true);
    await store.save(legacyKey, legacyRecord());

    const report = await migrateLegacyScopes(store);

    expect(report).toMatchObject({ found: 1, moved: 1, orphaned: 0, refusal: null });
    expect(Object.keys(store.data)).not.toContain(legacyKey);
    // The new layout was not touched: the debt set is still the one it was.
    const debts = await readDebtSet(SCOPE);
    expect(debts?.pending).toEqual(PENDING);
  });

  it('leaves an orphan alone when the debt store does not hold the same ids', async () => {
    freshIdb();
    const legacyKey = legacyStateKey(PLATFORM, SCOPE);
    const store = memoryStore();
    await store.save(stateKey(PLATFORM, SCOPE), null);
    // A live v2 record whose debt set says something else entirely — the copy in
    // the store is NOT this record's, so removing the record would lose ids.
    const state = initialState(PLATFORM, SCOPE);
    await saveHeader(store, state);
    store.data[legacyKey] = legacyRecord();

    const report = await migrateLegacyScopes(store);

    expect(report.found).toBe(1);
    expect(report.moved).toBe(0);
    expect(report.orphaned).toBe(1);
    expect(report.refusal?.reason).toBe('state-unreadable');
    // Byte for byte, still there.
    expect(store.data[legacyKey]).toEqual(legacyRecord());
  });
});
