/**
 * The debt set's pure-function half: enqueue / settle / count.
 * No I/O and no clock — every "fetch nothing twice" rule lives here, where unit
 * tests can pin it directly.
 */

import type { BackfillState } from './types';

/**
 * Merge the enumerated ids into the debt set.
 * Hard rules: **an archived id is never enqueued again**, and an id already in the
 * debt set is not enqueued twice.
 * Returns the ids that were genuinely new (used to state, in the report, how many
 * rows of this page were new).
 */
export function enqueueDebts(state: BackfillState, ids: readonly string[]): string[] {
  const archived = new Set(state.archived);
  const pending = new Set(state.pending);
  const added: string[] = [];
  for (const id of ids) {
    if (!id || archived.has(id) || pending.has(id)) continue;
    pending.add(id);
    state.pending.push(id);
    added.push(id);
  }
  return added;
}

/** Settle one debt: take it out of the debt set and record it as archived. Settling twice is idempotent. */
export function settleDebt(state: BackfillState, id: string): void {
  const at = state.pending.indexOf(id);
  if (at >= 0) state.pending.splice(at, 1);
  if (!state.archived.includes(id)) state.archived.push(id);
}

/**
 * 🔴 C20 · Take a debt **out of the set without settling it**: nothing goes into archived.
 *
 * There is exactly one caller: the engine, when the sink reports outright that
 * nothing was stored.
 * Its difference from settleDebt is that one line, `state.archived.push` — and
 * that line is the entirety of "the file never hit disk but the debt was struck
 * off anyway".
 * Why take it out of pending at all: **no retry is a product decision**; leaving
 * it at the head of the FIFO would make the next tick spin in place and turn
 * "no retry" into "retry forever".
 * Where it goes instead: the failure list in lib/backfill/failures.ts (the third
 * column of the same ledger).
 */
export function dropDebt(state: BackfillState, id: string): void {
  const at = state.pending.indexOf(id);
  if (at >= 0) state.pending.splice(at, 1);
}

/** The next debt to settle (FIFO: whatever was enumerated first is archived first — predictable behaviour). */
export function nextDebt(state: BackfillState): string | null {
  return state.pending[0] ?? null;
}

export function isArchived(state: BackfillState, id: string): boolean {
  return state.archived.includes(id);
}
