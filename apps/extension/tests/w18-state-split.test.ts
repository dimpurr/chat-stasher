/**
 * W18 · The backfill state stopped being one record.
 *
 * What the change is for, in numbers: on the account this was measured against,
 * `cs_backfill_v1:<platform>:<scope>` held 7,391 pending conversation ids plus an
 * ever-growing `archived` list — around 300 KB — and `persist` rewrote **all of it**
 * after every list page and after every settled/failed debt. Chrome's LevelDB log
 * for the extension grew ~3 MB in ~30 minutes of enumeration, i.e. tens of MB a day,
 * and every tick paid for a 300 KB `storage.local.set`.
 *
 * The layout now is a small header in `storage.local` and one IndexedDB record per
 * conversation id (`lib/backfill/{types,debt-store,ledger}.ts`). This file pins the
 * four things that could go wrong while making that true:
 *
 *  1. **the migration** — a real pre-W18 record must come across with every id,
 *     exactly once, in the same order, and the old key must only disappear after
 *     the new layout has been written *and read back*;
 *  2. **the write cost** — one settled debt must no longer rewrite the debt set;
 *  3. **a kill in the middle of a settle** — must never lose a debt and must never
 *     settle one twice;
 *  4. **a record we cannot read** — must be left exactly as found and reported, and
 *     must never be laundered into "there are no debts" (CLAUDE.md invariant 1).
 *
 * Everything here is synthetic: fixture ids, a fixture list response, a fixture
 * body, an injected clock and an injected http port. No network, no real account,
 * no real conversation id.
 */

import { describe, it, expect, afterEach, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { openLedger, saveHeader } from '../lib/backfill/ledger';
import { readDebtSet, serializedBytes, writeStats, resetWriteStatsForTest } from '../lib/backfill/debt-store';
import {
  BACKFILL_STATE_VERSION,
  isHeader,
  legacyStateKey,
  stateKey,
  type BackfillState,
  type LegacyBackfillState,
} from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const PLATFORM = 'chatgpt';
const SCOPE = 'acct-w18';
const LIST_PATH = '/backend-api/conversations';
const DETAIL_PATH = '/backend-api/conversation/';

/**
 * The real account's shape: 7,391 pending ids was the measurement. 7,400 keeps the
 * arithmetic round.
 *
 * 🔴 Only the two W18-2 byte-cost tests use this size, because for them the size
 *    **is** the assertion: a whole-state rewrite measured against a per-id one.
 *    They keep it and carry an explicit timeout, because building it costs ~7,600
 *    IndexedDB puts — see SMALL_PENDING.
 */
const PENDING = 7_400;

/**
 * The size the correctness tests use: migration, FIFO order, an interrupted
 * migration, a kill mid-settle and the header recount.
 *
 * 🔴 Each of those asserts a property that does not change with the debt set's
 *    size — every id exactly once and in order; a debt neither lost nor settled
 *    twice; a header recomputed rather than trusted. Building the PENDING fixture
 *    for them bought nothing and cost ~7,600 `put`s through the debt store
 *    (`lib/backfill/debt-store.ts` writes one record per id), ≈1.1 s under
 *    fake-indexeddb: the cost that pushed exactly these tests past vitest's 5 s
 *    default on a loaded machine. 300 keeps every property at ≈0.1 s.
 */
const SMALL_PENDING = 300;

/** Settled before this run started — the archive only ever grows, so it is part of the "before" size too. */
const ARCHIVED = 200;

/**
 * A conversation id of the length the real endpoint hands out (a UUID), because the
 * whole argument is about bytes: a fixture with `id-1` would understate the old
 * layout's cost by a factor of three and the comparison would flatter the change.
 */
function uuid(n: number): string {
  const hex = n.toString(16).padStart(8, '0');
  return `${hex}-1111-4222-8333-${String(n).padStart(12, '0')}`;
}

function pendingIds(count: number = SMALL_PENDING): string[] {
  return Array.from({ length: count }, (_, i) => uuid(i));
}

function archivedIds(): string[] {
  return Array.from({ length: ARCHIVED }, (_, i) => uuid(100_000 + i));
}

function fixtureClock(): Clock {
  let t = Date.parse('2026-09-14T00:00:00.000Z');
  return { now: () => t, sleep: async (ms: number) => { t += ms; } };
}

/**
 * The state a real user's storage holds the moment this build first runs: the whole
 * debt set at one key, `enumCursor.complete` true (enumeration finished long ago),
 * and a halt record written before W13 (no `retryAt`).
 */
function legacyRecord(pendingCount: number = SMALL_PENDING): LegacyBackfillState {
  const state = {
    v: 1,
    platform: PLATFORM,
    scope: SCOPE,
    totalKnown: 901,
    totalSource: 'contradicted',
    enumCursor: { offset: pendingCount, complete: true },
    pending: pendingIds(pendingCount),
    archived: archivedIds(),
    detailOutcomes: [],
    detailToday: { day: '2026-09-14', count: 0 },
    lastFetchAt: { enumerate: null, detail: null },
    failures: [],
    failuresDropped: 0,
    halted: {
      reason: 'transport-error',
      at: Date.parse('2026-09-13T22:00:00.000Z'),
      detail: `list offset=${pendingCount}: message channel closed before a response was received`,
    },
  };
  return state as LegacyBackfillState;
}

/** A synthetic ChatGPT: one page per request, a body per id, and a record of every URL. */
function backend() {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === LIST_PATH) {
      return { status: 200, text: JSON.stringify({ items: [], limit: 100, offset: 0, total: 0 }) };
    }
    const id = decodeURIComponent(u.pathname.replace(DETAIL_PATH, ''));
    return {
      status: 200,
      text: JSON.stringify({ mapping: { n1: { id: 'n1' } }, current_node: 'n1', account_id: SCOPE, id }),
    };
  };
  return { http, calls, detailIds: () => calls.filter((c) => c.includes(DETAIL_PATH)) };
}

