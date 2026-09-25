/**
 * Throttling — it has to be an **observable number**, not "it feels a bit slower".
 *
 * Why enumeration and body-fetching are paced separately:
 *  · ChatGPT's conversation list is GET /backend-api/conversations?offset=&limit=,
 *    the response carries its own total, and 1000 conversations enumerate in about
 *    10 pages ⇒ enumeration is about 10 requests in total, which is cheap.
 *  · What really has to be "gentle" is the 1000 body fetches that follow.
 *  Using one rate for both would either drag enumeration out to needlessly slow,
 *  or make body-fetching look nothing like a person.
 *
 * The defaults and their reasoning (configurable; these are only the defaults):
 *  · enumerate minIntervalMs = 2000 (one page every 2 seconds)
 *      ⇒ 1000 conversations ≈ 10 pages ≈ 20 seconds to enumerate. Slower than a
 *        human scrolling the sidebar, and only 10 requests in total.
 *  · detail minIntervalMs = 20000 (one conversation every 20 seconds)
 *      ⇒ an order of magnitude slower than any real person clicking through
 *        conversations one after another; it cannot read as burst fetching.
 *  · detail maxPerDay = 400 (at most 400 a day; ADR-033 doubled the old 200)
 *      ⇒ 1000 conversations spread over 2.5–3.3 days, matching the product's
 *        "everything slowly gets indexed over several days"; 400 × 20s ≈ 133
 *        minutes of sparse activity a day, inside a normal user's daily range.
 *
 * ⚠️ These numbers are **defaults chosen by the arithmetic above**, **not**
 *    measurements of the platform's rate limits — we have no logged-in session
 *    and should not go probing for thresholds. The real threshold can only be
 *    caught by halting on 429.
 *
 * ---------------------------------------------------------------------------
 * 🔴 W16 · **And then the frequency is made irregular on top of those minimums.**
 *
 * An exact 20.000-second gap between bodies is not gentler than a 20-second
 * average — it is more *legible*. A fixed cadence is the thing a rate limiter
 * (and a human watching a log) picks out first, and not one of the reference
 * implementations we compared against adds any irregularity of its own: a token
 * bucket at 13 req/s, 400 ms per batch of 100, 400-600 ms per request, and the
 * gentlest of them at 3 s per request, ≤50 per run, every 60 minutes. So the
 * leg stays far below every one of them in *rate*, and this change is only
 * about the *shape* of the gaps.
 *
 * 🔴 Every jitter band here is **`[0, extra]` added to the existing minimum**,
 *    so its floor *is* the number documented above. The minimum is not replaced,
 *    not lowered, and not reinterpreted as a mean: with `random = () => 0` the
 *    waits are byte-for-byte the old ones, which is what lets the tests that
 *    pinned exact intervals keep asserting the same numbers.
 */

import { systemRandom, uniformBetween, type RandomFn } from './random';

/** An injectable clock: tests count sleeps on a fake clock instead of really waiting. */
export interface Clock {
  now(): number;
  sleep(ms: number): Promise<void>;
}

export const systemClock: Clock = {
  now: () => Date.now(),
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
};

export interface PacePlan {
  /** The minimum interval between two requests, in milliseconds. */
  minIntervalMs: number;
  /** The daily cap; null = no cap (which is what the enumeration segment uses). */
  maxPerDay: number | null;
  /**
   * 🔴 W16 · How much **extra** delay may be drawn on top of `minIntervalMs`.
   * The actual interval is `minIntervalMs + uniform[0, jitterMs]`, so the floor
   * is always `minIntervalMs` and this field can only ever make the leg slower.
   *
   * optional, and absent means **0 = no jitter**. That is deliberate: every
   * `pace` override in the test suite (and every caller that wants the old exact
   * behaviour) keeps compiling and keeps behaving byte-identically without
   * knowing this field exists. Only `DEFAULT_PACE` — the production plan —
   * carries a non-zero band.
   */
  jitterMs?: number;
  /**
   * 🔴 W113 · **The band the day's cap is drawn from**, for a plan whose ceiling
   * is not `DAILY_CAP_MAX` (ADR-032's speed presets, lib/backfill/speed.ts).
   *
   * It exists because `drawDailyCap` used to hard-code `[DAILY_CAP_MIN,
   * DAILY_CAP_MAX]` and then clamp to `DAILY_CAP_MAX`. A preset that only lowered
   * `maxPerDay` would therefore not get a *drawn* cap at all: the roll would come
   * out somewhere in 300–400 and be clamped to, say, 200 on every single day —
   * i.e. exactly the fixed, maximally predictable ceiling W16 added the draw to
   * remove. So the band travels with the plan.
   *
   * optional, and absent ⇒ `DEFAULT_DAILY_CAP_BAND`, which is the pair this file
   * already published. Every caller that does not know this field exists — which
   * is every caller before W113, including each `pace` override in the suite —
   * keeps the drawn values it had, character for character.
   */
  dailyCapBand?: DailyCapBand;
}

