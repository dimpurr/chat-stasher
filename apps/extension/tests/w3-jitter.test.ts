/**
 * W16 · **The backfill leg is never periodic, and the jitter never goes below a
 * documented minimum.**
 *
 * The product requirement, in the owner's words: "Gentleness is the point. We
 * must be gentler than every competitor, and the frequency must not be steady —
 * it needs jitter."
 *
 * 🔴 What this file is for, and what it deliberately is not:
 *
 *  · **Not** "a random number is produced". A test that only checked "the value
 *    changed" would pass just as happily if the change were a *shortening*, and
 *    shortening a gap is the one thing this change may never do. So every test
 *    here asserts a **bound** as well as a variation, and the two boundary
 *    values are pinned exactly: `random = () => 0` must reproduce the old
 *    deterministic numbers character for character, and `random = () => 1` must
 *    land exactly on the documented ceiling.
 *  · **Not** a statistical argument. There is no "run it 10,000 times and hope".
 *    The draws are injected, so a boundary is a deterministic assertion about a
 *    named value, and the "many values" loops exist to show the *band* is never
 *    escaped, not to estimate a mean.
 *  · **Not** a second copy of the pacing tests. The files that pin the 20 s /
 *    2 s intervals and the interleave still do that, with `random: () => 0` —
 *    the bottom of every band, which is where those exact numbers live. This
 *    file owns the jitter itself.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';

import {
  armBackfillTick,
  BACKFILL_ALARM_NAME,
  BACKFILL_SAFETY_ALARM_NAME,
  BACKFILL_SAFETY_PERIOD_MINUTES,
  BACKFILL_TICK_DELAY_MAX_MINUTES,
  BACKFILL_TICK_DELAY_MIN_MINUTES,
  drawTickDelayMinutes,
  syncBackfillAlarm,
  type AlarmsApi,
} from '../lib/backfill/alarm';
import {
  DAILY_CAP_MAX,
  DAILY_CAP_MIN,
  DEFAULT_DETAIL_PACE,
  DEFAULT_ENUM_PACE,
  Pacer,
  drawDailyCap,
  type Clock,
  type PacePlan,
} from '../lib/backfill/pace';
import { uniformBetween, type RandomFn } from '../lib/backfill/random';
import {
  TRANSIENT_RETRY_BASE_MS,
  TRANSIENT_RETRY_MAX_MS,
  transientRetryDelayMs,
} from '../lib/backfill/types';
import { runBackfill } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { stateKey } from '../lib/backfill/types';

const ORIGIN = 'https://chatgpt.com';

// ---------------------------------------------------------------------------
// Draw sources
//
// Three shapes, each earning its place:
//  · `fixed(v)`   — pin a boundary exactly (the floor, the ceiling, a tick that
//                   must be reproducible).
//  · `seeded(s)`  — a deterministic *stream*, so "two consecutive draws differ"
//                   is a fact about the run rather than a probability.
//  · `sequence([…])` — a known list of draws, so a value that depends on *when*
//                   in a run it is taken can be asserted exactly.
// ---------------------------------------------------------------------------

const fixed = (v: number): RandomFn => () => v;

function seeded(seed: number): RandomFn {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 4294967296;
  };
}

/** The draws a test needs to name, and — after that — the last one forever. */
function sequence(values: number[]): RandomFn {
  let i = 0;
  return () => values[Math.min(i++, values.length - 1)]!;
}

/** A fake clock: sleep only advances virtual time and records it. */
function fakeClock(startMs = Date.parse('2026-08-17T00:00:00.000Z')): Clock & { nowMs: () => number } {
  let t = startMs;
  return {
    now: () => t,
    async sleep(ms: number) { t += ms; },
    nowMs: () => t,
  };
}

// ===========================================================================
// 1 · The tick delay is irregular, and it is bounded on both sides
// ===========================================================================