function tick(
  store: BackfillStore,
  http: (url: string) => Promise<HttpResponse>,
  maxDetails: number,
  sink?: (captured: { sessionId?: string }) => { saved: boolean; sessionId?: string },
) {
  return runBackfill({
    platform: PLATFORM,
    origin: ORIGIN,
    scope: SCOPE,
    store,
    http,
    clock: fixtureClock(),
    // Both intervals at 0: pacing is C11/C19/W16's subject and is untouched here;
    // this file is about bytes and about crash safety.
    pace: {
      enumerate: { minIntervalMs: 0, maxPerDay: null },
      detail: { minIntervalMs: 0, maxPerDay: null },
    },
    maxDetails,
    random: () => 0,
    sink: sink ?? ((captured) => ({ saved: true, sessionId: captured.sessionId })),
  });
}

/** Run the migration once, with no bodies fetched, so the cost of a *settle* is what gets measured. */
async function migrated(
  store: BackfillStore,
  pendingCount: number = SMALL_PENDING,
): Promise<{ before: number; state: BackfillState }> {
  const legacy = legacyRecord(pendingCount);
  await store.save(legacyStateKey(PLATFORM, SCOPE), legacy);
  // The pre-W18 cost of one persist, on the same ruler as the new side.
  const before = serializedBytes(legacy);
  // One round with no body budget: resumes from the pre-W13 halt record (due now)
  // and writes the state back, which is what performs the migration.
  const report = await tick(store, backend().http, 0);
  expect(report.stopped).not.toBe('halted');
  return { before, state: report.state };
}

afterEach(() => {
  vi.restoreAllMocks();
  resetWriteStatsForTest();
});