/**
 * W113 · The inclusive range a day's body cap is drawn from.
 *
 * 🔴 `band.max` is also the draw's hard ceiling, and `PacePlan.maxPerDay` is a second one: the value that
 *    reaches the engine is `min(rolled, band.max, maxPerDay)`. The two agreeing is what makes a preset's
 *    band its real limit rather than a wish (see `drawDailyCap`).
 */
export interface DailyCapBand {
  readonly min: number;
  readonly max: number;
}

export const DEFAULT_ENUM_PACE: PacePlan = {
  minIntervalMs: 2_000,
  maxPerDay: null,
  /**
   * One list page every 2-6 seconds. The list is about 10 cheap requests for a
   * whole account, so this segment does not need the wide band the bodies do;
   * the raw minimum is already slower than a human scrolling the sidebar, and
   * the point of the band is only that two pages never land 2.000 s apart.
   */
  jitterMs: 4_000,
};

export const DEFAULT_DETAIL_PACE: PacePlan = {
  minIntervalMs: 20_000,
  maxPerDay: 400,
  /**
   * One body every 20-45 seconds, drawn per body. This is the segment that
   * carries the 1000 expensive fetches, so it gets the wide band: the floor is
   * untouched (still an order of magnitude slower than any real person clicking
   * through conversations) and the ceiling stays far below the point where a
   * run of bodies would read as bulk fetching.
   */
  jitterMs: 25_000,
};

/**
 * 🔴 W16 · **The daily cap is drawn, not fixed.**
 *
 * The daily cap is the brake that actually governs this leg's rate (the alarm
 * only wakes it), so of all the knobs it is the one worth making unsteady: a
 * cap of exactly 400 every single day is the most predictable number the leg
 * publishes. It is drawn uniformly from `[DAILY_CAP_MIN, DAILY_CAP_MAX]` once
 * per local day and persisted with the day's counter.
 *
 * 🔴 It can only ever come out **at or below** `DAILY_CAP_MAX` — which is the
 *    plan's own `maxPerDay` — so this can never raise the rate. `drawDailyCap`
 *    takes the plan's ceiling and clamps to it, which also means an explicit
 *    small `maxPerDay` (every test that uses one) is still the hard ceiling and
 *    is not silently raised to `DAILY_CAP_MIN` by the roll.
 *
 * 🔴 ADR-033 · The band doubled on 2026-09-24: it used to be `[150, 200]`, and
 *    the drawn ceiling used to be 200. The old pair survives as
 *    `QUIET_DAILY_CAP_MIN` / `QUIET_DAILY_CAP_MAX` for ADR-032's quiet preset;
 *    the preset itself is not built here, and no caller reads them yet.
 */
export const DAILY_CAP_MIN = 300;
export const DAILY_CAP_MAX = 400;

/** ADR-032 · The values the drawn cap had before ADR-033 doubled it. The quiet preset's band, kept for the future preset. */
export const QUIET_DAILY_CAP_MIN = 150;
export const QUIET_DAILY_CAP_MAX = 200;

/**
 * The band a plan that does not name one draws from: the pair ADR-033 set.
 *
 * 🔴 It is the *default argument* below rather than a hard-coded pair inside the function, so that "no band
 *    named" and "this band" are the same code path — a separate branch is a second place for the two to
 *    disagree, and the disagreement would be silent in the direction that raises the rate.
 */
export const DEFAULT_DAILY_CAP_BAND: DailyCapBand = { min: DAILY_CAP_MIN, max: DAILY_CAP_MAX };

/** The day's cap: uniform in `band`, clamped to the plan's ceiling. `null` ⇒ nothing to draw. */
export function drawDailyCap(
  maxPerDay: number | null,
  random: RandomFn,
  band: DailyCapBand = DEFAULT_DAILY_CAP_BAND,
): number | null {
  if (maxPerDay === null) return null;
  // `+ 1` then floor makes the top of the range inclusive: random()=0 ⇒ band.min, random()→1 ⇒ band.max.
  const rolled = Math.floor(uniformBetween(random, band.min, band.max + 1));
  // The plan's ceiling always wins over the roll, so a smaller maxPerDay is never raised.
  // `band.max` is applied here too, so the function never returns more than the band's
  // own ceiling on its own, whatever ceiling a future caller passes.
  return Math.max(0, Math.min(rolled, band.max, maxPerDay));
}

/**
 * 🔴 W127 · **A list-only plan's pages are paced at the detail segment's rhythm.**
 *
 * `LIST_PAGES_PER_TICK` (engine.ts) replaced `Infinity` with 8, which bounds how
 * many pages one alarm wake may read but does **not** bound the rate: eight pages
 * on `DEFAULT_ENUM_PACE` land 2-6 s apart, so the tick's peak is 8 requests in
 * 16-48 s — about **10-30 req/min**, against the 8-10 requests a whole account's
 * list ever needs and against the standard plan's derived peak of about 4 req/min
 * (28-RATE-LIMITS §3.3 item 3). The per-tick cap and the per-request interval are
 * different brakes, and only the second one bounds the rate.
 *
 * So a plan with no body segment — the one whose enumeration *is* its whole leg —
 * reads its pages on the **same floor and band as a body fetch**: 20-45 s apart,
 * which is about 1.3-3 req/min, the detail plan's own rhythm, and still well above
 * the 30 s `Retry-After` floor. The daily cap is `null` because list pages are not
 * counted against it (only bodies are; see `DEFAULT_DETAIL_PACE.maxPerDay`).
 *
 * 🔴 The numbers are **derived from `DEFAULT_DETAIL_PACE`**, not copied, so "the
 *    detail-plan rhythm" cannot drift away from the detail plan: if the detail
 *    floor moves, this moves with it. The tick is longer (8 pages ≈ 160-360 s) but
 *    the alarm is one-shot and drawn after the tick ends (alarm.ts), so it cannot
 *    overlap the next wake.
 */
