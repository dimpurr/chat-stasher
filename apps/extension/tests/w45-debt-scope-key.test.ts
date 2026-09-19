/**
 * W45 · The debt store's identity is (platform, scope), and the loss of that
 * platform half destroyed a real account's debt set.
 *
 * Measured in a real logged-in Chrome on 2026-09-19: `cs_backfill_v2:chatgpt:default`
 * said 7,736 pending / 17 archived, three platforms shared the scope string
 * `default`, and the IndexedDB object store `debts` held **0 rows**. `replaceDebtSet`
 * makes one scope hold a snapshot and cleared the scope first, so the ordinary open
 * of a fresh, empty ledger for deepseek or gemini read every one of chatgpt's 7,736
 * rows as "not in the snapshot" and deleted them. `runBackfill` then loaded an empty
 * `pending`, returned `queue-empty` — which is a *normal* stop reason — and every
 * alarm tick reported `ran` while fetching nothing, for four hours.
 *
 * This file pins the four things that fix rests on:
 *
 *   1. **the identity** — two platforms that share one scope string cannot see,
 *      count or delete each other's rows;
 *   2. **the carry** — rows written before the platform was part of the key are
 *      moved to the new key once, verified before anything is removed, and a row
 *      whose platform cannot be established from storage is left where it is;
 *   3. **the refusal** — a header recording more ids than the store holds is a named
 *      `ledger-mismatch`, never `queue-empty`;
 *   4. **the repair** — the scope is made listable again exactly once, so the four
 *      hours of silence cannot become a permanent one.
 *
 * Everything here is synthetic: fixture ids, a fixture list response, fixture
 * bodies, an injected clock. No network, no real account, no conversation text.
 */

import { describe, it, expect, afterEach, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import {
  openLedger,
  ownersOfScope,
  recoverLedgerLoss,
} from '../lib/backfill/ledger';
import { carryLegacyDebtRows, readDebtSet, resetDebtDbConnectionForTest } from '../lib/backfill/debt-store';
import {
  BACKFILL_DB_NAME,
  BACKFILL_DB_VERSION,
  DEBTS_LEGACY_STORE,
} from '../lib/backfill/debt-store';
import { popupText, renderPopup, NO_FAILURES } from '../lib/popup-view';
import { stateKey, type BackfillHeader } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const LIST_PATH = '/backend-api/conversations';
const DETAIL_PATH = '/backend-api/conversation/';
const SCOPE = 'default';

/** The pre-W45 store's name and shape, spelled out because the fixture writes it by hand. */
const V1_DB_VERSION = 1;

function fixtureClock(): Clock {
  let t = Date.parse('2026-09-19T00:00:00.000Z');
  return { now: () => t, sleep: async (ms: number) => { t += ms; } };
}

/** A header, with the counts a caller wants it to claim. */
function headerWith(platform: string, scope: string, claimed: Partial<BackfillHeader> = {}): BackfillHeader {
  return {
    v: 2,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: true },
    pendingCount: 0,
    archivedCount: 0,
    detailToday: { day: '2026-09-19', count: 0 },
    halted: null,
    ...claimed,
  };
}

/**
 * 🔴 The fixture writes the **pre-W45** database by hand, at version 1, with the
 *    old `['scope','id']` key path — because that is what is really on a user's
 *    disk. Opening it through the production `openDb` afterwards is what exercises
 *    the version upgrade, and asserting that the old store is still there is what
 *    pins "the upgrade moves no data".
 */