describe('W16-1 · the tick cadence is drawn, not fixed', () => {
  it('🔴 consecutive gaps differ, and every one of them lies inside the documented band', () => {
    const random = seeded(20260914);
    const draws = Array.from({ length: 64 }, () => drawTickDelayMinutes(random));

    // "The frequency must not be steady": a fixed period would give one value.
    expect(new Set(draws).size).toBeGreaterThan(1);
    // Not merely "different" — different in the way a person's attention is:
    // two adjacent gaps are essentially never the same.
    let consecutiveEqual = 0;
    for (let i = 1; i < draws.length; i += 1) if (draws[i] === draws[i - 1]) consecutiveEqual += 1;
    expect(consecutiveEqual).toBe(0);

    for (const d of draws) {
      expect(d).toBeGreaterThanOrEqual(BACKFILL_TICK_DELAY_MIN_MINUTES);
      expect(d).toBeLessThanOrEqual(BACKFILL_TICK_DELAY_MAX_MINUTES);
    }
  });

  it('🔴 the floor is the fixed period it replaced, so the alarm can only ever tick LATER', () => {
    /**
     * The tick used to be `periodInMinutes: 5` — 288 wakes a day, exactly. The
     * new floor is that same 5 minutes, so the worst case is unchanged while the
     * mean drops. If this assertion is ever loosened to a smaller floor, the
     * change has started to *raise* the rate rather than lower it.
     */
    expect(BACKFILL_TICK_DELAY_MIN_MINUTES).toBe(5);
    for (const r of [0, 0.001, 0.5, 0.999, 1]) {
      expect(drawTickDelayMinutes(fixed(r))).toBeGreaterThanOrEqual(5);
    }
    // And the ceiling is above the floor: the band has real width.
    expect(BACKFILL_TICK_DELAY_MAX_MINUTES).toBeGreaterThan(BACKFILL_TICK_DELAY_MIN_MINUTES);
  });

  it('🔴 random=()=>0 lands exactly on the floor and random=()=>1 exactly on the ceiling', () => {
    expect(drawTickDelayMinutes(fixed(0))).toBe(BACKFILL_TICK_DELAY_MIN_MINUTES);
    expect(drawTickDelayMinutes(fixed(1))).toBe(BACKFILL_TICK_DELAY_MAX_MINUTES);
  });

  it('a hostile draw cannot escape the band: NaN, a negative or an over-1 land inside it', () => {
    // `Math.random` cannot return these, but the jitter is a promise about the
    // minimum, and a promise that holds only for well-behaved inputs is not one.
    for (const bad of [Number.NaN, -5, 1.5, Number.POSITIVE_INFINITY]) {
      const d = drawTickDelayMinutes(fixed(bad));
      expect(d).toBeGreaterThanOrEqual(BACKFILL_TICK_DELAY_MIN_MINUTES);
      expect(d).toBeLessThanOrEqual(BACKFILL_TICK_DELAY_MAX_MINUTES);
    }
    // The conservative direction on a non-finite draw is the floor, not the ceiling.
    expect(drawTickDelayMinutes(fixed(Number.NaN))).toBe(BACKFILL_TICK_DELAY_MIN_MINUTES);
  });
});

// ===========================================================================
// 2 · No wait is ever below today's minimum — the property, over many draws
// ===========================================================================

