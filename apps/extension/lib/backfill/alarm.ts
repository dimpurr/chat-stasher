/**
 * C19 · Let the backfill leg **wake up on its own**.
 *
 * ## Why an alarm was unavoidable
 * C13's heartbeat was "the live leg kicks it whenever it captures one". That was
 * the right choice then (zero cost, target information ready to hand, gentle
 * timing), but it has one fatal corollary:
 * 🔴 **a user who installs the extension and never opens that site again would
 *    never finish backfilling** — because no second live capture would ever kick
 *    it.
 * The backfill leg's product promise is "everything slowly gets filled in over
 * several days", and that cannot be delivered by a heartbeat that only fires when
 * the user is actively chatting.
 *
 * ## The cost (checked, 2026-08-17)
 * In Chrome's permission-warning list, `alarms` — like `storage` — **raises no
 * install warning at all**; what does raise one is the `downloads` we used to
 * have ("Manage your downloads").
 * ⇒ Adding this permission is invisible to the user, and what it buys is "this
 * leg really does move forward on its own".
 *
 * ## 🔴 Still off by default
 * BACKFILL_DEFAULT_ENABLED did not change a character (still false).
 * **The alarm is only created when the switch is on, and cleared the moment it is
 * turned off** — no consent, no alarm, and therefore no periodic behaviour at all.
 */

import type { BackfillStore } from './store';
import type { TickReason } from './schedule';

/** The alarm name. Creating the same name twice overwrites, so it is naturally idempotent. */
export const BACKFILL_ALARM_NAME = 'cs-backfill-tick';

/**
 * 🔴 Period = 5 minutes. This number is derived, not picked out of the air:
 *
 *  · **The floor**: Chrome imposes a minimum alarm period for packaged MV3
 *    extensions (1 minute); anything smaller is silently rounded up by the
 *    browser, and writing a number that cannot take effect only leaves code that
 *    disagrees with reality.
 *  · **The ceiling follows from the daily cap**: one tick clears exactly 1 debt
 *    (DEFAULT_TICK_DETAILS = 1), and the daily quota is
 *    DEFAULT_DETAIL_PACE.maxPerDay = 200.
 *    Once every 5 minutes ⇒ at most 288 wakes a day > 200 ⇒ **what actually caps
 *    the speed is the daily limit, not the alarm**. That is exactly the direction
 *    the product owner set: gentleness is governed by the daily cap.
 *    (A 10-minute period would give only 144 wakes a day < 200, making the alarm
 *     the bottleneck instead; 1000 conversations would drag past 7 days and the
 *     daily cap would be meaningless.)
 *  · **It does not fight the per-item interval**: 300 seconds ≫ the 20-second
 *    per-item minimum ⇒ on the alarm's path the interval gate is always a 0 wait;
 *    the interval only bites when the live leg kicks repeatedly (C19 task 3).
 *  · Each wake does one very small thing (read storage, fetch at most 1), which is
 *    friendly to MV3's SW lifecycle — short ticks × many, equivalent in progress
 *    to long ticks but gentler.
 */
export const BACKFILL_ALARM_PERIOD_MINUTES = 5;

export interface AlarmsApi {
  create(name: string, info: { periodInMinutes?: number; delayInMinutes?: number }): void | Promise<void>;
  clear(name: string): boolean | Promise<boolean>;
  get?(name: string): Promise<unknown>;
}

export type AlarmSyncResult = 'created' | 'kept' | 'cleared' | 'unavailable';

/**
 * Keep the alarm in step with the switch. **This is the only entry point to the
 * alarm's lifecycle.**
 *  · switch on ⇒ an alarm exists (an existing one is left alone, so every SW wake
 *    does not restart the period from zero);
 *  · switch off ⇒ cleared.
 * Without an alarms API (say, the node test environment) it returns 'unavailable'
 * and never pretends to have succeeded.
 */
export async function syncBackfillAlarm(
  alarms: AlarmsApi | null | undefined,
  enabled: boolean,
): Promise<AlarmSyncResult> {
  if (!alarms || typeof alarms.create !== 'function' || typeof alarms.clear !== 'function') {
    return 'unavailable';
  }
  if (!enabled) {
    await alarms.clear(BACKFILL_ALARM_NAME);
    return 'cleared';
  }
  if (typeof alarms.get === 'function') {
    const existing = await alarms.get(BACKFILL_ALARM_NAME);
    if (existing) return 'kept';
  }
  await alarms.create(BACKFILL_ALARM_NAME, { periodInMinutes: BACKFILL_ALARM_PERIOD_MINUTES });
  return 'created';
}

