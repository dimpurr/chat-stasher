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
 *
 * 🔴 **W45 · "The debt store is the authority" has one exception, and it is the
 *    interesting one.** The store is not merely the authority on *what is owed*; it
 *    is also the thing that can be **wrong about having any debts at all**. A store
 *    that has lost rows answers `0` in exactly the shape a genuinely empty store
 *    does, and reading that as `queue-empty` is "an unknown recorded as empty" at
 *    the top of the leg — measured, on a real machine, as four hours of alarm ticks
 *    reporting `ran` while fetching nothing. So the header is still not trusted for
 *    the counts, but it *is* read as a second witness to them: when it records more
 *    ids than the store holds, the two disagree about a fact they are both supposed
 *    to know, and that is a named refusal (`ledger-mismatch`) rather than an empty
 *    queue. See `openHeaderLedger` and `recoverLedgerLoss`.
 */

import type { BackfillStore } from './store';
import {
  applyDebtDiff,
  carryLegacyDebtRows,
  countHeaderWrite,
  legacyDebtRowCount,
  readDebtSet,
  replaceDebtSet,
  type DebtDiff,
  type DebtSetSnapshot,
  type LegacyCarryOutcome,
} from './debt-store';
import {
  BACKFILL_STATE_VERSION,
  LEGACY_STATE_VERSION,
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
      const applied = await applyDebtDiff(this.platform, this.scope, diff, this.nextSeq);
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
 * 🔴 W98 · **A versioned, one-time re-enumeration of a platform-scope, for a ledger
 * that predates a parser fix.**
 *
 * The problem this exists to solve, measured: a detail parser that refused real
 * bodies still enumerated the list and then **dropped** each id whose body it
 * refused (`dropDebt`, lib/backfill/debts.ts). The enumeration cursor is
 * consequently `complete`, so nothing ever reads the list again, and a dropped id
 * is never retried — a permanent hole in backfill coverage. Fixing the parser does
 * not, by itself, bring those ids back: the list has to be read once more.
 *
 * So a build that carries a parser fix declares a migration, and the first run of
 * any scope on that platform whose ledger predates it resets that scope's cursor
 * and records the migration as done. The reset is deliberately the **same two
 * fields** `recoverLedgerLoss` resets, and nothing else:
 *   · `enumCursor = { offset: 0, complete: false }` (this also clears `truncated`
 *     and any cursor/token — every paging mode's own "read from the first page");
 *   · the migration id is recorded on the header (`reenumerated`).
 * `pending`, `archived`, `parkedEmpty`, `emptyStreak`, `failures` and any halt are
 * untouched: this is not a repair for a lost debt set, it is a re-listing.
 *
 * 🔴 **It cannot become "re-list everything on every tick".** The marker is written
 *    in the same header write as the cursor reset, and `migrationsDue` reads it, so
 *    a scope that has run the migration is never due again — independent of what
 *    the re-listing itself finds or how it ends. The only way a second run happens
 *    is a **different** migration id (a later parser fix), which is the point of
 *    keying by id rather than by a boolean.
 *
 * 🔴 **Nothing is duplicated and no archived body is refetched, and that is not a
 *    property of this function — it is `enqueueDebts`'.** Re-listing hands the same
 *    ids back to `enqueueDebts`, which skips any id already in `pending` **or**
 *    `archived` (lib/backfill/debts.ts:16-27), so only the ids the old walk dropped
 *    come back. That is the deliberate division of labour: this module resets a
 *    cursor, and the debt set remains the authority on what is owed.
 *
 * 🔴 **Generic by construction.** The table is keyed by platform + migration id; the
 *    platform half is checked against the header the record lives on, so a future
 *    fix on another platform only adds a row and needs no change to the engine or to
 *    the header shape. Only Claude uses it today. `why` is for the reader of this
 *    file; it is never logged or shown (a migration id is not a fact about a
 *    conversation, but only the id and the platform are ever named in a log line).
 */
export interface ReenumerateMigration {
  /** Stable id recorded in the header once this migration has run for a scope. A later fix uses a new id. */
  readonly id: string;
  /** The platform whose scopes this applies to. The marker itself is stored per (platform, scope) on the header. */
  readonly platform: string;
  /** Why this platform's ledger must be listed once more. Documentation only; never logged. */
  readonly why: string;
}

export const REENUMERATE_MIGRATIONS: readonly ReenumerateMigration[] = [
  {
    id: 'claude-detail-walk-w92',
    platform: 'claude',
    why: 'the pre-W92 Claude detail walk refused every real body (an absent branch-root parent was read as '
      + 'a broken chain) and each refused id was dropped, while enumeration stayed complete; re-list once so '
      + 'enqueueDebts can bring back the ids that are in neither pending nor archived.',
  },
];

/**
 * The migrations that apply to this scope and have **not** run here yet.
 *
 * The whole one-shot property lives on this comparison: a migration id present in
 * `state.reenumerated` is never returned, and the field is persisted with the same
 * header write that resets the cursor (see `applyReenumerations`).
 */
export function migrationsDue(
  state: BackfillState,
  migrations: readonly ReenumerateMigration[] = REENUMERATE_MIGRATIONS,
): ReenumerateMigration[] {
  const done = state.reenumerated ?? {};
  return migrations.filter((migration) => migration.platform === state.platform && done[migration.id] === undefined);
}

/**
 * Apply every migration due for this scope: reset the enumeration cursor and record
 * each id, in memory. The caller persists the header (the engine does, once, before
 * the enumeration loop) — this function performs no I/O, like the rest of the debt
 * bookkeeping, so the one-shot decision is a pure function a unit test can pin.
 *
 * Returns the migrations applied, so the caller can say in a log line which one ran
 * (id + platform only).
 */
export function applyReenumerations(
  state: BackfillState,
  at: number,
  migrations: readonly ReenumerateMigration[] = REENUMERATE_MIGRATIONS,
): ReenumerateMigration[] {
  const due = migrationsDue(state, migrations);
  if (due.length === 0) return [];
  const done: Record<string, number> = { ...(state.reenumerated ?? {}) };
  for (const migration of due) done[migration.id] = at;
  state.reenumerated = done;
  state.enumCursor = { offset: 0, complete: false };
  return due;
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
 *
 * 🔴 W45 adds a fourth: **a readable record that disagrees with the debt store**.
 *    A header recording ids the store no longer holds is a *loss*, and it is the
 *    state that produced four hours of "ran, nothing happened" on a real machine.
 *    It gets its own reason (`ledger-mismatch`), never `queue-empty` — see
 *    `openHeaderLedger`.
 */
export async function openLedger(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<LedgerOpen> {
  const rawHeader = await store.load(stateKey(platform, scope));

  if (isHeader(rawHeader)) {
    return await openHeaderLedger(store, platform, scope, rawHeader);
  }

  const unreadable = unreadableStateRefusal(rawHeader, platform, scope);
  if (unreadable) {
    // Something is at our key and it is not a header we can read. Do not touch it,
    // do not fall back to the legacy key, do not write.
    return { ok: false, refusal: unreadable };
  }

  return await openFromLegacy(store, platform, scope);
}

/**
 * 🔴 W47 · **"What sits at this scope's key is not a record this build can read",
 * decided once, for two callers that must not disagree.**
 *
 * Why it is a function rather than the two lines it replaces, and why it takes the
 * already-loaded value rather than the store:
 *
 *  · `openLedger` asks it and refuses the run — that is the decision the whole
 *    engine's safety rests on;
 *  · the alarm tick's preflight (`findUnreadableState`, lib/backfill/alarm.ts) asks
 *    the *same* question on a tick that will never reach `openLedger`, because the
 *    gates stopped it first (no open platform page — the ordinary state of a
 *    laptop). Until W47 that tick reported `no-http-port` and the unreadable record
 *    was named by nothing at all, so `state.halted` being null in storage did not
 *    mean no refusal had happened.
 *
 * 🔴 One decision, two readers: a second copy of "is this a header?" would be free
 *    to drift, and the drift would be silent in the safe-looking direction — the
 *    preflight would name a refusal the engine would not have raised, or miss one
 *    it would. The wording of the refusal is shared with it for the same reason.
 *
 * It **writes nothing** and does not open the debt store: it is the one question
 * that can be answered from a single `storage.local` read, which is what makes it
 * affordable on every tick.
 */
export function unreadableStateRefusal(
  raw: unknown,
  platform: string,
  scope: string,
): LedgerRefusal | null {
  if (raw === null || raw === undefined) return null;
  if (isHeader(raw)) return null;
  return {
    reason: 'state-unreadable',
    detail: `the saved state at ${stateKey(platform, scope)} could not be read; `
      + 'it has been left untouched and this leg will not run against it',
  };
}

// ---------------------------------------------------------------------------
// W45 · A header that records debts the store does not hold
// ---------------------------------------------------------------------------

/**
 * Ids the header recorded and the store no longer holds.
 *
 * 🔴 **The compared quantity is the total, not `pending` or `archived` separately,
 *    and that is not a simplification — comparing either one alone is wrong.**
 *
 * `Ledger.save` writes the debt store first and the header second, so a worker
 * killed between the two leaves a header one persist behind the store. Walk a
 * single settle through that window: the store's `pending` drops by one and its
 * `archived` gains one, while the header still records the old, larger `pending`
 * count. So `store.pending < header.pendingCount` is the **ordinary shape of an
 * interrupted run** — and a check on `pending` alone would refuse every clean
 * restart, which is exactly what the first draft of this function did to
 * `tests/w18-state-split.test.ts`'s kill tests. A quiescent enqueue shows the
 * mirror image: there `store.pending` is *ahead* of the header. Neither direction
 * of either count works.
 *
 * The total does, because **a debt only ever leaves this ledger by `drop`**, which
 * takes an id out of `pending` and into neither list — and `drop` is a decision a
 * run makes about a conversation whose body was just fetched and refused, one per
 * persist (`runBackfill`'s loop persists immediately after each). Everything else
 * (enqueue, settle) leaves the total alone or raises it. So a header recording more
 * ids than the store holds is not something an interrupted run can produce at any
 * size, and a deficit of thousands is not something it can produce at all.
 *
 * 🔴 One consequence is deliberately accepted rather than smoothed over with a
 *    tolerance of one: a crash between a drop's two writes can present as a
 *    one-id deficit and be refused. It costs a re-listing that re-enqueues exactly
 *    that one conversation, because `archived` survives — and the alternative, a
 *    threshold, would mean a genuine loss is only reported once it is large enough
 *    to cross an arbitrary line. The project's rule is that the store is the
 *    authority; when the header contradicts it, the leg says so.
 */
export interface DebtLoss {
  /** Ids the header recorded, pending + archived. */
  recorded: number;
  /** Ids the store actually holds for this (platform, scope), pending + archived. */
  held: number;
  /** `recorded - held`. Always ≥ 1 when this is returned. */
  missing: number;
}

export function debtLossBetween(header: BackfillHeader, snapshot: DebtSetSnapshot): DebtLoss | null {
  const recorded = header.pendingCount + header.archivedCount;
  const held = snapshot.pending.length + snapshot.archived.length;
  if (held >= recorded) return null;
  return { recorded, held, missing: recorded - held };
}

/**
 * 🔴 W45 · **Which platforms `storage.local` says hold a debt set at this scope.**
 *
 * This is the evidence that lets a pre-W45 row be attributed at all, and it is the
 * only such evidence there is: the row itself is `{scope, id, state, seq}`, and the
 * scope string is not a platform. Both record layouts count — a `cs_backfill_v2:`
 * header and a `cs_backfill_v1:` record each say "this platform holds a debt set at
 * this scope".
 *
 * 🔴 **A rule about every platform, not a special case for one scope string.** It
 *    does not know or care that `default` is the sentinel a target carries before
 *    its organization is resolved; it says that a scope recorded by more than one
 *    platform is *ambiguous*, which is true for any scope string a future platform
 *    might share. That is the property the key change exists to restore, and this
 *    function is where it is read back out for rows that predate it.
 *
 * The platform is one path segment and the scope is everything after it, for the
 * same reason `legacyScopeFromKey` splits it that way: a scope is an account or
 * organization identifier and may itself contain a colon.
 */
export function ownersOfScope(keys: readonly string[], scope: string): string[] {
  const prefixes = [`cs_backfill_v${BACKFILL_STATE_VERSION}:`, `cs_backfill_v${LEGACY_STATE_VERSION}:`];
  const owners = new Set<string>();
  for (const key of keys) {
    for (const prefix of prefixes) {
      if (!key.startsWith(prefix)) continue;
      const rest = key.slice(prefix.length);
      const separator = rest.indexOf(':');
      if (separator < 1) continue;
      if (rest.slice(separator + 1) === scope) owners.add(rest.slice(0, separator));
    }
  }
  // Sorted so a detail string built from this is stable across runs and assertable.
  return [...owners].sort();
}

/** What a carry attempt did, or why it did not happen. `blocked` is a fragment for the refusal's detail. */
interface CarryAttempt {
  outcome: LegacyCarryOutcome | null;
  blocked: string | null;
}

/**
 * Try to move this scope's pre-W45 rows into `(platform, scope)`, but only when the
 * platform can be established from storage.
 *
 * Every branch that cannot establish it returns with the rows untouched. Handing
 * them to whichever platform happened to open first is exactly the mistake W45
 * exists to undo — and it would be worse here than it was there, because this code
 * would be doing it deliberately, to rows it can see.
 */
async function carryLegacyInto(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<CarryAttempt> {
  const rows = await legacyDebtRowCount(scope);
  if (rows === null) {
    return { outcome: null, blocked: 'the pre-W45 debt store could not be read' };
  }
  if (rows === 0) return { outcome: null, blocked: null };

  let keys: string[];
  try {
    keys = await store.keys();
  } catch (err) {
    return { outcome: null, blocked: `storage.local could not be listed (${(err as Error).message})` };
  }
  const owners = ownersOfScope(keys, scope);
  if (owners.length !== 1 || owners[0] !== platform) {
    return {
      outcome: null,
      blocked: owners.length === 0
        ? `no platform records the scope ${scope}`
        : `${owners.length} platforms (${owners.join(', ')}) record the scope ${scope}, so ${rows} pre-W45 id(s) cannot be attributed to one of them`,
    };
  }
  return { outcome: await carryLegacyDebtRows(platform, scope), blocked: null };
}

/**
 * The header path: a readable header, the debt store, and the disagreement that
 * must never be read as an empty queue.
 *
 * The carry attempt comes **before** the judgement, and only when the store is
 * behind. That ordering matters in both directions: a machine upgrading from the
 * pre-W45 layout has every id still in the old store, so judging first would refuse
 * every healthy scope on first open; and the check that gates the carry
 * (`legacyDebtRowCount`) is a `count()`, so the ordinary open of a normal scope does
 * not pay for a `storage.local` listing it has no use for.
 */
async function openHeaderLedger(
  store: BackfillStore,
  platform: string,
  scope: string,
  header: BackfillHeader,
): Promise<LedgerOpen> {
  const unavailable = (): LedgerOpen => ({
    ok: false,
    refusal: {
      reason: 'storage-unavailable',
      detail: 'the debt set could not be read from IndexedDB; without it there is no stop-and-resume',
    },
  });

  let snapshot = await readDebtSet(platform, scope);
  if (!snapshot) return unavailable();
  let loss = debtLossBetween(header, snapshot);

  let attempt: CarryAttempt = { outcome: null, blocked: null };
  if (loss) {
    attempt = await carryLegacyInto(store, platform, scope);
    if (attempt.outcome && attempt.outcome.moved > 0) {
      const again = await readDebtSet(platform, scope);
      if (!again) return unavailable();
      snapshot = again;
      loss = debtLossBetween(header, snapshot);
    }
  }

  if (loss) {
    return {
      ok: false,
      refusal: { reason: 'ledger-mismatch', detail: debtLossDetail(platform, scope, header, snapshot, loss, attempt) },
    };
  }
  return {
    ok: true,
    state: stateFrom(header, snapshot.pending, snapshot.archived),
    ledger: new Ledger(store, platform, scope, snapshot),
  };
}

/** The refusal's own words: the four numbers it was decided from, and what was tried before saying so. */
function debtLossDetail(
  platform: string,
  scope: string,
  header: BackfillHeader,
  snapshot: DebtSetSnapshot,
  loss: DebtLoss,
  attempt: CarryAttempt,
): string {
  const parts = [
    `the header at ${stateKey(platform, scope)} records ${header.pendingCount} pending and`
    + ` ${header.archivedCount} archived (${loss.recorded} in all), and the debt store holds`
    + ` ${snapshot.pending.length} and ${snapshot.archived.length} (${loss.held} in all) —`
    + ` ${loss.missing} recorded conversation id(s) are not there`,
  ];
  if (attempt.blocked) {
    parts.push(`pre-W45 rows for this scope could not be carried over: ${attempt.blocked}`);
  } else if (attempt.outcome) {
    const { moved, left, skipped, failed } = attempt.outcome;
    parts.push(
      failed
        ? `carrying the pre-W45 rows failed (${failed}); ${left} row(s) were left where they were`
        : `carried ${moved} pre-W45 row(s) for this scope, ${left} left unreadable`
          + (skipped ? ` (${skipped})` : ''),
    );
  }
  return parts.join('; ');
}

/**
 * 🔴 W45 · **The path back for a scope whose debts were destroyed.**
 *
 * Why it is needed at all: a wiped scope is not merely empty. Its `enumCursor` is
 * `complete`, so the enumeration will never run again for it — there is nothing left
 * that could refill it, and the leg would sit at zero pending forever while the
 * header went on claiming thousands. Resetting `enumCursor` is the whole of the
 * repair: `runBackfill` reads the list from the first page again on the next tick,
 * `enqueueDebts` writes the ids back, and the debt set is rebuilt.
 *
 * 🔴 **It cannot become "re-list everything on every tick".** Everything that made
 *    the scope un-listable is written in the *same* header write: the cursor is
 *    reset and the counts are taken from the store, so after this call the header no
 *    longer records a single id the store does not hold — the condition that calls
 *    this function is gone, and it cannot fire again until rows are destroyed again.
 *    There is no marker to forget to clear and no retry ladder to advance. The dead
 *    state is repaired once because the repaired state is not the dead state.
 *
 * 🔴 What it does **not** do, and why the header says so out loud: the ids are gone,
 *    so `archived` is gone with them. A conversation that was already archived cannot
 *    be told from one that was never fetched, and re-listing will enqueue it again —
 *    one duplicate body fetch, not a lost conversation. Saying that is the difference
 *    between a repair and a cover-up, which is why `relisted` is written into the
 *    header where the popup can read it rather than only into a log line.
 *
 * The header is **read back** before this returns, like every other write in this
 * file: a caller that reports "reset, the next run will re-list" on the strength of
 * a `storage.local.set` that silently did nothing would be wrong in exactly the way
 * this module exists to prevent.
 */
export type LedgerRecovery =
  | { ok: true; loss: DebtLoss; state: BackfillState }
  | { ok: false; why: 'no-loss' | 'state-unreadable' | 'storage-unavailable' | 'not-written'; detail: string };

export async function recoverLedgerLoss(
  store: BackfillStore,
  platform: string,
  scope: string,
  at: number,
): Promise<LedgerRecovery> {
  const rawHeader = await store.load(stateKey(platform, scope));
  if (!isHeader(rawHeader)) {
    return {
      ok: false,
      why: 'state-unreadable',
      detail: `the header at ${stateKey(platform, scope)} could not be read, so its enumeration cursor cannot be reset`,
    };
  }
  const snapshot = await readDebtSet(platform, scope);
  if (!snapshot) {
    return {
      ok: false,
      why: 'storage-unavailable',
      detail: 'the debt set could not be read from IndexedDB, so the counts to write back are unknown',
    };
  }
  const loss = debtLossBetween(rawHeader, snapshot);
  if (!loss) {
    // Already consistent. Doing nothing is the whole of the one-shot guarantee: a
    // second call cannot reset a cursor that the first call's write already made
    // consistent, so this can never become a periodic re-list.
    return { ok: false, why: 'no-loss', detail: 'the header and the debt store already agree; nothing was reset' };
  }

  const state = stateFrom(rawHeader, snapshot.pending, snapshot.archived);
  // 🔴 Two fields and one addition, and nothing else — `stateFrom` carried the rest
  //    over, including any halt record that was already there. Clearing that halt
  //    would be this repair reaching outside its job: it is not this function's
  //    record to drop, and a transient one carries the streak the next run is
  //    counting. The `truncated` mark goes with the cursor it belonged to: it says
  //    "we could not read past here", which stops being true the moment the reading
  //    starts again from the beginning.
  state.enumCursor = { offset: 0, complete: false };
  state.relisted = { at, recorded: loss.recorded, held: loss.held };

  try {
    await saveHeader(store, state);
    const back = await store.load(stateKey(platform, scope));
    if (!isHeader(back) || back.enumCursor.offset !== 0 || back.enumCursor.complete !== false) {
      return {
        ok: false,
        why: 'not-written',
        detail: 'the header was written but did not read back with the cursor reset; nothing else was changed',
      };
    }
  } catch (err) {
    return { ok: false, why: 'not-written', detail: (err as Error).message };
  }
  return { ok: true, loss, state };
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

  const written = await replaceDebtSet(platform, scope, {
    pending,
    archived,
    nextSeq: pending.length + archived.length + 1,
  });
  if (!written) {
    return refusal(
      `the pre-W18 record at ${legacyKey} could not be carried over (the debt store refused the write); `
      + 'it has been left untouched and NOT treated as an empty debt set',
    );
  }

  const readBack = await readDebtSet(platform, scope);
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
  const snapshot = await readDebtSet(platform, scope);
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
