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
import { systemRandom, uniformBetween, type RandomFn } from './random';
import { completeInterruptedMigration, openLedger, type LedgerRefusal } from './ledger';
import {
  isHeader,
  isLegacyState,
  legacyStateKey,
  stateKey,
  LEGACY_STATE_VERSION,
  type HaltReason,
  type StopReason,
} from './types';

/**
 * The jittered **one-shot** tick alarm. Creating the same name twice overwrites,
 * so it is naturally idempotent.
 *
 * 🔴 W16 · This is no longer a `periodInMinutes` alarm. It is armed with
 *    `delayInMinutes` alone and is re-armed after every tick with a **fresh
 *    random draw**, so the leg never wakes on a fixed cadence. See
 *    `BACKFILL_SAFETY_ALARM_NAME` for what stops a killed worker from breaking
 *    the chain forever.
 */
export const BACKFILL_ALARM_NAME = 'cs-backfill-tick';

/**
 * 🔴 W16 · The gap between two ticks is **drawn uniformly from `[5, 10]` minutes**
 * (mean 7.5). These numbers are derived, not picked out of the air:
 *
 *  · **The floor is 5 minutes — the exact fixed period this replaces.** That is
 *    the whole argument for `[5, 10]` rather than a wider band with the same
 *    mean (the task offered `[4, 11]`, also mean 7.5): W16's rule is that
 *    *jitter only ever adds delay*, and a 4-minute floor would be the removal of
 *    a documented minimum. With the floor at 5, **every draw is ≥ the old fixed
 *    period**, so the alarm can only ever tick later than before; its maximum
 *    wake rate stays bit-for-bit what it was (1440/5 = 288 a day) while its mean
 *    drops to 1440/7.5 = 192.
 *  · **The ceiling is 10 minutes.** It bounds how long the leg can sit still —
 *    which matters, because a one-shot alarm that is never re-armed is exactly
 *    the failure the watchdog below exists for.
 *  · **Both brakes now bite, neither alone.** Before W16 the alarm ticked 288
 *    times a day against a fixed cap of 200, so the cap was *always* what
 *    decided the rate and the alarm was pure overhead. With the gap drawn from
 *    `[5, 10]` the wake count is itself a random variable (144-288 a day, mean
 *    192) against a cap that is also a random variable (150-200, mean 175): on
 *    a day the cap drew 150 the alarm is the looser of the two, on a day it drew
 *    200 the cap is, and which one binds changes from day to day. That
 *    interleaving is the "the frequency must not be steady" the product asked
 *    for, expressed in the two places that govern the rate rather than
 *    cosmetically.
 *  · **The band is 2× wide**, so two consecutive gaps differ by a factor of up
 *    to 2 — plainly irregular to anyone watching, and impossible to distinguish
 *    from a person working through their own history.
 *  · **It still does not fight the per-item interval**: 300 seconds ≫ the
 *    20-second per-item minimum ⇒ on the alarm's path the interval gate is
 *    always a 0 wait; the interval only bites when the live leg kicks
 *    repeatedly (C19 task 3).
 *  · Each wake still does one very small thing (read storage, fetch at most 1),
 *    which is friendly to MV3's SW lifecycle.
 */
export const BACKFILL_TICK_DELAY_MIN_MINUTES = 5;
export const BACKFILL_TICK_DELAY_MAX_MINUTES = 10;
/** The mean of the uniform draw above, as a constant so the popup and the docs cannot disagree about it. */
export const BACKFILL_TICK_MEAN_MINUTES =
  (BACKFILL_TICK_DELAY_MIN_MINUTES + BACKFILL_TICK_DELAY_MAX_MINUTES) / 2;