async function seedPreW45Rows(factory: IDBFactory, scope: string, rows: unknown[]): Promise<void> {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const request = factory.open(BACKFILL_DB_NAME, V1_DB_VERSION);
    request.onupgradeneeded = () => {
      const store = request.result.createObjectStore(DEBTS_LEGACY_STORE, { keyPath: ['scope', 'id'] });
      store.createIndex('byScope', 'scope');
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(DEBTS_LEGACY_STORE, 'readwrite');
    for (const row of rows) tx.objectStore(DEBTS_LEGACY_STORE).put(row);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
  db.close();
}

/** What the pre-W45 store holds for one scope: a plain read, so "left untouched" is falsifiable. */
async function readPreW45Rows(factory: IDBFactory, scope: string): Promise<unknown[]> {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const request = factory.open(BACKFILL_DB_NAME, BACKFILL_DB_VERSION);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  if (!db.objectStoreNames.contains(DEBTS_LEGACY_STORE)) {
    db.close();
    return [];
  }
  const rows = await new Promise<unknown[]>((resolve, reject) => {
    const tx = db.transaction(DEBTS_LEGACY_STORE, 'readonly');
    const request = tx.objectStore(DEBTS_LEGACY_STORE).index('byScope').getAll(scope) as IDBRequest<unknown[]>;
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  db.close();
  return rows;
}

function preW45Row(scope: string, id: string, state: 'pending' | 'archived', seq: number) {
  return { scope, id, state, seq };
}

/**
 * A synthetic ChatGPT that hands back `ids` from the first list page and an empty
 * page after it — the only stopping signal the offset-paged plan recognises. Every
 * list request's `offset` is recorded, because "was the list read from the START" is
 * the whole difference between a repair and a re-list that skips the beginning.
 */
function chatgptBackend(ids: string[]) {
  const calls: string[] = [];
  const listOffsets: number[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === LIST_PATH) {
      const offset = Number(u.searchParams.get('offset') ?? '0');
      listOffsets.push(offset);
      const limit = Number(u.searchParams.get('limit') ?? '100');
      const page = ids.slice(offset, offset + limit);
      return {
        status: 200,
        text: JSON.stringify({ items: page.map((id) => ({ id })), limit, offset, total: ids.length }),
      };
    }
    const id = decodeURIComponent(u.pathname.replace(DETAIL_PATH, ''));
    return {
      status: 200,
      text: JSON.stringify({ mapping: { n1: { id: 'n1' } }, current_node: 'n1', account_id: SCOPE, id }),
    };
  };
  return { http, calls, listOffsets };
}

function tick(
  store: BackfillStore,
  http: (url: string) => Promise<HttpResponse>,
  maxDetails: number,
) {
  return runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope: SCOPE,
    store,
    http,
    clock: fixtureClock(),
    // Pacing is other tests' subject; both intervals at 0 keeps this file about identity.
    pace: {
      enumerate: { minIntervalMs: 0, maxPerDay: null },
      detail: { minIntervalMs: 0, maxPerDay: null },
    },
    maxDetails,
    random: () => 0,
    sink: (captured) => ({ saved: true, sessionId: captured.sessionId }),
  });
}

afterEach(() => {
  vi.restoreAllMocks();
  resetDebtDbConnectionForTest();
});

