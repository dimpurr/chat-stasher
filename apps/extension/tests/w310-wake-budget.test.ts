/**
 * W310 · **Each platform can reach its own daily cap again, and no account guard
 * moved to make that true.**
 *
 * ## The defect, stated as a rate budget
 *
 * A wake serves **one** platform (the walk serves the first runnable target and
 * stops), and W86c's fair rotation spreads the wakes over the `m` registered
 * runnable targets. So a platform's bodies/day was
 * `(1440 ÷ tickMean) ÷ m × bodiesPerTick`, while its own drawn cap is
 * `150–800`. At the pre-W310 band `[5, 10]` minutes that is
 * `(1440 ÷ 7.5) ÷ 5 ≈ 38` bodies/platform/day — an order of magnitude below even
 * the *gentle* cap's floor of 150. The cap was decorative; the alarm was the
 * brake. W309 measured the consequence: ChatGPT draining 7 107 conversations at
 * ~1.6–2 bodies/hour (~5 months).
 *
 * W310 divides the tick band by `m` (`[5, 10] ÷ 5 = [1, 2]`), so each platform's
 * *share* is back to the design's per-platform budget. This file simulates a
 * local day at both bands and pins the two facts that matter:
 *  1. the old band could not reach any cap, on its fastest possible day;
 *  2. the new band reaches the reachable cap bands, while the alarm still binds
 *     on a slow day — and every guard W310 was told not to touch still binds.
 *
 * ## What "simulated" means here, and what is deliberately not simulated
 *
 * Time is drawn, never slept: a day is a loop over `drawTickDelayMinutes` with an
 * injected draw, and the per-request pacing is exercised through the real
 * `Pacer` on a fake clock. The machine-wide arbiter is **modelled** (it lives in
 * `crates/chat-stasher/src/nativehost.rs`, which a node test cannot run); the
 * model is three lines and cites the Rust it mirrors, and the point it makes —
 * `faster`'s 600–800 band is cut to 400/day by the host, gentle and standard are
 * not — is a claim about the constants, not about the Rust's control flow.
 */

import { describe, expect, it } from 'vitest';

import {
  BACKFILL_TICK_DELAY_MAX_MINUTES,
  BACKFILL_TICK_DELAY_MIN_MINUTES,
  BACKFILL_TICK_MEAN_MINUTES,
  drawTickDelayMinutes,
  tickWalkOrder,
  type BackfillTarget,
  type TickCursor,
} from '../lib/backfill/alarm';
import { DEFAULT_DETAIL_PACE, Pacer, QUIET_DAILY_CAP_MIN, drawDailyCap, type Clock } from '../lib/backfill/pace';
import { SPEED_PLANS, type SpeedPreset } from '../lib/backfill/speed';
import type { RandomFn } from '../lib/backfill/random';

const DAY_MS = 86_400_000;
/** The stable channel's registered platforms: chatgpt, claude, deepseek, gemini, grok. */
const M_STABLE = 5;

/** The pre-W310 band, kept here as the control. A literal on purpose: the shipped
 * constants are the new ones, and the control must not move when they do. */
const OLD_TICK_FLOOR_MIN = 5;
const OLD_TICK_CEIL_MIN = 10;

const fixed = (v: number): RandomFn => () => v;

/** A fake clock: `sleep` only advances virtual time, so no test really waits. */
function fakeClock(startMs = Date.parse('2026-10-02T00:00:00.000Z')): Clock {
  let t = startMs;
  return {
    now: () => t,
    async sleep(ms: number) {
      t += ms;
    },
  };
}

/**
 * How many times the one-shot alarm is re-armed inside one local day, for a tick
 * whose gap is drawn from `[floor, ceil]` with the injected draw. The draw is
 * pinned, so this is a deterministic count and not a sample.
 */
function wakesInDay(draw: RandomFn, floorMin: number, ceilMin: number): number {
  let t = 0;
  let wakes = 0;
  for (;;) {
    const gapMs = (floorMin + draw() * (ceilMin - floorMin)) * 60_000;
    t += gapMs;
    if (t >= DAY_MS) return wakes;
    wakes += 1;
  }
}

const newWakes = (draw: RandomFn): number =>
  wakesInDay(draw, BACKFILL_TICK_DELAY_MIN_MINUTES, BACKFILL_TICK_DELAY_MAX_MINUTES);
const oldWakes = (draw: RandomFn): number => wakesInDay(draw, OLD_TICK_FLOOR_MIN, OLD_TICK_CEIL_MIN);

