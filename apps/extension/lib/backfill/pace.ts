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
 *  · detail maxPerDay = 200 (at most 200 a day)
 *      ⇒ 1000 conversations spread over exactly 5 days, matching the product's
 *        "everything slowly gets indexed over several days"; 200 × 20s ≈ 67
 *        minutes of sparse activity a day, inside a normal user's daily range.
 *
 * ⚠️ These numbers are **defaults chosen by the arithmetic above**, **not**
 *    measurements of the platform's rate limits — we have no logged-in session
 *    and should not go probing for thresholds. The real threshold can only be
 *    caught by halting on 429.
 */

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
}

export const DEFAULT_ENUM_PACE: PacePlan = { minIntervalMs: 2_000, maxPerDay: null };
export const DEFAULT_DETAIL_PACE: PacePlan = { minIntervalMs: 20_000, maxPerDay: 200 };

export interface BackfillPace {
  enumerate: PacePlan;
  detail: PacePlan;
}

export const DEFAULT_PACE: BackfillPace = {
  enumerate: DEFAULT_ENUM_PACE,
  detail: DEFAULT_DETAIL_PACE,
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
 */
export class Pacer {
  private last: number | null;
  readonly waits: number[] = [];

  constructor(
    readonly plan: PacePlan,
    private readonly clock: Clock,
    readonly label: string,
    seedLastAt: number | null = null,
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
    const wait = Math.max(0, this.plan.minIntervalMs - elapsed);
    if (wait > 0) await this.clock.sleep(wait);
    this.last = this.clock.now();
    this.waits.push(wait);
    return wait;
  }
}