describe('W16-2 · jitter only ever adds delay', () => {
  const SEGMENTS = [
    { label: 'enumerate', plan: DEFAULT_ENUM_PACE },
    { label: 'detail', plan: DEFAULT_DETAIL_PACE },
  ] as const;

  it('🔴 every one of 200 random draws gives a gap >= the documented minimum and <= the ceiling', async () => {
    const random = seeded(7);
    for (const { label, plan } of SEGMENTS) {
      const floor = plan.minIntervalMs;
      const ceiling = floor + (plan.jitterMs ?? 0);
      for (let i = 0; i < 200; i += 1) {
        const clock = fakeClock();
        // seedLastAt = the clock's own now ⇒ elapsed = 0 ⇒ the recorded wait IS
        // the interval that was drawn, which is the number that matters.
        const pacer = new Pacer(plan, clock, label, clock.now(), random);
        const wait = await pacer.gate();
        expect(wait, `${label} draw ${i}`).toBeGreaterThanOrEqual(floor);
        expect(wait, `${label} draw ${i}`).toBeLessThanOrEqual(ceiling);
      }
    }
  });

  it('🔴 random=()=>0 makes every wait exactly the old deterministic minimum', async () => {
    for (const { label, plan } of SEGMENTS) {
      const clock = fakeClock();
      const pacer = new Pacer(plan, clock, label, clock.now(), fixed(0));
      expect(await pacer.gate()).toBe(plan.minIntervalMs);
      // The two numbers the pace module has documented since C11, unchanged.
      expect(plan.minIntervalMs).toBe(label === 'detail' ? 20_000 : 2_000);
    }
  });

  it('🔴 random=()=>1 makes every wait exactly the documented ceiling', async () => {
    for (const { label, plan } of SEGMENTS) {
      const clock = fakeClock();
      const pacer = new Pacer(plan, clock, label, clock.now(), fixed(1));
      expect(await pacer.gate()).toBe(plan.minIntervalMs + (plan.jitterMs ?? 0));
    }
  });

  it('a pace plan with no jitterMs behaves exactly as it did before W16 (elapsed is still made up)', async () => {
    // Every `pace` override in the suite is written without `jitterMs`; absent
    // must mean 0, or those tests would silently become random.
    const plan: PacePlan = { minIntervalMs: 3_000, maxPerDay: null };
    expect(plan.jitterMs).toBeUndefined();
    const clock = fakeClock();
    const pacer = new Pacer(plan, clock, 'plain', clock.now() - 1_000, seeded(3));
    // 3_000 - 1_000 of it already elapsed ⇒ exactly the remainder, no band.
    expect(await pacer.gate()).toBe(2_000);
  });

  it('the wait is still clamped at 0 when more than the interval has already passed', async () => {
    // The alarm path: a tick arrives long after the last fetch. Nothing to make
    // up, and the `Math.max(0, …)` from C11 is untouched by the jitter.
    const clock = fakeClock();
    const pacer = new Pacer(DEFAULT_DETAIL_PACE, clock, 'detail', clock.now() - 10 * 60_000, fixed(1));
    expect(await pacer.gate()).toBe(0);
  });

  it('uniformBetween itself never returns outside its range, for any draw', () => {
    for (const r of [-1, 0, 0.25, 0.5, 0.999, 1, 2, Number.NaN]) {
      const v = uniformBetween(fixed(r), 10, 20);
      expect(v).toBeGreaterThanOrEqual(10);
      expect(v).toBeLessThanOrEqual(20);
    }
  });
});

// ===========================================================================
// 3 · The daily cap is drawn once per local day and kept
// ===========================================================================

/** A backend that lists nothing: enough for the day's cap to be drawn and persisted. */
function emptyBackend(): { http: () => Promise<{ status: number; text: string }>; calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    http: async () => {
      calls.push('list');
      return { status: 200, text: '{"items":[],"total":0}' };
    },
  };
}

async function runOnce(
  store: ReturnType<typeof memoryStore>,
  clock: Clock,
  random: RandomFn,
  scope: string,
  pace?: { minIntervalMs: number; maxPerDay: number | null; jitterMs?: number }[],
) {
  const be = emptyBackend();
  const report = await runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope,
    store,
    http: be.http as never,
    clock,
    random,
    ...(pace
      ? { pace: { enumerate: pace[0]!, detail: pace[1]! } }
      : {}),
  });
  return { report, be };
}