/**
 * The next cursor after one serve, mirroring `saveTickCursor`'s rule: rows
 * already stamped keep their relative order, never-served rows are materialised
 * behind them, and the row just served moves to the back (dense ranks `0..k-1`).
 */
function advanceCursor(
  targets: readonly BackfillTarget[],
  cursor: TickCursor | null,
  servedIndex: number,
): TickCursor {
  const keys = targets.map((t) => `${t.platform}\0${t.scope}`);
  const prev = cursor?.served ?? {};
  const stamped = keys.filter((k) => k in prev).sort((a, b) => prev[a]! - prev[b]!);
  const unstamped = keys.filter((k) => !(k in prev));
  const servedKey = keys[servedIndex]!;
  const order = [...stamped, ...unstamped].filter((k) => k !== servedKey);
  order.push(servedKey);
  const served: Record<string, number> = {};
  order.forEach((k, i) => {
    served[k] = i;
  });
  return { served, revision: (cursor?.revision ?? 0) + 1 };
}

/** What one platform receives in a day, given the day's wake count and its cap. */
function perPlatformDay(preset: SpeedPreset, wakes: number, capDraw: RandomFn, arbiterDailyCap: number) {
  const plan = SPEED_PLANS[preset];
  const band = plan.pace.detail.dailyCapBand!;
  const serves = Math.floor(wakes / M_STABLE);
  const cap = drawDailyCap(plan.pace.detail.maxPerDay, capDraw, band)!;
  const budget = serves * plan.tickDetails;
  return { serves, cap, band, budget, bodies: Math.min(budget, cap, arbiterDailyCap) };
}

// ===========================================================================
// 1 · The old band could never reach a cap — the defect, as a control
// ===========================================================================

describe('W310-1 · the pre-W310 band was the brake, not the cap', () => {
  it('🔴 even its fastest possible day left every platform below every cap band', () => {
    // The fastest day the old band can produce: every gap at the 5-minute floor.
    const wakes = oldWakes(fixed(0));
    expect(wakes).toBe(287);

    const serves = Math.floor(wakes / M_STABLE);
    expect(serves).toBe(57);

    // gentle: 57 bodies against a 150–200 cap → the cap can never bind.
    expect(serves * SPEED_PLANS.gentle.tickDetails).toBeLessThan(QUIET_DAILY_CAP_MIN);
    // standard: 114 against 300–400 → same.
    expect(serves * SPEED_PLANS.standard.tickDetails).toBeLessThan(300);
    // And slower draws only make it worse: the whole band is under every cap.
    expect(Math.floor(oldWakes(fixed(1)) / M_STABLE) * SPEED_PLANS.standard.tickDetails).toBeLessThan(300);
  });
});

// ===========================================================================
// 2 · The new band reaches the caps, and both brakes still bite
// ===========================================================================

describe('W310-2 · the W310 band makes the platform cap the brake again', () => {
  it('🔴 on a fast day the cap binds; on a slow day the alarm does — for gentle and standard', () => {
    const fastWakes = newWakes(fixed(0));
    const slowWakes = newWakes(fixed(1));
    expect(fastWakes).toBe(1439);
    expect(slowWakes).toBe(719);

    const fastServes = Math.floor(fastWakes / M_STABLE);
    const slowServes = Math.floor(slowWakes / M_STABLE);
    expect(fastServes).toBe(287);
    expect(slowServes).toBe(143);

    for (const preset of ['gentle', 'standard'] as const) {
      const plan = SPEED_PLANS[preset];
      const band = plan.pace.detail.dailyCapBand!;
      // The wake budget now overruns the cap on a fast day ⇒ the cap is the brake.
      expect(fastServes * plan.tickDetails, `${preset} fast`).toBeGreaterThanOrEqual(band.max);
      // …and underruns it on a slow day ⇒ the alarm is the brake. "Both brakes
      // bite, neither alone" survives, now at the per-platform level.
      expect(slowServes * plan.tickDetails, `${preset} slow`).toBeLessThan(band.min);
    }
  });

  it("🔴 the day's body count then equals the drawn cap, which lies in the band", () => {
    const fastWakes = newWakes(fixed(0));
    const { cap, band, bodies } = perPlatformDay('gentle', fastWakes, fixed(1), 400);
    expect(cap).toBe(band.max); // a draw of 1 rolls the top of the band
    expect(band.min).toBe(QUIET_DAILY_CAP_MIN);
    expect(bodies).toBe(cap);
  });

  it('🔴 the fair rotation still gives each platform exactly 1/m of the wakes', () => {
    const targets: BackfillTarget[] = Array.from({ length: M_STABLE }, (_, i) => ({
      platform: `p${i}`,
      origin: 'https://example.test',
      scope: 'default',
      at: i,
    }));
    let cursor: TickCursor | null = null;
    const seen: number[] = [];
    for (let w = 0; w < M_STABLE * 3; w += 1) {
      const served = tickWalkOrder(targets, cursor)[0]!;
      seen.push(served);
      cursor = advanceCursor(targets, cursor, served);
    }
    // Every block of m wakes is a permutation of the m platforms: no row is ever
    // skipped or served twice (the W86c bound, unchanged by W310).
    for (let round = 0; round < 3; round += 1) {
      expect(new Set(seen.slice(round * M_STABLE, (round + 1) * M_STABLE)).size).toBe(M_STABLE);
    }
  });
});