/**
 * 🔴 W16 · **The watchdog.**
 *
 * A one-shot alarm is removed by the browser the moment it fires, so between
 * firing and the end-of-tick re-arm there is a window in which nothing is
 * armed — and if the service worker is reclaimed inside that window, the
 * jittered chain stops. (The window is not only theoretical: a tick that is
 * killed mid-fetch never reaches its re-arm at all.)
 *
 * This alarm closes it with a plain **fixed period**, which is the one thing
 * that survives a dead service worker, because the browser holds it, not us.
 * Its period is the bound on how long a broken chain can stay broken: one hour.
 *
 * 🔴 It is a watchdog, **not a second heartbeat**: its handler does nothing at
 *    all while the jittered alarm is armed, so it adds no tick, no storage read,
 *    no page ping and no request to a healthy leg. It only acts when the chain
 *    is genuinely broken. The irregular cadence is therefore preserved — this
 *    costs 24 cheap wake-ups a day and nothing else.
 */
export const BACKFILL_SAFETY_ALARM_NAME = 'cs-backfill-safety';
export const BACKFILL_SAFETY_PERIOD_MINUTES = 60;

/** Draw the next gap between two ticks, in minutes. Never below the floor, never above the ceiling. */
export function drawTickDelayMinutes(random: RandomFn = systemRandom): number {
  return uniformBetween(random, BACKFILL_TICK_DELAY_MIN_MINUTES, BACKFILL_TICK_DELAY_MAX_MINUTES);
}

export interface AlarmsApi {
  create(name: string, info: { periodInMinutes?: number; delayInMinutes?: number }): void | Promise<void>;
  clear(name: string): boolean | Promise<boolean>;
  get?(name: string): Promise<unknown>;
}

export type AlarmSyncResult = 'created' | 'kept' | 'cleared' | 'unavailable';

/**
 * Arm the jittered one-shot tick alarm with a fresh draw. Returns the delay it
 * used, so a caller (or a test) can assert the draw without re-deriving it.
 *
 * 🔴 This is the only place the tick alarm is armed, and it is called from
 *    exactly two places: the end of every tick, and `syncBackfillAlarm` when the
 *    alarm is missing. Both draw afresh, so there is no "remembered period"
 *    anywhere that could turn this back into a metronome.
 */
export async function armBackfillTick(
  alarms: AlarmsApi,
  random: RandomFn = systemRandom,
): Promise<number> {
  const minutes = drawTickDelayMinutes(random);
  await alarms.create(BACKFILL_ALARM_NAME, { delayInMinutes: minutes });
  return minutes;
}

/**
 * Keep both alarms in step with the switch. **This is the only entry point to
 * the alarm's lifecycle.**
 *  · switch on ⇒ both alarms exist (an existing one is left alone, so every SW
 *    wake does not restart the countdown);
 *  · switch off ⇒ **both** are cleared.
 * Without an alarms API (say, the node test environment) it returns 'unavailable'
 * and never pretends to have succeeded.
 *
 * 🔴 Both, in both directions. A switch-off that cleared only the tick alarm
 *    would leave the watchdog firing for the rest of time with the leg
 *    disabled — "no consent ⇒ no periodic behaviour" would be false.
 */
export async function syncBackfillAlarm(
  alarms: AlarmsApi | null | undefined,
  enabled: boolean,
  random: RandomFn = systemRandom,
): Promise<AlarmSyncResult> {
  if (!alarms || typeof alarms.create !== 'function' || typeof alarms.clear !== 'function') {
    return 'unavailable';
  }
  if (!enabled) {
    await alarms.clear(BACKFILL_ALARM_NAME);
    await alarms.clear(BACKFILL_SAFETY_ALARM_NAME);
    return 'cleared';
  }
  // Without `get` we cannot tell "already armed" from "missing", so we arm both
  // unconditionally — the same degradation as before W16 (creating an alarm that
  // exists only overwrites it), and never a state with nothing armed.
  const armed = async (name: string): Promise<boolean> => {
    if (typeof alarms.get !== 'function') return false;
    return Boolean(await alarms.get(name));
  };
  let created = false;
  if (!(await armed(BACKFILL_ALARM_NAME))) {
    await armBackfillTick(alarms, random);
    created = true;
  }
  if (!(await armed(BACKFILL_SAFETY_ALARM_NAME))) {
    await alarms.create(BACKFILL_SAFETY_ALARM_NAME, { periodInMinutes: BACKFILL_SAFETY_PERIOD_MINUTES });
    created = true;
  }
  return created ? 'created' : 'kept';
}

