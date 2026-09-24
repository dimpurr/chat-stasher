/**
 * W98 · The re-enumeration marker is generic, one-shot, and carries across a header
 * round-trip.
 *
 * These are the pure-function halves of the W98 change (lib/backfill/ledger.ts):
 * which migrations are due for a scope, and what applying one resets. The engine's
 * use of them is covered end-to-end by w98-claude-requeue.test.ts.
 *
 * 🔴 Red-before-green: `REENUMERATE_MIGRATIONS` / `migrationsDue` /
 *    `applyReenumerations` and the `reenumerated` header field do not exist on `main`.
 */

import { describe, expect, it } from 'vitest';
import { REENUMERATE_MIGRATIONS, applyReenumerations, migrationsDue } from '../lib/backfill/ledger';
import { headerOf, initialState, isHeader, stateFrom, type BackfillHeader } from '../lib/backfill/types';

const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const MIGRATION_ID = 'claude-detail-walk-w92';

/** A complete Claude scope, cursor mid-list, with every other field populated. */
function completeClaudeState() {
  const state = initialState('claude', ORG);
  state.enumCursor = { offset: 7, complete: true, cursor: 42, token: 'opaque', truncated: 'short-page-inferred' };
  state.pending = ['p1-aaaaaaaa', 'p2-aaaaaaaa'];
  state.archived = ['a1-aaaaaaaa'];
  state.parkedEmpty = ['p2-aaaaaaaa'];
  state.emptyStreak = 2;
  state.failures = [];
  state.failuresDropped = 0;
  return state;
}

describe('W98 · the migration table is keyed by platform + id', () => {
  it('carries the Claude detail-walk fix under a stable id, on the Claude platform only', () => {
    expect(REENUMERATE_MIGRATIONS.some((m) => m.id === MIGRATION_ID && m.platform === 'claude')).toBe(true);
    expect(REENUMERATE_MIGRATIONS.every((m) => m.id.length > 0 && m.platform.length > 0)).toBe(true);
  });
});

describe('W98 · migrationsDue decides the one-shot', () => {
  it('a scope with no marker is due; the same scope after the marker is not', () => {
    const state = completeClaudeState();
    expect(migrationsDue(state).map((m) => m.id)).toEqual([MIGRATION_ID]);

    applyReenumerations(state, 1_000);
    expect(migrationsDue(state)).toEqual([]);
  });

  it('another platform is never due for the Claude migration', () => {
    const state = initialState('chatgpt', 'w98-scope');
    state.enumCursor = { offset: 3, complete: true };
    expect(migrationsDue(state)).toEqual([]);
  });

  it('a migration table for another platform is honoured (the mechanism is generic)', () => {
    const state = initialState('chatgpt', 'w98-scope');
    state.enumCursor = { offset: 3, complete: true };
    const due = applyReenumerations(state, 55, [{ id: 'chatgpt-fix-x', platform: 'chatgpt', why: 'synthetic' }]);
    expect(due.map((m) => m.id)).toEqual(['chatgpt-fix-x']);
    expect(state.reenumerated).toEqual({ 'chatgpt-fix-x': 55 });
    expect(state.enumCursor).toEqual({ offset: 0, complete: false });
  });
});

describe('W98 · applyReenumerations resets only the cursor', () => {
  it('resets the cursor to the first page, clearing truncated / cursor / token, and touches nothing else', () => {
    const state = completeClaudeState();
    const pending = [...state.pending];
    const archived = [...state.archived];
    const parked = [...(state.parkedEmpty ?? [])];

    const applied = applyReenumerations(state, 12_345);

    expect(applied.map((m) => m.id)).toEqual([MIGRATION_ID]);
    expect(state.enumCursor).toEqual({ offset: 0, complete: false });
    expect(state.reenumerated?.[MIGRATION_ID]).toBe(12_345);
    // The debt ledger and the empty-body guard are untouched: this is a re-listing,
    // not a repair of a lost debt set.
    expect(state.pending).toEqual(pending);
    expect(state.archived).toEqual(archived);
    expect(state.parkedEmpty).toEqual(parked);
    expect(state.emptyStreak).toBe(2);
  });

  it('is idempotent: a second call applies nothing and leaves the cursor where it was', () => {
    const state = completeClaudeState();
    applyReenumerations(state, 1);
    // Pretend the ordinary run advanced the cursor after the reset.
    state.enumCursor = { offset: 50, complete: false };

    expect(applyReenumerations(state, 2)).toEqual([]);
    expect(state.enumCursor).toEqual({ offset: 50, complete: false });
    expect(state.reenumerated?.[MIGRATION_ID]).toBe(1);
  });
});

describe('W98 · the marker survives the header round-trip, and an absent field reads as {}', () => {
  it('headerOf / stateFrom carry the marker', () => {
    const state = completeClaudeState();
    state.reenumerated = { [MIGRATION_ID]: 999 };
    const header = headerOf(state);
    expect(header.reenumerated).toEqual({ [MIGRATION_ID]: 999 });
    expect(stateFrom(header, state.pending, state.archived).reenumerated).toEqual({ [MIGRATION_ID]: 999 });
  });

  it('a pre-W98 header without the field reads as {} and is still a valid header', () => {
    const legacy: BackfillHeader = {
      v: 2,
      platform: 'claude',
      scope: ORG,
      totalKnown: null,
      totalSource: 'unknown',
      enumCursor: { offset: 9, complete: true },
      pendingCount: 0,
      archivedCount: 0,
      detailToday: { day: '2026-09-24', count: 0 },
      halted: null,
    };
    expect(isHeader(legacy)).toBe(true);
    expect(stateFrom(legacy, [], []).reenumerated).toEqual({});
    expect(migrationsDue(stateFrom(legacy, [], []))).toHaveLength(1);
  });
});