// ===========================================================================
// 1 · One scope string, two platforms
// ===========================================================================
describe('W45-1 · the debt set belongs to (platform, scope)', () => {
  it('🔴 two platforms sharing a scope string keep separate debt sets', async () => {
    const store = memoryStore();

    const mine = await openLedger(store, 'chatgpt', SCOPE);
    expect(mine.ok).toBe(true);
    if (!mine.ok) return;
    await mine.ledger.save({ ...mine.state, pending: ['c-1', 'c-2', 'c-3'] });

    // A second platform whose scope string happens to be the same one. Its own set
    // is empty — and that must be a statement about *its* rows, not about the
    // store as a whole.
    const theirs = await openLedger(store, 'deepseek', SCOPE);
    expect(theirs.ok).toBe(true);
    if (!theirs.ok) return;
    expect(theirs.state.pending).toEqual([]);
    await theirs.ledger.save({ ...theirs.state, pending: ['d-9'] });

    // Both ledgers reopened through the ordinary path, and both still own their own
    // set. Nothing here needs the rows to be deleted for the assertion to fail:
    // reading by scope alone hands one platform the other's ids.
    const reopenedA = await openLedger(store, 'chatgpt', SCOPE);
    const reopenedB = await openLedger(store, 'deepseek', SCOPE);
    expect(reopenedA.ok && reopenedA.state.pending).toEqual(['c-1', 'c-2', 'c-3']);
    expect(reopenedB.ok && reopenedB.state.pending).toEqual(['d-9']);
    expect(reopenedA.ok && reopenedA.state.archived).toEqual([]);
    expect(reopenedB.ok && reopenedB.state.archived).toEqual([]);
  });

  it('🔴 the ordinary open of a fresh, empty ledger does not delete the other platform\'s rows', async () => {
    const store = memoryStore();

    const first = await openLedger(store, 'chatgpt', SCOPE);
    if (!first.ok) throw new Error(first.refusal.detail);
    await first.ledger.save({ ...first.state, pending: ['c-1', 'c-2', 'c-3'] });

    // This is the destructive step, reproduced exactly: open a fresh ledger for
    // another platform at the same scope and persist it. `replaceDebtSet`'s
    // clear-the-scope-first is what used to run here, off `readRows(scope)`.
    const second = await openLedger(store, 'deepseek', SCOPE);
    if (!second.ok) throw new Error(second.refusal.detail);
    await second.ledger.save({ ...second.state, pending: ['d-9'] });

    expect(await readDebtSet('chatgpt', SCOPE)).toMatchObject({ pending: ['c-1', 'c-2', 'c-3'], archived: [] });
    expect(await readDebtSet('deepseek', SCOPE)).toMatchObject({ pending: ['d-9'], archived: [] });

    // And the counts the two ledgers write are their own, not the store's total.
    const headerA = await store.load(stateKey('chatgpt', SCOPE));
    const headerB = await store.load(stateKey('deepseek', SCOPE));
    expect((headerA as BackfillHeader).pendingCount).toBe(3);
    expect((headerB as BackfillHeader).pendingCount).toBe(1);
  });

  it('a settle on one platform leaves the other\'s row for the same id alone', async () => {
    const store = memoryStore();
    const a = await openLedger(store, 'chatgpt', SCOPE);
    if (!a.ok) throw new Error(a.refusal.detail);
    await a.ledger.save({ ...a.state, pending: ['shared-id'] });
    const b = await openLedger(store, 'deepseek', SCOPE);
    if (!b.ok) throw new Error(b.refusal.detail);
    await b.ledger.save({ ...b.state, pending: ['shared-id'] });

    // The same conversation id under two platforms. Settling it on one is a move
    // between that platform's two lists and nothing else.
    await a.ledger.save({ ...a.state, pending: [], archived: ['shared-id'] });

    expect(await readDebtSet('chatgpt', SCOPE)).toMatchObject({ pending: [], archived: ['shared-id'] });
    expect(await readDebtSet('deepseek', SCOPE)).toMatchObject({ pending: ['shared-id'], archived: [] });
  });
});

