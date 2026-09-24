/**
 * W113 · ADR-032 §3 — the three speed presets.
 *
 * What is being pinned here, and why each one is worth pinning:
 *
 *  1. **Which stored values are believed.** `presetFrom` accepts exactly three names. Every other shape —
 *     absent, null, a number, an object, a name from a newer build — answers the default. The failure this
 *     prevents is the one that costs the platform something: coercing an unreadable value into whichever
 *     preset is nearest, which can only ever move the rate **up**.
 *  2. **Which numbers each preset is.** Gentle is the pair ADR-033 moved away from, standard is the pair it
 *     moved to, and both are read from the constants W111 left behind for exactly this purpose — so the
 *     test fails if either side drifts from the decision record rather than from a literal typed twice.
 *  3. 🔴 **The shape invariant.** All three presets must keep the *same* per-request intervals and the same
 *     jitter. This is ADR-033's own boundary — the totals move, the burst shape does not — and it is what makes
 *     "faster" mean "more per day", never "burstier". A future edit that shortened the detail gap for one
 *     preset would break this test rather than ship.
 *  4. **The cap stays drawn, and drawn inside the preset's band.** A preset that only lowered the ceiling
 *     would produce a fixed cap (pace.ts's `dailyCapBand` note); a stored cap from another preset must not
 *     be able to raise the rate (W16's rule).
 *  5. **The stored round trip**, including the read-back: `writeSpeedPreset` reports what was *persisted*,
 *     not what it hoped for.
 *
 * This file does not touch the network, the DOM, or `storage.local`: every case runs against
 * `lib/backfill/store.ts`'s in-memory store.
 */

import { describe, it, expect } from 'vitest';
import { memoryStore } from '../lib/backfill/store';
import {
  DEFAULT_SPEED_PRESET,
  presetFrom,
  readSpeedPlan,
  readSpeedPreset,
  SPEED_PLANS,
  SPEED_PRESET_KEY,
  SPEED_PRESET_ORDER,
  speedPlanFor,
  writeSpeedPreset,
  type SpeedPreset,
} from '../lib/backfill/speed';
import {
  DAILY_CAP_MAX,
  DAILY_CAP_MIN,
  DEFAULT_PACE,
  drawDailyCap,
  QUIET_DAILY_CAP_MAX,
  QUIET_DAILY_CAP_MIN,
} from '../lib/backfill/pace';
import { DEFAULT_TICK_DETAILS, QUIET_TICK_DETAILS } from '../lib/backfill/schedule';

const ALL: readonly SpeedPreset[] = SPEED_PRESET_ORDER;

