import { describe, it, expect } from 'vitest';
import {
  enqueueDebts,
  settleDebt,
  dropDebt,
  nextDebt,
  isArchived,
} from '../lib/backfill/debts';
import { BACKFILL_STATE_VERSION, type BackfillState } from '../lib/backfill/types';

/**
 * W619 · the debt set's pure functions, pinned directly.
 *
 * `lib/backfill/debts.ts` calls itself the debt set's pure-function
 * half — no I/O and no clock, "where unit tests can pin it directly" —
 * yet its rules had only indirect coverage: `enqueueDebts`' duplicate
 * blocking through `tests/c11-backfill.test.ts`, and `settleDebt`
 * through the engine. This file pins each function synchronously, with
 * the `BackfillState` fixtures built by hand: no store, no clock, no
 * timers, no engine. Synthetic ids only; not one line touches a
 * platform endpoint.
 */

/** A hand-built BackfillState: the debt set's two columns and the fields the type requires. */
function state(pending: string[] = [], archived: string[] = []): BackfillState {
  return {
    v: BACKFILL_STATE_VERSION,
    platform: 'chatgpt',
    scope: 'acct-fixture',
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pending: [...pending],
    archived: [...archived],
    detailToday: { day: '2026-08-17', count: 0 },
    halted: null,
  };
}

describe('W619 · dropDebt (C20): struck off without being settled', () => {
  it('takes the id out of pending and adds nothing to archived', () => {
    const st = state(['a', 'b', 'c']);
    dropDebt(st, 'b');
    expect(st.pending).toEqual(['a', 'c']);
    expect(st.archived).toEqual([]);
    expect(isArchived(st, 'b')).toBe(false);
  });

  it('is the one line that differs from settleDebt: the same removal, no archive column written', () => {
    const dropped = state(['a', 'b']);
    dropDebt(dropped, 'a');
    const settled = state(['a', 'b']);
    settleDebt(settled, 'a');
    // Both took 'a' out of pending...
    expect(dropped.pending).toEqual(settled.pending);
    // ...but only settleDebt wrote the archive line.
    expect(dropped.archived).toEqual([]);
    expect(settled.archived).toEqual(['a']);
  });

  it('an id that is not in pending is a no-op: state unchanged, still not archived', () => {
    const st = state(['a', 'b'], ['c']);
    dropDebt(st, 'not-there');
    expect(st.pending).toEqual(['a', 'b']);
    expect(st.archived).toEqual(['c']);
    expect(isArchived(st, 'not-there')).toBe(false);
  });
});

describe('W619 · settleDebt is idempotent', () => {
  it('settling twice archives once, and the second settle leaves pending unchanged', () => {
    const st = state(['a', 'b', 'c']);
    settleDebt(st, 'b');
    expect(st.pending).toEqual(['a', 'c']);
    expect(st.archived).toEqual(['b']);
    settleDebt(st, 'b');
    expect(st.archived).toEqual(['b']);
    expect(st.pending).toEqual(['a', 'c']);
  });
});

describe('W619 · enqueueDebts: only genuinely new ids', () => {
  it('skips empty-string ids, already-archived ids and already-pending ids, and returns exactly the new ids in order', () => {
    const st = state(['p1', 'p2'], ['done']);
    const added = enqueueDebts(st, ['p1', '', 'done', 'n1', 'p2', 'n2', '']);
    expect(added).toEqual(['n1', 'n2']);
    expect(st.pending).toEqual(['p1', 'p2', 'n1', 'n2']);
    expect(st.archived).toEqual(['done']);
  });
});

describe('W619 · nextDebt is FIFO', () => {
  it('answers whatever was enumerated first, and null for an empty set', () => {
    expect(nextDebt(state())).toBeNull();
    const st = state();
    enqueueDebts(st, ['first', 'second', 'third']);
    expect(nextDebt(st)).toBe('first');
    dropDebt(st, 'first');
    expect(nextDebt(st)).toBe('second');
    settleDebt(st, 'second');
    expect(nextDebt(st)).toBe('third');
  });
});

describe('W619 · isArchived reads the archived column only', () => {
  it('an id still in pending is not archived', () => {
    const st = state(['still-owed'], ['settled']);
    expect(isArchived(st, 'settled')).toBe(true);
    expect(isArchived(st, 'still-owed')).toBe(false);
    expect(isArchived(st, 'never-seen')).toBe(false);
  });
});
