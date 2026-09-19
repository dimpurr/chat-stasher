/**
 * W18 · Where the backfill's state actually lives, and how a run reads and writes it.
 *
 * Two records, deliberately:
 *   · the **header** (`BackfillHeader`) at `storage.local` `cs_backfill_v2:<platform>:<scope>` —
 *     the cursor, the counters, the halt record, the daily cap; small, written as
 *     often as the old whole state was;
 *   · the **debt set** (the conversation ids) in IndexedDB, one record per id —
 *     written only for the ids that actually changed.
 *
 * 🔴 What this module exists to protect: *the whole secret of stop-and-resume* is
 *    that a debt is on disk before it is worked on and off disk the moment it is
 *    settled. That is unchanged. What changed is that doing so no longer rewrites
 *    the 7,391 ids that did not move (see debt-store.ts for the measurements).
 *
 * 🔴 The debt store is the authority for what is owed. The header's `pendingCount`
 *    and `archivedCount` are a convenience for the popup, are re-derived on every
 *    load, and are re-derived *from the store* rather than trusted, so a header
 *    written before an interrupted run can never be the last word.
 */

import type { BackfillStore } from './store';
import {
  applyDebtDiff,
  countHeaderWrite,
  readDebtSet,
  replaceDebtSet,
  type DebtDiff,
  type DebtSetSnapshot,
} from './debt-store';
import {
  BACKFILL_STATE_VERSION,
  headerOf,
  initialState,
  isHeader,
  isLegacyState,
  legacyStateKey,
  stateFrom,
  stateKey,
  type BackfillHeader,
  type BackfillState,
  type HaltReason,
  type LegacyBackfillState,
} from './types';

/** Why a run may not start at all. `state-unreadable` and `storage-unavailable` are different facts — see HaltReason. */
export interface LedgerRefusal {
  reason: HaltReason;
  detail: string;
}

export type LedgerOpen =
  | { ok: true; state: BackfillState; ledger: Ledger }
  | { ok: false; refusal: LedgerRefusal };

/**
 * The write side of one run: it remembers what was last persisted and writes only
 * the difference.
 *
 * 🔴 The diff is computed against the **last persisted** sets, not against a fresh
 *    read, because the whole object of the exercise is to not re-read (or
 *    re-write) 7,391 ids per tick. `nextSeq` is carried across so the FIFO order
 *    of new debts survives a restart.
 */
export class Ledger {
  private pending: Set<string>;
  private archived: Set<string>;
  private nextSeq: number;

  constructor(
    private readonly store: BackfillStore,
    private readonly platform: string,
    private readonly scope: string,
    snapshot: DebtSetSnapshot,
  ) {
    this.pending = new Set(snapshot.pending);
    this.archived = new Set(snapshot.archived);
    this.nextSeq = snapshot.nextSeq;
  }

  /**
   * Persist the header and whatever moved in the debt set.
   *
   * 🔴 **Debt first, header second, and the header is never trusted.** If the
   *    worker is killed between the two, what survives is a debt set that is ahead
   *    of the header's counts — and `loadState` recomputes those counts from the
   *    store, so the stale header costs nothing. The other order would commit a
   *    counter for a settle that did not happen, i.e. spend a day's quota on work
   *    that will be done again.
   *
   * 🔴 A debt store that refuses the write is **not** swallowed: it throws, and the
   *    run ends. That is the pre-W18 behaviour too (`storage.local.set` rejecting
   *    propagated the same way) and it is the only honest one — silently continuing
   *    would mean fetching bodies while the ledger of what is owed stops moving,
   *    i.e. "crawl from scratch on every restart", the failure store.ts warns about.
   *    Returning a boolean the caller might ignore would be a hole that looks like
   *    a return value.
   */
  async save(state: BackfillState): Promise<void> {
    const diff = this.diffAgainst(state);

    if (diff.enqueue.length > 0 || diff.settle.length > 0 || diff.drop.length > 0) {
      const applied = await applyDebtDiff(this.scope, diff, this.nextSeq);
      if (!applied) {
        throw new Error(
          '[chat-stasher] the backfill debt store refused a write; refusing to fetch bodies'
          + ' against a debt set that cannot be updated',
        );
      }
      // Only after the transaction committed: a failed write must not move the
      // baseline, or the next attempt would believe these ids are already down.
      for (const id of diff.enqueue) this.pending.add(id);
      for (const id of diff.settle) {
        this.pending.delete(id);
        this.archived.add(id);
      }
      for (const id of diff.drop) this.pending.delete(id);
      this.nextSeq += diff.enqueue.length + diff.settle.length;
    }

    const header = headerOf(state);
    countHeaderWrite(header);
    await this.store.save(stateKey(this.platform, this.scope), header);
  }

