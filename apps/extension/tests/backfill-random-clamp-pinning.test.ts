/**
 * W612 · **The exact clamp mapping, pinned.**
 *
 * `lib/backfill/random.ts` carries a promise in prose: the draw is clamped into
 * `[0, 1]` **before** it is scaled, so no hostile or broken source of randomness
 * can push a value below `min` or above `max`, and `NaN` lands on `min` — the
 * conservative direction. That promise is the whole reason the function exists:
 * "jitter only ever adds delay" is a claim about every draw, not about the draws
 * a well-behaved `Math.random` happens to make.
 *
 * 🔴 **Why this file exists when `tests/w3-jitter.test.ts` already has one.**
 *     That assertion is *containment*:
 *
 *         for (const r of [-1, 0, 0.25, 0.5, 0.999, 1, 2, NaN]) {
 *           const v = uniformBetween(fixed(r), 10, 20);
 *           expect(v).toBeGreaterThanOrEqual(10);
 *           expect(v).toBeLessThanOrEqual(20);
 *         }
 *
 *     Every draw it lists satisfies it, and so does a mapping that clamps to the
 *     *wrong* end of the range, one that reflects instead of clamping, one that
 *     folds `min` and `max` together, and one that special-cases `NaN` to `max`.
 *     Containment is the property the leg needs; it is not the property the file
 *     documents, and a change that kept the first while breaking the second would
 *     leave the suite green. So the mapping itself is asserted here, exactly,
 *     per named draw.
 *
 * 🔴 **The two things a containment test cannot see, named.**
 *     · *Which* end an out-of-range draw lands on. `-0.2 → min` and `-0.2 → max`
 *       both satisfy `v >= 10 && v <= 20`, and they are opposite behaviours: one
 *       keeps the documented floor, the other silently jumps to the ceiling.
 *     · *What an out-of-range draw becomes.* An unclamped `2` gives `30`, which
 *       containment does catch; the mapping is pinned anyway, because the next
 *       shape — a clamp applied after the scaling, `min(max, 30)` — also gives
 *       20 and is a different function with a different failure mode on an
 *       inverted range. This file names the value, not the bound.
 *
 * The `(min, max)` is fixed at `(10, 20)` throughout, so a failure names a
 * mapping rather than a range. Every draw is synthetic and injected; nothing
 * here reads the real clock, the network, or any user data.
 */

import { readdirSync, readFileSync } from 'node:fs';
import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  BACKFILL_TICK_DELAY_MAX_MINUTES,
  BACKFILL_TICK_DELAY_MIN_MINUTES,
  drawTickDelayMinutes,
} from '../lib/backfill/alarm';
import { systemRandom, uniformBetween, type RandomFn } from '../lib/backfill/random';
import {
  TAB_HELLO_MAX_INTERVAL_MS,
  TAB_HELLO_MIN_INTERVAL_MS,
  drawHelloDelayMs,
} from '../lib/backfill/tab-hello';

/** The fixed range every mapping assertion below is made against. */
const MIN = 10;
const MAX = 20;

/** A draw source that always returns the same value — a boundary named in the assertion. */
const draw = (value: number): RandomFn => () => value;

