import { describe, expect, it } from 'vitest';
import { barGeometry, monthsGeometry, ringDash, unknownTimeTotal } from '../lib/coverage-charts';

describe('coverage chart input edges', () => {
  it('omits bars when the track width is unusable', () => {
    for (const width of [0, -1, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
      expect(barGeometry({ archived: 1, owed: 2, failed: 0, remainder: 0 }, width)).toEqual([]);
    }
  });

  it('keeps valid bar segments when a runtime count is non-finite', () => {
    const out = barGeometry({ archived: Number.NaN, owed: 2, failed: Number.POSITIVE_INFINITY, remainder: 0 }, 10);
    expect(out).toEqual([{ tone: 'owed', x: 0, w: 10 }]);
    expect(out.every(({ x, w }) => Number.isFinite(x) && Number.isFinite(w))).toBe(true);
    expect(out.every(({ x, w }) => x >= 0 && w >= 0 && x + w <= 10)).toBe(true);
  });

  it('omits rings for non-finite or non-positive radii', () => {
    for (const radius of [0, -1, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
      expect(ringDash(50, radius)).toBeNull();
    }
  });

  it('omits monthly geometry for unusable width or height', () => {
    const months = [{ month: '2026-01', archived: 1, pending: 0 }];
    const unknown = { archived: 0, pending: 0 };
    for (const width of [0, -1, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
      expect(monthsGeometry(months, unknown, { width, height: 10, gap: 1 })).toBeNull();
    }
    for (const height of [0, -1, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
      expect(monthsGeometry(months, unknown, { width: 10, height, gap: 1 })).toBeNull();
    }
  });

  it('normalizes unusable gaps while keeping every column inside the track', () => {
    const months = [
      { month: '2026-01', archived: 2, pending: 0 },
      { month: '2026-02', archived: 0, pending: 2 },
    ];
    for (const gap of [0, -2, 50, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
      const out = monthsGeometry(months, { archived: 0, pending: 0 }, { width: 10, height: 8, gap });
      expect(out).not.toBeNull();
      for (const column of out!.columns) {
        expect([column.x, column.w, column.hArchived, column.hPending, column.hUnknown, column.total].every(Number.isFinite)).toBe(true);
        expect(column.x).toBeGreaterThanOrEqual(0);
        expect(column.w).toBeGreaterThanOrEqual(0);
        expect(column.x + column.w).toBeLessThanOrEqual(10);
        expect(column.hArchived + column.hPending + column.hUnknown).toBeLessThanOrEqual(8);
      }
    }
  });

  it('ignores non-finite monthly and unknown counts without losing valid counts', () => {
    const out = monthsGeometry([
      { month: '2026-01', archived: Number.NaN, pending: 3 },
      { month: '2026-02', archived: Number.POSITIVE_INFINITY, pending: 2 },
      { month: '2026-03', archived: 0, pending: Number.NEGATIVE_INFINITY },
    ], { archived: 1, pending: Number.NaN }, { width: 20, height: 10, gap: 1 });
    expect(out?.columns.map(({ kind, key, total }) => [kind, key, total])).toEqual([
      ['month', '2026-01', 3],
      ['month', '2026-02', 2],
      ['unknown', '', 1],
    ]);
    expect(unknownTimeTotal({ archived: 1, pending: Number.NaN })).toBe(1);
    for (const column of out!.columns) {
      expect([column.x, column.w, column.hArchived, column.hPending, column.hUnknown, column.total].every(Number.isFinite)).toBe(true);
      expect(column.x + column.w).toBeLessThanOrEqual(20);
      expect(column.hArchived + column.hPending + column.hUnknown).toBeLessThanOrEqual(10);
    }
  });
});