  /**
   * What moved since the last successful `save`, in the order the ids hold in the
   * in-memory set.
   *
   * 🔴 O(ids in memory) per call, with sets on both sides. The earlier draft used
   *    `state.pending.includes(id)` for the drops, which is O(n²) — 55 million
   *    comparisons per persist at this account's size, i.e. the fix would have been
   *    slower than the problem it replaced. Both sides are `Set`s for that reason
   *    and the comment is here so the shape is not "simplified" back.
   */
  private diffAgainst(state: BackfillState): DebtDiff {
    const stillOwed = new Set(state.pending);
    const nowArchived = new Set(state.archived);

    const enqueue: string[] = [];
    for (const id of state.pending) if (!this.pending.has(id)) enqueue.push(id);
    const settle: string[] = [];
    for (const id of state.archived) if (!this.archived.has(id)) settle.push(id);

    const drop: string[] = [];
    for (const id of this.pending) {
      // 🔴 `settleDebt` and `dropDebt` both take an id out of pending, and the two
      //    must not be confused here. An id that left pending and landed in
      //    `archived` was **settled**; one that left pending and landed nowhere was
      //    **dropped**. Reading "gone from pending" as "dropped" would emit a
      //    tombstone for every settle, deleting the very record the settle had
      //    just written — a debt struck off the ledger entirely, which is the
      //    failure this whole file exists to prevent. (It happened: the first
      //    draft of this function lost three settled ids, and the restart test
      //    caught it.)
      if (!stillOwed.has(id) && !nowArchived.has(id)) drop.push(id);
    }
    return { enqueue, settle, drop };
  }
}

/**
 * Write **only** the header, touching no debt record.
 *
 * Two callers, and neither is the engine: the migration, and a test that wants to
 * change a cursor or a counter without pretending the debt set moved. It is
 * exported rather than kept private because doing it from outside by hand is
 * exactly the `store.save(stateKey(...), wholeState)` call this change removed —
 * and that call now lands a record the loader refuses to read.
 *
 * 🔴 The counts are taken from the state's own sets, so the header may not
 *    disagree with itself. It may still lag the debt store (a settle is written
 *    there first); that is fine, because `loadState` recomputes both counts from
 *    the store and never trusts these.
 */
export async function saveHeader(store: BackfillStore, state: BackfillState): Promise<void> {
  const header = headerOf(state);
  countHeaderWrite(header);
  await store.save(stateKey(state.platform, state.scope), header);
}

/**
 * Read one scope's state, migrating the pre-W18 record if that is what is there.
 *
 * The three outcomes are three different facts and are kept apart all the way to
 * the user (CLAUDE.md invariant 1):
 *   · **no record at all** ⇒ nothing has ever been recorded for this scope ⇒ an
 *     empty set, which is a measurement. Nothing is written — a scope that has not
 *     happened yet must not acquire a state record just by being asked about;
 *   · **a readable record** ⇒ its content;
 *   · **a record we cannot read** ⇒ a refusal. Never an empty set, and not one
 *     byte is written: the record is left exactly as it was found.
 */