describe('W113 · the preset table', () => {
  it('offers exactly three presets, quietest first', () => {
    expect([...SPEED_PRESET_ORDER]).toEqual(['gentle', 'standard', 'faster']);
    expect(Object.keys(SPEED_PLANS).sort()).toEqual(['faster', 'gentle', 'standard']);
  });

  it('🔴 the default is gentle, as ADR-032 §3 and the W114 dispatch both say', () => {
    expect(DEFAULT_SPEED_PRESET).toBe('gentle');
    // The default is also what an unknown value answers — the two must not drift apart, or "we could not
    // read your choice" and "you chose gentle" would be different rates.
    expect(presetFrom(undefined)).toBe(DEFAULT_SPEED_PRESET);
  });

  /**
   * 🔴 Gentle is the pair ADR-033 moved away from and W111 kept for this preset. Asserting against the
   *    constants rather than against 150/200 is the point: if someone moves either side, this fails.
   */
  it('gentle is the pre-ADR-033 pair, read from the constants kept for it', () => {
    const plan = SPEED_PLANS.gentle;
    expect(plan.tickDetails).toBe(QUIET_TICK_DETAILS);
    expect(plan.pace.detail.maxPerDay).toBe(QUIET_DAILY_CAP_MAX);
    expect(plan.pace.detail.dailyCapBand).toEqual({ min: QUIET_DAILY_CAP_MIN, max: QUIET_DAILY_CAP_MAX });
    expect(plan.carriesRisk).toBe(false);
    // The two pairs must actually differ, or "three presets" would be one preset with three names.
    expect(QUIET_DAILY_CAP_MAX).toBeLessThan(DAILY_CAP_MIN);
  });

  it('standard is the ADR-033 pair, read from the constants W111 shipped', () => {
    const plan = SPEED_PLANS.standard;
    expect(plan.tickDetails).toBe(DEFAULT_TICK_DETAILS);
    expect(plan.pace.detail.maxPerDay).toBe(DAILY_CAP_MAX);
    expect(plan.pace.detail.dailyCapBand).toEqual({ min: DAILY_CAP_MIN, max: DAILY_CAP_MAX });
    expect(plan.carriesRisk).toBe(false);
  });

  it('faster is above standard on both knobs and carries the risk note', () => {
    const { standard, faster } = SPEED_PLANS;
    expect(faster.carriesRisk).toBe(true);
    expect(faster.tickDetails).toBeGreaterThan(standard.tickDetails);
    expect(faster.pace.detail.maxPerDay!).toBeGreaterThan(standard.pace.detail.maxPerDay!);
    expect(faster.pace.detail.dailyCapBand!.min).toBeGreaterThanOrEqual(standard.pace.detail.dailyCapBand!.max);
    // Only the one preset may warn; a warning on all three is a warning on none.
    expect(ALL.filter((p) => SPEED_PLANS[p].carriesRisk)).toEqual(['faster']);
  });

  it('the presets are strictly ordered: each is faster than the one before, on both knobs', () => {
    const details = ALL.map((p) => SPEED_PLANS[p].tickDetails);
    const caps = ALL.map((p) => SPEED_PLANS[p].pace.detail.maxPerDay!);
    for (let i = 1; i < ALL.length; i += 1) {
      expect(details[i]!).toBeGreaterThan(details[i - 1]!);
      expect(caps[i]!).toBeGreaterThan(caps[i - 1]!);
    }
  });

  /**
   * 🔴 ADR-033's boundary, as a test. `faster` raises the totals; it must not touch the *shape* of the
   *    traffic. Every field that decides "how close together two requests may be" is compared against
   *    `DEFAULT_PACE`, which is the plan the engine shipped before presets existed — so this also pins
   *    that introducing presets changed nothing about pacing for the presets that do not mean to change it.
   */
  it('🔴 every preset keeps the shipped per-request intervals and jitter (totals move, shape does not)', () => {
    for (const preset of ALL) {
      const { detail, enumerate } = SPEED_PLANS[preset].pace;
      expect(detail.minIntervalMs, preset).toBe(DEFAULT_PACE.detail.minIntervalMs);
      expect(detail.jitterMs, preset).toBe(DEFAULT_PACE.detail.jitterMs);
      expect(enumerate.minIntervalMs, preset).toBe(DEFAULT_PACE.enumerate.minIntervalMs);
      expect(enumerate.jitterMs, preset).toBe(DEFAULT_PACE.enumerate.jitterMs);
      // The list segment is capped by neither a day nor a preset (pace.ts: enumeration has no maxPerDay),
      // and a preset must not quietly give it one.
      expect(enumerate.maxPerDay, preset).toBeNull();
      expect(enumerate.dailyCapBand, preset).toBeUndefined();
    }
  });
});

describe('W113 · what a stored value is allowed to mean', () => {
  it('accepts exactly the three names', () => {
    for (const preset of ALL) expect(presetFrom(preset)).toBe(preset);
    // Written the way a JSON round trip would give them back, for the same reason.
    expect(presetFrom(JSON.parse('"standard"'))).toBe('standard');
  });

  it('🔴 anything else answers the default rather than the nearest guess', () => {
    const strangers: unknown[] = [
      undefined, null, 0, 1, '', ' ', 'gentle ', 'GENTLE', 'Gentle', 'standard\n', 'faster!',
      'quiet', 'normal', 'turbo', {}, [], { preset: 'faster' }, true, Number.NaN,
    ];
    for (const raw of strangers) {
      expect(presetFrom(raw), JSON.stringify(raw) ?? 'undefined').toBe(DEFAULT_SPEED_PRESET);
    }
    // The direction that matters, spelled out: nothing unrecognised may select the risky preset.
    expect(strangers.map(presetFrom)).not.toContain('faster');
  });

  it('speedPlanFor and readSpeedPlan agree with presetFrom', () => {
    for (const preset of ALL) expect(speedPlanFor(preset)).toBe(SPEED_PLANS[preset]);
    expect(speedPlanFor('nonsense')).toBe(SPEED_PLANS[DEFAULT_SPEED_PRESET]);
  });
});

