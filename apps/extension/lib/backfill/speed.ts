/**
 * ADR-032 §3 · **The three speed presets, and the one place they are decided.**
 *
 * ADR-032 asked for three presets (gentle / standard / faster, the last one carrying a risk note) and
 * ADR-033 then moved the *numbers* they are built from: it doubled the daily cap and the per-tick body
 * budget for every stable platform and wrote down which preset each pair of values belongs to
 * ("the new values become the *standard* preset's baseline; *gentle* is the pre-speedup old values").
 * W111 made the doubled pair the shipped
 * constants and left the pre-doubling pair in place under names that say what they are for
 * (`QUIET_DAILY_CAP_MIN` / `QUIET_DAILY_CAP_MAX` in pace.ts, `QUIET_TICK_DETAILS` in schedule.ts), each
 * with the note *"kept for ADR-032's quiet preset; the preset itself is not built here, and no caller
 * reads them yet."* This module is that caller.
 *
 * ## What a preset is allowed to change, and what it is not
 *
 * A preset changes **exactly two knobs**:
 *   · the band the day's body cap is drawn from (`PacePlan.dailyCapBand`, `PacePlan.maxPerDay`);
 *   · how many bodies one tick may fetch (`TickDeps.maxDetails`).
 *
 * It changes **nothing else**. In particular the per-request gaps (detail `20 s + uniform[0,25 s]`,
 * enumeration `2 s + uniform[0,4 s]`) and the `[5,10]`-minute tick interval are identical in all three
 * presets. That is ADR-033's own boundary, kept here so that "faster" can never quietly become "burstier":
 * it raises how much is done, never how fast any single request follows another.
 *
 * ## 🔴 The default is `gentle`, and that is a behaviour decision, not a detail
 *
 * ADR-032 §3 says gentle is the default, and the W113 dispatch repeats it. It sits in genuine tension with ADR-033, whose
 * stated reason for doubling was that the measured backlog (chatgpt 7,703 owed, ≤184/day) would take tens
 * of days — and W111 shipped the doubled numbers as the constants the engine used. So this file decides
 * *which of the two the product does when the user has expressed no preference*, and the answer is
 * **gentle**: VISION.md's gentleness is a requirement rather than a compromise ("gentleness is a
 * requirement, not a concession"),
 * and the doubled values remain available, one control away, as the standard preset.
 *
 * The observable consequence, stated plainly because it is a change to recorded behaviour: a user who has
 * never touched the control is backfilled at 150–200 bodies/day and 1 body/tick, where W111's constants
 * alone would have given 300–400 and 2. Flipping `DEFAULT_SPEED_PRESET` to `'standard'` restores it, and
 * that is the one line to change if the owner wants ADR-033's numbers to apply without a choice being made.
 *
 * ## Unreadable is not a preference
 *
 * `presetFrom` accepts a stored value only when it is one of the three names. Anything else — a value from
 * a newer build, a half-written record, a number — falls back to the default rather than being coerced
 * into the nearest preset. Guessing which of "gentle" and "faster" a stranger meant is the one way this
 * module could raise the request rate without anybody asking it to.
 */

import { DEFAULT_PACE, QUIET_DAILY_CAP_MIN, QUIET_DAILY_CAP_MAX, type BackfillPace, type PacePlan } from './pace';
import { DEFAULT_TICK_DETAILS, QUIET_TICK_DETAILS } from './schedule';
import type { BackfillStore } from './store';

/** The three names, and the only three values `presetFrom` will ever return. */
export type SpeedPreset = 'gentle' | 'standard' | 'faster';

/**
 * Where the choice lives. Same `cs_*` key family as every other extension record, so it needs no new
 * permission and no new store: `storage.local` is already declared for exactly this kind of setting.
 */
export const SPEED_PRESET_KEY = 'cs_backfill_speed_v1';

/**
 * 🔴 The default, and the only line to change to make ADR-033's numbers apply without a choice being made.
 * See the module comment for why it is this value.
 */
export const DEFAULT_SPEED_PRESET: SpeedPreset = 'gentle';

/**
 * A daily-cap band. Always paired with a plan whose `maxPerDay` is the band's ceiling, so a stored cap can
 * never exceed what the preset promised (see `drawDailyCap`).
 */
export interface DailyCapBand {
  readonly min: number;
  readonly max: number;
}

export interface SpeedPlan {
  readonly preset: SpeedPreset;
  /** How many bodies one tick may fetch at most. Becomes `TickDeps.maxDetails`. */
  readonly tickDetails: number;
  /** The pace plan handed to `runBackfill`. Its `detail.dailyCapBand` is this preset's band. */
  readonly pace: BackfillPace;
  /**
   * Whether choosing this preset warrants a warning before it takes effect (ADR-032 §3: the fast
   * preset carries a risk note).
   * A boolean rather than a sentence: the wording is a catalog key resolved at paint time, and this table
   * is built at module load, so it may not hold a language.
   */
  readonly carriesRisk: boolean;
}

