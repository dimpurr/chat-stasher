/**
 * 🔴 W296 · **One 429 slows every request to that platform for the rest of the local day.**
 *
 * ## What was missing, and why the existing brakes did not cover it
 *
 * Two brakes already answer a refusal, and both are *bounded in time*:
 *
 *  · the **retry ladder** (`types.ts`, `TRANSIENT_RETRY_BASE_MS['rate-limited']`) stops
 *    the leg for 7.5-15 min and doubles from there;
 *  · the platform's own **`Retry-After`** (W127) may replace that one delay, clamped
 *    to `[30 s, 15 min]` — and is reported to the machine-wide arbiter as well, which
 *    covers seconds to minutes across the installs on one machine.
 *
 * Both expire in minutes, and both are *scoped to the thing that was refused*. The
 * rate that produced the 429 is set by the **speed preset** (`speed.ts`), which is
 * drawn from storage and has no notion of the platform having complained: the tick
 * after the backoff ends resumes at exactly the preset's rhythm. The reference
 * implementations that handle this at all do the same thing (28-RATE-LIMITS §5 B2/B3:
 * Echoes switches its whole retry ladder on the first 429, and stops that service's
 * queue for 300 s) — nobody we surveyed slows *the rest of the day*.
 *
 * A 429 is the one piece of evidence this project has about a platform's real
 * threshold (`pace.ts` says so in as many words: "the only real threshold we have
 * evidence for is the one a 429 would reveal"). Ignoring it once the ladder expires
 * means the leg re-approaches the same threshold on the same rhythm that just
 * tripped it. This module is the response that outlives the ladder: the platform
 * said "too many requests", so for the rest of that day we ask for fewer.
 *
 * ## The rule
 *
 *  · **Trigger**: a response whose status is 429 (`daySlowTriggeredBy`), observed at
 *    the **request gateway** — `coordinatedTick`'s `coordinatedHttp`
 *    (entrypoints/background.ts), which its own comment calls "the sole gateway for
 *    requests made by live, alarm, retry, resume and popup discovery". That is the
 *    only place "every request to that platform" is literally true of: the engine
 *    sees one run's responses, the gateway sees the one port all of them are sent
 *    through. Only 429 — a 503 is written down as its own thing (`Retry-After` is
 *    read for it too, W127), but "the platform says we are over its request budget"
 *    is what a 429 means, and a day-long brake is the response to *that*. The set
 *    lives in one exported predicate so widening it is one line with one test.
 *  · **Effect**: `daySlowPlan` — every request gap of the chosen plan is doubled
 *    (`DAY_SLOW_INTERVAL_FACTOR`), and the tick fetches at most the gentlest preset's
 *    body budget. Both directions are *downward only*: the transform can halve the
 *    request rate and shrink a tick's work, never the reverse.
 *  · **Duration**: until the next **local** midnight (`localMidnightAfter`). The
 *    record holds an absolute instant, so the brake survives a service-worker
 *    restart with no timer to re-arm — a restarted worker reads the same instant and
 *    stays slowed until it passes, and stops being slowed with nobody having to
 *    clear anything.
 *  · **Scope**: **per platform**, not per scope and not per account. The 429 was
 *    answered by the platform, and every scope of that platform (`chatgpt:<workspace>`
 *    is several) shares the same budget. This is the same granularity the machine-wide
 *    arbiter speaks in (36-EXTENSION-TOPOLOGY §5 D1).
 *
 * ## Why a failure to read it is *not* treated as "slowed"
 *
 * An unreadable or absent record answers `0` — "no brake" — and that is a decision
 * rather than an oversight, on the same reasoning `speed.ts` gives for the preset it
 * cannot read: this value only ever selects between two rates **we chose ourselves**,
 * so "we could not read our own brake" and "we have no brake" have the same honest
 * answer, and the baseline it falls back to is already the conservative one. The
 * alternative — treating an unreadable record as "slowed" — would let a storage
 * hiccup hold the leg down indefinitely, which is the mirror image of the mistake
 * this project refuses to make in the other direction (an unknown must not be
 * presented as a fact; here the *control* consequence of falling back is the baser
 * rate, not a claimed measurement).
 *
 * What must **not** happen is a partial write. `recordPlatformRateLimit` reads the
 * whole record first and rethrows if it cannot, so a failed read never becomes a
 * write that silently drops another platform's brake.
 *
 * ## What this is not
 *
 * It is not the machine-wide arbiter (D1, `account-lease.ts` / `native-host.ts`): that
 * exists so *several installs on one machine* do not add up, and it speaks seconds to
 * minutes. This one is per install — the state is `storage.local`, so two profiles on
 * one machine each keep their own day brake — and it is the *local* memory of "this
 * platform refused us today".
 */

