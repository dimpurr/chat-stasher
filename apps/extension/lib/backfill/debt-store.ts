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
 *
 * 🔴 **W45 · The key is `(platform, scope, id)`, and it used to be `(scope, id)`.**
 *    That missing platform destroyed a real account's debt set: three platforms
 *    share the scope string `default`, and `replaceDebtSet` makes one scope hold a
 *    snapshot, so opening a fresh ledger for a second platform at that scope read
 *    the first platform's 7,736 ids as deletions and removed every one of them. The
 *    depth of the fix is in `replaceDebtSet`'s own note; the shape of it is here,
 *    in the key path, because the key is the only thing that decides which records
 *    a write may delete. Rows written before that change are carried over by
 *    `carryLegacyDebtRows`, and a row whose platform cannot be established from
 *    what is on disk is left exactly where it is.
 */

/**
 * One conversation's place in the ledger. `seq` is what makes the pending order a FIFO that survives a restart.
 *
 * 🔴 **W45 · `platform` used to be missing, and that omission destroyed a real
 *    account's debt set.** The record was keyed and indexed by `scope` alone, and
 *    the scope string is the *account* axis — on the measured machine three
 *    platforms shared the scope `default`. `replaceDebtSet` makes one scope hold a
 *    snapshot, so the ordinary open of a fresh, empty ledger for deepseek or gemini
 *    read chatgpt's 7,736 rows as "not in the snapshot" and deleted every one of
 *    them. A debt set belongs to a (platform, scope) pair; a key that cannot say so
 *    cannot keep two of them apart, and the difference was 7,736 lost ids.
 */
export interface DebtRecord {
  platform: string;
  scope: string;
  id: string;
  state: 'pending' | 'archived';
  /** Monotonic within a (platform, scope). A debt settled and later re-enqueued takes a fresh one, i.e. goes to the back. */
  seq: number;
}

export const BACKFILL_DB_NAME = 'chat-stasher-backfill';
/**
 * 🔴 1 → 2 in W45: the debt store's key grew the platform. A version bump is the
 *    only way to change an IndexedDB key path, and there is deliberately **no data
 *    movement inside `onupgradeneeded`** — a version-change transaction cannot ask
 *    `storage.local` which platform an old row belonged to, and a migration that
 *    guessed would be the same bug with a new name. See DEBTS_LEGACY_STORE.
 */
export const BACKFILL_DB_VERSION = 2;
/** The W45 store. Keyed by (platform, scope, id). */
export const DEBTS_STORE = 'debts_by_platform';
/**
 * 🔴 W45 · **The pre-W45 store, kept exactly where it is.**
 *
 * It is named `debts` because that is the name it already had on disk: an
 * IndexedDB store cannot be renamed, so the new layout had to take a new name and
 * leave this one alone. Nothing writes here any more; the only reader is
 * `carryLegacyDebtRows`, and the only writer of its records was the W18 migration.
 *
 * 🔴 Why its rows are not simply moved during the upgrade: a row here holds
 *    `{scope, id, state, seq}` and **no platform**. The upgrade transaction cannot
 *    see `storage.local`, which is the only thing that records "platform P holds a
 *    debt set at scope S", so any platform it stamped on a row would be invented.
 *    Instead the rows stay put until a caller who *can* read that evidence asks for
 *    them — and a row whose platform cannot be established from what is stored stays
 *    here, unread, uncounted and undeleted, for as long as that is true.
 */
export const DEBTS_LEGACY_STORE = 'debts';
/**
 * One index on `(platform, scope)`, and nothing cleverer.
 *
 * 🔴 It is deliberately **not** a `['platform','scope','state','seq']` index read
 *    through an `IDBKeyRange`. `IDBKeyRange` is a global of its own, separate
 *    from `indexedDB`, and a test environment that installs a factory without
 *    also installing the range constructor makes every ranged read throw —
 *    which, inside a `catch` that turns failures into "could not be read", would
 *    have looked exactly like an empty debt set. `getAll([platform, scope])`
 *    takes a plain array key, no range object at all, and `lib/outbox.ts` reads
 *    its store the same way. The order is restored in memory by `seq` (a few
 *    thousand numbers, sorted once per run).
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
          // 🔴 `DEBTS_LEGACY_STORE` is deliberately **not** touched here, and not
          //    because it was forgotten: see its own note. Creating this store next
          //    to it is the whole of the upgrade.
          const store = db.createObjectStore(DEBTS_STORE, { keyPath: ['platform', 'scope', 'id'] });
          store.createIndex(DEBTS_INDEX, ['platform', 'scope']);
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

/**
 * Raw records from one store's index, or `null` when they cannot be read at all.
 * `missing` decides the meaning of an absent store: see `legacyDebtRowCount`.
 */
