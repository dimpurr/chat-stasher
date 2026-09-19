/**
 * W18 · The debt set's own store: one record per conversation id.
 *
 * Why this exists at all: `cs_backfill_v1:<platform>:<scope>` held the whole debt
 * set, and `persist` rewrote all of it after every list page and after every
 * settled/failed debt. On the real account this task was measured against that is
 * 7,391 pending ids ≈ 296 KB rewritten to move one id — tens of MB of LevelDB
 * churn a day, and a slow `storage.local.set` on every tick.
 *
 * 🔴 **The whole point is that settling one debt touches one record.** With the set
 *    laid out per id, a settle is one `delete` + one `put` inside **one readwrite
 *    transaction**, and IndexedDB's own atomicity is what makes "killed between the
 *    two writes of a settle" impossible to observe: either the transaction
 *    committed (the debt is settled, exactly once) or it did not (the debt is
 *    still pending, and the next run fetches it again). Neither outcome loses a
 *    debt.
 *
 * Storage: IndexedDB, for the same reason `lib/outbox.ts` uses it — `storage.local`
 * is a key/value bag that is rewritten whole, and this is a set of tens of
 * thousands of small records that are read and written one at a time. The
 * manifest already carries `unlimitedStorage`, so the browser will not evict it.
 *
 * 🔴 Reads obey the same rule as the outbox's: `null` means **could not be read**,
 *    never "empty". An unknown must never be recorded as empty (CLAUDE.md
 *    invariant 1), and here the temptation is a single `?? []`.
 */

/** One conversation's place in the ledger. `seq` is what makes the pending order a FIFO that survives a restart. */
export interface DebtRecord {
  scope: string;
  id: string;
  state: 'pending' | 'archived';
  /** Monotonic within a scope. A debt settled and later re-enqueued takes a fresh one, i.e. goes to the back. */
  seq: number;
}

export const BACKFILL_DB_NAME = 'chat-stasher-backfill';
export const BACKFILL_DB_VERSION = 1;
export const DEBTS_STORE = 'debts';
/**
 * One index on `scope`, and nothing cleverer.
 *
 * 🔴 It is deliberately **not** a compound `['scope','state','seq']` index read
 *    through an `IDBKeyRange`. `IDBKeyRange` is a global of its own, separate
 *    from `indexedDB`, and a test environment that installs a factory without
 *    also installing the range constructor makes every ranged read throw —
 *    which, inside a `catch` that turns failures into "could not be read", would
 *    have looked exactly like an empty debt set. `getAll(scope)` takes a plain
 *    key, no range object at all, and `lib/outbox.ts` reads its store the same
 *    way. The order is restored in memory by `seq` (a few thousand numbers,
 *    sorted once per run).
 */
export const DEBTS_INDEX = 'byScope';

/** A whole scope's debt set, in FIFO order. */
export interface DebtSetSnapshot {
  pending: string[];
  archived: string[];
  /** One past the highest `seq` seen — where the next addition starts. */
  nextSeq: number;
}

/** What one `persist` wants the store to become. Both lists are ids; `drop` and `settle` differ in where the id lands. */
export interface DebtDiff {
  /** Newly owed. */
  enqueue: string[];
  /** Settled: out of pending, into archived. */
  settle: string[];
  /** Out of pending, into neither (a sink that reported `saved:false` — see debts.ts dropDebt). */
  drop: string[];
}

// ---------------------------------------------------------------------------
// Test-only write accounting
// ---------------------------------------------------------------------------

/**
 * A pessimistic stand-in for the per-record framing both storage layers add
 * around a value (LevelDB journal records, IndexedDB record headers). It is
 * counted once per record written on **both** sides of the before/after
 * comparison, so it cannot be what makes the ratio look good — and the ratio is
 * ~3 orders of magnitude, so a constant of this size cannot hide a regression.
 */
export const WRITE_RECORD_OVERHEAD = 64;

export interface WriteStats {
  /** How many distinct writes reached a storage layer. */
  operations: number;
  /** Payload bytes handed over, plus `WRITE_RECORD_OVERHEAD` per record. */
  bytes: number;
  /** Same total, split by destination, so a report can say where the bytes went. */
  bytesByStore: Record<'debt' | 'header', number>;
}