// ===========================================================================
// 2 · Carrying the rows that predate the platform half of the key
// ===========================================================================
describe('W45-2 · the pre-W45 rows are carried over, once', () => {
  it('🔴 the upgrade moves no data, the carry moves the readable rows, and a re-run is a no-op', async () => {
    const factory = new IDBFactory();
    (globalThis as any).indexedDB = factory;
    const store = memoryStore();
    const scope = 'acct-w45-carry';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, { pendingCount: 3, archivedCount: 1 }));

    const rows = [
      preW45Row(scope, 'p-1', 'pending', 1),
      preW45Row(scope, 'p-2', 'pending', 2),
      preW45Row(scope, 'p-3', 'pending', 3),
      preW45Row(scope, 'a-1', 'archived', 4),
      // Not a shape this build can read: `state` is neither of the two values.
      // It is left exactly where it is, and it is why `left` is 1 rather than 0.
      { scope, id: 'junk-1', state: 'half-fetched', seq: 5 },
    ];
    await seedPreW45Rows(factory, scope, rows);

    const opened = await openLedger(store, 'chatgpt', scope);
    expect(opened.ok).toBe(true);
    if (!opened.ok) throw new Error(opened.refusal.detail);
    // Every readable row is where a run looks for it now, in `seq` order.
    expect(opened.state.pending).toEqual(['p-1', 'p-2', 'p-3']);
    expect(opened.state.archived).toEqual(['a-1']);
    // The unreadable row is neither counted nor deleted.
    expect(await readPreW45Rows(factory, scope)).toEqual([{ scope, id: 'junk-1', state: 'half-fetched', seq: 5 }]);

    // A re-run moves nothing (the target is not empty, so nothing is merged) and a
    // second open is a plain read: no loss, no refusal, same set.
    const again = await carryLegacyDebtRows('chatgpt', scope);
    expect(again).toMatchObject({ moved: 0, left: 1, skipped: 'target-not-empty', failed: null });
    const reopened = await openLedger(store, 'chatgpt', scope);
    expect(reopened.ok).toBe(true);
    if (!reopened.ok) return;
    expect(reopened.state.pending).toEqual(['p-1', 'p-2', 'p-3']);
    expect(await readPreW45Rows(factory, scope)).toHaveLength(1);
  });

  it('🔴 a scope two platforms record is not attributed to either, and the row stays put', async () => {
    const factory = new IDBFactory();
    (globalThis as any).indexedDB = factory;
    const store = memoryStore();
    await store.save(stateKey('chatgpt', SCOPE), headerWith('chatgpt', SCOPE, { pendingCount: 2 }));
    await store.save(stateKey('deepseek', SCOPE), headerWith('deepseek', SCOPE));
    await seedPreW45Rows(factory, SCOPE, [preW45Row(SCOPE, 'p-1', 'pending', 1), preW45Row(SCOPE, 'p-2', 'pending', 2)]);

    const opened = await openLedger(store, 'chatgpt', SCOPE);
    expect(opened.ok).toBe(false);
    if (opened.ok) return;
    // The refusal is about the loss, and its detail says why the rows could not be
    // handed to anybody: the scope has more than one recorded owner.
    expect(opened.refusal.reason).toBe('ledger-mismatch');
    expect(opened.refusal.detail).toContain('2 platforms (chatgpt, deepseek)');
    expect(opened.refusal.detail).toContain('cannot be attributed');
    expect(await readPreW45Rows(factory, SCOPE)).toHaveLength(2);

    // The other platform's ledger is untouched by any of this: its own header says
    // zero and its own store holds zero, which is a genuine agreement.
    const other = await openLedger(store, 'deepseek', SCOPE);
    expect(other.ok).toBe(true);
  });

  it('ownersOfScope reads both record layouts, and the scope is everything after the first colon', () => {
    expect(ownersOfScope(
      ['cs_backfill_v2:chatgpt:default', 'cs_backfill_v2:deepseek:default', 'cs_backfill_v1:gemini:org:with:colons'],
      'default',
    )).toEqual(['chatgpt', 'deepseek']);
    expect(ownersOfScope(['cs_backfill_v2:claude:org:with:colons'], 'org:with:colons')).toEqual(['claude']);
    expect(ownersOfScope([], 'default')).toEqual([]);
  });
});