describe('W16-3 · the daily cap is a drawn number, not a fixed one', () => {
  it('🔴 the draw covers [150, 200] and never the ceiling-plus-one', () => {
    const random = seeded(99);
    const caps = new Set<number>();
    for (let i = 0; i < 500; i += 1) {
      const cap = drawDailyCap(DAILY_CAP_MAX, random)!;
      expect(cap).toBeGreaterThanOrEqual(DAILY_CAP_MIN);
      expect(cap).toBeLessThanOrEqual(DAILY_CAP_MAX);
      expect(Number.isInteger(cap)).toBe(true);
      caps.add(cap);
    }
    // It really is a range and not one value dressed up as a draw.
    expect(caps.size).toBeGreaterThan(20);
    // Both ends are reachable, and they are exactly the documented ones.
    expect(drawDailyCap(DAILY_CAP_MAX, fixed(0))).toBe(DAILY_CAP_MIN);
    expect(drawDailyCap(DAILY_CAP_MAX, fixed(1))).toBe(DAILY_CAP_MAX);
  });

  it('🔴 it can never exceed the plan ceiling, so it can never raise a caller’s cap', () => {
    // The 400 is not a law of the universe, it is the *plan's* ceiling. A caller
    // that asked for less must not be given more by the roll.
    for (const requested of [0, 1, 7, 299, 300, 399, 400]) {
      for (const r of [0, 0.5, 1]) {
        expect(drawDailyCap(requested, fixed(r))).toBeLessThanOrEqual(requested);
      }
    }
    // `null` = "this segment has no cap"; there is nothing to draw.
    expect(drawDailyCap(null, fixed(1))).toBeNull();
  });

  it('🔴 drawn once per local day, persisted, and NOT re-rolled upward by a restart', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    /**
     * The tell: the draw source changes between the two runs. If the cap were
     * re-rolled on the second run it would jump to the top of the band (200);
     * because it is persisted with the counter it must still be the first value.
     */
    let draw = 0;
    const random: RandomFn = () => draw;

    await runOnce(store, clock, random, 'cap-once');
    const afterFirst = (await store.load(stateKey('chatgpt', 'cap-once'))) as { detailToday: { day: string; count: number; cap?: number } };
    expect(afterFirst.detailToday.cap).toBe(DAILY_CAP_MIN);

    draw = 1;                       // a restart with a different draw source
    await runOnce(store, clock, random, 'cap-once');
    const afterSecond = (await store.load(stateKey('chatgpt', 'cap-once'))) as { detailToday: { day: string; count: number; cap?: number } };
    expect(afterSecond.detailToday.cap).toBe(DAILY_CAP_MIN);   // not the top of the band
    expect(afterSecond.detailToday.day).toBe(afterFirst.detailToday.day);
  });

  it('🔴 a NEW day draws a new cap — the irregularity is across days, not within one', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    let draw = 0;
    await runOnce(store, clock, () => draw, 'cap-days');
    const day1 = (await store.load(stateKey('chatgpt', 'cap-days'))) as { detailToday: { day: string; cap?: number } };
    expect(day1.detailToday.cap).toBe(DAILY_CAP_MIN);

    // Tomorrow (the same scope, a fresh UTC date).
    const tomorrow = fakeClock(Date.parse('2026-08-18T00:00:00.000Z'));
    draw = 1;
    await runOnce(store, tomorrow, () => draw, 'cap-days');
    const day2 = (await store.load(stateKey('chatgpt', 'cap-days'))) as { detailToday: { day: string; cap?: number } };
    expect(day2.detailToday.day).not.toBe(day1.detailToday.day);
    expect(day2.detailToday.cap).toBe(DAILY_CAP_MAX);
    expect(day2.detailToday.cap).toBeLessThanOrEqual(DAILY_CAP_MAX);
  });

  it('🔴 the enforced cap is the smaller of the drawn one and the plan ceiling', async () => {
    const store = memoryStore();
    // A plan that asked for 3 bodies a day, with a draw source that would have
    // picked 200. The roll must not raise it.
    const { report } = await runOnce(store, fakeClock(), fixed(1), 'cap-plan', [
      { minIntervalMs: 0, maxPerDay: null },
      { minIntervalMs: 0, maxPerDay: 3 },
    ]);
    const st = (await store.load(stateKey('chatgpt', 'cap-plan'))) as { detailToday: { cap?: number } };
    expect(st.detailToday.cap).toBe(3);
    expect(report.stopped).toBe('queue-empty');   // nothing to fetch, but the cap is what it is
  });
});

// ===========================================================================
// 4 · The backoff with jitter
// ===========================================================================

