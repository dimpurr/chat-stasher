/**
 * W296 · **One 429 on a platform slows every request to it for the rest of the local day.**
 *
 * The gap this closes, and why the two brakes that already exist did not close it:
 *
 *  · the retry ladder stops the leg for 7.5-15 min and doubles from there, and the
 *    platform's own `Retry-After` (W127) may replace that one delay — both are
 *    *minutes* long and both expire back onto the very preset rhythm that produced
 *    the 429;
 *  · this record outlives both: it is written into `storage.local`, keyed by
 *    platform, and read by the one function that turns a stored preset into a rate
 *    (`presetTickOptions`, entrypoints/background.ts), so the tick after the ladder
 *    ends still asks for half the requests.
 *
 * Everything here is synthetic: fixture ids, fixture responses, an injected clock.
 * No network, no account, no conversation text.
 */

import { describe, it, expect } from 'vitest';

import { memoryStore } from '../lib/backfill/store';
import { DEFAULT_LIST_ONLY_ENUM_PACE, DEFAULT_PACE } from '../lib/backfill/pace';
import { SPEED_PLANS, type SpeedPlan } from '../lib/backfill/speed';
import { QUIET_TICK_DETAILS } from '../lib/backfill/schedule';
import {
  DAY_SLOW_INTERVAL_FACTOR,
  DAY_SLOW_KEY,
  DAY_SLOW_VERSION,
  daySlowPlan,
  daySlowTriggeredBy,
  isPlatformDaySlowed,
  localMidnightAfter,
  readDaySlowUntil,
  recordPlatformRateLimit,
} from '../lib/backfill/day-slow';

const T0 = Date.parse('2026-10-01T12:00:00.000Z');

// ===========================================================================
// A · the rule, and the one status that carries it
// ===========================================================================

describe('W296-A · only a 429 arms the brake', () => {
  it('🔴 429 arms it; 403, 5xx and 2xx do not', () => {
    expect(daySlowTriggeredBy(429)).toBe(true);
    for (const status of [200, 204, 400, 401, 403, 404, 500, 502, 503, 504]) {
      expect(daySlowTriggeredBy(status), String(status)).toBe(false);
    }
  });
});

describe('W296-A · the window is the rest of the local day', () => {
  it('🔴 the next local midnight, strictly after now, and 00:00:00.000 local', () => {
    const now = T0;
    const until = localMidnightAfter(now);
    expect(until).toBeGreaterThan(now);
    // Strictly after: a 429 landing exactly on local midnight still gets a whole day.
    const atMidnight = new Date(now);
    atMidnight.setHours(0, 0, 0, 0);
    const midnightMs = atMidnight.getTime();
    expect(localMidnightAfter(midnightMs)).toBeGreaterThan(midnightMs);
    // The instant really is local midnight of the following local day.
    const end = new Date(until);
    expect([end.getHours(), end.getMinutes(), end.getSeconds(), end.getMilliseconds()]).toEqual([0, 0, 0, 0]);
    expect(end.getFullYear()).toBe(new Date(midnightMs).getFullYear());
    expect(end.getDate()).not.toBe(new Date(midnightMs).getDate());
    // Never more than a day and an hour out, whatever a DST transition does inside it.
    expect(until - now).toBeLessThanOrEqual(25 * 60 * 60 * 1000);
  });
});

// ===========================================================================
// B · the record: written per platform, survives a worker restart, expires on the clock
// ===========================================================================

