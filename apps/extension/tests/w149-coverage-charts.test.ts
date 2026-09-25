/**
 * W149 · the chart geometry the coverage page draws, pinned.
 *
 * Pure numbers, so these assertions are about honesty rather than rendering:
 * a segment may not appear for a count of zero, a ring may not appear for a
 * percentage the model refused to produce, the unknown-time column may never
 * leak into the months, and an all-zero row has no shape at all — the caller
 * owes that case a sentence, not an empty drawing.
 */

import { describe, it, expect } from 'vitest';
import { barGeometry, monthLabel, monthsGeometry, ringDash, unknownTimeTotal } from '../lib/coverage-charts';

describe('W149 · barGeometry', () => {
  it('splits the width in proportion to the counts, in a fixed order', () => {
    const out = barGeometry({ archived: 3, owed: 1, failed: 0, remainder: 0 }, 80);
    expect(out).toEqual([
      { tone: 'archived', x: 0, w: 60 },
      { tone: 'owed', x: 60, w: 20 },
    ]);
  });

  it('🔴 drops zero-count segments, so no sliver can read as a measurement', () => {
    expect(barGeometry({ archived: 0, owed: 5, failed: 0, remainder: 0 }, 100))
      .toEqual([{ tone: 'owed', x: 0, w: 100 }]);
  });

  it('🔴 an all-zero row has no geometry at all, whatever the store situation', () => {
    expect(barGeometry({ archived: 0, owed: 0, failed: 0, remainder: 0 }, 100)).toEqual([]);
  });

  it('a known total beyond the rows listed so far is its own remainder segment', () => {
    const out = barGeometry({ archived: 1, owed: 1, failed: 0, remainder: 2 }, 100);
    expect(out.map((s) => [s.tone, Math.round(s.w)])).toEqual([
      ['archived', 25],
      ['owed', 25],
      ['remainder', 50],
    ]);
  });

  it('never exceeds the track, even with adversarial counts', () => {
    const out = barGeometry({ archived: Number.MAX_SAFE_INTEGER / 2, owed: 3, failed: 4, remainder: 5 }, 10);
    const widest = Math.max(...out.map((s) => s.x + s.w));
    expect(widest).toBeLessThanOrEqual(10 + 1e-6);
  });
});

describe('W149 · ringDash', () => {
  it('🔴 a null percentage has no ring: the caller may not draw what the model refused', () => {
    expect(ringDash(null, 10)).toBeNull();
  });

  it('half the circumference for 50%', () => {
    expect(ringDash(50, 10)).toEqual({ dash: Math.PI * 10, gap: Math.PI * 10 });
  });

  it('clamps out-of-range values rather than drawing an open or negative gap', () => {
    expect(ringDash(120, 5)!.dash).toBeCloseTo(2 * Math.PI * 5, 6);
    expect(ringDash(-4, 5)!.dash).toBe(0);
  });
});

describe('W149 · monthsGeometry', () => {
  const months = [
    { month: '2026-01', archived: 2, pending: 1 },
    { month: '2026-02', archived: 0, pending: 0 },
    { month: '2026-03', archived: 0, pending: 3 },
  ];

  it('🔴 a month with no conversations is not drawn as an empty column', () => {
    const out = monthsGeometry(months, { archived: 0, pending: 0 }, { width: 100, height: 40, gap: 4 })!;
    expect(out.columns.map((c) => c.key)).toEqual(['2026-01', '2026-03']);
  });

  it('🔴 nothing to draw is null, so the caller owes that case a sentence', () => {
    expect(monthsGeometry([], { archived: 0, pending: 0 }, { width: 100, height: 40, gap: 4 })).toBeNull();
    expect(monthsGeometry([{ month: '2026-01', archived: 0, pending: 0 }], { archived: 0, pending: 0 }, { width: 100, height: 40, gap: 4 })).toBeNull();
  });

  it('the unknown-time bucket is its own final column, never a month', () => {
    const out = monthsGeometry(months, { archived: 1, pending: 2 }, { width: 120, height: 40, gap: 4 })!;
    expect(out.columns.map((c) => c.key)).toEqual(['2026-01', '2026-03', '']);
    const last = out.columns[out.columns.length - 1]!;
    expect(last.kind).toBe('unknown');
    // The unknown bucket's total (3) is the max here, so its column is full height.
    expect(last.hUnknown).toBeCloseTo(40, 5);
    expect(last.hArchived).toBe(0);
    expect(unknownTimeTotal({ archived: 1, pending: 2 })).toBe(3);
  });

  it('an unknown bucket of zero adds no column', () => {
    const out = monthsGeometry(months, { archived: 0, pending: 0 }, { width: 100, height: 40, gap: 4 })!;
    expect(out.columns.every((c) => c.kind === 'month')).toBe(true);
  });

  it('heights are proportional to the tallest column, stacked left from the middle for the unknown smaller totals', () => {
    const out = monthsGeometry(months, { archived: 0, pending: 0 }, { width: 100, height: 30, gap: 5 })!;
    // 2026-01 total 3 = the max: both parts reach the top. 2026-03 total 3 as well here.
    const first = out.columns[0]!;
    expect(first.hArchived + first.hPending).toBeCloseTo(30, 5);
    const second = out.columns[1]!;
    expect(second.hArchived).toBe(0);
    expect(second.hPending).toBeCloseTo(30, 5);
    expect(out.max).toBe(3);
  });

  it('many months keep equal widths and a stride that keeps labels spare', () => {
    const many = Array.from({ length: 24 }, (_, i) => ({ month: `2025-${String((i % 12) + 1).padStart(2, '0')}`, archived: 1, pending: 0 }));
    const out = monthsGeometry(many, { archived: 0, pending: 0 }, { width: 480, height: 40, gap: 2 })!;
    const widths = new Set(out.columns.map((c) => c.w));
    expect(widths.size).toBe(1);
    expect(out.columns[1]!.x - out.columns[0]!.x).toBeCloseTo(out.columns[0]!.w + 2, 5);
    expect(out.stride).toBe(4);
  });
});

describe('W149 · monthLabel', () => {
  it('renders YYYY-MM as two short numeric groups, locale-neutral', () => {
    expect(monthLabel('2026-03')).toBe('26/03');
  });

  it('passes through anything that is not a YYYY-MM key rather than mangling it', () => {
    expect(monthLabel('unlisted')).toBe('unlisted');
  });
});