async function readIndexAll<T>(
  storeName: string,
  indexName: string,
  key: IDBValidKey,
  missing: 'null' | 'empty',
): Promise<T[] | null> {
  const db = await openDb();
  if (!db) return null;
  if (!db.objectStoreNames.contains(storeName)) return missing === 'empty' ? [] : null;
  try {
    const tx = db.transaction(storeName, 'readonly');
    return await requestToPromise(
      tx.objectStore(storeName).index(indexName).getAll(key) as IDBRequest<T[]>,
    );
  } catch {
    // A transaction that fails part way through is "could not be read", not "empty".
    return null;
  }
}

/** One (platform, scope)'s raw records, or `null` when the store cannot be read at all. */
function readRows(platform: string, scope: string): Promise<DebtRecord[] | null> {
  return readIndexAll<DebtRecord>(DEBTS_STORE, DEBTS_INDEX, [platform, scope], 'null');
}

/**
 * The whole debt set for one (platform, scope), pending first in FIFO order.
 * 🔴 `null` (never an empty snapshot) when the store cannot be read.
 */
export async function readDebtSet(platform: string, scope: string): Promise<DebtSetSnapshot | null> {
  const rows = await readRows(platform, scope);
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
  platform: string,
  scope: string,
  diff: DebtDiff,
  nextSeq: number,
): Promise<boolean> {
  const db = await openDb();
  if (!db) return false;

  const puts: DebtRecord[] = [];
  let seq = nextSeq;
  for (const id of diff.enqueue) {
    puts.push({ platform, scope, id, state: 'pending', seq: seq++ });
  }
  for (const id of diff.settle) {
    // A settled id takes a fresh `seq`: order only ever mattered for ids that are
    // still owed, and the id is leaving `pending` for good. `settleDebt` is
    // idempotent, so settling one twice cannot resurrect a `seq` that is already
    // gone.
    puts.push({ platform, scope, id, state: 'archived', seq: seq++ });
  }
  // An id that is enqueued *and* dropped inside one persist is owed again — the
  // enqueue is the later fact, so it must not be deleted by the drop's tombstone.
  // (Not reachable from the engine's tick order today; written down because the
  // other order silently loses the debt, and a lost debt is what this file is for.)
  const owed = new Set(diff.enqueue);
  const deletes: Array<[string, string, string]> = diff.drop
    .filter((id) => !owed.has(id))
    .map((id) => [platform, scope, id]);

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
 * Replace one (platform, scope)'s whole debt set, in one transaction. The migration's write.
 *
 * 🔴 Clears the (platform, scope) first, so a half-finished earlier attempt cannot
 *    leave records behind that the new set does not mention. `readDebtSet` is the
 *    caller's verification step; this function makes no promise about content.
 *
 * 🔴 W45 · "Clears the (platform, scope)" is the whole of the fix and it is worth
 *    reading twice, because the same sentence with `scope` alone was the defect:
 *    the clear is computed from `readRows(platform, scope)`, i.e. from the rows of
 *    **this pair and nothing else**. Before W45 it read `readRows(scope)`, so
 *    opening an empty ledger for a second platform at the same scope made every row
 *    of the first platform's set a deletion.
 */
export async function replaceDebtSet(
  platform: string,
  scope: string,
  snapshot: DebtSetSnapshot,
): Promise<boolean> {
  const db = await openDb();
  if (!db) return false;

  const records: DebtRecord[] = [];
  let seq = 1;
  for (const id of snapshot.pending) records.push({ platform, scope, id, state: 'pending', seq: seq++ });
  for (const id of snapshot.archived) records.push({ platform, scope, id, state: 'archived', seq: seq++ });

  try {
    const existing = await readRows(platform, scope);
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
    const deletions: Array<[string, string, string]> = [];
    for (const row of existing) {
      const want = wanted.get(row.id);
      if (!want || want.state !== row.state || want.seq !== row.seq) deletions.push([platform, scope, row.id]);
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

// ---------------------------------------------------------------------------
// W45 · Carrying the pre-platform rows over
// ---------------------------------------------------------------------------

/** Why nothing was carried. `null` on the outcome means "it was carried", not "fine". */
export type LegacyCarrySkip =
  /** This database has no pre-W45 store, or nothing in it for this scope. Nothing to do. */
  | 'no-legacy-rows'
  /**
   * The (platform, scope) already holds rows. The pre-W45 rows are **left alone**:
   * merging two sets would have to invent which `seq` wins, and the old rows carry
   * no evidence that they are the same set as the new ones rather than a stale
   * copy of it.
   */
  | 'target-not-empty';

export interface LegacyCarryOutcome {
  /** Rows now recorded under (platform, scope). */
  moved: number;
  /** Pre-W45 rows left exactly where they were. See `failed` for the two reasons. */
  left: number;
  skipped: LegacyCarrySkip | null;
  /**
   * Non-null when the move could not be finished or **could not be verified**. When
   * this is set, not one pre-W45 row was deleted — the whole point of reading the
   * new records back before removing the old ones is that a failed move leaves the
   * original in place.
   */
  failed: string | null;
}

/**
 * Is this one pre-W45 row a shape this build can carry?
 *
 * The row was written by the W18 migration, so its fields should be
 * `{scope, id, state, seq}`. Nothing guarantees it: the store is a storage-layer
 * object that any earlier build — or anything else living in this database — could
 * have written. A row that fails here is **not** repaired, not guessed at and not
 * deleted; it is counted in `left` and stays where it is.
 */
function readableLegacyRow(
  value: unknown,
  platform: string,
  scope: string,
): { key: IDBValidKey; record: DebtRecord } | null {
  if (!value || typeof value !== 'object') return null;
  const row = value as Partial<DebtRecord> & { scope?: unknown };
  if (typeof row.id !== 'string' || row.id === '') return null;
  if (row.state !== 'pending' && row.state !== 'archived') return null;
  if (typeof row.seq !== 'number' || !Number.isFinite(row.seq)) return null;
  // The key is the record's own, built from the scope it was found under — this
  // store's key path is `[scope, id]`. A row whose `scope` is not the string we
  // queried by cannot be named, so it is left rather than deleted blind.
  if (row.scope !== scope) return null;
  return { key: [scope, row.id], record: { platform, scope, id: row.id, state: row.state, seq: row.seq } };
}

/**
 * 🔴 W45 · **How many pre-W45 rows still sit under this scope.** `null` = could not
 *   be read, which is a different fact from `0`.
 *
 * It is a `count()` and not a read on purpose: its one caller asks the question on
 * a path that may repeat on every tick while a scope is in the `ledger-mismatch`
 * state, and paying for a full `getAll` of a 7,700-row set to learn that there are
 * none would be the same shape of waste the W36b note warns about.
 */
export async function legacyDebtRowCount(scope: string): Promise<number | null> {
  const db = await openDb();
  if (!db) return null;
  // No pre-W45 store at all is a measurement — this database never had one, so it
  // holds no such rows — and not "could not be read".
  if (!db.objectStoreNames.contains(DEBTS_LEGACY_STORE)) return 0;
  try {
    const tx = db.transaction(DEBTS_LEGACY_STORE, 'readonly');
    return await requestToPromise(
      tx.objectStore(DEBTS_LEGACY_STORE).index(DEBTS_INDEX).count(scope) as IDBRequest<number>,
    );
  } catch {
    return null;
  }
}

/**
 * 🔴 W45 · **Carry the pre-W45 rows under `scope` to `(platform, scope)`.**
 *
 * The caller has already established that `platform` is the scope's **only**
 * recorded owner (see `ownersOfScope` in lib/backfill/ledger.ts). That is not a
 * detail of this function's implementation, it is the whole of its evidence: a
 * pre-W45 row holds `{scope, id, state, seq}` and no platform, so the platform has
 * to come from somewhere else on disk, and the only somewhere else is the record of
 * which platform holds that scope. When two platforms record the same scope string,
 * no row under it can be attributed and none is moved.
 *
 * The order is the W18 migration's, step for step, and it is the whole safety
 * argument:
 *
 *   1. read the pre-W45 rows for this scope;
 *   2. write the readable ones under `(platform, scope)`, in one transaction;
 *   3. **read them back** and require every one, with the same state and `seq`;
 *   4. only then delete exactly the rows that were verified.
 *
 * A failure before step 4 deletes nothing, so the next attempt starts from the same
 * complete set. Step 4's deletions are `delete [scope, id]` over the keys that came
 * back, never "clear the scope" — a row that could not be read is not a row that may
 * be removed.
 *
 * `null` means the database would not open; every other outcome is reported in
 * `LegacyCarryOutcome`, `failed` included, so a caller never has to read a `null`
 * as "there was nothing to do".
 */
export async function carryLegacyDebtRows(
  platform: string,
  scope: string,
): Promise<LegacyCarryOutcome | null> {
  const db = await openDb();
  if (!db) return null;
  if (!db.objectStoreNames.contains(DEBTS_LEGACY_STORE)) {
    return { moved: 0, left: 0, skipped: 'no-legacy-rows', failed: null };
  }

  let legacy: unknown[];
  try {
    const tx = db.transaction(DEBTS_LEGACY_STORE, 'readonly');
    legacy = await requestToPromise(
      tx.objectStore(DEBTS_LEGACY_STORE).index(DEBTS_INDEX).getAll(scope) as IDBRequest<unknown[]>,
    );
  } catch {
    return null;
  }
  if (legacy.length === 0) return { moved: 0, left: 0, skipped: 'no-legacy-rows', failed: null };

  const existing = await readRows(platform, scope);
  if (existing === null) {
    return { moved: 0, left: legacy.length, skipped: null, failed: 'the new debt set could not be read' };
  }
  if (existing.length > 0) {
    return { moved: 0, left: legacy.length, skipped: 'target-not-empty', failed: null };
  }

  const readable: Array<{ key: IDBValidKey; record: DebtRecord }> = [];
  for (const row of legacy) {
    const parsed = readableLegacyRow(row, platform, scope);
    if (parsed) readable.push(parsed);
  }
  const left = legacy.length - readable.length;

  try {
    const tx = db.transaction(DEBTS_STORE, 'readwrite');
    const store = tx.objectStore(DEBTS_STORE);
    for (const { record } of readable) store.put(record);
    await txDone(tx);
  } catch {
    return { moved: 0, left: legacy.length, skipped: null, failed: 'the debt store refused the write' };
  }

  const readBack = await readRows(platform, scope);
  if (readBack === null) {
    return { moved: 0, left: legacy.length, skipped: null, failed: 'the carried rows could not be read back' };
  }
  const back = new Map(readBack.map((r) => [r.id, r]));
  for (const { record } of readable) {
    const got = back.get(record.id);
    if (!got || got.state !== record.state || got.seq !== record.seq) {
      // 🔴 No id in the detail, not even a prefix: this string is persisted (the
      //    header, the tick record) and a conversation id is the one thing the
      //    privacy rule keeps out of a stored trace. The count says as much as the
      //    diagnosis needs.
      return { moved: 0, left: legacy.length, skipped: null, failed: 'a carried row came back different' };
    }
  }

  try {
    const tx = db.transaction(DEBTS_LEGACY_STORE, 'readwrite');
    const store = tx.objectStore(DEBTS_LEGACY_STORE);
    for (const { key } of readable) store.delete(key);
    await txDone(tx);
  } catch {
    // The rows are in **both** stores now. That is recoverable — the carry is a
    // `put` of the same `(platform, scope, id)` and this function refuses to run
    // over a non-empty target — and deleting nothing is the side to fail on.
    return {
      moved: readable.length,
      left,
      skipped: null,
      failed: 'the carried rows could not be removed from the pre-W45 store',
    };
  }

  for (const { record } of readable) countWrite('debt', record);
  for (const { key } of readable) countWrite('debt', key);
  return { moved: readable.length, left, skipped: null, failed: null };
}