export const DEFAULT_LIST_ONLY_ENUM_PACE: PacePlan = {
  minIntervalMs: DEFAULT_DETAIL_PACE.minIntervalMs,
  maxPerDay: null,
  jitterMs: DEFAULT_DETAIL_PACE.jitterMs,
};

export interface BackfillPace {
  enumerate: PacePlan;
  detail: PacePlan;
  /**
   * 🔴 W127 · Optional and absent ⇒ `DEFAULT_LIST_ONLY_ENUM_PACE`. It is a separate
   * seat so a test (or a future preset) can slow list-only enumeration without
   * touching the one-page-per-tick rhythm every body-bearing plan already has.
   */
  listOnlyEnumerate?: PacePlan;
}

export const DEFAULT_PACE: BackfillPace = {
  enumerate: DEFAULT_ENUM_PACE,
  detail: DEFAULT_DETAIL_PACE,
  listOnlyEnumerate: DEFAULT_LIST_ONLY_ENUM_PACE,
};

/**
 * The throttler for one segment. The first request does not wait (there is no
 * "previous one" to speak of); after that it makes up the full minimum interval.
 * It records waits/totalWaitedMs so that "the throttling really took effect" is a
 * number that can be pasted into a report.
 *
 * 🔴 C19 · BUG-3's fix is this one parameter, `seedLastAt`:
 *    A Pacer is per-run (runBackfill constructs a new one each time), and at
 *    runtime a tick clears only 1 debt ⇒ every gate() call was that run's first
 *    ⇒ identically 0 ⇒ the 20-second minimum interval written in pace.ts
 *    **never once took effect in the browser** (measured in C17-3.B2).
 *    Here the caller feeds in "the moment of the last real fetch" (stored in
 *    BackfillState.lastFetchAt, surviving across ticks and restarts) as the seed,
 *    and the interval takes effect across ticks.
 *    seedLastAt = null ⇒ behaviour byte-identical to C11 (a genuine "we have never
 *    fetched anything").
 *
 * 🔴 W16 · `random` is the injected source of randomness for the gap. It is the
 *    **last** parameter so that every existing `new Pacer(plan, clock, label,
 *    seed)` keeps its exact meaning; production passes nothing and gets
 *    `Math.random`, tests pass `() => 0` and get the documented minimum.
 */
export class Pacer {
  private last: number | null;
  readonly waits: number[] = [];

  constructor(
    readonly plan: PacePlan,
    private readonly clock: Clock,
    readonly label: string,
    seedLastAt: number | null = null,
    private readonly random: RandomFn = systemRandom,
  ) {
    this.last = seedLastAt;
  }

  /** The moment of the last "go-ahead". The caller must persist it for the interval to survive across ticks. */
  get lastAt(): number | null {
    return this.last;
  }

  get totalWaitedMs(): number {
    return this.waits.reduce((a, b) => a + b, 0);
  }

  /** Call before every real request. Returns the milliseconds actually waited. */
  async gate(): Promise<number> {
    const now = this.clock.now();
    if (this.last === null) {
      this.last = now;
      this.waits.push(0);
      return 0;
    }
    const elapsed = now - this.last;
    /**
     * 🔴 W16 · The interval is `minIntervalMs + a fresh uniform draw`, not the
     * bare minimum. Two properties are load-bearing:
     *
     *  · the draw is added **on top of** the minimum, never mixed into it, so
     *    the floor is exactly the documented number and the band can only ever
     *    widen the gap. `jitterMs` absent/0 ⇒ this line reduces to the C19 line
     *    `minIntervalMs - elapsed` character for character.
     *  · the draw is taken **per gate, not per run** — a Pacer lives for one run
     *    and at runtime a run clears a single debt, but the anchor it measures
     *    from is persisted, so the interval that actually lands between two real
     *    requests is the jittered one, on every tick, not once per run.
     *
     * `wait` can still be 0: when the elapsed time already exceeds the drawn
     * interval (an alarm waking after a longer pause) there is nothing to make
     * up. That is the same `Math.max(0, …)` as before and is not a shortfall —
     * the *gap between two requests* is what is bounded, and it is.
     */
    const interval = this.plan.minIntervalMs + uniformBetween(this.random, 0, this.plan.jitterMs ?? 0);
    const wait = Math.max(0, interval - elapsed);
    if (wait > 0) await this.clock.sleep(wait);
    this.last = this.clock.now();
    this.waits.push(wait);
    return wait;
  }
}