/** Whether the jittered chain is currently armed. The watchdog's whole decision. */
export async function isBackfillChainArmed(alarms: AlarmsApi | null | undefined): Promise<boolean> {
  if (!alarms || typeof alarms.get !== 'function') return false;
  return Boolean(await alarms.get(BACKFILL_ALARM_NAME));
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

/**
 * 🔴 W31c · Remove one target, by the same key `rememberTarget` dedups on.
 *
 * Why this exists at all, and why it is not `rememberTarget` with an empty scope:
 * a target registered before its scope was known carries the `'default'` sentinel
 * (see entrypoints/background.ts's scoped-platform registration). Once the scope
 * **is** known, that row is not a target any more — it names no account, and the
 * alarm would keep waking for it, halting on a scope that cannot be looked up,
 * every tick, forever. `rememberTarget` cannot express that: it dedups by
 * platform+scope, so writing the real scope leaves the sentinel row exactly where
 * it was. Same shape as `forgetTab` in lib/backfill/tab-port.ts, for the same
 * reason ("the registry converges on its own" needs a way to converge).
 *
 * 🔴 It never touches the target's **state**: the halt record written under the
 *    sentinel scope stays where it is, because this removes a registration, not a
 *    fact. Nothing about the archive, the debt set or the ledger moves here.
 */
export async function forgetTarget(
  store: BackfillStore | null,
  platform: string,
  scope: string,
): Promise<void> {
  if (!store) return;
  const next = (await loadTargets(store)).filter(
    (t) => !(t.platform === platform && t.scope === scope),
  );
  await store.save(BACKFILL_TARGETS_KEY, next);
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

/** The key prefix the pre-W18 layout wrote at: `cs_backfill_v1:`. */
export const LEGACY_STATE_KEY_PREFIX = `cs_backfill_v${LEGACY_STATE_VERSION}:`;

/**
 * 🔴 W36b · **What one `migrateLegacyScopes` sweep did.**
 *
 * Three counts rather than a bare refusal, because "there was nothing to move"
 * and "there was something and it would not move" are the two facts a user needs
 * to tell apart, and the tick trace's single `halted` field cannot carry both.
 * `found` and `moved` are measurements: 0 is 0, undecorated.
 */
export interface LegacyMigration {
  /** How many pre-W18 keys the scan found in `storage.local`. */
  found: number;
  /** How many pre-W18 records the new layout now holds (a moved record, or an orphaned one whose copy was confirmed and cleared). */
  moved: number;
  /** How many pre-W18 keys sit beside a new-layout record that does not match them — left exactly where they are. */
  orphaned: number;
  /** The first refusal, or null. The same one-field rule the tick trace follows. */
  refusal: LedgerRefusal | null;
}

const NOTHING_TO_MIGRATE: LegacyMigration = { found: 0, moved: 0, orphaned: 0, refusal: null };

/**
 * The `{platform, scope}` a `cs_backfill_v1:<platform>:<scope>` key names, or null
 * when the key is not one of ours.
 *
 * 🔴 The platform is one path segment and the scope is **everything after it**,
 *    not a second segment: a scope is an account/organization identifier and may
 *    itself contain a colon, while a platform id never does (it is the `id` field
 *    of lib/contract.ts's platform table). Splitting on the last colon instead
 *    would name a platform that does not exist and quietly skip the record.
 */
export function legacyScopeFromKey(key: string): { platform: string; scope: string } | null {
  if (!key.startsWith(LEGACY_STATE_KEY_PREFIX)) return null;
  const rest = key.slice(LEGACY_STATE_KEY_PREFIX.length);
  const separator = rest.indexOf(':');
  if (separator <= 0 || separator === rest.length - 1) return null;
  return { platform: rest.slice(0, separator), scope: rest.slice(separator + 1) };
}

/**
 * 🔴 W36/W36b · **Carry any pre-W18 record over now, before the gates decide
 * anything — and find it by scanning `storage.local`, not the target registry.**
 *
 * ## Why the migration cannot live only inside a run any more
 *
 * `runBackfill` opens the ledger, and the ledger is what migrates — so the whole
 * storage layout moved only on a tick that got all the way to "a fetch is about
 * to happen": switch on, host answering, a registered target **and an open
 * platform tab** (the http port). The first real-Chrome acceptance was in
 * exactly the other state for days — tabs closed, every tick blocked at
 * `no-http-port` — so the user's 7,737-id v1 record sat there untouched, no
 * `cs_backfill_v2:*` key existed, the popup had no debt set to show, and nothing
 * anywhere said so.
 *
 * ## Why W36 was still not enough, and what changed
 *
 * W36 hung the migration off the top of the alarm tick but walked
 * `loadTargets()` — the scopes the user is *currently registered for*. So a
 * pre-W18 record whose scope is not in that registry was visited by nothing at
 * all, and on a machine whose switch is off, or in the 5-10 minutes before the
 * next tick, neither the tick nor anything else touched it either. The layout is
 * a property of `storage.local`, not of the registry: the scan below enumerates
 * the **keys**, so every pre-W18 record is reachable from every caller of this
 * function — the tick preflight and the popup's first state load alike.
 *
 * ## What this does not change
 *
 *  · **no scope is invented.** The keys are the ones already in storage; a
 *    `{platform, scope}` comes out of the key itself, and `openLedger` refuses
 *    any record whose own identity disagrees with its address;
 *  · **a layout that has already moved costs one `keys()` read and nothing
 *    else.** The ledger is opened only for a key that really is `cs_backfill_v1:*`
 *    — opening one reads the scope's whole debt set back out of IndexedDB, the
 *    very read W18 exists to stop paying per tick;
 *  · **a refusal still writes nothing.** `openLedger` is the migration's only
 *    entry point and it leaves an unreadable record exactly as it found it; what
 *    is new is that the reason is returned to the caller, which puts it in the
 *    tick trace and in the popup instead of dropping it on the floor.
 */
export async function migrateLegacyScopes(store: BackfillStore | null): Promise<LegacyMigration> {
  if (!store) return NOTHING_TO_MIGRATE;

  let keys: string[];
  try {
    keys = await store.keys();
  } catch (err) {
    // 🔴 "I could not look" is not "there is nothing to find". The sweep reports
    //    the refusal it can name — the store itself — rather than a zero that
    //    would read as a completed migration.
    console.warn('[chat-stasher] backfill state preflight could not list storage keys', (err as Error).message);
    return {
      ...NOTHING_TO_MIGRATE,
      refusal: {
        reason: 'storage-unavailable',
        detail: 'storage.local could not be listed, so the pre-W18 records (if any) were not looked for',
      },
    };
  }

  const report: LegacyMigration = { ...NOTHING_TO_MIGRATE };
  for (const key of keys.sort()) {
    const named = legacyScopeFromKey(key);
    if (!named) continue;
    report.found += 1;
    // 🔴 One unreadable scope must not hide the ones behind it: `continue`, never
    //    `return`. (W36 returned here; a store that threw on the first key skipped
    //    every later scope, including healthy ones.)
    let outcome: OneMigration;
    try {
      outcome = await migrateOneLegacyKey(store, named.platform, named.scope, key);
    } catch (err) {
      console.warn('[chat-stasher] backfill state preflight read failed', (err as Error).message);
      outcome = {
        kind: 'refused',
        refusal: {
          reason: 'storage-unavailable',
          detail: 'a pre-W18 record could not be read out of storage.local, so it was not moved',
        },
      };
    }
    if (outcome.kind === 'moved') report.moved += 1;
    else if (outcome.kind === 'orphaned') report.orphaned += 1;
    // 🔴 An orphan that could not be cleared is a refusal like an ordinary one:
    //    either way the old record is still holding this account's ids and nothing
    //    moved it. Only the first is kept — they are the same class of fact and
    //    the trace has one field for it.
    if (outcome.kind !== 'moved' && report.refusal === null) report.refusal = outcome.refusal;
  }
  return report;
}

/** What one pre-W18 key turned out to be. Three different facts, never collapsed. */
type OneMigration =
  | { kind: 'moved' }
  /** Carried over, but its copy could not be confirmed against the debt store. */
  | { kind: 'orphaned'; refusal: LedgerRefusal }
  | { kind: 'refused'; refusal: LedgerRefusal };

/**
 * One pre-W18 key. Two shapes, and they are different facts:
 *  · the scope has **no** v2 header ⇒ the ordinary migration, through
 *    `openLedger` (its only entry point, so the whole safety argument is one
 *    function);
 *  · the scope **has** one ⇒ a migration interrupted between its header write and
 *    its last step. `openLedger` would never look at the old key again, so the
 *    copy is confirmed against the debt store and, if it matches, cleared —
 *    `completeInterruptedMigration` states the proof it insists on.
 */
async function migrateOneLegacyKey(
  store: BackfillStore,
  platform: string,
  scope: string,
  key: string,
): Promise<OneMigration> {
  const legacy = await store.load(key);
  if (isHeader(await store.load(stateKey(platform, scope)))) {
    if (!isLegacyState(legacy) || legacy.platform !== platform || legacy.scope !== scope) {
      return {
        kind: 'orphaned',
        refusal: {
          reason: 'state-unreadable',
          detail: `the pre-W18 record at ${key} sits beside a new-layout record and is not a record this `
            + 'build can read; it has been left untouched',
        },
      };
    }
    const refusal = await completeInterruptedMigration(store, platform, scope, key, legacy);
    return refusal === null ? { kind: 'moved' } : { kind: 'orphaned', refusal };
  }
  const opened = await openLedger(store, platform, scope);
  return opened.ok ? { kind: 'moved' } : { kind: 'refused', refusal: opened.refusal };
}

export interface BackfillTickRecord {
  /** When this tick happened (Date.now()). */
  at: number;
  /** Whether it actually ran (reason === 'ran'). */
  ran: boolean;
  /** The named outcome. The same set of values tickBackfill and the popup use. */
  reason: TickReason;
  /** How many backfill targets the registry held when this tick woke. 0 is 0, undecorated. */
  targets: number;
  /**
   * 🔴 W36 · **How the run itself ended**, when there was one.
   *
   * Why `ran: true` was not enough — the first real-Chrome acceptance read
   * `{ran: true, reason: 'ran'}` next to a storage layout that had not moved and
   * concluded that the W18 migration had never run. It had run: `tickBackfill`
   * reports `ran` whenever `runBackfill` **returns**, and a run that halts on a
   * state record it cannot read returns a report like any other. The two
   * outcomes were indistinguishable in the trace, so the one fact that would
   * have answered the question in seconds — the run's own stop reason and the
   * halt it recorded — was thrown away by `recordAlarmTick`.
   *
   * Optional, and read as "no run happened": a record written before this field
   * existed (or by a tick blocked at a gate) still parses, and the popup's
   * `isTickRecord` does not require it — the same compatibility rule the other
   * optional fields in this project follow.
   */
  stopped?: StopReason | null;
  /**
   * The `HaltReason` the run left behind, or the one a refusal reached **before**
   * the run could start (see `migrateLegacyScopes`) — `null` when neither
   * happened. This is the field that turns "it ran and nothing moved" into a
   * named fact.
   */
  halted?: HaltReason | null;
  /**
   * The halt's own technical detail, verbatim. Metadata only by construction —
   * the engine's details name keys, paths, statuses and counts, never a
   * conversation body (CLAUDE.md's privacy rule, and the reason the detail is
   * safe to persist at all).
   */
  detail?: string | null;
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