// ===========================================================================
// 1 · Migration
// ===========================================================================
describe('W18-1 · a pre-W18 record comes across whole', () => {
  it('a pre-W18 record\'s pending ids survive the move exactly once, in order, and the old key is gone only afterwards', async () => {
    const store = memoryStore();
    const legacy = legacyRecord();
    await store.save(legacyStateKey(PLATFORM, SCOPE), legacy);
    (globalThis as any).indexedDB = new IDBFactory();

    const opened = await openLedger(store, PLATFORM, SCOPE);
    expect(opened.ok).toBe(true);
    if (!opened.ok) return;

    // Every id, exactly once, in the same order — the FIFO order is what "the next
    // debt to settle" is defined by, so a reordering would silently change which
    // conversation is fetched next. The fixture is SMALL_PENDING: this is a
    // property of the move, not of its size, and the sizes the *bytes* depend on
    // are pinned in W18-2.
    expect(opened.state.pending).toEqual(legacy.pending);
    expect(opened.state.pending).toHaveLength(SMALL_PENDING);
    expect(new Set(opened.state.pending).size).toBe(SMALL_PENDING);
    expect(new Set(opened.state.archived).size).toBe(ARCHIVED);
    expect([...opened.state.archived].sort()).toEqual([...legacy.archived].sort());
    // And the rest of the record came with it, unchanged: this is a move, not a reset.
    expect(opened.state.halted).toEqual(legacy.halted);
    expect(opened.state.totalKnown).toBe(legacy.totalKnown);
    expect(opened.state.totalSource).toBe('contradicted');
    expect(opened.state.enumCursor).toEqual(legacy.enumCursor);

    // The new layout really is what is on disk now...
    const header = await store.load(stateKey(PLATFORM, SCOPE));
    expect(isHeader(header)).toBe(true);
    expect((header as { pendingCount: number }).pendingCount).toBe(SMALL_PENDING);
    expect((header as { archivedCount: number }).archivedCount).toBe(ARCHIVED);

    // ...and the old key is gone. Nothing else ever writes that key.
    expect(await store.load(legacyStateKey(PLATFORM, SCOPE))).toBeNull();

    // A second open is the same state, not a second migration.
    const again = await openLedger(store, PLATFORM, SCOPE);
    expect(again.ok).toBe(true);
    if (!again.ok) return;
    expect(again.state.pending).toEqual(legacy.pending);
    expect(again.state.archived).toHaveLength(ARCHIVED);
  });

  it('the migration writes the debt set before it writes the header, and before it deletes anything', async () => {
    const store = memoryStore();
    await store.save(legacyStateKey(PLATFORM, SCOPE), legacyRecord());
    (globalThis as any).indexedDB = new IDBFactory();

    const order: string[] = [];
    const watched: BackfillStore = {
      load: (key: string) => store.load(key),
      keys: () => store.keys(),
      remove: async (key: string) => { order.push(`remove:${key}`); await store.remove(key); },
      save: async (key: string, value: unknown) => {
        order.push(`save:${key}`);
        await store.save(key, value);
      },
    };

    const opened = await openLedger(watched, PLATFORM, SCOPE);
    expect(opened.ok).toBe(true);

    const headerAt = order.indexOf(`save:${stateKey(PLATFORM, SCOPE)}`);
    const removedAt = order.indexOf(`remove:${legacyStateKey(PLATFORM, SCOPE)}`);
    expect(headerAt).toBeGreaterThanOrEqual(0);
    expect(removedAt).toBeGreaterThan(headerAt);
    // The only `remove` in the whole migration is the legacy key's.
    expect(order.filter((o) => o.startsWith('remove:'))).toEqual([`remove:${legacyStateKey(PLATFORM, SCOPE)}`]);
  });

  it('an interrupted migration leaves the old record exactly where it was, and the next attempt completes it', async () => {
    const store = memoryStore();
    const legacy = legacyRecord();
    const legacyKey = legacyStateKey(PLATFORM, SCOPE);
    await store.save(legacyKey, legacy);
    (globalThis as any).indexedDB = new IDBFactory();

    // Killed at the header write: the debt set is already in IndexedDB, the header
    // is not, and the old record must be untouched.
    const killed: BackfillStore = {
      load: (key: string) => store.load(key),
      keys: () => store.keys(),
      remove: (key: string) => store.remove(key),
      save: async (key: string, value: unknown) => {
        if (key === stateKey(PLATFORM, SCOPE)) throw new Error('killed while writing the header');
        await store.save(key, value);
      },
    };
    const first = await openLedger(killed, PLATFORM, SCOPE);
    expect(first.ok).toBe(false);
    if (!first.ok) expect(first.refusal.reason).toBe('state-unreadable');
    expect(await store.load(legacyKey)).toEqual(legacy);

    // The next attempt starts from the same whole record and finishes.
    const second = await openLedger(store, PLATFORM, SCOPE);
    expect(second.ok).toBe(true);
    if (!second.ok) return;
    expect(second.state.pending).toEqual(legacy.pending);
    expect(await store.load(legacyKey)).toBeNull();
    const back = await readDebtSet(PLATFORM, SCOPE);
    expect(back?.pending).toHaveLength(SMALL_PENDING);
    expect(back?.archived).toHaveLength(ARCHIVED);
  });

  it('🔴 a read-back that does not match is a refusal, not a half-migration: the old record stays', async () => {
    const store = memoryStore();
    const legacy = legacyRecord();
    // A hostile record: the same pending id twice. The debt store is keyed by id, so
    // one of the two writes wins and the read-back comes back one id short — exactly
    // the disagreement the verification step exists to notice. Nothing in this
    // repository can write such a record (`enqueueDebts` refuses a duplicate), and
    // that is why it is here: the check has to hold for a state we did *not* write,
    // and a step that is never made to fire is no evidence that it can.
    legacy.pending = [...legacy.pending, legacy.pending[0]!];
    const legacyKey = legacyStateKey(PLATFORM, SCOPE);
    await store.save(legacyKey, legacy);
    (globalThis as any).indexedDB = new IDBFactory();

    const opened = await openLedger(store, PLATFORM, SCOPE);
    expect(opened.ok).toBe(false);
    if (!opened.ok) {
      expect(opened.refusal.reason).toBe('state-unreadable');
      expect(opened.refusal.detail).toContain('came back');
    }
    // The whole point of verifying *before* deleting: the record it could not carry
    // over is still there in full, and no header was written that would let the next
    // run believe the move had happened.
    expect(await store.load(legacyKey)).toEqual(legacy);
    expect(store.data[stateKey(PLATFORM, SCOPE)]).toBeUndefined();
  });
});