describe('W296-B · recordPlatformRateLimit writes one platform, and readDaySlowUntil honours it', () => {
  it('🔴 a 429 leaves a brake for that platform, and a fresh store over the same data still sees it', async () => {
    const store = memoryStore();
    const until = await recordPlatformRateLimit(store, 'chatgpt', T0);
    expect(until).toBe(localMidnightAfter(T0));
    expect(store.data[DAY_SLOW_KEY]).toEqual({
      v: DAY_SLOW_VERSION,
      platforms: { chatgpt: { until, at: T0 } },
    });
    expect(await isPlatformDaySlowed(store, 'chatgpt', T0)).toBe(true);
    expect(await readDaySlowUntil(store, 'chatgpt', T0)).toBe(until);
    // A service-worker restart is a new reader over the same storage.local snapshot.
    const restarted = memoryStore(JSON.parse(JSON.stringify(store.data)) as Record<string, unknown>);
    expect(await isPlatformDaySlowed(restarted, 'chatgpt', T0 + 60_000)).toBe(true);
  });

  it('🔴 the brake is per platform — another platform is untouched', async () => {
    const store = memoryStore();
    await recordPlatformRateLimit(store, 'chatgpt', T0);
    expect(await isPlatformDaySlowed(store, 'claude', T0)).toBe(false);
    await recordPlatformRateLimit(store, 'claude', T0);
    const record = store.data[DAY_SLOW_KEY] as { platforms: Record<string, unknown> };
    expect(Object.keys(record.platforms).sort()).toEqual(['chatgpt', 'claude']);
  });

  it('🔴 it ends at the stored instant, and an expired entry is simply not a brake', async () => {
    const store = memoryStore();
    const until = await recordPlatformRateLimit(store, 'chatgpt', T0);
    expect(await isPlatformDaySlowed(store, 'chatgpt', until - 1)).toBe(true);
    expect(await isPlatformDaySlowed(store, 'chatgpt', until)).toBe(false);
    expect(await isPlatformDaySlowed(store, 'chatgpt', until + 86_400_000)).toBe(false);
  });

  it('🔴 a second 429 the same day is idempotent in effect and re-stamps `at`', async () => {
    const store = memoryStore();
    const first = await recordPlatformRateLimit(store, 'chatgpt', T0);
    const second = await recordPlatformRateLimit(store, 'chatgpt', T0 + 3_600_000);
    expect(second).toBe(first);
    const record = store.data[DAY_SLOW_KEY] as { platforms: Record<string, { until: number; at: number }> };
    expect(record.platforms.chatgpt).toEqual({ until: first, at: T0 + 3_600_000 });
  });

  it('🔴 expired entries of other platforms are dropped, live ones are kept', async () => {
    const yesterday = memoryStore();
    await recordPlatformRateLimit(yesterday, 'deepseek', T0);
    const carried = JSON.parse(JSON.stringify(yesterday.data)) as Record<string, unknown>;
    // Move the clock past that brake's end, then arm a different platform.
    const tomorrow = T0 + 36 * 60 * 60 * 1000;
    const store = memoryStore(carried);
    await recordPlatformRateLimit(store, 'chatgpt', tomorrow);
    const record = store.data[DAY_SLOW_KEY] as { platforms: Record<string, unknown> };
    expect(Object.keys(record.platforms)).toEqual(['chatgpt']);
  });

  it('🔴 a missing, foreign or unusable record is no brake — never a read that throws', async () => {
    expect(await readDaySlowUntil(null, 'chatgpt', T0)).toBe(0);
    expect(await readDaySlowUntil(memoryStore(), 'chatgpt', T0)).toBe(0);
    for (const garbage of [
      'not a record',
      42,
      { v: 999, platforms: { chatgpt: { until: T0 + 1, at: T0 } } },
      { v: DAY_SLOW_VERSION, platforms: [] },
      { v: DAY_SLOW_VERSION, platforms: { chatgpt: 'soon' } },
      { v: DAY_SLOW_VERSION, platforms: { chatgpt: { until: Number.NaN, at: T0 } } },
      { v: DAY_SLOW_VERSION, platforms: { chatgpt: { at: T0 } } },
    ]) {
      const store = memoryStore({ [DAY_SLOW_KEY]: garbage });
      expect(await readDaySlowUntil(store, 'chatgpt', T0), JSON.stringify(garbage)).toBe(0);
    }
  });

  it('🔴 a store that cannot be read is no brake; a store that cannot be read is not written to', async () => {
    const unreadable = {
      load: async () => {
        throw new Error('storage unavailable');
      },
      save: async () => {
        throw new Error('must not be reached');
      },
      remove: async () => undefined,
      keys: async () => [],
    };
    expect(await readDaySlowUntil(unreadable, 'chatgpt', T0)).toBe(0);
    // The read-modify-write must not become a write that drops another platform's brake.
    await expect(recordPlatformRateLimit(unreadable, 'chatgpt', T0)).rejects.toThrow('storage unavailable');
  });

  it('🔴 no store at all is a refusal to write, not a silent no-op', async () => {
    await expect(recordPlatformRateLimit(null, 'chatgpt', T0)).rejects.toThrow(/no storage/);
  });
});

// ===========================================================================
// C · the plan: every gap doubles, the tick's budget only ever drops
// ===========================================================================

