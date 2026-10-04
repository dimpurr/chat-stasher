import { describe, expect, it } from 'vitest';
import { retryMinutesLeft } from '../lib/backfill/progress';
import type { HaltRecord } from '../lib/backfill/types';

const halt = (retryAt?: number): HaltRecord => ({
  reason: 'transport-error',
  at: 0,
  detail: 'synthetic fixture',
  ...(retryAt === undefined ? {} : { retryAt }),
});

describe('retryMinutesLeft with invalid and boundary timestamps', () => {
  it('keeps missing and due retry times at zero and rounds valid future instants up', () => {
    expect(retryMinutesLeft(halt(), 0)).toBe(0);
    expect(retryMinutesLeft(halt(0), 0)).toBe(0);
    expect(retryMinutesLeft(halt(59_999), 0)).toBe(1);
    expect(retryMinutesLeft(halt(60_000), 0)).toBe(1);
    expect(retryMinutesLeft(halt(60_001), 0)).toBe(2);
    expect(retryMinutesLeft(halt(-1), 0)).toBe(0);
    // Negative epoch values remain valid timestamps; this instant is one ms ahead.
    expect(retryMinutesLeft(halt(0), -1)).toBe(1);
  });

  it('never returns NaN or infinity when either timestamp is non-finite', () => {
    const invalidTimes = [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY];
    for (const retryAt of invalidTimes) {
      const minutes = retryMinutesLeft(halt(retryAt), 0);
      expect(Number.isFinite(minutes), `retryAt ${retryAt}`).toBe(true);
      expect(minutes).toBe(0);
    }
    for (const now of invalidTimes) {
      const minutes = retryMinutesLeft(halt(60_000), now);
      expect(Number.isFinite(minutes), `now ${now}`).toBe(true);
      expect(minutes).toBe(0);
    }
  });

  it('does not return infinity when subtracting extreme finite timestamps overflows', () => {
    const minutes = retryMinutesLeft(halt(Number.MAX_VALUE), -Number.MAX_VALUE);
    expect(Number.isFinite(minutes)).toBe(true);
    expect(minutes).toBe(0);
  });
});