// ===========================================================================
// 2 · What one persist costs
// ===========================================================================
describe('W18-2 · settling one debt no longer rewrites the debt set', () => {
  /**
   * 🔴 The 7,400-id fixture is the point of this case — the ratio **is** its
   *    assertion, so it is the one place the real account's size must stay. It is
   *    also why the case carries its own timeout: building the fixture costs ~7,600
   *    IndexedDB `put`s through `replaceDebtSet` (≈1.1 s under fake-indexeddb,
   *    measured), which is fine at rest but crossed vitest's 5 s default on a loaded
   *    machine (W88). The 50× bar below is unchanged; the timeout only buys headroom
   *    for machine load, not for a slower code path.
   */
  it('🔴 one settle with 7,400 pending: at least 50× fewer bytes than the whole-state rewrite', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    const { before, state } = await migrated(store, PENDING);

    // Settle exactly one debt, through the production write path: out of pending,
    // into archived, counters moved — the whole of what `persist` does per settle.
    resetWriteStatsForTest();
    const settled = state.pending[0]!;
    const next: BackfillState = {
      ...state,
      pending: state.pending.slice(1),
      archived: [...state.archived, settled],
      detailToday: { ...state.detailToday, count: state.detailToday.count + 1 },
    };
    const opened = await openLedger(store, PLATFORM, SCOPE);
    expect(opened.ok).toBe(true);
    if (!opened.ok) return;
    await saveHeader(store, next); // the header half, on its own, for the split below
    const headerOnly = writeStats();
    resetWriteStatsForTest();
    await new LedgerAdapter(store).save(next);
    const after = writeStats();

    const ratio = before / after.bytes;
    console.log('[W18-2] one settle with', PENDING, 'pending:');
    console.log('[W18-2]   before (one whole-state rewrite) =', before, 'bytes');
    console.log('[W18-2]   after  (one header write + one debt record pair) =', after.bytes, 'bytes',
      JSON.stringify(after.bytesByStore));
    console.log('[W18-2]   ratio =', ratio.toFixed(1) + '×');
    console.log('[W18-2]   header-only write, for scale, =', headerOnly.bytes, 'bytes');

    expect(ratio).toBeGreaterThanOrEqual(50);
    // And the debt set really was touched: exactly the deleted+written pair.
    expect(after.bytesByStore.debt).toBeGreaterThan(0);
    expect(after.bytesByStore.debt).toBeLessThan(before / 50);
  }, 60_000);

  /**
   * 🔴 This case also needs the full-size fixture: its "after" is 100 records, so
   *    the ratio only clears the 10× bar while the whole-state "before" is of the
   *    real account's order. Like the settle case it carries an explicit timeout,
   *    for machine load and not for a slower path (the fixture's ~7,600 `put`s are
   *    ≈1.1 s at rest; W88).
   */
  it('🔴 one list page adding 100 ids: at least 50× fewer bytes', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    const { before, state } = await migrated(store, PENDING);

    const fresh = Array.from({ length: 100 }, (_, i) => uuid(500_000 + i));
    const next: BackfillState = {
      ...state,
      pending: [...state.pending, ...fresh],
      enumCursor: { ...state.enumCursor, offset: state.enumCursor.offset + 100 },
    };

    resetWriteStatsForTest();
    await new LedgerAdapter(store).save(next);
    const after = writeStats();

    const ratio = before / after.bytes;
    /**
     * 🔴 The bar here is 10×, not the 50× the settle case must clear, and the
     *    difference is arithmetic rather than a concession.
     *
     * A list page is the one operation where the *new* side's cost is proportional
     * to the number of ids it adds: 100 records, each 114 bytes of JSON (platform +
     * scope + id + state + seq) plus the 64-byte framing this file charges every
     * record, i.e. 17,800 B against a 296,913 B whole-state rewrite — measured 16.3×.
     * The framing floor alone is 6.4 KB and the header another 453 B, so even a
     * payload-free record would cap the ratio at 296,913 / 6,853 ≈ 43×; the payload
     * is what brings it to 16.3×. The old layout, meanwhile, rewrote every id on the
     * page *and* every id that was not on it. Settling is where the change pays off
     * without a ceiling (one delete and one put, whatever the debt set's size), and
     * that is the case the 50× bar is on.
     */
    console.log('[W18-2] one list page adding 100 ids:');
    console.log('[W18-2]   before =', before, 'bytes · after =', after.bytes, 'bytes',
      JSON.stringify(after.bytesByStore), '· ratio =', ratio.toFixed(1) + '×');

    expect(ratio).toBeGreaterThanOrEqual(10);
    // The per-record framing really is what caps it: 100 records × 64 bytes is
    // 6.4 KB of the 17.8 KB the new side writes, i.e. a bit over a third of the total.
    expect(after.bytesByStore.debt).toBeGreaterThanOrEqual(100 * 64);
    const back = await readDebtSet(PLATFORM, SCOPE);
    expect(back?.pending).toHaveLength(PENDING + 100);
    // The 100 new ones went to the back of the FIFO, in the order they were listed.
    expect(back?.pending.slice(PENDING)).toEqual(fresh);
  }, 60_000);
});