describe('W16-5 · the transient backoff gains jitter and keeps its cap', () => {
  it('🔴 the delay stays inside [0.5, 1.0] x the exponential, and under the cap, at every attempt', () => {
    const random = seeded(1234);
    for (const reason of ['transport-error', 'rate-limited'] as const) {
      const base = TRANSIENT_RETRY_BASE_MS[reason];
      const cap = TRANSIENT_RETRY_MAX_MS[reason];
      for (let attempt = 1; attempt <= 40; attempt += 1) {
        const exponential = base * 2 ** Math.min(Math.max(1, attempt) - 1, 40);
        const floor = 0.5 * exponential;
        const ceiling = Math.min(exponential, cap);
        for (let i = 0; i < 20; i += 1) {
          const d = transientRetryDelayMs(reason, attempt, random);
          expect(d, `${reason} attempt ${attempt}`).toBeGreaterThanOrEqual(Math.min(floor, ceiling));
          expect(d, `${reason} attempt ${attempt}`).toBeLessThanOrEqual(ceiling);
          expect(d, `${reason} attempt ${attempt}`).toBeLessThanOrEqual(cap);
        }
      }
    }
  });

  it('🔴 both ends of the band are exact: half the exponential, and the exponential itself', () => {
    for (const reason of ['transport-error', 'rate-limited'] as const) {
      const base = TRANSIENT_RETRY_BASE_MS[reason];
      const cap = TRANSIENT_RETRY_MAX_MS[reason];
      // Attempt 1 is below the cap for both ladders, so the band is exact here —
      // this is the C19/C23 number (`5 * 60_000`) halved and whole.
      expect(base).toBeLessThan(cap);
      expect(transientRetryDelayMs(reason, 1, fixed(0))).toBe(base / 2);
      expect(transientRetryDelayMs(reason, 1, fixed(1))).toBe(base);
      // Once the exponential passes the cap the band collapses onto it: a long
      // streak must not be able to jitter its way below the ceiling.
      expect(transientRetryDelayMs(reason, 40, fixed(0))).toBe(cap);
      expect(transientRetryDelayMs(reason, 40, fixed(1))).toBe(cap);
    }
  });

  it('a jittered delay is never longer than the un-jittered one — full jitter only lowers', () => {
    /**
     * The un-jittered ladder is `transientRetryDelayMs(…, fixed(1))` — the top of
     * the band is the old deterministic value, exactly. So this asserts the whole
     * band sits at or below the number C19 shipped, which is what makes "the
     * backoff never gets *more* patient" a checked fact rather than a claim.
     */
    const random = seeded(555);
    for (const reason of ['transport-error', 'rate-limited'] as const) {
      for (let attempt = 1; attempt <= 20; attempt += 1) {
        const unjittered = transientRetryDelayMs(reason, attempt, fixed(1));
        for (let i = 0; i < 10; i += 1) {
          expect(transientRetryDelayMs(reason, attempt, random)).toBeLessThanOrEqual(unjittered);
        }
      }
    }
  });
});

// ===========================================================================
// 5 · The chain can never stay broken, and a healthy chain is never ticked
//
// Everything below enters through defineBackground's real callbacks: the alarm
// listener, the storage-change listener and syncAlarmWithSwitch are production
// code. Only the network is replaced (the engine's http port is injected), so
// not one request really goes out.
// ===========================================================================

const ENABLED_KEY = 'cs_backfill_enabled_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';

const stored: Record<string, unknown> = {};
const alarmBook = new Map<string, { periodInMinutes?: number; delayInMinutes?: number }>();
const alarmCreates: string[] = [];
const alarmClears: string[] = [];
const alarmListeners: Array<(alarm: { name?: string }) => void> = [];
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const storageChangeListeners: Array<(changes: Record<string, { newValue?: unknown }>, area: string) => void> = [];
const listCalls: string[] = [];

const fakeBrowser: any = {
  runtime: {
    id: 'w16-test-extension',
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    onStartup: { addListener() {} },
  },
  storage: {
    onChanged: { addListener(fn: any) { storageChangeListeners.push(fn); } },
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...stored };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in stored ? stored[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(stored, values); },
      async remove(keys: string[]) { for (const k of keys) delete stored[k]; },
    },
  },
  alarms: {
    create(name: string, info: { periodInMinutes?: number; delayInMinutes?: number }) {
      alarmBook.set(name, info);
      alarmCreates.push(name);
    },
    async clear(name: string) {
      const had = alarmBook.delete(name);
      alarmClears.push(name);
      return had;
    },
    async get(name: string) { return alarmBook.get(name); },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    // Deliberately unable to fetch: this file must not depend on a tab, and a
    // tick that gets this far has a bug of its own.
    async sendMessage() { throw new Error('no live tab in W16'); },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
};

/** The runtime seam's clock. This section never sleeps, so a constant "now" is enough. */
const runtimeClock = { now: () => 1_700_000_000_000, sleep: async () => { /* virtual */ } };

/**
 * Import background, inject the seams **before** `defineBackground` runs (the
 * startup `syncAlarmWithSwitch` uses the draw source, so it has to be in place
 * first), then await the async setup.
 */
async function boot(drawSource: RandomFn): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({
    clock: runtimeClock,
    random: drawSource,
  });
  mod.configureBackfillTransport(async () => {
    listCalls.push('list');
    return { status: 200, text: '{"items":[],"total":0}' };
  });
  expect(mod.default()).toBeUndefined();
  await mod.backgroundSetupSettled();
  return mod;
}