/** Gentle: pre-ADR-033 values, the default. Nothing here is new — it is the pair ADR-033 moved away from. */
const GENTLE_BAND: DailyCapBand = { min: QUIET_DAILY_CAP_MIN, max: QUIET_DAILY_CAP_MAX };

/** Standard: ADR-033's doubled pair. The numbers W111 shipped as the engine's own constants. */
const STANDARD_BAND: DailyCapBand = { min: 300, max: 400 };

/**
 * 🔴 Faster · **the one band no ADR fixes**, so here is the rule that produced it and what it is not.
 *
 * It is standard doubled again — the same two knobs, by the same factor, by the same argument ADR-033
 * gave: only the totals move, never the burst shape. 600–800 bodies/day and 4 per
 * tick, with the per-request gaps and the tick interval untouched.
 *
 * It is **not** a measurement. `pace.ts`'s own note applies verbatim: these are numbers chosen by
 * arithmetic, not observations of a platform's rate limits, and the only real threshold we have evidence
 * for is the one a 429 would reveal. That is why the preset carries `carriesRisk` and why the coverage
 * page has to print the risk next to the control rather than in a footnote.
 */
const FASTER_BAND: DailyCapBand = { min: 600, max: 800 };

/** Build a plan from a band, keeping every part of the pace that is not the cap. */
function planFor(preset: SpeedPreset, band: DailyCapBand, tickDetails: number, carriesRisk: boolean): SpeedPlan {
  const detail: PacePlan = {
    ...DEFAULT_PACE.detail,
    // The ceiling the day's draw is clamped to. `drawDailyCap` can never return more than this, so the
    // preset's band is also its hard limit — the property W16's comment insists on.
    maxPerDay: band.max,
    dailyCapBand: band,
  };
  return {
    preset,
    tickDetails,
    pace: { enumerate: DEFAULT_PACE.enumerate, detail },
    carriesRisk,
  };
}

/**
 * The table. Every preset is built from `DEFAULT_PACE.detail`, so a future change to the *shape* of the
 * pacing (a different gap or jitter) reaches all three at once and cannot be applied to one of them only.
 */
export const SPEED_PLANS: Readonly<Record<SpeedPreset, SpeedPlan>> = {
  gentle: planFor('gentle', GENTLE_BAND, QUIET_TICK_DETAILS, false),
  standard: planFor('standard', STANDARD_BAND, DEFAULT_TICK_DETAILS, false),
  faster: planFor('faster', FASTER_BAND, DEFAULT_TICK_DETAILS * 2, true),
};

/** The presets in the order the UI offers them: quietest first, because that is the order of the promise. */
export const SPEED_PRESET_ORDER: readonly SpeedPreset[] = ['gentle', 'standard', 'faster'];

/**
 * Read a stored value as a preset.
 *
 * 🔴 Only the three exact names are accepted. Everything else — `undefined`, `null`, a number, an object,
 *    a name from a newer build — is the default. There is deliberately no `String(raw)` and no trimming:
 *    both would turn a value we do not understand into one we act on, and the direction that error can
 *    take is upwards.
 */
export function presetFrom(raw: unknown): SpeedPreset {
  if (raw === 'gentle' || raw === 'standard' || raw === 'faster') return raw;
  return DEFAULT_SPEED_PRESET;
}

/** The plan for a stored value. The one function a caller needs. */
export function speedPlanFor(raw: unknown): SpeedPlan {
  return SPEED_PLANS[presetFrom(raw)];
}

/**
 * Read the stored preset.
 *
 * A store that is absent or that throws answers the **default**, and that is deliberate rather than
 * convenient: this value only ever selects between three rates we chose ourselves, so "we could not read
 * your preference" and "you have no preference" have the same honest answer. (It is the opposite of the
 * debt ledger, where the two are different facts and `schedule.ts`'s `no-store` gate refuses to run.)
 */
export async function readSpeedPreset(store: BackfillStore | null): Promise<SpeedPreset> {
  if (!store) return DEFAULT_SPEED_PRESET;
  try {
    return presetFrom(await store.load(SPEED_PRESET_KEY));
  } catch {
    return DEFAULT_SPEED_PRESET;
  }
}

/** The plan the next tick will run with. */
export async function readSpeedPlan(store: BackfillStore | null): Promise<SpeedPlan> {
  return SPEED_PLANS[await readSpeedPreset(store)];
}

/**
 * Write the choice. Returns what was actually persisted **after reading it back**, so a caller that shows
 * "standard is on" is showing a value that survived the write rather than the one it hoped for. A store
 * that throws is not swallowed: the caller's control has to put itself back.
 */
export async function writeSpeedPreset(store: BackfillStore | null, preset: SpeedPreset): Promise<SpeedPreset> {
  if (!store) throw new Error('[chat-stasher] no storage to record the speed preset in');
  await store.save(SPEED_PRESET_KEY, preset);
  return await readSpeedPreset(store);
}
