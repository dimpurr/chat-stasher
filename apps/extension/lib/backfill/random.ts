/**
 * W16 · **The one source of randomness in the backfill leg.**
 *
 * The product requirement is that the backfill leg must be gentler than every
 * reference implementation *and never periodic*: "the frequency must not be
 * steady — it needs jitter". Before this file, `grep -rniE "jitter|Math\.random"
 * lib/backfill` matched nothing: the tick alarm ran on a fixed 5-minute period
 * and every pacer wait was an exact subtraction, so the leg was a metronome.
 *
 * 🔴 Why the randomness is a **parameter** rather than a direct `Math.random()`
 *    call at each site: a jittered interval cannot be asserted at all if the
 *    test cannot choose the draw. With one injected function, a test passes
 *    `() => 0` and every jittered value is exactly the documented floor, or
 *    `() => 1` and every one is exactly the documented ceiling — so "never below
 *    the minimum" stops being a statistical claim about many runs and becomes a
 *    single deterministic assertion about the two boundary values that matter.
 *
 * 🔴 The rule the whole change obeys: **jitter only ever adds delay.** Every
 *    call site passes a range whose `min` is today's exact documented minimum,
 *    so a draw can never move a request earlier than the number written down in
 *    the docs. `uniformBetween` enforces that at the arithmetic level too: a
 *    `random()` that returns `NaN`, a negative number or `1.5` is clamped into
 *    `[0, 1]` before scaling, so no hostile or broken source of randomness can
 *    push a value below `min` or above `max`.
 */

/** A source of randomness in `[0, 1)`. `Math.random` in production. */
export type RandomFn = () => number;

/** The production source of randomness. Nothing in this leg calls `Math.random` directly. */
export const systemRandom: RandomFn = () => Math.random();

/**
 * A value uniform in `[min, max]`.
 *
 * The draw is clamped into `[0, 1]` **before** it is scaled. That is not
 * defensive decoration: the one invariant this change must not break is "never
 * below a documented minimum", and an unclamped draw of, say, `-0.2` would
 * return a number below `min` from a function whose only documented promise is
 * that it does not. `NaN` lands on `min` (the drawing is `min + 0`), which is
 * the conservative direction.
 */
export function uniformBetween(random: RandomFn, min: number, max: number): number {
  const raw = random();
  const clamped = Number.isFinite(raw) ? Math.min(1, Math.max(0, raw)) : 0;
  return min + clamped * (max - min);
}