function fireAlarm(name: string): void {
  for (const fn of alarmListeners) fn({ name });
}

async function waitForAsyncEvent(): Promise<void> {
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
}

beforeEach(() => {
  for (const k of Object.keys(stored)) delete stored[k];
  alarmBook.clear();
  alarmCreates.length = 0;
  alarmClears.length = 0;
  alarmListeners.length = 0;
  runtimeListeners.length = 0;
  storageChangeListeners.length = 0;
  listCalls.length = 0;
  vi.resetModules();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

describe('W16-4 · the chain is never broken, and a healthy chain is never ticked', () => {
  it('🔴 a worker killed before re-arming: the watchdog’s wake leaves an alarm armed again', async () => {
    stored[ENABLED_KEY] = true;
    stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
    const mod = await boot(fixed(0));

    // The switch is on, so boot armed both alarms.
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
    expect(alarmBook.has(BACKFILL_SAFETY_ALARM_NAME)).toBe(true);

    // ---- simulate the service worker being reclaimed mid-tick ----
    // The browser consumed the one-shot when it fired and the re-arm at the end
    // of the tick never ran, so nothing is armed and nothing will ever wake us.
    alarmBook.delete(BACKFILL_ALARM_NAME);
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);

    // The next wake is the watchdog, which is held by the browser and therefore
    // survived. It must notice the broken chain and restore it.
    alarmCreates.length = 0;
    fireAlarm(BACKFILL_SAFETY_ALARM_NAME);
    await mod.backfillTickSettled();

    expect(alarmCreates).toContain(BACKFILL_ALARM_NAME);
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
  });

  it('🔴 a real tick re-arms the next one with a FRESH draw', async () => {
    stored[ENABLED_KEY] = true;
    stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
    let draw = 0;
    const mod = await boot(() => draw);

    // The boot-time arm used draw 0 ⇒ the floor.
    expect(alarmBook.get(BACKFILL_ALARM_NAME)).toEqual({
      delayInMinutes: BACKFILL_TICK_DELAY_MIN_MINUTES,
    });

    // A new draw for the next gap. If the re-arm reused the old number, or the
    // alarm had been left on a period, this would still read 5.
    draw = 1;
    fireAlarm(BACKFILL_ALARM_NAME);
    await mod.backfillTickSettled();

    expect(alarmBook.get(BACKFILL_ALARM_NAME)).toEqual({
      delayInMinutes: BACKFILL_TICK_DELAY_MAX_MINUTES,
    });
    // …and it really was a tick that ran (against the injected, empty backend),
    // not a re-arm bolted onto a no-op.
    expect(listCalls.length).toBeGreaterThan(0);
  });

  it('🔴 the watchdog does NOT tick a healthy chain: it adds no create and no request', async () => {
    stored[ENABLED_KEY] = true;
    stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
    const mod = await boot(fixed(0));

    // The chain is armed (the boot-time sync). A watchdog fire must therefore do
    // nothing at all — otherwise the leg would gain a fixed hourly tick, which
    // is exactly the periodic behaviour W16 removed.
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
    const creates = alarmCreates.slice();
    const requests = listCalls.length;

    fireAlarm(BACKFILL_SAFETY_ALARM_NAME);
    await mod.backfillTickSettled();

    expect(alarmCreates).toEqual(creates);
    expect(listCalls.length).toBe(requests);
  });

  it('🔴 the watchdog refuses to work with the switch off — no consent, no periodic behaviour', async () => {
    stored[ENABLED_KEY] = false;
    const mod = await boot(fixed(0));

    expect(await mod.runBackfillWatchdog()).toBe('disabled');
    // Nothing was armed while disabled, and the watchdog did not arm anything.
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
    expect(alarmBook.has(BACKFILL_SAFETY_ALARM_NAME)).toBe(false);
  });

  it('🔴 switch off clears BOTH alarms, through the real storage-change path', async () => {
    stored[ENABLED_KEY] = true;
    stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
    await boot(fixed(0));
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
    expect(alarmBook.has(BACKFILL_SAFETY_ALARM_NAME)).toBe(true);

    // The popup flips the switch: a real write to storage, which the real
    // onChanged listener turns into an alarm sync.
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), false);
    for (const fn of storageChangeListeners) fn({ [ENABLED_KEY]: { newValue: false } }, 'local');
    await waitForAsyncEvent();

    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
    expect(alarmBook.has(BACKFILL_SAFETY_ALARM_NAME)).toBe(false);
    expect(alarmClears).toContain(BACKFILL_ALARM_NAME);
    expect(alarmClears).toContain(BACKFILL_SAFETY_ALARM_NAME);
  });
});