// ---------------------------------------------------------------------------
// The backfill target registry
//
// When the alarm wakes, the SW is brand new: no current tab, no account, nothing.
// That was one of the reasons C13 refused to use a timer. The answer is not to
// guess but to record, on the live leg's kick, the target (platform / origin /
// scope) it **already has to hand**.
// So the alarm always uses "the account the user really did use", and not one
// character has to be invented.
// ---------------------------------------------------------------------------

export const BACKFILL_TARGETS_KEY = 'cs_backfill_targets_v1';
export const MAX_TARGET_ENTRIES = 8;

export interface BackfillTarget {
  platform: string;
  origin: string;
  scope: string;
  /** When this target was last seen. Used for ordering only. */
  at: number;
}

function isTarget(v: unknown): v is BackfillTarget {
  if (!v || typeof v !== 'object') return false;
  const t = v as Partial<BackfillTarget>;
  return typeof t.platform === 'string'
    && typeof t.origin === 'string'
    && typeof t.scope === 'string'
    && typeof t.at === 'number';
}

export async function loadTargets(store: BackfillStore | null): Promise<BackfillTarget[]> {
  if (!store) return [];
  const raw = await store.load(BACKFILL_TARGETS_KEY);
  return Array.isArray(raw) ? raw.filter(isTarget) : [];
}

/** Record one target (deduplicated by platform+scope, most recent first). */
export async function rememberTarget(
  store: BackfillStore | null,
  target: BackfillTarget,
): Promise<BackfillTarget[]> {
  if (!store) return [];
  const rest = (await loadTargets(store)).filter(
    (t) => !(t.platform === target.platform && t.scope === target.scope),
  );
  const next = [target, ...rest].slice(0, MAX_TARGET_ENTRIES);
  await store.save(BACKFILL_TARGETS_KEY, next);
  return next;
}

// ---------------------------------------------------------------------------
// C30 · The **trace** of the alarm's tick
//
// The defect as seen on a real machine (reproduced in C29): the alarm woke every
// 5 minutes and, because the registry was empty, **did nothing at all** — no
// error, no log, no trace in storage. What the user saw was "archiving" next to
// "not one conversation has been enumerated yet", two sentences contradicting
// each other, and nothing anywhere to say what had actually happened.
//
// 🔴 So: **every alarm wake, whether it ran or not, writes down what it did and
//    why.** Into storage, not into memory: an MV3 SW is reclaimed the moment it
//    goes idle, and an in-memory lastTick is long gone before the user opens the
//    popup — which is exactly the "nothing can be found out" seen on a real machine.
// ---------------------------------------------------------------------------

/** The trace of the alarm's most recent tick. Same cs_* key family; no new permission. */
export const BACKFILL_LAST_TICK_KEY = 'cs_backfill_lasttick_v1';

export interface BackfillTickRecord {
  /** When this tick happened (Date.now()). */
  at: number;
  /** Whether it actually ran (reason === 'ran'). */
  ran: boolean;
  /** The named outcome. The same set of values tickBackfill and the popup use. */
  reason: TickReason;
  /** How many backfill targets the registry held when this tick woke. 0 is 0, undecorated. */
  targets: number;
}

function isTickRecord(v: unknown): v is BackfillTickRecord {
  if (!v || typeof v !== 'object') return false;
  const r = v as Partial<BackfillTickRecord>;
  return typeof r.at === 'number'
    && typeof r.ran === 'boolean'
    && typeof r.reason === 'string'
    && typeof r.targets === 'number';
}

/** Read the trace. Unreadable / wrong shape ⇒ null ("I do not know" is also the truth; do not invent a row). */
export async function loadLastTick(store: BackfillStore | null): Promise<BackfillTickRecord | null> {
  if (!store) return null;
  const raw = await store.load(BACKFILL_LAST_TICK_KEY);
  return isTickRecord(raw) ? raw : null;
}

/**
 * Write the trace. **Best-effort**: a failed write only reaches console.warn —
 * the trace exists so that a human can see what happened, and it must never
 * itself become a new reason to block this leg.
 */
export async function saveLastTick(
  store: BackfillStore | null,
  record: BackfillTickRecord,
): Promise<void> {
  if (!store) return;
  try {
    await store.save(BACKFILL_LAST_TICK_KEY, record);
  } catch (err) {
    console.warn('[chat-stasher] backfill tick record write failed', (err as Error).message);
  }
}