import { DEFAULT_LIST_ONLY_ENUM_PACE, type BackfillPace, type PacePlan } from './pace';
import { QUIET_TICK_DETAILS } from './schedule';
import type { SpeedPlan } from './speed';
import type { BackfillStore } from './store';

/**
 * The one key. Same `cs_*` family as every other extension record, so it needs no new
 * permission and no new store — `storage.local` is already declared for this kind of
 * setting (`speed.ts`'s key sits beside it).
 */
export const DAY_SLOW_KEY = 'cs_backfill_day_slow_v1';

/**
 * Shape version. A record that is not this version is not read as one — see
 * `isDaySlowRecord`. There is no migration: a brake is regenerated by the next 429,
 * and inventing one from a shape we guessed at would be the only way this value could
 * make the leg *slower* than a platform asked for.
 */
export const DAY_SLOW_VERSION = 1;

/**
 * How much longer every request gap becomes once this platform has answered a 429 today.
 *
 * Two is the smallest factor that plainly halves the request rate (gap doubles ⇒ rate
 * halves), and it applies to *every* segment, so the slowdown cannot be undone by
 * moving work from one segment to another. It is a factor rather than a fixed floor so
 * it composes with whatever preset is in force: a `faster` plan's 20 s body gap becomes
 * 40 s, and the gentlest plan's becomes 40 s too.
 */
export const DAY_SLOW_INTERVAL_FACTOR = 2;

/** One platform's brake: when it ends, and when the 429 that set it was seen. */
export interface DaySlowEntry {
  /** Absolute instant the brake ends — the next local midnight after the 429. */
  readonly until: number;
  /** When the 429 was observed (for a reader that wants to say "today at …"). */
  readonly at: number;
}

export interface DaySlowRecord {
  readonly v: typeof DAY_SLOW_VERSION;
  /** Platform id → that platform's brake. */
  readonly platforms: Record<string, DaySlowEntry>;
}

/**
 * Does this response status arm the brake?
 *
 * One exported predicate rather than a `=== 429` at the call site, so the rule has a
 * single home and the test that pins it is about the rule rather than about an
 * implementation detail. `503` is deliberately not here: it is a service state, and
 * W127 already reads its `Retry-After` for the immediate wait. Widening this is one
 * line and one test.
 */
export function daySlowTriggeredBy(status: number): boolean {
  return status === 429;
}

/**
 * The next local midnight strictly after `nowMs`.
 *
 * Local, not UTC, and the difference is the whole point of the function: the daily
 * cap's own day key (`dayKeyOf`, types.ts) is a **UTC** day, so reusing it here would
 * end the brake at 11:00 for a user in UTC+11 — half a "rest of the day". Built from
 * local calendar parts rather than by adding 24 h, so a DST transition inside the
 * window still lands on 00:00 local.
 */
export function localMidnightAfter(nowMs: number): number {
  const at = new Date(nowMs);
  return new Date(at.getFullYear(), at.getMonth(), at.getDate() + 1, 0, 0, 0, 0).getTime();
}

/** Is this value a record this build wrote? Anything else is not read as one. */
function isDaySlowRecord(value: unknown): value is DaySlowRecord {
  if (!value || typeof value !== 'object') return false;
  const record = value as Partial<DaySlowRecord>;
  return record.v === DAY_SLOW_VERSION
    && typeof record.platforms === 'object'
    && record.platforms !== null
    && !Array.isArray(record.platforms);
}

/** Whether one entry is usable: a finite `until` and an `at`. Anything else is not a brake. */
function isDaySlowEntry(value: unknown): value is DaySlowEntry {
  if (!value || typeof value !== 'object') return false;
  const entry = value as Partial<DaySlowEntry>;
  return typeof entry.until === 'number' && Number.isFinite(entry.until)
    && typeof entry.at === 'number' && Number.isFinite(entry.at);
}

/**
 * When this platform's brake ends, or `0` for "no brake".
 *
 * `0` is the answer for a missing record, a record of another shape, a missing or
 * unusable platform entry, and an instant that has already passed — four different
 * inputs whose only shared property is that none of them slows this tick. That is the
 * deliberate opposite of `parseRetryAfterMs`'s "absent ≠ 0" rule, because the two
 * answer different questions: there, `0` would be a *claim* ("the platform said
 * now"); here it is the absence of a brake we set ourselves (see the module comment).
 */