export async function openLedger(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<LedgerOpen> {
  const rawHeader = await store.load(stateKey(platform, scope));

  if (isHeader(rawHeader)) {
    const snapshot = await readDebtSet(scope);
    if (!snapshot) {
      return {
        ok: false,
        refusal: {
          reason: 'storage-unavailable',
          detail: 'the debt set could not be read from IndexedDB; without it there is no stop-and-resume',
        },
      };
    }
    return {
      ok: true,
      state: stateFrom(rawHeader, snapshot.pending, snapshot.archived),
      ledger: new Ledger(store, platform, scope, snapshot),
    };
  }

  if (rawHeader !== null && rawHeader !== undefined) {
    // Something is at our key and it is not a header we can read. Do not touch it,
    // do not fall back to the legacy key, do not write.
    return {
      ok: false,
      refusal: {
        reason: 'state-unreadable',
        detail: `the saved state at ${stateKey(platform, scope)} could not be read; `
          + 'it has been left untouched and this leg will not run against it',
      },
    };
  }

  return await openFromLegacy(store, platform, scope);
}

/**
 * No v2 record: either there is genuinely nothing, or there is a pre-W18 v1 record
 * to carry over.
 */
async function openFromLegacy(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<LedgerOpen> {
  const legacyKey = legacyStateKey(platform, scope);
  const raw = await store.load(legacyKey);

  if (raw === null || raw === undefined) {
    // No record anywhere: a genuinely fresh scope. Nothing is written here — see
    // the note in openLedger.
    const fresh = initialState(platform, scope);
    return {
      ok: true,
      state: fresh,
      ledger: new Ledger(store, platform, scope, { pending: [], archived: [], nextSeq: 1 }),
    };
  }

  if (!isLegacyState(raw)) {
    return {
      ok: false,
      refusal: {
        reason: 'state-unreadable',
        detail: `the pre-W18 record at ${legacyKey} is not a state this build can read; `
          + 'it has been left untouched and NOT treated as an empty debt set',
      },
    };
  }

  // The legacy record's own platform/scope must agree with the key it was found
  // under, for the same reason the popup insists on it: a set whose identity and
  // whose address disagree is not one to trust with real ids.
  if (raw.platform !== platform || raw.scope !== scope) {
    return {
      ok: false,
      refusal: {
        reason: 'state-unreadable',
        detail: `the pre-W18 record at ${legacyKey} names ${raw.platform}/${raw.scope}; `
          + 'it has been left untouched and NOT treated as an empty debt set',
      },
    };
  }

  return await migrate(store, platform, scope, legacyKey, raw);
}

/**
 * Carry a pre-W18 record over to the new layout.
 *
 * The order is the whole safety argument, and every step before the last one
 * leaves the legacy record exactly where it was, so a failure at any point means
 * the next attempt starts from the same complete record:
 *
 *   1. write the ids into the debt store (one transaction, scope cleared first);
 *   2. **read them back** and require every id, exactly once, in the same order;
 *   3. write the header;
 *   4. read the header back and require it to parse;
 *   5. only now remove the legacy key.
 *
 * 🔴 Step 5 is the only deletion in this file, and it is not the archive being
 *    rotated: every id it held is already in the debt store, verified in step 2.
 *    Before step 2 passes, nothing is deleted at all.
 */
async function migrate(
  store: BackfillStore,
  platform: string,
  scope: string,
  legacyKey: string,
  legacy: LegacyBackfillState,
): Promise<LedgerOpen> {
  const pending = legacy.pending;
  const archived = legacy.archived;
  const refusal = (detail: string): LedgerOpen => ({
    ok: false,
    refusal: { reason: 'state-unreadable', detail },
  });

  const written = await replaceDebtSet(scope, { pending, archived, nextSeq: pending.length + archived.length + 1 });
  if (!written) {
    return refusal(
      `the pre-W18 record at ${legacyKey} could not be carried over (the debt store refused the write); `
      + 'it has been left untouched and NOT treated as an empty debt set',
    );
  }

  const readBack = await readDebtSet(scope);
  if (!readBack) {
    return refusal(
      `the pre-W18 record at ${legacyKey} was written to the debt store but could not be read back; `
      + 'it has been left untouched and NOT treated as an empty debt set',
    );
  }
  const mismatch = compareSets(pending, archived, readBack);
  if (mismatch) {
    return refusal(
      `the pre-W18 record at ${legacyKey} did not survive the move (${mismatch}); `
      + 'it has been left untouched and NOT treated as an empty debt set',
    );
  }

  // The header is built from the *old* record's own fields, so the numbers the
  // popup shows after a migration are the numbers it showed before it.
  const header: BackfillHeader = {
    v: BACKFILL_STATE_VERSION,
    platform,
    scope,
    totalKnown: legacy.totalKnown,
    totalSource: legacy.totalSource,
    enumCursor: legacy.enumCursor,
    pendingCount: pending.length,
    archivedCount: archived.length,
    detailOutcomes: legacy.detailOutcomes,
    detailToday: legacy.detailToday,
    lastFetchAt: legacy.lastFetchAt,
    failures: legacy.failures,
    failuresDropped: legacy.failuresDropped,
    halted: legacy.halted,
  };
  const state = stateFrom(header, readBack.pending, readBack.archived);
  try {
    countHeaderWrite(header);
    await store.save(stateKey(platform, scope), header);
    if (!isHeader(await store.load(stateKey(platform, scope)))) {
      return refusal(
        `the pre-W18 record at ${legacyKey} was moved but the new header did not read back; `
        + 'it has been left untouched and NOT treated as an empty debt set',
      );
    }
  } catch (err) {
    // 🔴 A store that throws here must produce the same **named** outcome as a store
    //    that answers with something unreadable. Letting the exception out would
    //    leave the user with an aborted run and no trace of why, and the record that
    //    could not be moved is exactly the thing they need to be told about.
    return refusal(
      `the pre-W18 record at ${legacyKey} could not be moved into the new layout`
      + ` (${(err as Error).message}); it has been left untouched and NOT treated as an empty debt set`,
    );
  }

  await store.remove(legacyKey);
  return {
    ok: true,
    state,
    ledger: new Ledger(store, platform, scope, readBack),
  };
}

/**
 * 🔴 W36b · **Finish a migration that was interrupted between its header write and
 * its last step.**
 *
 * The one way this build can leave two records for one scope: `migrate` writes the
 * debt set (step 1), reads it back (step 2), writes the header (step 3), reads the
 * header back (step 4) — and then, before step 5, the worker is killed. The scope
 * now has a readable v2 header **and** the pre-W18 key it came from. `openLedger`
 * looks at `stateKey` first and stops there, so it never sees the old key again:
 * the resurrection is permanent, and (since W36b's preflight scans `storage.local`
 * for `cs_backfill_v1:*`) it would also be *re-visited on every tick and every
 * popup open* — paying one `keys()` read and one whole debt-set read, forever, for
 * a record that was already carried over.
 *
 * The removal is safe by the same argument `migrate`'s step 5 rests on, and it is
 * **checked here rather than assumed**: the ids in the debt store must be exactly
 * the ids in the old record — same pending sequence, same archived set — before a
 * byte is deleted. If they are not, nothing is removed and the caller is told why.
 * The comparison costs one debt-set read, but it runs only while an orphan exists,
 * so the state it repairs converges to "no orphan" instead of costing this every
 * tick.
 *
 * 🔴 This is not the archive being rotated. Nothing here is source data: every id
 *    it removes a copy of is already in the debt store, verified first.
 */
export async function completeInterruptedMigration(
  store: BackfillStore,
  platform: string,
  scope: string,
  legacyKey: string,
  legacy: LegacyBackfillState,
): Promise<LedgerRefusal | null> {
  const snapshot = await readDebtSet(scope);
  if (!snapshot) {
    return {
      reason: 'storage-unavailable',
      detail: `the pre-W18 record at ${legacyKey} sits beside a new-layout record whose debt set could `
        + 'not be read from IndexedDB; nothing has been removed',
    };
  }
  const mismatch = compareSets(legacy.pending, legacy.archived, snapshot);
  if (mismatch) {
    return {
      reason: 'state-unreadable',
      detail: `the pre-W18 record at ${legacyKey} sits beside a new-layout record and the two do not agree `
        + `(${mismatch}); it has been left untouched`,
    };
  }
  await store.remove(legacyKey);
  return null;
}

/** `null` when the move was exact: same pending sequence, same archived set. */
function compareSets(
  pending: readonly string[],
  archived: readonly string[],
  readBack: DebtSetSnapshot,
): string | null {
  if (pending.length !== readBack.pending.length) {
    return `${readBack.pending.length} of ${pending.length} pending id(s) came back`;
  }
  for (let i = 0; i < pending.length; i += 1) {
    if (pending[i] !== readBack.pending[i]) return `pending id at position ${i} came back different`;
  }
  const expected = new Set(archived);
  if (expected.size !== archived.length) {
    // The old record itself held a duplicate. Not a reason to lose data, but not a
    // shape to write down as if it were the truth either.
    return `the pre-W18 record held ${archived.length - expected.size} duplicate archived id(s)`;
  }
  if (readBack.archived.length !== expected.size) {
    return `${readBack.archived.length} of ${expected.size} archived id(s) came back`;
  }
  for (const id of readBack.archived) {
    if (!expected.has(id)) return 'an archived id came back that was not in the pre-W18 record';
  }
  return null;
}