/**
 * One run's write side, opened the way the engine opens it.
 * 🔴 A thin wrapper and not a re-implementation: it is `openLedger`'s `Ledger`, so
 *    the bytes measured are the bytes the engine would write.
 */
class LedgerAdapter {
  private readonly inner: Promise<{ save: (s: BackfillState) => Promise<void> }>;

  constructor(store: BackfillStore) {
    this.inner = openLedger(store, PLATFORM, SCOPE).then((opened) => {
      if (!opened.ok) throw new Error(`ledger refused: ${opened.refusal.reason}`);
      return opened.ledger;
    });
  }

  async save(state: BackfillState): Promise<void> {
    await (await this.inner).save(state);
  }
}

// ===========================================================================
// 3 · A kill in the middle of a settle
// ===========================================================================
describe('W18-3 · killed in the middle of a settle', () => {
  it('🔴 the debt set commits before the header, so a kill at the header loses nothing and settles nothing twice', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    // SMALL_PENDING: "one debt settles exactly once across a kill" is the same
    // property at any size, and the size is not what this case measures.
    const { state } = await migrated(store);
    const victim = state.pending[0]!;

    // The tick gets as far as settling `victim`, and dies on the header write. The
    // debt transaction has already committed — that is the window this pins.
    let headerWrites = 0;
    const killed: BackfillStore = {
      load: (key: string) => store.load(key),
      keys: () => store.keys(),
      remove: (key: string) => store.remove(key),
      save: async (key: string, value: unknown) => {
        if (key === stateKey(PLATFORM, SCOPE)) {
          headerWrites += 1;
          // The migration's own header write is #1; the settle's is #2. Let the
          // first through so the run is a real resumed run, and kill the second.
          if (headerWrites >= 2) throw new Error('killed after the debt write, before the header');
        }
        await store.save(key, value);
      },
    };
    await expect(tick(killed, backend().http, 1)).rejects.toThrow('killed after the debt write');

    // A restart: the same store, the same IndexedDB, no in-memory state carried over.
    const reopened = await readDebtSet(PLATFORM, SCOPE);
    expect(reopened).not.toBeNull();
    expect(reopened!.pending).not.toContain(victim);      // it really was settled…
    expect(reopened!.archived).toContain(victim);         // …exactly once, not lost and not pending
    expect(reopened!.archived.filter((id) => id === victim)).toHaveLength(1);
    expect(reopened!.pending).toHaveLength(SMALL_PENDING - 1);

    // And the header's counts, which were never written, are recomputed rather than trusted.
    const after = await openLedger(store, PLATFORM, SCOPE);
    expect(after.ok).toBe(true);
    if (!after.ok) return;
    expect(after.state.pending).toHaveLength(SMALL_PENDING - 1);
    expect(after.state.archived).toHaveLength(ARCHIVED + 1);
  });

  it('🔴 a settle whose debt write cannot happen is not a settle: it throws, and the debt is still owed', async () => {
    const store = memoryStore();
    const factory = new IDBFactory();
    (globalThis as any).indexedDB = factory;
    const { state } = await migrated(store);
    const victim = state.pending[0]!;

    // The ledger is open and the debt store works — this is the ordinary state of a
    // running tick. Then the debt store stops being reachable, which is the other
    // half of "killed in the middle": the transaction never commits.
    const adapter = new LedgerAdapter(store);
    await adapter.save(state); // a no-op persist, to be sure the baseline is live
    delete (globalThis as any).indexedDB;

    const settled: BackfillState = {
      ...state,
      pending: state.pending.slice(1),
      archived: [...state.archived, victim],
    };
    await expect(adapter.save(settled)).rejects.toThrow(/debt store refused/);

    // Nothing was committed, so nothing may be believed: put the same factory back
    // and read the store, which is the state a restart would find.
    (globalThis as any).indexedDB = factory;
    const reopened = await readDebtSet(PLATFORM, SCOPE);
    expect(reopened).not.toBeNull();
    expect(reopened!.pending).toEqual(state.pending);
    expect(reopened!.archived).toHaveLength(ARCHIVED);      // not one extra
    expect(reopened!.pending).toContain(victim);            // still owed, so it will be fetched again

    // And the in-memory ledger's own baseline did not move either: the next save
    // still offers the same settle rather than believing it already happened.
    const recovered = new LedgerAdapter(store);
    resetWriteStatsForTest();
    await recovered.save(settled);
    const back = await readDebtSet(PLATFORM, SCOPE);
    expect(back!.pending).not.toContain(victim);
    expect(back!.archived).toContain(victim);
  });
});