// ===========================================================================
// 6 · The alarm's own lifecycle, at the module level
// ===========================================================================

describe('W16-6 · syncBackfillAlarm arms a one-shot tick and a fixed-period watchdog', () => {
  function register(): { api: AlarmsApi; book: Map<string, any>; creates: string[]; clears: string[] } {
    const book = new Map<string, any>();
    const creates: string[] = [];
    const clears: string[] = [];
    return {
      book,
      creates,
      clears,
      api: {
        create(name, info) { book.set(name, info); creates.push(name); },
        clear(name) { const had = book.delete(name); clears.push(name); return had; },
        async get(name) { return book.get(name); },
      },
    };
  }

  it('🔴 the tick alarm is a one-shot with a jittered delay, the watchdog a fixed period', async () => {
    const { api, book } = register();
    expect(await syncBackfillAlarm(api, true, fixed(0))).toBe('created');

    const tick = book.get(BACKFILL_ALARM_NAME);
    expect(tick.periodInMinutes).toBeUndefined();          // one-shot, or it would be a metronome
    expect(tick.delayInMinutes).toBe(BACKFILL_TICK_DELAY_MIN_MINUTES);
    expect(book.get(BACKFILL_SAFETY_ALARM_NAME)).toEqual({
      periodInMinutes: BACKFILL_SAFETY_PERIOD_MINUTES,     // fixed, or it could not survive a dead worker
    });

    // A second sync leaves both alone: every SW wake must not restart the countdown.
    expect(await syncBackfillAlarm(api, true, fixed(1))).toBe('kept');
    expect(book.get(BACKFILL_ALARM_NAME).delayInMinutes).toBe(BACKFILL_TICK_DELAY_MIN_MINUTES);
  });

  it('🔴 a missing tick alarm is re-armed on the next sync, with a fresh draw', async () => {
    const { api, book } = register();
    await syncBackfillAlarm(api, true, fixed(0));
    // The one-shot fired and the re-arm was skipped.
    book.delete(BACKFILL_ALARM_NAME);
    expect(await syncBackfillAlarm(api, true, fixed(1))).toBe('created');
    expect(book.get(BACKFILL_ALARM_NAME).delayInMinutes).toBe(BACKFILL_TICK_DELAY_MAX_MINUTES);
  });

  it('armBackfillTick returns the delay it used, so a caller never has to re-derive it', async () => {
    const { api, book } = register();
    const used = await armBackfillTick(api, fixed(0.5));
    expect(used).toBe((BACKFILL_TICK_DELAY_MIN_MINUTES + BACKFILL_TICK_DELAY_MAX_MINUTES) / 2);
    expect(book.get(BACKFILL_ALARM_NAME)).toEqual({ delayInMinutes: used });
  });

  it('no alarms API ⇒ "unavailable", never a pretended success', async () => {
    expect(await syncBackfillAlarm(null, true)).toBe('unavailable');
    expect(await syncBackfillAlarm(undefined, false)).toBe('unavailable');
  });
});

// `DEFAULT_ENUM_PACE` / `DEFAULT_DETAIL_PACE` are used above through SEGMENTS;
// this keeps the removal of a jitter band from passing unnoticed.
describe('W16-0 · the production pace plan really carries both bands', () => {
  it('both segments have a non-zero jitter band', () => {
    const enumBand = DEFAULT_ENUM_PACE.jitterMs ?? 0;
    const detailBand = DEFAULT_DETAIL_PACE.jitterMs ?? 0;
    expect(enumBand).toBeGreaterThan(0);
    expect(detailBand).toBeGreaterThan(0);
    // Documentation-shaped: the exact numbers the popup and the docs quote —
    // [2, 6] s per list page and [20, 45] s per body.
    expect(DEFAULT_ENUM_PACE.minIntervalMs + enumBand).toBe(6_000);
    expect(DEFAULT_DETAIL_PACE.minIntervalMs + detailBand).toBe(45_000);
  });
});
