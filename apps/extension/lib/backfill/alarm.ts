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
  currentReleaseChannel,
  isPlatformActiveInChannel,
  type ReleaseChannel,
} from '../contract';
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
 *    192), and at `DEFAULT_TICK_DETAILS` bodies per wake (ADR-033: 2) the alarm's
 *    daily capacity is 288-576 bodies (mean 384) against a cap that is also a
 *    random variable (300-400, mean 350). A day that wakes 144 times is under
 *    every cap draw, so the alarm binds; a day that wakes 288 times is over every
 *    draw, so the cap binds; which one leads changes from day to day. That
 *    interleaving is the "the frequency must not be steady" the product asked
 *    for, expressed in the two places that govern the rate rather than
 *    cosmetically.
 *  · **The band is 2× wide**, so two consecutive gaps differ by a factor of up
 *    to 2 — plainly irregular to anyone watching, and impossible to distinguish
 *    from a person working through their own history.
 *  · **It still does not fight the per-item interval**: 300 seconds ≫ the
 *    20-second per-item minimum ⇒ the **first** body of an alarm tick has a
 *    0 wait (the last fetch was minutes ago); the interval bites only on the
 *    second body of the same tick (ADR-033), and when the live leg kicks
 *    repeatedly (C19 task 3).
 *  · Each wake still does one very small thing (read storage, fetch at most
 *    `DEFAULT_TICK_DETAILS`),
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
 * So the cursor records who has been served, and the next tick prefers the
 * targets that have waited longest. It is a single small `storage.local` value,
 * in the same `cs_backfill_*` key family; it is not the header, and it survives
 * an MV3 service-worker reclaim exactly as the trace does.
 *
 * 🔴 W86 · **The cursor names a target, and it used to name a slot.** It was
 *    `{ served: <index> }` — a position in `cs_backfill_targets_v1` — and that
 *    array is not a fixed list: `rememberTarget` **prepends** on every live
 *    capture, so a capture moves one row to the head and shifts the rows above it
 *    down by one. An index read back after that names a *different row*, and the
 *    walk starts one row too early. Where the captured row sat at or ahead of the
 *    cursor, the row the cursor was standing for is served again — in the worst
 *    arrangement, observed as the head being served on every single wake while the
 *    other scopes were never reached at all (W79 recorded the reordering live: a
 *    claude live capture prepended mid-run and persisted in the registry). A
 *    removal has the same shape from the other side: the index then names the row
 *    one *later* than intended, so the row directly behind the served one is
 *    skipped for a cycle.
 *
 *    So the cursor stores the served target's **identity** — `platform` and
 *    `scope`, which is exactly the pair `rememberTarget` dedups on and therefore a
 *    unique key in the registry. Positional drift cannot happen because no
 *    position is stored.
 *
 * 🔴 W86b · **But "the row after the target I served last" is still a rule about
 *    order, and the registry's order is not ours to depend on.**
 *
 *    W86's walk started at the row holding the served identity, **plus one**. The
 *    identity is looked up correctly; which row gets the tick is nonetheless
 *    decided by where that identity *currently sits*, and `rememberTarget` is a
 *    move-to-front. Let the registry be `[A, B, C]` with all three runnable, and
 *    let the user re-capture the scope they were just served (the ordinary act of
 *    switching back to a tab) before each wake: from the third wake on the served
 *    identity is at index 0 every time, so the walk starts at index 1 every time
 *    and the rotation degenerates to `A, B, A, B, …` while `C` — registered and
 *    runnable throughout — is never that slot. A removal is the same defect from
 *    the other side: with the stored identity absent the walk started at index 0,
 *    the head took the wake, and a row that is never the head waited forever.
 *    🩸 The bound W86 wrote on that fallback — "a cursor whose target just
 *    disappeared can cost the other targets one turn. It cannot cost them more
 *    than that" — was false: with `[A, B, C, D]` and the cursor's row forgotten,
 *    `D`, the successor, waits two turns.
 *
 *    So the cursor stops describing **a row** and starts describing **how long
 *    each identity has waited**: a small map from identity to the serve-order rank
 *    of the wake that last served it, in the same key. A wake walks the runnable
 *    targets **least recently served first** — the smallest rank wins, the row just
 *    served goes to the back — and stamps the identity it really served. Registry
 *    order is only a tie-break, so no prepend, removal or permutation can move a
 *    target out of its turn.
 *
 *    🔴 🩸 W86c §1 · The bound that actually holds, and the one that did not.
 *    The comment W86b wrote here — "every runnable target is served within *one
 *    pass over the registry*" — and its ordering rule ("never-served before
 *    served") together stated something false. Raking never-served targets at the
 *    *front* means a brand-new identity takes the very next wake, so a stream of
 *    newcomers — one freshly registered runnable scope before each wake — jumps
 *    the queue forever and an old stamped row that stays registered is never
 *    served (that is the R86b §1 defect, reproduced by the Grok run). So
 *    **never-served now ranks at the back** (§1): a newcomer joins behind the rows
 *    already waiting and is materialised there on its first save, so it rounds
 *    forward like everyone else instead of ahead of them.
 *
 *    The bound that holds, stated exactly as the tests assert it: with `m`
 *    registered runnable targets, `saveTickCursor` keeps the stamps dense (§2) and
 *    puts the row just served at the back, so the walk is a round robin over the
 *    whole set of `m` — **every row that stays registered is served within `m`
 *    real-work wakes, regardless of how many newcomers arrive, and each newcomer
 *    waits at most `m` wakes while at least one cursor mirror accepts writes.**
 *    The session mirror carries successful writes across worker reclaims in one
 *    browser session. If both session and local writes fail, a fresh worker can
 *    reload a stale cursor, so this bound does not hold across reclaims until a
 *    mirror accepts a write. Reorder never changes the smallest rank, and a
 *    skipped non-runnable row keeps its rank, so neither can push out a runnable
 *    row's turn.
 *
 *    The map is bounded by the registry: it is pruned to the registered
 *    identities on every write, `MAX_TARGET_ENTRIES` caps that at 8, and since
 *    W86c (§2) the stamps are dense `0..k-1` on every save — so no value ever
 *    grows past the registry cap, and the `Number.MAX_SAFE_INTEGER` ceiling is
 *    un-reachable (a stored ceiling is renormalised away on its first save).
 *
 * 🔴 Unreadable / absent / **not the map this build writes** ⇒ `null` ("nothing
 * has been served"): every target is never-served and the walk is the registry in
 * its own order. That is the safe fallback — it is the old behaviour, and it can
 * never skip a platform, because with nothing stamped the walk still examines
 * every row. A garbage byte at this key must not be turned into "serve nothing",
 * and neither must a target that was forgotten, evicted by `MAX_TARGET_ENTRIES`,
 * or renamed to a scope this build has not seen: none of those names an identity
 * the registry holds, and a target the registry does not hold is simply not on
 * the walk.
 *
 *    🩸 It also refuses the two shapes an upgrading profile can hold.
 *    `{ served: <n> }` (pre-W86) names a slot in a registry that has since been
 *    reordered, and `{ platform, scope }` (W86) is one identity with no record of
 *    how long anyone has waited. Reading either of them as a map would invent a
 *    whole schedule out of one value, so both read as no cursor at all and the
 *    first wake after the upgrade starts with every target never-served — one
 *    turn's cost, once, instead of a schedule that is wrong for everyone.
 *
 *    🔴 W76b's rule survives the re-shape unchanged: **a value that does not name
 *    a target is not a cursor.** It was written when the cursor was an index,
 *    because `1.5` satisfied "finite number ≥ 0": `(1.5 + 1) % n` is not an
 *    integer, `targets[2.5]` is `undefined`, every slot `continue`d — and because
 *    nobody ran, `saveTickCursor` was never reached, so the same `1.5` was loaded
 *    by every later wake and one bad byte served nobody *forever*. What replaces
 *    the index is a stamp, and a stamp that is not a non-negative safe integer is
 *    not a stamp.
 */
export const BACKFILL_CURSOR_KEY = 'cs_backfill_cursor_v1';

/**
 * 🔴 W86b · **How long each target has waited, as a serve-order rank.**
 *
 * A rank rather than a wall-clock time, deliberately: the walk only ever compares
 * these values with each other, and a clock that steps backwards (an NTP
 * correction, the user changing the system time) would make the target served
 * last look like the one that has waited longest. Since W86c (§2) the ranks are
 * **dense**: `saveTickCursor` renormalises every kept stamp to `0..k-1` on each
 * write, and the row just served is placed at the highest rank, so the values
 * never grow past the count of registered rows (and therefore never past
 * `MAX_TARGET_ENTRIES`).
 *
 * An identity that is **absent** has never been served — a different fact from
 * "served at the oldest rank 0" — so it sorts **behind** every stamp (at the back
 * of the rotation, §1), not ahead of them.
 */
export interface TickCursor {
  /**
   * Identity key (`targetIdentityKey`) → the rank of the wake that last served
   * that target, condensed to a dense `0..k-1` on every save. An identity that is
   * **absent** has never been served and sorts at the back of the rotation.
   */
  served: Record<string, number>;
  /** Write generation used to select the newer local/session mirror. */
  revision?: number;
}

/**
 * `platform`+`scope` as one map key: the pair `rememberTarget` dedups on, so it
 * names exactly one registry row.
 *
 * 🔴 The separator is a NUL, as in `rememberOrganizationScopedTarget`, because a
 *    platform id is one path segment that never contains one while a scope is an
 *    opaque account identifier that can contain anything else — so no two
 *    identities can produce the same key.
 */
function targetIdentityKey(platform: string, scope: string): string {
  return `${platform}\0${scope}`;
}

/**
 * The stored value as a map, or `null` when it is not one this build writes.
 *
 * Two rules, and they are deliberately not the same rule:
 *  · the **outer** shape has to be the map. Anything else — a bare number, a
 *    string, `{ served: 1 }`, an array, or either of the two shapes an upgrading
 *    profile holds (the pre-W86 position and the W86 identity) — is not a cursor
 *    at all, and reads as "nothing has been served" (`null`);
 *  · a single **entry** whose stamp is not a non-negative safe integer is dropped
 *    and the rest of the map is kept. One corrupt entry costs one target its
 *    place in the rotation: its row reads as never-served and, since W86c (§1),
 *    joins the rotation at the *back* — served within `k` wakes — so the cost is
 *    bounded and safe, while discarding the whole map because of it would cost
 *    every other target the schedule it has.
 */
function readTickCursor(raw: unknown): TickCursor | null {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return null;
  const map = (raw as { served?: unknown }).served;
  if (typeof map !== 'object' || map === null || Array.isArray(map)) return null;
  const served: Record<string, number> = {};
  for (const [key, stamp] of Object.entries(map as Record<string, unknown>)) {
    // A key this build writes is always `<platform>\0<scope>`. A key without the
    // separator names no target, so it is not a stamp — and requiring the
    // separator also means no stored key can ever be `__proto__`.
    if (!key.includes('\0')) continue;
    if (typeof stamp !== 'number' || !Number.isSafeInteger(stamp) || stamp < 0) continue;
    served[key] = stamp;
  }
  const revision = (raw as { revision?: unknown }).revision;
  return {
    served,
    ...(typeof revision === 'number' && Number.isSafeInteger(revision) && revision >= 0
      ? { revision }
      : {}),
  };
}

const BACKFILL_CURSOR_SESSION_KEY = `${BACKFILL_CURSOR_KEY}:session`;

type CursorSessionArea = {
  get: (defaults: Record<string, unknown>) => Promise<Record<string, unknown>>;
  set: (values: Record<string, unknown>) => Promise<void>;
};

/** The session mirror survives worker restarts and is cleared when the browser restarts. */
function cursorSessionArea(): CursorSessionArea | null {
  const g = globalThis as {
    browser?: { storage?: { session?: CursorSessionArea } };
    chrome?: { storage?: { session?: CursorSessionArea } };
  };
  const area = g.browser?.storage?.session ?? g.chrome?.storage?.session;
  return area && typeof area.get === 'function' && typeof area.set === 'function' ? area : null;
}

/** The map restricted to the identities `targets` holds. */
function pruneToRegistry(
  cursor: TickCursor,
  targets: readonly BackfillTarget[],
): Record<string, number> {
  const registered = new Set(targets.map((t) => targetIdentityKey(t.platform, t.scope)));
  const served: Record<string, number> = {};
  for (const [key, stamp] of Object.entries(cursor.served)) {
    if (registered.has(key)) served[key] = stamp;
  }
  return served;
}

/**
 * 🔴 W86b · **The order one wake walks the registry in: least recently served
 * first.** ✓ W86c sharpens the never-served end of it (§1).
 *
 * A row with a stamp is ranked by that stamp; a row with **no** stamp — never
 * served, a corrupt stamp dropped, or a brand-new identity — is ranked at the
 * **back** (`Infinity`), not ahead of every stamp. This rotation survives worker
 * reclaims when either cursor mirror accepts its write; if both writes fail, the
 * in-worker rotation is lost at reclaim and a stale map can repeat. Ties (equal stamps, or the
 * whole never-served group) are broken by the registry's own order. That is the
 * whole selection rule, and it reads nothing but the stamps — no position, no
 * stored order, no "after the last one" — so a registry that was prepended to,
 * reordered or partially emptied since the previous wake cannot move a target
 * out of its turn.
 *
 * 🩸 W86c §1 **Why never-served ranks at the back.** `-1` (ahead of every stamp)
 * made each brand-new identity take the wake, so a stream of newcomers — one
 * fresh registered scope before every wake — jumped the queue forever, and an
 * old stamped row that stayed registered was never served. `Infinity` is the
 * *back*: a newcomer joins behind the rows already waiting, so it cannot push
 * them out of their turn. It is the back no matter how large the stored stamps
 * are (not "max stamp + 1", which is what fails to round-trip for a stored
 * `Number.MAX_SAFE_INTEGER` in §2) — so even a pre-§2 profile that stored the
 * ceiling value is served safely here, before its first renormalising save.
 *
 * The **bound that holds** — what the property loop asserts: let `m` be the number
 * of registered runnable targets. Because every save renormalises the stamps to
 * dense ranks `0..m-1` and places the row just served at the back (§2), the
 * rounds are a round robin over the whole registered set of `m`: each is served
 * once in every `m` real-work wakes, **no matter how many newcomers arrive, while
 * at least one storage mirror accepts cursor writes** (a newcomer is materialised
 * at the back on its first save and then rounds forward like everyone else, so it
 * waits at most `m` wakes). The session mirror preserves successful writes across
 * worker reclaims in the browser session; if both mirror writes fail, the in-worker
 * rotation is lost at reclaim and a stale map can repeat. Reorder never changes
 * the smallest stamp, and a skipped non-runnable row keeps its stamp, so it cannot
 * push out a runnable row's turn either.
 *
 * `null` — no stamp is known for anyone — is every target never-served, all at
 * the back, tie-broken to the registry in its own order, which examines every
 * row and so can never skip a platform.
 *
 * The result is the **indices** of `targets`, so the caller keeps its own
 * per-row bookkeeping (the tick trace names the platform it examined) against the
 * rows it already has.
 *
 * `Infinity` is deliberately never stored: it exists only in this transient
 * ranking, and `readTickCursor` rejects any stored stamp that is not a safe
 * integer, so a real byte can never contain it.
 */
export function tickWalkOrder(
  targets: readonly BackfillTarget[],
  cursor: TickCursor | null,
): number[] {
  const ranked = targets.map((target, index) => ({
    index,
    // `Infinity` rather than a missing field or `-1`: a never-served target must
    // sort **after** every stamp (at the back of the rotation, §1), and every
    // safe-integer stamp is < `Infinity`. `Infinity` is also immune to the size
    // of the stamps, which `max+1` is not for a stored `MAX_SAFE_INTEGER` (§2).
    stamp: cursor?.served?.[targetIdentityKey(target.platform, target.scope)] ?? Infinity,
  }));
  // Two `Infinity`s make `a.stamp - b.stamp` `NaN`, which is falsy in `||`, so
  // the never-served group and every equal-stamp tie fall through to the index
  // (registry order) tie-break — exactly the rule described above.
  ranked.sort((a, b) => a.stamp - b.stamp || a.index - b.index);
  return ranked.map((r) => r.index);
}

/**
 * 🔴 W76b · **What this *worker* has read back or served, which is newer than
 * anything it can read out of storage when the write is failing.**
 *
 * `saveTickCursor` is best-effort by design (a failed write must not fail the
 * tick), and it logged and moved on — but the walk's order used to be *only* read
 * back from storage, so a write that keeps failing pinned it at the last value
 * that ever landed: the head monopoly W76 removed, restored by an unrelated
 * storage fault, with nothing in the trace to say so.
 *
 * So the same fact is kept in memory as well, and it is the one that decides: it
 * advances on every serve whether or not either storage write succeeds. The
 * session mirror carries that map across worker reclaims within the browser
 * session; local storage remains durable across browser restarts. Revisions let
 * a worker select the newer mirror if only one write succeeds. If both areas
 * reject writes, a fresh worker can reload only the last persisted map and may
 * repeat a row until one area accepts a write.
 *
 * `null` = this worker has neither read nor served any schedule yet, so storage
 * is the only witness.
 */
let servedThisWorker: TickCursor | null = null;

/**
 * Read the cursor. Unreadable / absent / not the shape this build writes ⇒
 * `null` ("nothing has been served"), never a fabricated schedule.
 *
 * 🔴 W76b · The in-memory map wins while it exists: it is the same value the
 *    successful writes store, and the only one that keeps advancing when they
 *    fail. Reading storage into it is also what lets a later `saveTickCursor`
 *    write the whole schedule back rather than only the stamp it is adding.
 */
export async function loadTickCursor(store: BackfillStore | null): Promise<TickCursor | null> {
  if (servedThisWorker === null && store !== null) {
    let local: TickCursor | null = null;
    let session: TickCursor | null = null;
    try {
      local = readTickCursor(await store.load(BACKFILL_CURSOR_KEY));
    } catch {
      // The session mirror can still preserve rotation during a local read fault.
    }
    const area = cursorSessionArea();
    if (area) {
      try {
        const got = await area.get({ [BACKFILL_CURSOR_SESSION_KEY]: null });
        session = readTickCursor(got[BACKFILL_CURSOR_SESSION_KEY]);
      } catch {
        // Session storage is optional; keep the local answer if the mirror is unavailable.
      }
    }
    servedThisWorker = session && (!local || (session.revision ?? 0) > (local.revision ?? 0))
      ? session
      : local;
  }
  return servedThisWorker;
}

/**
 * 🔴 W86c · Rebuild the rotation after one serve: prune to the registry, **renormalise
 * to dense ranks `0..k-1`**, and place the row just served at the back. Best-effort
 * write, same rule as the trace: a failed write is logged, never a reason to fail
 * the tick.
 *
 * The next map is built as a head-to-tail order, then collapsed to dense ranks:
 *   · rows that were already stamped keep their relative order (smaller rank =
 *     served longer ago — {@link saveTickCursor}'s direct expression of §1's
 *     "least recently served first");
 *   · registered rows with **no** stamp — the newcomers `tickWalkOrder` ranked at
 *     the back — are materialised behind them, in registry order (§1: a newcomer
 *     joins the rotation at the back, then rounds forward like everyone else, so
 *     it waits at most `m` wakes without jumping anyone's queue);
 *   · the row just served is moved to the very back (most recently served).
 *
 * Renormalising to `0..k-1` is the whole of §2: `k` is at most the number of
 * registered rows, which `MAX_TARGET_ENTRIES` caps at 8, so **no stored value ever
 * grows past the registry cap**, the `Number.MAX_SAFE_INTEGER` ceiling is
 * un-reachable, and the byte round-trips indefnitely. The smaller-rank-waiting /
 * served-to-back shape is preserved under the densification, so the round-robin
 * each wake serves is unchanged by it.
 *
 * `targets` is the registry **as this wake read it**, and it is what the map is
 * pruned against: an identity the registry no longer holds names no row, so its
 * stamp could never decide anything again, and keeping it would grow the map for
 * as long as scopes come and go. Pruning at the one place the map is written is
 * what keeps it bounded by `MAX_TARGET_ENTRIES`.
 */
export async function saveTickCursor(
  store: BackfillStore | null,
  targets: readonly BackfillTarget[],
  platform: string,
  scope: string,
): Promise<void> {
  const prev = pruneToRegistry(servedThisWorker ?? { served: {} }, targets);
  const registered = targets.map((t) => targetIdentityKey(t.platform, t.scope));
  const servedKey = targetIdentityKey(platform, scope);

  // Head-to-tail order of the next rotation (smallest rank served soonest):
  const order: string[] = Object.keys(prev).sort((a, b) => prev[a]! - prev[b]!);
  for (const key of registered) {
    if (!(key in prev)) order.push(key); // a newcomer (§1) joins the back
  }
  const servedAt = order.indexOf(servedKey);
  if (servedAt !== -1) order.splice(servedAt, 1);
  order.push(servedKey); // the row just served is the most recent → the back

  // Dense ranks 0..k-1 in that order (§2): the served row carries the largest,
  // k-1, so no value ever reaches k and none can exceed MAX_TARGET_ENTRIES.
  const served: Record<string, number> = {};
  for (let i = 0; i < order.length; i += 1) served[order[i]!] = i;

  // 🔴 Before the write, not after: a write that throws still happened as far as
  //    the walk is concerned, and the next wake must start after it (see
  //    `servedThisWorker`).
  servedThisWorker = { served, revision: (servedThisWorker?.revision ?? 0) + 1 };
  const session = cursorSessionArea();
  if (session) {
    try {
      await session.set({ [BACKFILL_CURSOR_SESSION_KEY]: servedThisWorker });
    } catch (err) {
      console.warn('[chat-stasher] backfill session cursor write failed', (err as Error).message);
    }
  }
  if (!store) return;
  try {
    await store.save(BACKFILL_CURSOR_KEY, servedThisWorker);
  } catch (err) {
    console.warn('[chat-stasher] backfill tick cursor write failed', (err as Error).message);
  }
}

/**
 * W76 · Why a target was passed over by a tick's fair rotation without running.
 * A small closed set — each value is a different fact, and none of them is
 * "it ran" (a target that ran consumes its tick and is reported as `served`).
 *
 * 🔴 W76b · The last two are a **different kind** of "passed over", and the set
 *    keeps them apart on purpose. `no-http-port`/`halted`/`waiting-retry` are
 *    reasons the target *could not* run. `daily-cap`/`state-unreadable` are
 *    reasons it *would have made no request*: the engine reaches
 *    `finish('daily-cap')` and `openLedger`'s unreadable-record refusal before it
 *    fetches anything, so a tick spent on either is a tick the platforms behind
 *    are owed. Both are decidable from this scope's stored header alone — see
 *    `tickIdleReason`, which is also where the two no-request classes that are
 *    **not** skip-able are written down and why.
 */
export type TickSkipReason =
  | 'no-http-port'
  | 'halted'
  | 'waiting-retry'
  | 'daily-cap'
  | 'state-unreadable';

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

/** Every valid row in storage, before any channel filter. The write side needs these. */
async function loadRawTargets(store: BackfillStore): Promise<BackfillTarget[]> {
  const raw = await store.load(BACKFILL_TARGETS_KEY);
  return Array.isArray(raw) ? raw.filter(isTarget) : [];
}

/**
 * 🔴 W91b · Split stored rows into the ones this channel **serves** and the
 * leftover rows owned by another channel.
 *
 * The filter must happen on **read** only: a stable build has to ignore an
 * experimental platform's row without deleting it, so every writer below takes
 * the raw rows, edits only the active half, and appends the leftover half back
 * unchanged. Nothing in this module can then drop a row it did not own.
 */
function partitionTargetsByChannel(
  targets: readonly BackfillTarget[],
  channel: ReleaseChannel,
): { active: BackfillTarget[]; leftover: BackfillTarget[] } {
  const active: BackfillTarget[] = [];
  const leftover: BackfillTarget[] = [];
  for (const t of targets) {
    if (isPlatformActiveInChannel(t.platform, channel)) active.push(t);
    else leftover.push(t);
  }
  return { active, leftover };
}

/**
 * 🔴 W91 · The target registry, read for one release channel.
 *
 * `channel` exists so a test can drive the stable channel deterministically
 * instead of relying on the build-time constant the suite pins to `dev`; every
 * production caller omits it and gets the active build's channel. The filter is
 * what makes a stable build **ignore** an experimental platform's leftover row
 * rather than serve it — the row is left in storage untouched (W91b).
 */
export async function loadTargets(
  store: BackfillStore | null,
  channel: ReleaseChannel = currentReleaseChannel(),
): Promise<BackfillTarget[]> {
  if (!store) return [];
  return (await loadRawTargets(store)).filter((t) => isPlatformActiveInChannel(t.platform, channel));
}

/**
 * 🔴 W54 · **Why one registry row left, in one written-down word.**
 *
 * A registry write can remove rows for two different reasons, and they must not
 * be conflated because what they say about the scope is different:
 *  · `'non-organization'` (W49/W49b): the row named no account — a leftover
 *    conversation title or the unresolved sentinel — and the platform it
 *    belonged to now has a real organization row. That drop is permanent.
 *  · `'registry-cap'` (W54): the row lost its seat in the `MAX_TARGET_ENTRIES`
 *    cache. The scope is real (it can be a live organization); only its seat is
 *    gone, and a later capture re-registers it.
 *
 * Why the second reason had to become a recorded fact rather than an
 * implementation detail: before W54 the cap was a plain `slice`, so an evicted
 * row fell out of both bookkeepings a write has — it never entered `dropped`
 * (which only the organization-scoped writer filled, and only with
 * non-organization rows of the platform being collapsed) and it was not in
 * `stillPresent` (built from what survived). The registry row vanished while
 * the scope's `cs_backfill_v2:<platform>:<scope>` header stayed behind, and
 * since W49b the popup hides a header whose scope is no longer a registered
 * target — an orphaned ledger, invisible as well as uncleaned. An organization
 * could be the row that fell off, which is the same starvation D1 was written
 * against, arriving through a different door.
 */
export type TargetDropReason = 'non-organization' | 'registry-cap';

/** One registry row a write removed, with the reason it may never be silent about. */
export interface DroppedTarget {
  target: BackfillTarget;
  reason: TargetDropReason;
}

/**
 * 🔴 W54 · Apply D4's rule to every row a registry write dropped, whatever
 * dropped it. A row that lost its seat also loses its local ledger header
 * (`cs_backfill_v2:<platform>:<scope>` — halt, cursor, failures), unless the
 * same platform+scope still holds a seat: stored storage can carry a duplicated
 * row, one copy can fall off the cap while the other stays, and the header
 * belongs to the identity, not to either copy. The host archive is append-only
 * and is never touched.
 */
async function removeDroppedHeaders(
  store: BackfillStore,
  dropped: readonly DroppedTarget[],
  next: readonly BackfillTarget[],
): Promise<void> {
  const stillPresent = new Set(next.map((t) => `${t.platform}\0${t.scope}`));
  for (const { target } of dropped) {
    if (stillPresent.has(`${target.platform}\0${target.scope}`)) continue;
    await store.remove(stateKey(target.platform, target.scope));
  }
}

/**
 * 🔴 W54 · The rows one registry write's cap evicts, recorded with their reason.
 *
 * Pure: given the active rows a write keeps *before* the cap is applied — the
 * `[target, ...rest]` both writers build — this is the tail `MAX_TARGET_ENTRIES`
 * slices off, as `dropped` entries with `reason: 'registry-cap'`. Both writers
 * take their eviction record from here so the record cannot drift from the
 * slice it describes, and a test can pin the reason without intercepting
 * anything.
 *
 * 🔴 The record is deliberately **not** a console line. This module warns for
 *    faults — a write that failed, a store that could not be listed — and a cap
 *    eviction is not one: it is the bounded cache doing what it is for, exactly
 *    like W49b's collapse drop, which also removes a row and its header without
 *    a warn. What would make the eviction *silent* is the pre-W54 shape: the
 *    row falls out of both of a write's bookkeepings (`dropped` and
 *    `stillPresent`), so its `cs_backfill_v2:<platform>:<scope>` header stays
 *    behind — orphaned, and since W49b hidden by the popup's registered-scope
 *    filter. Recording the eviction as a drop with its reason is what makes it
 *    honest: the header is removed with the row (`removeDroppedHeaders`), so
 *    storage tells one story instead of leaving evidence no surface can reach.
 */
export function capEvictions(
  withIncoming: readonly BackfillTarget[],
  cap: number = MAX_TARGET_ENTRIES,
): DroppedTarget[] {
  return withIncoming
    .slice(cap)
    .map((target) => ({ target, reason: 'registry-cap' }));
}

/**
 * Record one target (deduplicated by platform+scope, most recent first).
 *
 * 🔴 W91b · The cap is applied to the **active** rows only, and the leftover
 * rows of the other channel are appended untouched: an experimental row must
 * neither consume a stable slot nor be evicted when a stable row is recorded.
 *
 * 🔴 W54 · The rows the cap pushes off the tail are *recorded drops*, never a
 *    silent truncation: each one carries `reason: 'registry-cap'` (the record
 *    comes from `capEvictions`, so it cannot drift from the slice), loses its
 *    local ledger header (`removeDroppedHeaders`), and never stays behind as
 *    an orphan the popup hides — whatever the row kind.
 */
export async function rememberTarget(
  store: BackfillStore | null,
  target: BackfillTarget,
  channel: ReleaseChannel = currentReleaseChannel(),
): Promise<BackfillTarget[]> {
  if (!store) return [];
  if (!isPlatformActiveInChannel(target.platform, channel)) {
    return await loadTargets(store, channel);
  }
  const { active, leftover } = partitionTargetsByChannel(await loadRawTargets(store), channel);
  const rest = active.filter(
    (t) => !(t.platform === target.platform && t.scope === target.scope),
  );
  const withIncoming = [target, ...rest];
  const nextActive = withIncoming.slice(0, MAX_TARGET_ENTRIES);
  const dropped: DroppedTarget[] = capEvictions(withIncoming);
  await store.save(BACKFILL_TARGETS_KEY, [...nextActive, ...leftover]);
  await removeDroppedHeaders(store, dropped, nextActive);
  return nextActive;
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
 *
 * 🔴 W91b · It reads the **raw** registry and removes only the named row, so
 *    every other row — including an experimental platform's leftover — is
 *    carried through the write byte-for-byte.
 */
export async function forgetTarget(
  store: BackfillStore | null,
  platform: string,
  scope: string,
): Promise<void> {
  if (!store) return;
  const next = (await loadRawTargets(store)).filter(
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
 * rows — the collapsed non-organization ones, and 🔴 W54 also every row the
 * `MAX_TARGET_ENTRIES` cap pushes off the tail, whatever its kind — lose their
 * `cs_backfill_v2:<platform>:<scope>` header (halt, cursor, failures): that key
 * is not unreachable — the popup walks every header — and claiming nothing was
 * written there was false. The host archive is append-only and is not touched.
 * A header whose scope is re-inserted (collapsing title → `'default'` when no
 * org exists) is kept, and so is the header of an evicted duplicate whose
 * identity still holds a seat.
 *
 * 🔴 W54 · An eviction is recorded, never silent: every row the cap pushes off
 *    is a `dropped` entry with `reason: 'registry-cap'`, taken from
 *    `capEvictions` so the record cannot drift from the slice. Before W54 an
 *    evicted organization fell out of both `dropped` and `stillPresent`: its
 *    registry row gone, its ledger header left behind, hidden since W49b by the
 *    popup's registered-scope filter. Cap evictions are by-design cache
 *    pressure, so like W49b's collapse drop they carry no console warn — this
 *    module's warns are for faults; the eviction's record is the drop and the
 *    header that leaves with the row.
 *
 * The caller names what an organization looks like, because this module does
 * not know any platform's identifier shape.
 *
 * 🔴 W91b · Like `rememberTarget`, this reads the **raw** registry, splits it by
 *    the active channel, edits only the active rows, and appends the leftover
 *    rows back untouched. A stable build therefore cannot drop a dev build's
 *    Perplexity/Kimi target — and their ledger headers are not touched either,
 *    because only rows of the active platform are ever `dropped`, by the
 *    collapse or by the cap alike.
 */
export async function rememberOrganizationScopedTarget(
  store: BackfillStore | null,
  target: BackfillTarget,
  isOrganizationScope: (scope: string) => boolean,
  channel: ReleaseChannel = currentReleaseChannel(),
): Promise<BackfillTarget[]> {
  if (!store) return [];
  const { active: current, leftover } = partitionTargetsByChannel(await loadRawTargets(store), channel);
  const incomingIsOrg = isOrganizationScope(target.scope);
  const dropped: DroppedTarget[] = [];
  const kept: BackfillTarget[] = [];
  for (const t of current) {
    if (t.platform === target.platform && !isOrganizationScope(t.scope)) {
      dropped.push({ target: t, reason: 'non-organization' });
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
    const withIncoming = [target, ...rest];
    next = withIncoming.slice(0, MAX_TARGET_ENTRIES);
    // 🔴 W54 · Whatever the row kind — the collapse above only ever records
    //    non-organization rows of this platform, so an organization pushed off
    //    the cap would otherwise fall out of both `dropped` and `stillPresent`,
    //    its registry row gone and its ledger header left invisible behind it.
    dropped.push(...capEvictions(withIncoming));
  }
  await store.save(BACKFILL_TARGETS_KEY, [...next, ...leftover]);
  await removeDroppedHeaders(store, dropped, next);
  return next;
}

/**
 * 🔴 W49 · Drop every target for `platform` whose scope is **not** an
 * organization, and remove those scopes' local ledger headers.
 *
 * Prefer `rememberOrganizationScopedTarget` when a row is being stored: that
 * path is one registry write and will not insert an unresolved row in front
 * of a live organization. This remains the drop-only form.
 *
 * 🔴 W91b · It reads the **raw** registry and drops only rows of the named
 *    platform, so another channel's leftover rows survive the write untouched.
 */
export async function forgetNonOrganizationTargets(
  store: BackfillStore | null,
  platform: string,
  isOrganizationScope: (scope: string) => boolean,
): Promise<void> {
  if (!store) return;
  const current = await loadRawTargets(store);
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
export async function migrateLegacyScopes(
  store: BackfillStore | null,
  channel: ReleaseChannel = currentReleaseChannel(),
): Promise<LegacyMigration> {
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
    if (!named || !isPlatformActiveInChannel(named.platform, channel)) continue;
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
  channel: ReleaseChannel = currentReleaseChannel(),
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
    if (!named || !isPlatformActiveInChannel(named.platform, channel)) continue;
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
  /** Which page-owned fact established a Claude scope for this tick. */
  claudeScopeSource?: 'observed' | 'cookie' | 'organizations-endpoint';
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