let stats: WriteStats = { operations: 0, bytes: 0, bytesByStore: { debt: 0, header: 0 } };

/** Snapshot of the counter. Never reset by anything except `resetWriteStatsForTest`. */
export function writeStats(): WriteStats {
  return { operations: stats.operations, bytes: stats.bytes, bytesByStore: { ...stats.bytesByStore } };
}

export function resetWriteStatsForTest(): void {
  stats = { operations: 0, bytes: 0, bytesByStore: { debt: 0, header: 0 } };
}

/**
 * The byte count of one write, as the storage layer would see it.
 * 🔴 This is "bytes we hand over", not "bytes the engine writes to disk": the real
 *    cost also includes the store's own journal framing, which is why
 *    `WRITE_RECORD_OVERHEAD` is added here rather than ignored.
 */
function byteLength(value: unknown): number {
  return new TextEncoder().encode(JSON.stringify(value) ?? 'null').byteLength;
}

/**
 * The byte count of one value as a storage layer would see it — the exact measure
 * the counter above uses.
 *
 * 🔴 Exported so a test can put the **before** on the same ruler as the after: the
 *    pre-W18 layout's cost of one persist is "the size of the whole state", and
 *    measuring that with a different function than the new side would make the
 *    comparison meaningless. It is the same arithmetic, by construction.
 */
export function serializedBytes(value: unknown): number {
  return byteLength(value) + WRITE_RECORD_OVERHEAD;
}

function countWrite(where: 'debt' | 'header', value: unknown): void {
  const bytes = byteLength(value) + WRITE_RECORD_OVERHEAD;
  stats.operations += 1;
  stats.bytes += bytes;
  stats.bytesByStore[where] += bytes;
}

/**
 * The header's write goes through `BackfillStore.save`, which is a port this module
 * does not own — so the ledger calls this before handing the value over. Exported
 * for that one caller and for the tests that need the same measure of the old
 * whole-state write.
 */
export function countHeaderWrite(value: unknown): void {
  countWrite('header', value);
}

// ---------------------------------------------------------------------------
// IndexedDB plumbing (the same shape as lib/outbox.ts)
// ---------------------------------------------------------------------------

function idbFactory(): IDBFactory | null {
  const g = globalThis as { indexedDB?: IDBFactory };
  return g.indexedDB ?? null;
}

/**
 * 🔴 The cache remembers **which factory** it was opened from, and re-opens when
 *    that changes. In the browser there is one factory for the life of the
 *    context, so this is a plain memo. Under test it is not: a suite that models
 *    "a brand-new, empty database per case" swaps `globalThis.indexedDB` for a
 *    fresh `IDBFactory`, and a cache keyed on nothing would keep handing back a
 *    connection to the previous test's database — i.e. a test would read another
 *    test's debts and the failure would look like a logic bug.
 */
let dbPromise: { factory: IDBFactory; opened: Promise<IDBDatabase | null> } | null = null;

/**
 * Open the debt database, or answer `null` for "it would not open".
 *
 * 🔴 W36b · **A failed open is not remembered.** The memo above used to cache
 *    whatever the first attempt produced, `null` included — so one refused open
 *    (a blocked upgrade, a factory that threw, a browser that had not finished
 *    starting IndexedDB) became this worker's answer for the rest of its life.
 *    Downstream that is `readDebtSet() === null` ⇒ `openLedger` refuses with
 *    `storage-unavailable`, and in the migration that means the pre-W18 record is
 *    never carried over: **every later attempt re-reads the same cached `null`**,
 *    the old key keeps its ids, and the only trace is a refusal whose cause was
 *    one transient open. "Could not be read just now" is not "cannot be read"
 *    (CLAUDE.md invariant 1), so only a **successful** open is kept; a failure is
 *    retried by the next caller.
 *
 * 🔴 A `blocked` open answers `null` rather than waiting forever (an upgrade held
 *    by another connection has no bound, and a promise that never settles would
 *    hang the caller just as silently). The retry above is what makes that safe:
 *    once the other connection goes away, the next call opens normally.
 */