export async function readDaySlowUntil(
  store: BackfillStore | null,
  platform: string,
  now: number,
): Promise<number> {
  if (!store) return 0;
  let raw: unknown;
  try {
    raw = await store.load(DAY_SLOW_KEY);
  } catch {
    return 0;
  }
  if (!isDaySlowRecord(raw)) return 0;
  const entry = raw.platforms[platform];
  if (!isDaySlowEntry(entry)) return 0;
  return entry.until > now ? entry.until : 0;
}

/**
 * Whether every request to this platform is slowed right now. The one predicate the
 * tick path needs; `readDaySlowUntil` is exported for a caller that wants to say when.
 */
export async function isPlatformDaySlowed(
  store: BackfillStore | null,
  platform: string,
  now: number,
): Promise<boolean> {
  return (await readDaySlowUntil(store, platform, now)) > 0;
}

/**
 * Arm the brake for `platform` until the next local midnight.
 *
 * 🔴 The read-modify-write is deliberate and its failure mode is chosen: if the record
 *    cannot be read, this **throws** rather than writing a fresh one-platform record.
 *    Writing anyway would be a write that silently deletes every other platform's
 *    brake — the one way this function could make things worse than doing nothing.
 *
 * Returns the instant the brake now ends, so a caller can log or show it without a
 * second read.
 */
export async function recordPlatformRateLimit(
  store: BackfillStore | null,
  platform: string,
  now: number,
): Promise<number> {
  if (!store) throw new Error('[chat-stasher] no storage to record a platform rate limit in');
  const until = localMidnightAfter(now);
  const raw = await store.load(DAY_SLOW_KEY);
  const existing = isDaySlowRecord(raw) ? raw.platforms : {};
  // Entries that have already expired are dropped here and nowhere else, so the record
  // cannot grow one platform-key per platform per day forever. Dropping them loses
  // nothing: an expired entry is already ignored by `readDaySlowUntil`.
  const platforms: Record<string, DaySlowEntry> = {};
  for (const [id, entry] of Object.entries(existing)) {
    if (isDaySlowEntry(entry) && entry.until > now) platforms[id] = entry;
  }
  platforms[platform] = { until, at: now };
  await store.save(DAY_SLOW_KEY, { v: DAY_SLOW_VERSION, platforms } satisfies DaySlowRecord);
  return until;
}

/**
 * The plan a slowed tick runs with.
 *
 * Two changes, both downward:
 *  · **every gap doubles** — `minIntervalMs` and `jitterMs` on the enumeration, the
 *    body and the list-only segments. The jitter field is only carried when the plan
 *    already had one, so a plan written before this field existed keeps `undefined`
 *    rather than gaining a `0` it did not have;
 *  · **the tick's body budget drops to the gentlest preset's** (`QUIET_TICK_DETAILS`),
 *    never raised. A `faster` user's four-bodies-per-tick becomes one, so the day's
 *    total shrinks as well as its pace.
 *
 * `listOnlyEnumerate` is filled in when the plan does not carry one, because the
 * engine falls back to `DEFAULT_LIST_ONLY_ENUM_PACE` otherwise and that fallback would
 * be the *uns*slowed list-only rhythm — the one segment (Perplexity-shaped plans, whose
 * enumeration is the whole leg) where this transform has to bite.
 *
 * Everything else — the preset name, `carriesRisk`, the daily band — is passed
 * through untouched. The day's cap was already drawn and persisted by the time a 429
 * could arrive, so changing the band would be a field that reads as a brake and is not
 * one.
 */
export function daySlowPlan(plan: SpeedPlan): SpeedPlan {
  const slower = (segment: PacePlan): PacePlan => ({
    ...segment,
    minIntervalMs: segment.minIntervalMs * DAY_SLOW_INTERVAL_FACTOR,
    ...(segment.jitterMs === undefined
      ? {}
      : { jitterMs: segment.jitterMs * DAY_SLOW_INTERVAL_FACTOR }),
  });
  const pace: BackfillPace = {
    enumerate: slower(plan.pace.enumerate),
    detail: slower(plan.pace.detail),
    listOnlyEnumerate: slower(plan.pace.listOnlyEnumerate ?? DEFAULT_LIST_ONLY_ENUM_PACE),
  };
  return {
    ...plan,
    tickDetails: Math.min(plan.tickDetails, QUIET_TICK_DETAILS),
    pace,
  };
}