// ===========================================================================
// 3 · A header that records more than the store holds is a refusal
// ===========================================================================
describe('W45-3 · a header/store disagreement is named, never silence', () => {
  it('🔴 the run refuses with ledger-mismatch instead of reporting queue-empty', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    // The measured shape: a header recording 7,736 ids and a store holding none.
    await store.save(stateKey('chatgpt', SCOPE), headerWith('chatgpt', SCOPE, {
      pendingCount: 7_736,
      archivedCount: 17,
      enumCursor: { offset: 7_736, complete: true },
    }));

    const server = chatgptBackend([]);
    const report = await tick(store, server.http, 10);

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('ledger-mismatch');
    // Not one request, and the refusal names the numbers it was decided from.
    expect(server.calls).toEqual([]);
    expect(report.halted?.detail).toContain('7736 pending and 17 archived');
    expect(report.halted?.detail).toContain('0 in all');
  });

  it('🔴 an empty set whose header agrees is still queue-empty: the refusal is not a general "no debts"', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    await store.save(stateKey('chatgpt', SCOPE), headerWith('chatgpt', SCOPE));

    const server = chatgptBackend([]);
    const report = await tick(store, server.http, 10);
    expect(report.stopped).toBe('queue-empty');
    expect(report.halted).toBeNull();
  });

  it('a store that is AHEAD of the header is not a disagreement: that is just an interrupted run', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    const seeded = await openLedger(store, 'chatgpt', SCOPE);
    if (!seeded.ok) throw new Error(seeded.refusal.detail);
    await seeded.ledger.save({ ...seeded.state, pending: ['c-2'], archived: ['c-1'] });
    // What a kill between the debt write and the header write leaves behind, for a
    // persist that settled one debt: the store advanced, the header did not — it
    // still records the state from before that persist. The two totals are equal,
    // and that is exactly why the check is on the total.
    await store.save(stateKey('chatgpt', SCOPE), headerWith('chatgpt', SCOPE, {
      pendingCount: 2,
      archivedCount: 0,
      enumCursor: { offset: 3, complete: true },
    }));

    const opened = await openLedger(store, 'chatgpt', SCOPE);
    expect(opened.ok).toBe(true);
    if (!opened.ok) return;
    expect(opened.state.pending).toEqual(['c-2']);
    expect(opened.state.archived).toEqual(['c-1']);
  });
});