function openDb(): Promise<IDBDatabase | null> {
  const factory = idbFactory();
  if (factory && dbPromise?.factory === factory) return dbPromise.opened;
  if (!factory) {
    dbPromise = null;
    return Promise.resolve(null);
  }
  const entry = {
    factory,
    opened: new Promise<IDBDatabase | null>((resolve) => {
      let request: IDBOpenDBRequest;
      try {
        request = factory.open(BACKFILL_DB_NAME, BACKFILL_DB_VERSION);
      } catch {
        resolve(null);
        return;
      }
      request.onupgradeneeded = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains(DEBTS_STORE)) {
          const store = db.createObjectStore(DEBTS_STORE, { keyPath: ['scope', 'id'] });
          store.createIndex(DEBTS_INDEX, 'scope');
        }
      };
      request.onsuccess = () => resolve(request.result);
      // A database that will not open is a named refusal for the caller, never a silent "there were no debts".
      request.onerror = () => resolve(null);
      request.onblocked = () => resolve(null);
    }),
  };
  dbPromise = entry;
  void entry.opened.then((db) => {
    // Only the entry this call installed may be dropped: a later call that already
    // re-opened (a different factory) must not have its memo cleared from here.
    if (db === null && dbPromise === entry) dbPromise = null;
  });
  return entry.opened;
}

/**
 * Only for tests that model a service-worker restart inside one process — and for
 * the case where a suite keeps the same factory but wants a connection that did
 * not already exist. Most suites do not need it: swapping `globalThis.indexedDB`
 * invalidates the cache by itself (see the note on `dbPromise`).
 */
export function resetDebtDbConnectionForTest(): void {
  dbPromise = null;
}

/** Does this context have an IndexedDB API at all? Different question from "can the debt set be read". */
export function debtStoreApiPresent(): boolean {
  return idbFactory() !== null;
}

function requestToPromise<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error('IndexedDB request failed'));
  });
}

function txDone(tx: IDBTransaction): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error ?? new Error('IndexedDB transaction failed'));
    tx.onabort = () => reject(tx.error ?? new Error('IndexedDB transaction aborted'));
  });
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/** One scope's raw records, or `null` when the store cannot be read at all. */
async function readRows(scope: string): Promise<DebtRecord[] | null> {
  const db = await openDb();
  if (!db) return null;
  try {
    const tx = db.transaction(DEBTS_STORE, 'readonly');
    return await requestToPromise(
      tx.objectStore(DEBTS_STORE).index(DEBTS_INDEX).getAll(scope) as IDBRequest<DebtRecord[]>,
    );
  } catch {
    // A transaction that fails part way through is "could not be read", not "empty".
    return null;
  }
}

/**
 * The whole debt set for one scope, pending first in FIFO order.
 * 🔴 `null` (never an empty snapshot) when the store cannot be read.
 */
