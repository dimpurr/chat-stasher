/** W427 · Accessible coverage chart labels name the platform and what the counts mean. */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { buildCoverage, localMonthKey, type CoverageInput, type CoverageScopeInput } from '../lib/coverage';
import { coverageView } from '../lib/coverage-view';
import { initialState, type BackfillHeader } from '../lib/backfill/types';

const NOW = Date.UTC(2026, 8, 24, 12, 0, 0);

function fixture(totalKnown: number | null = null): CoverageInput {
  const platform = 'chatgpt';
  const scope = 'default';
  const initial = initialState(platform, scope);
  const header: BackfillHeader = {
    ...initial,
    v: 2,
    platform,
    scope,
    totalKnown,
    totalSource: totalKnown === null ? 'unknown' : 'response-total',
    enumCursor: { offset: 3, complete: false },
    pendingCount: 2,
    archivedCount: 1,
    detailToday: { day: '2026-09-24', count: 0 },
    halted: null,
  };
  const inputScope: CoverageScopeInput = {
    platform,
    scope,
    header,
    debt: { pending: ['pending-a', 'pending-b'], archived: ['stored-a'], times: new Map() },
    registered: true,
    skippedReason: null,
  };
  return {
    scopes: [inputScope],
    enabled: true,
    hostPaused: false,
    presetRaw: 'gentle',
    tick: null,
    now: NOW,
    monthKey: localMonthKey,
  };
}

beforeEach(() => {
  vi.stubGlobal('browser', withI18n({} as never));
});

describe('W427 · coverage chart accessibility', () => {
  it('names the platform and count meaning while retaining the partial count wording', () => {
    const [card] = coverageView(buildCoverage(fixture()), NOW).cards;
    expect(card).toBeDefined();
    expect(card!.barAccessibleLabel).toContain('chatgpt');
    expect(card!.barAccessibleLabel).toContain('conversation counts');
    expect(card!.barAccessibleLabel).toContain('captured in full 1');
    expect(card!.barAccessibleLabel).toContain('still owed 2');
    expect(card!.listed).toContain('≥ 3');
  });

  it('keeps unknown totals, labelled estimates, and response-total remainder wording', () => {
    const card = coverageView(buildCoverage(fixture()), NOW).cards[0]!;
    expect(card.detailRows.map((row) => row.value).join(' ')).toContain('does not provide a total');
    expect(card.eta).toContain('estimate:');
    expect(card.legend.some((entry) => entry.tone === 'remainder')).toBe(false);
  });

  it('keeps a platform-provided remainder explicitly partial in the accessible label', () => {
    const card = coverageView(buildCoverage(fixture(9)), NOW).cards[0]!;
    expect(card.bar.remainder).toBe(6);
    expect(card.barAccessibleLabel).toContain('chatgpt conversation counts');
    expect(card.barAccessibleLabel).toContain("the platform's total says more exist beyond the ones listed so far 6");
  });
});
