/** Sparse identity and chip boundaries in the coverage view model. */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { buildCoverage, type CoverageInput, type CoverageRow, type CoverageScopeInput } from '../lib/coverage';
import { chipOf, hueKeyOf, monogramOf } from '../lib/coverage-view';
import { initialState } from '../lib/backfill/types';
import { applyUiLocale } from '../lib/i18n';
import { catalogFetch, withI18n } from './i18n-harness';

const NOW = Date.UTC(2026, 8, 24, 12, 0, 0);

function scope(platform: string): CoverageScopeInput {
  return {
    platform,
    scope: 'default',
    header: { ...initialState(platform, 'default'), pendingCount: 1, archivedCount: 0 },
    debt: { pending: ['synthetic-pending'], archived: [], times: new Map() },
    registered: true,
    skippedReason: null,
  };
}

function reportFor(platform: string) {
  const input: CoverageInput = {
    scopes: [scope(platform)],
    enabled: true,
    hostPaused: false,
    presetRaw: 'gentle',
    tick: null,
    now: NOW,
  };
  return buildCoverage(input);
}

beforeEach(async () => {
  vi.stubGlobal('browser', withI18n({} as never));
  vi.stubGlobal('fetch', catalogFetch());
  await applyUiLocale('auto');
});

describe('coverage view sparse identity', () => {
  it('uses a question mark when the platform identifier is empty', () => {
    const row = reportFor('').rows[0]!;

    expect(monogramOf(row.platform)).toBe('?');
    expect(hueKeyOf(row.platform)).toBe('default');
  });

  it('uses the neutral hue for an unknown platform name while keeping its initial monogram', () => {
    const row = reportFor('synthetic-platform').rows[0]!;

    expect(hueKeyOf(row.platform)).toBe('default');
    expect(monogramOf(row.platform)).toBe('S');
  });
});

describe('coverage status chip sparse boundary', () => {
  it('does not claim retrying for a halted row without a halt record', () => {
    const row = { ...reportFor('synthetic-platform').rows[0]!, state: 'halted', halt: null } as CoverageRow;

    expect(chipOf(row)).toEqual({ word: 'stopped', tone: 'bad' });
  });

  it('keeps the localized label paired with the retry tone when a transient halt is present', async () => {
    const row = reportFor('synthetic-platform').rows[0]!;
    row.state = 'halted';
    row.halt = {
      reason: 'rate-limited',
      detail: 'synthetic rate limit',
      at: NOW,
      retryAt: NOW + 60_000,
    };

    expect(chipOf(row)).toEqual({ word: 'retrying', tone: 'wait' });

    await applyUiLocale('zh_CN');
    expect(chipOf(row)).toEqual({ word: '重试中', tone: 'wait' });
  });
});
