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
import { completeInterruptedMigration, openLedger, unreadableStateRefusal, type LedgerRefusal } from './ledger';
import {
  BACKFILL_STATE_VERSION,
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

/**
 * 🔴 W76 · **The fair-rotation cursor.**
 *
 * The alarm's loop used to walk `cs_backfill_targets_v1` from the head and
 * `break` at the first target whose result was not `no-http-port` (W72 §1). The
 * registry is ordered most-recently-captured-first, so the head platform with an
 * open tab took **every** tick and every platform after it was never served
 * while that held — and a head that was permanently halted, or waiting out a
 * transient backoff, still returned a non-`no-http-port` result and consumed
 * the tick too, blocking everyone else forever.
 *
 * The cursor records the index of the target that was served last. The next
 * tick starts its walk at the target **after** it (round-robin over the
 * registry, wrapping around), so no platform can be starved by a more-recently
 * captured one. It is a single small `storage.local` value, in the same
 * `cs_backfill_*` key family; it is not the header, and it survives an MV3
 * service-worker reclaim exactly as the trace does.
 *
 * 🔴 Unreadable / absent ⇒ `null` ("no position has been served"): the walk
 * starts at the registry head. That is the safe fallback — it is the old
 * behaviour, it can never skip a platform forever, and a garbage byte at this
 * key must not be turned into "serve nothing".
 */
export const BACKFILL_CURSOR_KEY = 'cs_backfill_cursor_v1';

export interface TickCursor {
  /** The index, in `cs_backfill_targets_v1` order, of the target served by the most recent tick. */
  served: number;
}

function isTickCursor(v: unknown): v is TickCursor {
  return typeof v === 'object' && v !== null
    && typeof (v as { served?: unknown }).served === 'number'
    && Number.isFinite((v as { served: number }).served)
    && (v as { served: number }).served >= 0;
}

/** Read the cursor. Unreadable / absent ⇒ `null` ("start at the head"), never a fabricated position. */
export async function loadTickCursor(store: BackfillStore | null): Promise<number | null> {
  if (!store) return null;
  const raw = await store.load(BACKFILL_CURSOR_KEY);
  return isTickCursor(raw) ? raw.served : null;
}

/** Write the cursor. Best-effort, same rule as the trace: a failed write is logged, never a reason to fail the tick. */
export async function saveTickCursor(
  store: BackfillStore | null,
  served: number,
): Promise<void> {
  if (!store) return;
  try {
    await store.save(BACKFILL_CURSOR_KEY, { served } satisfies TickCursor);
  } catch (err) {
    console.warn('[chat-stasher] backfill tick cursor write failed', (err as Error).message);
  }
}

/**
 * W76 · Why a target was passed over by a tick's fair rotation without running.
 * A small closed set — each value is a different fact, and none of them is
 * "it ran" (a target that ran consumes its tick and is reported as `served`).
 */
export type TickSkipReason = 'no-http-port' | 'halted' | 'waiting-retry';

/**
 * 🔴 W76 · **The fairness half of the tick trace.**
 *
 * `ran: true`/`reason` alone cannot say *which* platform got the tick and which
 * were passed over and why — and the whole defect is that the answer used to be
 * "always the head, and the rest were never looked at". So every tick that
 * walks the registry records:
 *  · `served` — the platform id that ran (or `null` when the whole rotation
 *    found nothing that would run this tick);
 *  · `skipped` — every platform examined this tick and passed over, with the
 *    reason code. Platforms after the served one are not listed; the walk stops
 *    at the first thing that runs.
 *
 * Platform ids and reason codes only — no origins, no scopes, no conversation
 * text (CLAUDE.md privacy rule). Reason codes are counts/comparisons, never a
 * body.
 */
export interface TickSchedule {
  served: string | null;
  skipped: Array<{ platform: string; reason: TickSkipReason }>;
}

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
 * 🔴 It never touches the target's **state**: this is the one-row primitive
 *    `rememberTarget` cannot express. Dropping a *non-organization* Claude row
 *    goes through `rememberOrganizationScopedTarget`, which also removes that
 *    scope's local ledger header — see that function.
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

/**
 * 🔴 W49 / W49b · Store one scoped-platform row, drop every non-organization
 * row for that platform, and do it in **one** registry write.
 *
 * `rememberTarget` prepends and dedups by platform+scope. Collapsing a leftover
 * conversation title onto `'default'` through "delete non-orgs, then prepend
 * default" therefore put the unresolved sentinel *in front of* a live
 * organization the registry already held. The alarm's loop `break`s on any
 * reason other than `no-http-port`, so a halt written on `'default'` starved
 * the organization until a new capture dropped the sentinel.
 *
 * So: a value that is not an organization is stored as the unresolved
 * sentinel only when this platform has **no** organization row. If one
 * already exists, the non-organization rows are dropped and the organization
 * is left where it is — never outranked by `'default'`. Two organization
 * rows still coexist.
 *
 * The registry is one `save`. A kill between a previous delete-then-insert
 * pair left the platform with no row; that window is gone. Dropped
 * non-organization rows also lose their `cs_backfill_v2:<platform>:<scope>`
 * header (halt, cursor, failures): that key is not unreachable — the popup
 * walks every header — and claiming nothing was written there was false.
 * The host archive is append-only and is not touched. A header whose scope
 * is re-inserted (collapsing title → `'default'` when no org exists) is kept.
 *
 * The caller names what an organization looks like, because this module does
 * not know any platform's identifier shape.
 */
export async function rememberOrganizationScopedTarget(
  store: BackfillStore | null,
  target: BackfillTarget,
  isOrganizationScope: (scope: string) => boolean,
): Promise<BackfillTarget[]> {
  if (!store) return [];
  const current = await loadTargets(store);
  const incomingIsOrg = isOrganizationScope(target.scope);
  const dropped: BackfillTarget[] = [];
  const kept: BackfillTarget[] = [];
  for (const t of current) {
    if (t.platform === target.platform && !isOrganizationScope(t.scope)) {
      dropped.push(t);
    } else {
      kept.push(t);
    }
  }
  const platformHasOrg = kept.some(
    (t) => t.platform === target.platform && isOrganizationScope(t.scope),
  );
  let next: BackfillTarget[];
  if (!incomingIsOrg && platformHasOrg) {
    next = kept;
  } else {
    const rest = kept.filter(
      (t) => !(t.platform === target.platform && t.scope === target.scope),
    );
    next = [target, ...rest].slice(0, MAX_TARGET_ENTRIES);
  }
  await store.save(BACKFILL_TARGETS_KEY, next);
  const stillPresent = new Set(next.map((t) => `${t.platform}\0${t.scope}`));
  for (const t of dropped) {
    if (stillPresent.has(`${t.platform}\0${t.scope}`)) continue;
    await store.remove(stateKey(t.platform, t.scope));
  }
  return next;
}

/**
 * 🔴 W49 · Drop every target for `platform` whose scope is **not** an
 * organization, and remove those scopes' local ledger headers.
 *
 * Prefer `rememberOrganizationScopedTarget` when a row is being stored: that
 * path is one registry write and will not insert an unresolved row in front
 * of a live organization. This remains the drop-only form.
 */
export async function forgetNonOrganizationTargets(
  store: BackfillStore | null,
  platform: string,
  isOrganizationScope: (scope: string) => boolean,
): Promise<void> {
  if (!store) return;
  const current = await loadTargets(store);
  const dropped: BackfillTarget[] = [];
  const next: BackfillTarget[] = [];
  for (const t of current) {
    if (t.platform === platform && !isOrganizationScope(t.scope)) dropped.push(t);
    else next.push(t);
  }
  await store.save(BACKFILL_TARGETS_KEY, next);
  for (const t of dropped) {
    await store.remove(stateKey(t.platform, t.scope));
  }
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
 * The key prefix the current layout writes at: `cs_backfill_v2:`.
 *
 * 🔴 Derived from the same constant `stateKey()` uses, not typed out a second
 *    time: `findUnreadableState` walks these keys, and a prefix that disagreed
 *    with the one the writer uses would silently find nothing — an absence read as
 *    "no record is unreadable".
 */
export const STATE_KEY_PREFIX = `cs_backfill_v${BACKFILL_STATE_VERSION}:`;

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
  return scopeFromStateKey(key, LEGACY_STATE_KEY_PREFIX);
}

/**
 * The `{platform, scope}` any `cs_backfill_v<n>:` key names, or null when the key
 * is not one of ours.
 *
 * 🔴 W47 · The same split, for the same reason, applied to the **current** layout
 *    as well as the pre-W18 one. `legacyScopeFromKey` is this function with the v1
 *    prefix, so the two can never disagree about what a scope containing a colon
 *    is called — which is the one thing the caller below relies on to name the
 *    scope a record it cannot read belongs to.
 */
export function scopeFromStateKey(
  key: string,
  prefix: string,
): { platform: string; scope: string } | null {
  if (!key.startsWith(prefix)) return null;
  const rest = key.slice(prefix.length);
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

/**
 * 🔴 W47 · **"Is there a record in the current layout that this build cannot read?"
 *
 * ## The hole this closes, and why the engine's own refusal could not close it
 *
 * `openLedger` refuses a run whose state record it cannot read, and since W36 the
 * run's own report carries that refusal into the tick trace. But the run only
 * happens on a tick that got past every gate — switch on, host answering, a
 * registered target **and an open platform page**. With the tabs closed, which is
 * the ordinary state of a laptop, no tick ever opens the ledger, so the refusal is
 * reached by nothing: the trace reads `no-http-port`, the popup shows no debt set,
 * and `state.halted` being absent from storage does **not** mean no refusal
 * happened. That is the same shape of hole W36b closed for the pre-W18 layout, and
 * it is closed the same way: by looking at `storage.local` itself.
 *
 * ## What it costs, and why it is not an `openLedger` per scope
 *
 * One `keys()` read of the area, then **one `storage.local` read per current-layout
 * key** — the header of a scope, which is small. It does not open a ledger: opening
 * one reads the scope's whole debt set back out of IndexedDB, which is the
 * per-tick cost W18 exists to remove, and it is why this cannot simply call
 * `openLedger` and throw the state away. The migration above already pays a
 * `keys()` read of its own; the two are kept apart rather than merged because they
 * answer different questions and either one is allowed to be the only one that
 * finds something.
 *
 * ## Nothing is written, and nothing is invented
 *
 * It **loads** the records and writes nothing — not the header, not the record it
 * found, not a marker of its own. The refusal is decided by the very function
 * `openLedger` uses (`unreadableStateRefusal`, lib/backfill/ledger.ts), so this can
 * neither raise a refusal the engine would not raise nor miss one it would; where
 * the refusal is stored is the caller's decision (the tick trace, whose key is a
 * constant and not derived from the scope — see `recordAlarmTick`).
 *
 * 🔴 A key that names no scope (`cs_backfill_v2:` with nothing after it) is not a
 *    record and is skipped, not reported: it is not a thing this build wrote, and
 *    naming it would be inventing a scope out of a malformed key.
 */
export async function findUnreadableState(
  store: BackfillStore | null,
): Promise<LedgerRefusal | null> {
  if (!store) return null;

  let keys: string[];
  try {
    keys = await store.keys();
  } catch (err) {
    // "I could not look" is not "there is nothing wrong", and it is not silent
    // either: the migration sweep in the same tick already refuses with
    // `storage-unavailable` for the very same failure, and returning a second
    // refusal here would make the trace name whichever happened to run first. So
    // this is the log line, and the trace's one field is the migration's.
    console.warn('[chat-stasher] backfill state preflight could not list storage keys', (err as Error).message);
    return null;
  }

  for (const key of keys.sort()) {
    const named = scopeFromStateKey(key, STATE_KEY_PREFIX);
    if (!named) continue;
    let raw: unknown;
    try {
      raw = await store.load(key);
    } catch (err) {
      // 🔴 This one **is** reported, unlike the `keys()` failure above: the
      //    migration never loads a current-layout key, so nothing else in the tick
      //    would ever notice, and a record that cannot be read at all is not the
      //    same fact as a record that is not there.
      console.warn('[chat-stasher] backfill state preflight read failed', (err as Error).message);
      return {
        reason: 'storage-unavailable',
        detail: `a backfill state record at ${key} could not be read out of storage.local, so this build `
          + 'cannot tell whether the scope it names is usable',
      };
    }
    const refusal = unreadableStateRefusal(raw, named.platform, named.scope);
    if (refusal) return refusal;
  }
  return null;
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

/**
 * 🔴 W51 · What one alarm tick's tab-registry recovery sweep did.
 *
 * `null` (and a missing field on a record written before this existed) means
 * the tick never swept — it never reached the port gate, or it already had a
 * channel. `{ looked: false }` is "we wanted to look and could not"
 * (`tabs.query` missing or it threw). `{ looked: true, registered: 0,
 * crowded: 0 }` is a completed sweep that found no answering unregistered
 * tab. `{ looked: true, crowded: N }` is a completed sweep that found
 * answering tabs and refused them because the registry was already full of
 * live-listed rows. Those are different facts; collapsing a refusal into
 * "no-http-port, nothing else" is the hole this field exists to close.
 *
 * Counts only: origins, tab ids and URLs stay out of the trace. `deferred`
 * is a count of eligible tabs the sweep did not ping because it hit its cap
 * — a sweep that pinged everyone it wanted to writes `deferred: 0`. `crowded`
 * is a count of answering tabs the sweep refused for want of a slot — a
 * sweep that registered everyone who answered writes `crowded: 0`. A ping
 * cap and a full registry are not the same fact.
 *
 * 🔴 W62b · **And a fourth fact: the sweep had not concluded.** W62 gave the
 *    tick a trace *before* the sweep, so that the migration and the gate
 *    decision reach storage without waiting on liveness pings that cannot
 *    change them. That first write has to say what is true at that instant —
 *    that this tick is about to look and has not finished looking — and it
 *    may not borrow any of the three values above, because each of them is a
 *    finished statement. `null` in particular reads as "this tick never
 *    swept", which is exactly what a tick that is *about to* sweep is not;
 *    and if the worker is reclaimed mid-sweep, or the sweep throws after
 *    `tabs.query` has already pruned or registered rows, or the tick's final
 *    save fails, that first record is the one that stays. It would then
 *    report a tick that never looked — a false statement about an event that
 *    did happen, which is the one thing this field exists to prevent.
 *
 *    `{ sweeping: true }` is therefore not a fourth *outcome*; it is the
 *    record saying it has no outcome yet. It is written only by the
 *    provisional trace, and it is replaced in the same tick by one of the
 *    three real values whenever the tick gets to finish (`background.ts`,
 *    the final `recordAlarmTick`). A reader must treat it as provisional —
 *    see `isSweepNotConcluded`, and `lastTickNote`, which says so rather
 *    than printing the tick's state as a verdict.
 *
 *    It is deliberately a variant *without* a `looked` key rather than a
 *    third value of `looked`: `looked` stays a boolean, so every existing
 *    `if (sweep.looked)` keeps meaning what it meant, and a reader that
 *    forgets this state fails to compile rather than silently taking the
 *    `undefined` for "did not look".
 */
export type TabSweepTrace =
  | { looked: false }
  | {
      looked: true;
      queried: number;
      pruned: number;
      pinged: number;
      registered: number;
      /** Eligible unknown tabs not pinged because the sweep hit its cap. 0 if it pinged everyone it wanted to. */
      deferred: number;
      /** Answering tabs refused because the registry was already full of live-listed rows. 0 if every answering tab was registered. */
      crowded: number;
    }
  | TabSweepNotConcluded;

/**
 * 🔴 W62b · **The one written form of "this tick's sweep has no outcome yet".**
 *
 * Its own interface rather than an inline member of the union so that a writer
 * can name the state and a reader can narrow to it — the constant below is the
 * only value a writer is meant to pass, and `isSweepNotConcluded` is the only
 * predicate a reader needs.
 */
export interface TabSweepNotConcluded {
  sweeping: true;
}

/**
 * 🔴 W62b · The value the provisional trace writes, in one spelling.
 *
 * Exported rather than written as an object literal at the call site, so that
 * `background.ts` and any reader are talking about the same state by name, and
 * a grep for `SWEEP_NOT_CONCLUDED` finds every place that produces one.
 */
export const SWEEP_NOT_CONCLUDED: TabSweepNotConcluded = { sweeping: true };

/**
 * 🔴 W62b · **Is this trace something the tick said, or something it has not
 * said yet?**
 *
 * The single predicate for the difference, so no reader has to re-derive it
 * from the shape — a reader that spelled `'sweeping' in sweep` itself would be
 * a second copy of this rule, free to disagree with the first.
 *
 * `true` means the record is **provisional**: the tick that wrote it had not
 * concluded its tab sweep, so the record is not a verdict on that tick and must
 * not be rendered as one. It does **not** mean the sweep failed, and it is not
 * `{ looked: false }` (which is "we tried to look and could not") nor `null`
 * (which stays "this tick never swept").
 */
export function isSweepNotConcluded(
  trace: TabSweepTrace | null | undefined,
): trace is TabSweepNotConcluded {
  return typeof trace === 'object' && trace !== null
    && (trace as { sweeping?: unknown }).sweeping === true;
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
   *
   * 🔴 W47 · **How the tick ended, not only how a run did.** W36 filled this from
   *    the run's report, and that left it `null` on every path that never reached
   *    the run — a tick blocked at a gate came out as `{stopped: null, halted:
   *    null}`, which is the same "it did nothing, and nothing says why" the field
   *    was added to end. So it now carries the tick's **own** named outcome when
   *    there was no run: the value `tickBlockReason`/`tickBackfill` returned, from
   *    the same closed set `reason` above holds. Values are read out of the two
   *    existing closed sets and nothing else — the field answers "how did this
   *    end", and both layers answer it in words that already existed.
   */
  stopped?: StopReason | TickReason | null;
  /**
   * The `HaltReason` the run left behind, or the one a refusal reached **before**
   * the run could start (`migrateLegacyScopes`, `findUnreadableState`, or the
   * engine's own `openLedger` refusal) — `null` when neither happened. This is the
   * field that turns "it ran and nothing moved" into a named fact.
   *
   * 🔴 W47 · **A refusal is named here on ticks that never reach the engine too.**
   *    See `findUnreadableState`: without it, a scope whose state record this build
   *    cannot read was reported as `no-http-port` whenever no platform page was
   *    open, so `null` here did not mean "no refusal happened" — it meant "no
   *    refusal was looked for".
   */
  halted?: HaltReason | null;
  /**
   * The halt's own technical detail, verbatim. Metadata only by construction —
   * the engine's details name keys, paths, statuses and counts, never a
   * conversation body (CLAUDE.md's privacy rule, and the reason the detail is
   * safe to persist at all).
   */
  detail?: string | null;
  /**
   * 🔴 W51 · **Whether this tick swept `chrome.tabs` for a live tab the
   * registry had lost.** See `TabSweepTrace`. Optional so a record written
   * before this field existed still parses (`isTickRecord` does not require
   * it); `null` is the written form of "never swept".
   *
   * 🔴 W62b · `{ sweeping: true }` is the fourth value and is not an outcome:
   *    it is the provisional record saying its sweep had not concluded. A
   *    reader that shows this field to a person must ask
   *    `isSweepNotConcluded` first (see `lastTickNote`) — on a provisional
   *    record the other fields (`ran`, `stopped`, `halted`) are the tick's
   *    state *while it was still running*, not its result.
   */
  tabSweep?: TabSweepTrace | null;
  /**
   * 🔴 W76 · **The fairness half of the trace** — which platform was served and
   *    which were passed over and why, for a tick that walked the registry. See
   *    `TickSchedule`. Optional so a record written before this field existed
   *    still parses (`isTickRecord` does not require it) — the same
   *    compatibility rule the other optional fields follow.
   */
  schedule?: TickSchedule;
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