afterEach(() => {
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// 1 · The documented mapping, one named draw at a time
// ---------------------------------------------------------------------------

describe('W612 · uniformBetween clamps before it scales', () => {
  it('🔴 draw 0 is exactly min and draw 1 is exactly max', () => {
    // The two boundary values the rest of the suite's exact numbers live at:
    // `random = () => 0` is how the pacing tests reproduce the documented
    // minimum interval, so this is that claim, stated directly.
    expect(uniformBetween(draw(0), MIN, MAX)).toBe(MIN);
    expect(uniformBetween(draw(1), MIN, MAX)).toBe(MAX);
  });

  it('🔴 a draw below 0 lands on min, and one above 1 on max — not on each other', () => {
    // Containment cannot tell these two apart; the direction is the whole point.
    // A floor that became a ceiling would still pass `v >= 10 && v <= 20`.
    expect(uniformBetween(draw(-0.2), MIN, MAX)).toBe(MIN);
    expect(uniformBetween(draw(1.5), MIN, MAX)).toBe(MAX);
    // And a little further out, so the mapping is a clamp and not a nudge.
    expect(uniformBetween(draw(-1_000), MIN, MAX)).toBe(MIN);
    expect(uniformBetween(draw(1_000), MIN, MAX)).toBe(MAX);
  });

  it('🔴 NaN lands on min — the conservative direction', () => {
    // Not `max`. `min + 0` is the drawing, and it is the only end of the range
    // that cannot make the leg faster than documented. `toBe` is an identity
    // comparison, so a `NaN` that leaked through fails here rather than
    // satisfying a comparison written the other way round.
    expect(uniformBetween(draw(Number.NaN), MIN, MAX)).toBe(MIN);
  });

  it('🔴 the infinities clamp by sign: +Infinity to max, -Infinity to min', () => {
    // An infinity has a sign, so it clamps like any other out-of-range draw:
    // `+Infinity` is a positive draw past `1` and lands on `max`, `-Infinity` a
    // negative draw below `0` and lands on `min`. `NaN` is the only non-finite
    // input with no position on the line, which is why it alone takes the
    // conservative floor. Stated exactly because propagating either infinity
    // would produce a delay of `Infinity` ms (a value that satisfies no interval
    // check downstream), and because `+Infinity` reaching `max` rather than
    // `min` is the difference between a clamp and an arbitrary fallback.
    expect(uniformBetween(draw(Number.POSITIVE_INFINITY), MIN, MAX)).toBe(MAX);
    expect(uniformBetween(draw(Number.NEGATIVE_INFINITY), MIN, MAX)).toBe(MIN);
    // No draw, hostile or broken, escapes the range — which is what
    // w3-jitter.test.ts's containment assertion covers, restated here so the two
    // halves of the promise live in one file.
    for (const hostile of [-1e308, -1, -0.2, 0, 0.5, 1, 1.5, 1e308, Number.NaN]) {
      const value = uniformBetween(draw(hostile), MIN, MAX);
      expect(value).toBeGreaterThanOrEqual(MIN);
      expect(value).toBeLessThanOrEqual(MAX);
      expect(Number.isFinite(value)).toBe(true);
    }
  });

  it('🔴 min === max returns that value for every draw, degenerate range included', () => {
    // A zero-width range is the case where the arithmetic would arrive at the
    // right answer by accident: `5 + anything × 0` is 5 whatever the draw is, so
    // this pins the *documented* answer rather than leaning on a coincidence. It
    // also covers the `NaN` draw, which is the one input that would otherwise
    // escape — `5 + NaN × 0` is `NaN`, not 5, unless the clamp runs first.
    for (const value of [0, 0.25, 1, -0.2, 1.5, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(uniformBetween(draw(value), 5, 5)).toBe(5);
    }
  });

  it('🔴 the clamp happens before the scaling, not after it', () => {
    // With `min = 10, max = 20` and a draw of 2, no clamp at all gives
    // `10 + 2 × 10 = 30`; clamping first gives `10 + 1 × 10 = 20`. Stated as a
    // value, because the order is only observable through it.
    expect(uniformBetween(draw(2), MIN, MAX)).toBe(MAX);
    expect(uniformBetween(draw(2), MIN, MAX)).not.toBe(30);

    // ⚠️ An honest limit of what a test can see here, so this is not read as more
    //    than it is: for a range with `min < max`, a clamp applied *after* the
    //    scaling (`max(min, min(max, …))`) returns the same endpoint for every
    //    out-of-range draw, because the out-of-range value saturates to the bound
    //    the pre-clamp would have selected anyway. The two orders therefore
    //    coincide on every value this file names, and what is really pinned is the
    //    mapping — "no draw escapes the range, and an out-of-range draw lands on
    //    the bound rather than being scaled past it". What a post-scaling clamp
    //    could *not* rescue is an inverted range, which no call site passes and
    //    which the function does not document, so nothing here asserts on it.
    //
    // The second range is the one an after-the-fact clamp could not even name a
    // bound for: `[0, 100]` with a draw of 2 is 200 unclamped, so the answer here
    // says the draw was capped before it was scaled.
    expect(uniformBetween(draw(2), 0, 100)).toBe(100);

    // And an interior draw must stay interior — a mapping that snapped every
    // input to an endpoint would still pass every assertion above.
    expect(uniformBetween(draw(0.5), MIN, MAX)).toBe(15);
    expect(uniformBetween(draw(0.25), MIN, MAX)).toBe(12.5);
  });

  it('preserves the documented endpoints of the bands the leg actually uses', () => {
    // The clamp is only interesting because real call sites pass real bands; this
    // pins that those bands' two ends are exactly reproducible, which is what
    // lets the rest of the suite assert exact millisecond numbers.
    expect(drawHelloDelayMs(draw(0))).toBe(TAB_HELLO_MIN_INTERVAL_MS);
    expect(drawHelloDelayMs(draw(1))).toBe(TAB_HELLO_MAX_INTERVAL_MS);
    expect(drawHelloDelayMs(draw(Number.NaN))).toBe(TAB_HELLO_MIN_INTERVAL_MS);
    expect(drawHelloDelayMs(draw(-0.2))).toBe(TAB_HELLO_MIN_INTERVAL_MS);
    expect(drawHelloDelayMs(draw(1.5))).toBe(TAB_HELLO_MAX_INTERVAL_MS);
    expect(drawTickDelayMinutes(draw(0))).toBe(BACKFILL_TICK_DELAY_MIN_MINUTES);
    expect(drawTickDelayMinutes(draw(1))).toBe(BACKFILL_TICK_DELAY_MAX_MINUTES);
    expect(drawTickDelayMinutes(draw(Number.NaN))).toBe(BACKFILL_TICK_DELAY_MIN_MINUTES);
    // The tick band's own spread is 1 minute, so a clamped ceiling is not a
    // round-number coincidence and the two ends are distinguishable.
    expect(BACKFILL_TICK_DELAY_MAX_MINUTES).toBeGreaterThan(BACKFILL_TICK_DELAY_MIN_MINUTES);
  });
});

// ---------------------------------------------------------------------------
// 2 · `systemRandom` is the production default, and it is the only one
// ---------------------------------------------------------------------------

/**
 * The source's own text, with comments removed, so a mention of `Math.random` in
 * prose does not read as a call. Block comments go first; the line-comment pass
 * refuses a `//` preceded by `:` so a URL's `https://` survives it.
 */
function codeOnly(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

const BACKFILL_DIR = new URL('../lib/backfill/', import.meta.url);

describe('W612 · one source of randomness for the backfill leg', () => {
  it('🔴 systemRandom is Math.random, and it is what an omitted argument gets', () => {
    // The default is asserted through a stub rather than by reading the source,
    // because "the default argument is `systemRandom`" is a claim about
    // behaviour and the argument list is where it is actually visible to a
    // caller that passes nothing.
    const stub = vi.spyOn(Math, 'random').mockReturnValue(0.25);
    expect(systemRandom()).toBe(0.25);
    // Call sites that take `random` as an optional/ defaulted parameter.
    expect(drawHelloDelayMs()).toBe(
      TAB_HELLO_MIN_INTERVAL_MS + 0.25 * (TAB_HELLO_MAX_INTERVAL_MS - TAB_HELLO_MIN_INTERVAL_MS),
    );
    expect(drawTickDelayMinutes()).toBe(
      BACKFILL_TICK_DELAY_MIN_MINUTES +
        0.25 * (BACKFILL_TICK_DELAY_MAX_MINUTES - BACKFILL_TICK_DELAY_MIN_MINUTES),
    );
    // A stubbed draw outside [0, 1] is still clamped, so even a hostile
    // replacement for the production source cannot escape the range.
    stub.mockReturnValue(-0.2);
    expect(systemRandom()).toBe(-0.2);
    expect(drawHelloDelayMs()).toBe(TAB_HELLO_MIN_INTERVAL_MS);
    expect(drawTickDelayMinutes()).toBe(BACKFILL_TICK_DELAY_MIN_MINUTES);
  });

  it('🔴 no module under lib/backfill calls Math.random directly', () => {
    // The structural half of "one source of randomness": `uniformBetween`'s clamp
    // is only the leg's single funnel if nothing bypasses it, and a direct
    // `Math.random()` at a call site is exactly that bypass — it draws outside
    // the clamp, so a hostile or broken source there would be uncaught.
    const files = readdirSync(BACKFILL_DIR)
      .filter((name) => name.endsWith('.ts'))
      .sort();
    expect(files).toContain('random.ts');
    // The one file allowed to name it is the one that wraps it.
    const offenders = files.filter((name) => {
      if (name === 'random.ts') return false;
      return codeOnly(readFileSync(new URL(name, BACKFILL_DIR), 'utf8')).includes('Math.random');
    });
    expect(offenders).toEqual([]);
  });

  it('🔴 random.ts wraps Math.random exactly once', () => {
    // A second wrapper would be a second source of randomness wearing the same
    // name, and the clamp in `uniformBetween` would only cover the one the leg
    // actually calls.
    const source = codeOnly(readFileSync(new URL('random.ts', BACKFILL_DIR), 'utf8'));
    expect(source.match(/Math\.random/g)).toHaveLength(1);
  });
});