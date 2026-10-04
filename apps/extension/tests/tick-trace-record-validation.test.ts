import { describe, expect, it } from 'vitest';
import { BACKFILL_LAST_TICK_KEY, loadLastTick } from '../lib/backfill/alarm';
import type { BackfillStore } from '../lib/backfill/store';

function syntheticStore(record: unknown): BackfillStore {
  return {
    load: async (key) => key === BACKFILL_LAST_TICK_KEY ? record : undefined,
    save: async () => {},
    remove: async () => {},
    keys: async () => [BACKFILL_LAST_TICK_KEY],
  };
}

describe('loadLastTick required-field validation', () => {
  it.each([
    ['NaN timestamp', { at: Number.NaN }],
    ['positive infinity timestamp', { at: Number.POSITIVE_INFINITY }],
    ['negative infinity timestamp', { at: Number.NEGATIVE_INFINITY }],
    ['negative target count', { targets: -1 }],
    ['fractional target count', { targets: 1.5 }],
    ['unknown reason', { reason: 'future-or-corrupt-reason' }],
  ])('rejects a record with a %s', async (_label, changedField) => {
    const record = {
      at: 1_760_000_000_000,
      ran: false,
      reason: 'no-http-port',
      targets: 1,
      ...changedField,
    };

    await expect(loadLastTick(syntheticStore(record))).resolves.toBeNull();
  });

  it('accepts a valid synthetic record with documented optional fields omitted', async () => {
    const record = {
      at: 1_760_000_000_000,
      ran: false,
      reason: 'no-http-port',
      targets: 0,
    };

    await expect(loadLastTick(syntheticStore(record))).resolves.toEqual(record);
  });
});