export async function readDebtSet(scope: string): Promise<DebtSetSnapshot | null> {
  const rows = await readRows(scope);
  if (rows === null) return null;
  // One scope's rows, in `seq` order — this is the FIFO order the debt set had in
  // memory, restored from the only thing that can restore it across a restart. A
  // tie is broken by id so the order is total and a re-read of an unchanged store
  // cannot shuffle it.
  const ordered = [...rows].sort((a, b) => (a.seq - b.seq) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  let maxSeq = 0;
  for (const row of ordered) if (row.seq > maxSeq) maxSeq = row.seq;
  return {
    pending: ordered.filter((r) => r.state === 'pending').map((r) => r.id),
    archived: ordered.filter((r) => r.state === 'archived').map((r) => r.id),
    nextSeq: maxSeq + 1,
  };
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/**
 * Apply one persist's worth of change.
 *
 * 🔴 **One transaction.** The enqueues, the settles and the drops of a single
 *    `persist` commit together or not at all, and a settle is a delete from
 *    `pending` plus a put into `archived` inside that same transaction — so there
 *    is no observable state in which a debt has been struck off but not recorded
 *    as archived. Doing these as separate writes is what would open the window the
 *    task asks about.
 *
 * 🔴 `nextSeq` is threaded through so the FIFO order is stable: the new pending
 *    ids are numbered in the order the caller lists them, which is the order they
 *    hold in the in-memory debt set.
 */
export async function applyDebtDiff(
  scope: string,
  diff: DebtDiff,
  nextSeq: number,
): Promise<boolean> {
  const db = await openDb();
  if (!db) return false;

  const puts: DebtRecord[] = [];
  let seq = nextSeq;
  for (const id of diff.enqueue) {
    puts.push({ scope, id, state: 'pending', seq: seq++ });
  }
  for (const id of diff.settle) {
    // A settled id takes a fresh `seq`: order only ever mattered for ids that are
    // still owed, and the id is leaving `pending` for good. `settleDebt` is
    // idempotent, so settling one twice cannot resurrect a `seq` that is already
    // gone.
    puts.push({ scope, id, state: 'archived', seq: seq++ });
  }
  // An id that is enqueued *and* dropped inside one persist is owed again — the
  // enqueue is the later fact, so it must not be deleted by the drop's tombstone.
  // (Not reachable from the engine's tick order today; written down because the
  // other order silently loses the debt, and a lost debt is what this file is for.)
  const owed = new Set(diff.enqueue);
  const deletes: Array<[string, string]> = diff.drop
    .filter((id) => !owed.has(id))
    .map((id) => [scope, id]);

  try {
    const tx = db.transaction(DEBTS_STORE, 'readwrite');
    const store = tx.objectStore(DEBTS_STORE);
    for (const record of puts) store.put(record);
    for (const key of deletes) store.delete(key);
    await txDone(tx);
  } catch {
    return false;
  }
  for (const record of puts) countWrite('debt', record);
  for (const key of deletes) countWrite('debt', key);
  return true;
}

/**
 * Replace one scope's whole debt set, in one transaction. The migration's write.
 *
 * 🔴 Clears the scope first, so a half-finished earlier attempt cannot leave
 *    records behind that the new set does not mention. `readDebtSet` is the
 *    caller's verification step; this function makes no promise about content.
 */
export async function replaceDebtSet(scope: string, snapshot: DebtSetSnapshot): Promise<boolean> {
  const db = await openDb();
  if (!db) return false;

  const records: DebtRecord[] = [];
  let seq = 1;
  for (const id of snapshot.pending) records.push({ scope, id, state: 'pending', seq: seq++ });
  for (const id of snapshot.archived) records.push({ scope, id, state: 'archived', seq: seq++ });

  try {
    const existing = await readRows(scope);
    if (existing === null) return false;

    // 🔴 Written as a **difference**, and that is not an optimisation for its own
    //    sake. The obvious "delete the scope's keys, then write the set" costs one
    //    `delete` request per existing record, and a re-run of an interrupted
    //    migration therefore issues 7,600 of them for a set it is about to write
    //    back byte for byte. Measured under fake-indexeddb that is 93 seconds for
    //    this fixture; the browser is faster but it is the same shape of waste.
    //    Comparing first makes a re-run cost nothing and touches exactly the
    //    records that genuinely differ — which is also a stronger statement about
    //    what this function does: it makes the scope hold `snapshot`, and does not
    //    care how it got there.
    const wanted = new Map(records.map((r) => [r.id, r]));
    const deletions: Array<[string, string]> = [];
    for (const row of existing) {
      const want = wanted.get(row.id);
      if (!want || want.state !== row.state || want.seq !== row.seq) deletions.push([scope, row.id]);
    }
    const present = new Map(existing.map((r) => [r.id, r]));
    const writes = records.filter((r) => {
      const had = present.get(r.id);
      return !had || had.state !== r.state || had.seq !== r.seq;
    });

    if (deletions.length > 0 || writes.length > 0) {
      const tx = db.transaction(DEBTS_STORE, 'readwrite');
      const store = tx.objectStore(DEBTS_STORE);
      for (const key of deletions) store.delete(key);
      for (const record of writes) store.put(record);
      await txDone(tx);
    }
    // Counted from what was really written, not from the size of the set — a
    // no-op re-run has to measure as a no-op.
    for (const key of deletions) countWrite('debt', key);
    for (const record of writes) countWrite('debt', record);
  } catch {
    return false;
  }
  return true;
}