// ===========================================================================
// 3 · The account guards W310 was told not to touch — still bind
// ===========================================================================

describe('W310-3 · every account guard is untouched', () => {
  it('🔴 the per-request gap is still 20–45 s, on a simulated clock', async () => {
    const floor = fakeClock();
    const floorPacer = new Pacer(DEFAULT_DETAIL_PACE, floor, 'detail', null, fixed(0));
    expect(await floorPacer.gate()).toBe(0); // the first body of a wake never waits
    expect(await floorPacer.gate()).toBe(DEFAULT_DETAIL_PACE.minIntervalMs); // 20 s floor

    const ceil = fakeClock();
    const ceilPacer = new Pacer(DEFAULT_DETAIL_PACE, ceil, 'detail', null, fixed(1));
    await ceilPacer.gate();
    expect(await ceilPacer.gate()).toBe(DEFAULT_DETAIL_PACE.minIntervalMs + DEFAULT_DETAIL_PACE.jitterMs!); // 45 s ceiling
  });

  it('🔴 the machine-wide 400/day arbiter still binds where a preset would exceed it', () => {
    // nativehost.rs's coordination token: `detail_count >= 400` ⇒ wait, per
    // (machine, platform, account_key). It is per platform, so it does not
    // strangle a platform's own 300–400 cap — only `faster`'s 600–800 band.
    const ARBITER_DAILY_CAP = 400;
    const fastServes = Math.floor(newWakes(fixed(0)) / M_STABLE);

    const faster = SPEED_PLANS.faster;
    const fasterBudget = fastServes * faster.tickDetails;
    expect(fasterBudget).toBeGreaterThan(ARBITER_DAILY_CAP);
    expect(Math.min(fasterBudget, faster.pace.detail.dailyCapBand!.max, ARBITER_DAILY_CAP)).toBe(ARBITER_DAILY_CAP);

    for (const preset of ['gentle', 'standard'] as const) {
      expect(SPEED_PLANS[preset].pace.detail.dailyCapBand!.max).toBeLessThanOrEqual(ARBITER_DAILY_CAP);
    }
  });

  it('🔴 chrome.alarms: both bounds are whole minutes above its 30-second floor', () => {
    // Chrome clamps alarms to at most once every 30 s (Chrome 120+), so a
    // delayInMinutes below 0.5 would be silently raised and the draw would stop
    // meaning anything. The band is `[5, 10] ÷ m` with m = 5.
    expect(BACKFILL_TICK_DELAY_MIN_MINUTES).toBeGreaterThanOrEqual(0.5);
    expect(Number.isInteger(BACKFILL_TICK_DELAY_MIN_MINUTES)).toBe(true);
    expect(Number.isInteger(BACKFILL_TICK_DELAY_MAX_MINUTES)).toBe(true);
    expect(BACKFILL_TICK_DELAY_MIN_MINUTES).toBe(OLD_TICK_FLOOR_MIN / M_STABLE);
    expect(BACKFILL_TICK_DELAY_MAX_MINUTES).toBe(OLD_TICK_CEIL_MIN / M_STABLE);
    expect(BACKFILL_TICK_MEAN_MINUTES).toBe(1.5);
    expect(drawTickDelayMinutes(fixed(0))).toBe(BACKFILL_TICK_DELAY_MIN_MINUTES);
    expect(drawTickDelayMinutes(fixed(1))).toBe(BACKFILL_TICK_DELAY_MAX_MINUTES);
  });
});