// ===========================================================================
// 4 · The path back
// ===========================================================================
describe('W45-4 · a wiped scope is made listable again, exactly once', () => {
  it('🔴 the cursor is reset once, the list is read from the start, and it never re-lists again', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    // The state four hours of silence was made of: enumeration finished long ago,
    // a header full of ids, and a debt store with nothing in it. Without the repair
    // this scope can never enumerate again, so it can never refill either.
    await store.save(stateKey('chatgpt', SCOPE), headerWith('chatgpt', SCOPE, {
      pendingCount: 3,
      archivedCount: 2,
      enumCursor: { offset: 5, complete: true },
      // A halt record this repair has no business touching, from a stop that has
      // already expired. It has to still be there afterwards.
      halted: {
        reason: 'transport-error',
        at: Date.parse('2026-09-18T23:00:00.000Z'),
        detail: 'list offset=5: message channel closed before a response was received',
        attempts: 2,
        retryAt: Date.parse('2026-09-18T23:05:00.000Z'),
      },
    }));

    const server = chatgptBackend(['c-1', 'c-2', 'c-3']);

    // Run 1 — the refusal. Nothing is fetched, and the scope is prepared to be
    // listed again in the same breath.
    const refused = await tick(store, server.http, 10);
    expect(refused.stopped).toBe('halted');
    expect(refused.halted?.reason).toBe('ledger-mismatch');
    expect(server.calls).toEqual([]);
    expect(refused.halted?.detail).toContain('enumeration cursor has been reset');

    const repaired = await store.load(stateKey('chatgpt', SCOPE)) as BackfillHeader;
    expect(repaired.enumCursor).toEqual({ offset: 0, complete: false });
    // The repair resets the reading and nothing else: it invents no halt record of
    // its own, and it does not drop one that was already there. The refusal is
    // carried by the run's own report and the tick record, not by editing this.
    expect(repaired.halted).toMatchObject({ reason: 'transport-error', attempts: 2 });
    // The counts are the store's now, and the loss is written down where it survives
    // the refill — a repaired state is not the same thing as a state that was never broken.
    expect(repaired.pendingCount).toBe(0);
    expect(repaired.archivedCount).toBe(0);
    expect(repaired.relisted).toMatchObject({ recorded: 5, held: 0 });

    // Run 2 — the refill. The list really is read from the first page again, and
    // the conversations come back as debts and are worked through.
    const refilled = await tick(store, server.http, 10);
    expect(server.listOffsets[0]).toBe(0);
    expect(server.listOffsets.filter((o) => o === 0)).toHaveLength(1);
    expect(refilled.state.archived).toEqual(['c-1', 'c-2', 'c-3']);
    const afterRefill = await store.load(stateKey('chatgpt', SCOPE)) as BackfillHeader;
    expect(afterRefill.archivedCount).toBe(3);
    // The record of the loss is not erased by the recovery succeeding.
    expect(afterRefill.relisted).toMatchObject({ recorded: 5, held: 0 });

    // Run 3 — and it cannot happen again. The list is never read from the start a
    // second time: the page-0 request this scope made is the one run 2 made, and
    // there is no way to make another without the cursor being reset again.
    const third = await tick(store, server.http, 10);
    expect(third.stopped).toBe('queue-empty');
    expect(server.listOffsets.filter((o) => o === 0)).toEqual([0]);
  });

  it('the repair refuses to touch a scope that does not need it, so it can never become a periodic re-list', async () => {
    const store = memoryStore();
    (globalThis as any).indexedDB = new IDBFactory();
    await store.save(stateKey('chatgpt', SCOPE), headerWith('chatgpt', SCOPE, {
      enumCursor: { offset: 12, complete: true },
    }));

    const recovery = await recoverLedgerLoss(store, 'chatgpt', SCOPE, 1);
    expect(recovery).toMatchObject({ ok: false, why: 'no-loss' });
    const untouched = await store.load(stateKey('chatgpt', SCOPE)) as BackfillHeader;
    expect(untouched.enumCursor).toEqual({ offset: 12, complete: true });
  });
});

// ===========================================================================
// 5 · The popup can say it
// ===========================================================================
describe('W45-5 · the popup reads both halves out', () => {
  it('a re-listed scope says what was lost and that the list is read again', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('chatgpt', SCOPE, {
        pendingCount: 0,
        archivedCount: 3,
        relisted: { at: Date.parse('2026-09-19T04:00:00.000Z'), recorded: 7_753, held: 0 },
      }),
      target: { platform: 'chatgpt', scope: SCOPE },
      failures: NO_FAILURES,
    });
    const out = popupText(view);
    expect(out).toContain('2026-09-19 04:00:00 UTC');
    expect(out).toContain('listed 7753 conversation id(s)');
    expect(out).toContain('held only 0');
    expect(out).toContain('Nothing was deleted from your archive');
  });

  it('a scope that was never re-listed says nothing about it', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('chatgpt', SCOPE, { archivedCount: 3 }),
      target: { platform: 'chatgpt', scope: SCOPE },
      failures: NO_FAILURES,
    });
    expect(popupText(view)).not.toContain('read again from the start');
  });

  it('the tick line for ledger-mismatch is its own sentence, not the generic "stopped" one', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: null,
      target: null,
      failures: NO_FAILURES,
      lastTick: {
        at: Date.parse('2026-09-19T04:00:00.000Z'),
        ran: true,
        reason: 'ran',
        targets: 1,
        stopped: 'halted',
        halted: 'ledger-mismatch',
        detail: 'the header records 7736 pending and 17 archived (7753 in all)',
      },
    });
    const out = popupText(view);
    expect(out).toContain('refused to fetch anything');
    expect(out).toContain('read again from the start');
    expect(out).not.toContain('stopped before it could finish');
  });
});