describe('W113 · reading and writing the choice', () => {
  it('an install with no stored choice runs at the shipped default', async () => {
    const store = memoryStore();
    expect(await readSpeedPreset(store)).toBe(DEFAULT_SPEED_PRESET);
    expect((await readSpeedPlan(store)).tickDetails).toBe(SPEED_PLANS[DEFAULT_SPEED_PRESET].tickDetails);
  });

  it('an unreadable stored value is the default, not a refusal and not a guess', async () => {
    const store = memoryStore({ [SPEED_PRESET_KEY]: 'warp' });
    expect(await readSpeedPreset(store)).toBe(DEFAULT_SPEED_PRESET);
  });

  it('the choice round-trips, and the write reports what was persisted', async () => {
    const store = memoryStore();
    for (const preset of ALL) {
      expect(await writeSpeedPreset(store, preset)).toBe(preset);
      expect(store.data[SPEED_PRESET_KEY]).toBe(preset);
      expect(await readSpeedPreset(store)).toBe(preset);
      expect((await readSpeedPlan(store)).tickDetails).toBe(SPEED_PLANS[preset].tickDetails);
    }
  });

  it('a store that cannot hold the choice is not quietly treated as having accepted it', async () => {
    // A store whose write throws must reach the caller: the control that shows "faster is on" would
    // otherwise be showing a rate nothing is running at.
    const store = memoryStore();
    store.save = async () => { throw new Error('quota'); };
    await expect(writeSpeedPreset(store, 'faster')).rejects.toThrow('quota');
    expect(await readSpeedPreset(store)).toBe(DEFAULT_SPEED_PRESET);
  });
});

describe('W113 · the daily cap under each preset', () => {
  it('🔴 is drawn inside the preset\'s own band, and reaches both ends of it', () => {
    for (const preset of ALL) {
      const { maxPerDay, dailyCapBand: band } = SPEED_PLANS[preset].pace.detail;
      expect(band, preset).toBeDefined();
      // random() = 0 and random() → 1 are the two boundaries: the band's own ends, not the global pair.
      expect(drawDailyCap(maxPerDay, () => 0, band), preset).toBe(band!.min);
      expect(drawDailyCap(maxPerDay, () => 1, band), preset).toBe(band!.max);
      // A mid draw stays inside.
      const mid = drawDailyCap(maxPerDay, () => 0.5, band)!;
      expect(mid, preset).toBeGreaterThanOrEqual(band!.min);
      expect(mid, preset).toBeLessThanOrEqual(band!.max);
    }
  });

  it('gentle draws below standard\'s floor, which is the whole point of having both', () => {
    const gentle = SPEED_PLANS.gentle.pace.detail;
    const standard = SPEED_PLANS.standard.pace.detail;
    expect(drawDailyCap(gentle.maxPerDay, () => 1, gentle.dailyCapBand))
      .toBeLessThan(drawDailyCap(standard.maxPerDay, () => 0, standard.dailyCapBand)!);
  });

  it('🔴 a stored cap from another preset cannot raise this one\'s rate', () => {
    for (const preset of ALL) {
      const { maxPerDay, dailyCapBand: band } = SPEED_PLANS[preset].pace.detail;
      // "stored" is clamped by the engine as `min(storedCap, maxPerDay)`; here the draw itself must also
      // refuse to exceed either the band or the ceiling, whatever a caller passes.
      expect(drawDailyCap(10, () => 1, band), preset).toBe(10);
      expect(drawDailyCap(maxPerDay, () => 1, band), preset).toBe(band!.max);
    }
  });

  it('a plan that names no band keeps the band this file published before presets existed', () => {
    // The default argument is what every pre-W113 caller relies on; it must stay the ADR-033 pair.
    expect(drawDailyCap(DAILY_CAP_MAX, () => 0)).toBe(DAILY_CAP_MIN);
    expect(drawDailyCap(DAILY_CAP_MAX, () => 1)).toBe(DAILY_CAP_MAX);
  });
});
