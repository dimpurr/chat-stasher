import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { DAY_SLOW_KEY, recordPlatformRateLimit } from '../lib/backfill/day-slow';

const TIME_ZONE = 'America/New_York';

function memoryStore() {
  const data: Record<string, unknown> = {};
  return {
    data,
    async load(key: string) { return data[key]; },
    async save(key: string, value: unknown) { data[key] = value; },
    async remove(key: string) { delete data[key]; },
    async keys() { return Object.keys(data); },
  };
}

function expectLocalMidnight(instant: number, expectedIso: string): void {
  expect(instant).toBe(Date.parse(expectedIso));
  const local = new Date(instant);
  expect([
    local.getFullYear(), local.getMonth(), local.getDate(),
    local.getHours(), local.getMinutes(), local.getSeconds(), local.getMilliseconds(),
  ]).toEqual([
    Number(expectedIso.slice(0, 4)), Number(expectedIso.slice(5, 7)) - 1, Number(expectedIso.slice(8, 10)),
    0, 0, 0, 0,
  ]);
}

async function expectRecorded429At(now: number, expectedUntilIso: string): Promise<number> {
  const store = memoryStore();
  const until = await recordPlatformRateLimit(store, 'chatgpt', now);
  const record = store.data[DAY_SLOW_KEY] as { platforms: Record<string, { at: number; until: number }> };
  expect(record.platforms.chatgpt).toEqual({ at: now, until });
  expect(until).toBeGreaterThan(now);
  expectLocalMidnight(until, expectedUntilIso);
  return until;
}

beforeEach(() => {
  vi.stubEnv('TZ', TIME_ZONE);
});

afterEach(() => {
  vi.unstubAllEnvs();
});

describe(`local day-slow expiry in ${TIME_ZONE}`, () => {
  it('expires a 429 just before spring-forward midnight at that midnight', async () => {
    const now = Date.parse('2026-03-08T04:59:59.999Z');
    const until = await expectRecorded429At(now, '2026-03-08T05:00:00.000Z');
    expect(until - now).toBe(1);
  });

  it('expires a 429 exactly at spring-forward midnight at the strictly next midnight', async () => {
    const now = Date.parse('2026-03-08T05:00:00.000Z');
    const until = await expectRecorded429At(now, '2026-03-09T04:00:00.000Z');
    expect(until - now).toBe(23 * 60 * 60 * 1000);
  });

  it('expires a 429 just before fall-back midnight at that midnight', async () => {
    const now = Date.parse('2026-11-01T03:59:59.999Z');
    const until = await expectRecorded429At(now, '2026-11-01T04:00:00.000Z');
    expect(until - now).toBe(1);
  });

  it('expires a 429 exactly at fall-back midnight at the strictly next midnight', async () => {
    const now = Date.parse('2026-11-01T04:00:00.000Z');
    const until = await expectRecorded429At(now, '2026-11-02T05:00:00.000Z');
    expect(until - now).toBe(25 * 60 * 60 * 1000);
  });
});