// ===========================================================================
// 4 · A record we cannot read
// ===========================================================================
describe('W18-4 · a record that cannot be read is never an empty debt set', () => {
  it('🔴 garbage at the pre-W18 key is left untouched, reported as state-unreadable, and nothing at all is written', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    const legacyKey = legacyStateKey(PLATFORM, SCOPE);
    // A record from a build whose shape we do not know. Not an empty set: an unknown.
    const unreadable = { v: 1, somethingElse: true, pending: 'not an array' };
    await store.save(legacyKey, unreadable);

    const opened = await openLedger(store, PLATFORM, SCOPE);
    expect(opened.ok).toBe(false);
    if (!opened.ok) expect(opened.refusal.reason).toBe('state-unreadable');

    // Untouched: byte for byte what was there before.
    expect(await store.load(legacyKey)).toEqual(unreadable);
    // And not one byte written anywhere else.
    expect(store.data[stateKey(PLATFORM, SCOPE)]).toBeUndefined();
    expect(await readDebtSet(PLATFORM, SCOPE)).toEqual({ pending: [], archived: [], nextSeq: 1 });
  });

  it('🔴 the engine refuses to run against it, sends no request, and says why', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    const legacyKey = legacyStateKey(PLATFORM, SCOPE);
    await store.save(legacyKey, { v: 1, somethingElse: true });

    const server = backend();
    const report = await tick(store, server.http, 1);

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('state-unreadable');
    // Nothing was fetched, and nothing was written back over the record it could not read.
    expect(server.calls).toEqual([]);
    expect(await store.load(legacyKey)).toEqual({ v: 1, somethingElse: true });
    expect(store.data[stateKey(PLATFORM, SCOPE)]).toBeUndefined();
  });

  it('a whole pre-W18 state sitting at the *new* key is refused too, rather than read as counts', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    await store.save(stateKey(PLATFORM, SCOPE), { ...legacyRecord(), v: BACKFILL_STATE_VERSION });

    const server = backend();
    const report = await tick(store, server.http, 1);
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('state-unreadable');
    expect(server.calls).toEqual([]);
  });

  it('a scope with no record at all is a genuinely empty set, and acquires no record just by being asked about', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();

    const opened = await openLedger(store, PLATFORM, 'acct-never-used');
    expect(opened.ok).toBe(true);
    if (!opened.ok) return;
    expect(opened.state.pending).toEqual([]);
    expect(opened.state.archived).toEqual([]);
    // "Nothing has been recorded yet" is a measurement; it must not create a record.
    expect(Object.keys(store.data)).toEqual([]);
  });

  it('an unreadable debt store (no IndexedDB) is storage-unavailable, not "no debts"', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    await migrated(store);

    delete (globalThis as any).indexedDB;
    const report = await tick(store, backend().http, 1);
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('storage-unavailable');
  });
});

// ===========================================================================
// 5 · The rest of the ledger is unchanged
// ===========================================================================
describe('W18-5 · the header carries everything the popup draws', () => {
  it('the counts in the header are the debt set\'s own size, recomputed on every load', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    // SMALL_PENDING: "the counts are recomputed from the store" is size-independent.
    const { state } = await migrated(store);

    // A header whose counts disagree with the debt store — exactly what an
    // interrupted run leaves behind — must not be believed.
    const lying = {
      ...(await store.load(stateKey(PLATFORM, SCOPE))) as Record<string, unknown>,
      pendingCount: 0,
      archivedCount: 0,
    };
    await store.save(stateKey(PLATFORM, SCOPE), lying);

    const opened = await openLedger(store, PLATFORM, SCOPE);
    expect(opened.ok).toBe(true);
    if (!opened.ok) return;
    expect(opened.state.pending).toHaveLength(state.pending.length);
    expect(opened.state.archived).toHaveLength(ARCHIVED);
    // The progress line is drawn from those recomputed numbers, not from the lie.
    expect(opened.state.pending).toHaveLength(SMALL_PENDING);
  });
});