describe('W296-C · daySlowPlan only ever slows', () => {
  it('🔴 every segment gap doubles, including the list-only one the preset plans omit', () => {
    for (const plan of Object.values(SPEED_PLANS)) {
      const slowed = daySlowPlan(plan);
      expect(slowed.pace.detail.minIntervalMs).toBe(plan.pace.detail.minIntervalMs * DAY_SLOW_INTERVAL_FACTOR);
      expect(slowed.pace.detail.jitterMs).toBe((plan.pace.detail.jitterMs ?? 0) * DAY_SLOW_INTERVAL_FACTOR);
      expect(slowed.pace.enumerate.minIntervalMs).toBe(
        plan.pace.enumerate.minIntervalMs * DAY_SLOW_INTERVAL_FACTOR,
      );
      expect(slowed.pace.enumerate.jitterMs).toBe(
        (plan.pace.enumerate.jitterMs ?? 0) * DAY_SLOW_INTERVAL_FACTOR,
      );
      // The presets never carry one, and the engine's fallback would be the *uns*slowed
      // list-only rhythm — so it has to be filled in, not left to the fallback.
      expect(plan.pace.listOnlyEnumerate).toBeUndefined();
      expect(slowed.pace.listOnlyEnumerate?.minIntervalMs).toBe(
        DEFAULT_LIST_ONLY_ENUM_PACE.minIntervalMs * DAY_SLOW_INTERVAL_FACTOR,
      );
      expect(slowed.pace.listOnlyEnumerate?.jitterMs).toBe(
        (DEFAULT_LIST_ONLY_ENUM_PACE.jitterMs ?? 0) * DAY_SLOW_INTERVAL_FACTOR,
      );
    }
  });

  it('🔴 a plan that already carries a list-only pace has that one doubled, not replaced', () => {
    const withListOnly: SpeedPlan = {
      ...SPEED_PLANS.gentle,
      pace: { ...SPEED_PLANS.gentle.pace, listOnlyEnumerate: { ...DEFAULT_LIST_ONLY_ENUM_PACE, minIntervalMs: 5_000 } },
    };
    expect(daySlowPlan(withListOnly).pace.listOnlyEnumerate?.minIntervalMs).toBe(10_000);
  });

  it('🔴 the tick budget drops to the gentlest preset and never rises', () => {
    expect(SPEED_PLANS.faster.tickDetails).toBeGreaterThan(QUIET_TICK_DETAILS);
    expect(daySlowPlan(SPEED_PLANS.faster).tickDetails).toBe(QUIET_TICK_DETAILS);
    expect(daySlowPlan(SPEED_PLANS.standard).tickDetails).toBe(QUIET_TICK_DETAILS);
    expect(daySlowPlan(SPEED_PLANS.gentle).tickDetails).toBe(QUIET_TICK_DETAILS);
    // A plan already below the gentlest budget stays where its author put it.
    const tiny: SpeedPlan = { ...SPEED_PLANS.gentle, tickDetails: 0 };
    expect(daySlowPlan(tiny).tickDetails).toBe(0);
  });

  it('🔴 nothing else moves: preset, risk note and the daily band are passed through', () => {
    const slowed = daySlowPlan(SPEED_PLANS.faster);
    expect(slowed.preset).toBe('faster');
    expect(slowed.carriesRisk).toBe(true);
    expect(slowed.pace.detail.maxPerDay).toBe(SPEED_PLANS.faster.pace.detail.maxPerDay);
    expect(slowed.pace.detail.dailyCapBand).toEqual(SPEED_PLANS.faster.pace.detail.dailyCapBand);
  });

  it('🔴 a segment with no jitter field does not gain one', () => {
    const bare: SpeedPlan = {
      ...SPEED_PLANS.gentle,
      pace: {
        enumerate: { minIntervalMs: 1_000, maxPerDay: null },
        detail: { minIntervalMs: 2_000, maxPerDay: null },
      },
    };
    const slowed = daySlowPlan(bare);
    expect('jitterMs' in slowed.pace.enumerate).toBe(false);
    expect('jitterMs' in slowed.pace.detail).toBe(false);
    expect(slowed.pace.enumerate.minIntervalMs).toBe(2_000);
  });

  it('🔴 the shipped default plan really did slow (a sanity check on the input table)', () => {
    const slowed = daySlowPlan(SPEED_PLANS.gentle);
    expect(slowed.pace.detail.minIntervalMs).toBeGreaterThan(DEFAULT_PACE.detail.minIntervalMs);
    expect(slowed.pace.enumerate.minIntervalMs).toBeGreaterThan(DEFAULT_PACE.enumerate.minIntervalMs);
  });
});

// ===========================================================================
// D · the flow that arms it — a real round, through the real entry point
//
// 🔴 `w296-day-slow-flow.test.ts` is where a 429 is driven through
//    background.ts's request gateway; it is a separate file because it boots the
//    real entry point against a fake browser and this one is pure.
// ===========================================================================
