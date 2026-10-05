import { describe, expect, it } from 'vitest';
import {
  CONNECT_DELIVERY_KEY,
  LAST_EXPORT_KEY,
  loadConnectDelivery,
  loadLastExport,
} from '../lib/outbox';

function metadataStore(values: Record<string, unknown>) {
  return {
    load: async (key: string) => values[key] ?? null,
  } as never;
}

describe('persisted outbox summary metadata validation', () => {
  it.each([Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, -1])(
    'does not trust a ConnectDelivery with invalid time %s',
    async (at) => {
      const store = metadataStore({ [CONNECT_DELIVERY_KEY]: { at, count: 2 } });
      expect(await loadConnectDelivery(store)).toBeNull();
    },
  );

  it.each([Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, -1, 1.5])(
    'does not trust a ConnectDelivery with invalid count %s',
    async (count) => {
      const store = metadataStore({ [CONNECT_DELIVERY_KEY]: { at: 1_700_000_000_000, count } });
      expect(await loadConnectDelivery(store)).toBeNull();
    },
  );

  it.each([Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, -1])(
    'does not trust a LastExport with invalid time %s',
    async (at) => {
      const store = metadataStore({
        [LAST_EXPORT_KEY]: { at, entries: 2, bytes: 100, filename: 'synthetic.jsonl' },
      });
      expect(await loadLastExport(store)).toBeNull();
    },
  );

  it.each([Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, -1, 1.5])(
    'does not trust a LastExport with invalid entry count %s',
    async (entries) => {
      const store = metadataStore({
        [LAST_EXPORT_KEY]: { at: 1_700_000_000_000, entries, bytes: 100, filename: 'synthetic.jsonl' },
      });
      expect(await loadLastExport(store)).toBeNull();
    },
  );

  it.each([null, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, -1, 1.5])(
    'does not trust a LastExport with invalid byte size %s',
    async (bytes) => {
      const store = metadataStore({
        [LAST_EXPORT_KEY]: { at: 1_700_000_000_000, entries: 2, bytes, filename: 'synthetic.jsonl' },
      });
      expect(await loadLastExport(store)).toBeNull();
    },
  );

  it('accepts valid records and preserves the missing-bytes fallback', async () => {
    const deliveryStore = metadataStore({ [CONNECT_DELIVERY_KEY]: { at: 0, count: 0 } });
    expect(await loadConnectDelivery(deliveryStore)).toEqual({ at: 0, count: 0 });

    const exportStore = metadataStore({
      [LAST_EXPORT_KEY]: { at: 0, entries: 0, filename: 'synthetic.jsonl' },
    });
    expect(await loadLastExport(exportStore)).toEqual({
      at: 0,
      entries: 0,
      bytes: 0,
      filename: 'synthetic.jsonl',
    });
  });
});
