/**
 * C13 · The backfill leg's **runtime wiring**.
 *
 * After C11/C12 the backfill leg's modules were complete and its tests were green,
 * but entrypoints/background.ts never imported runBackfill from end to end — which
 * is to say it could never execute in a browser. This file is that missing wire:
 * it writes "when does one run happen" as a pure, testable, switchable function,
 * which background.ts wakes alongside the live leg's trigger.
 *
 * Heartbeats (as of W2): two — the live leg's accompanying kick when it is woken
 * (C13), and chrome.alarms waking on a period (C19, see lib/backfill/alarm.ts).
 * Both go through the same tickBackfill, with identical gate ordering. C13 refused
 * a timer because the manifest had no 'alarms' then; C19 added it to permissions,
 * so that reason no longer holds.
 *
 * 🔴 Why exactly 1 debt per tick (DEFAULT_TICK_DETAILS):
 *    An MV3 service worker is reclaimed when idle, so one tick has to be short.
 *    The engine persists immediately after every debt it clears, so "short ticks ×
 *    many" and "one long tick" are equivalent in progress — but the former is
 *    friendly to the SW lifecycle and naturally gentler, which is exactly the
 *    product's "quietly finish over several days".
 */

import { runBackfill, type BackfillOptions, type HttpPort, type RunReport } from './engine';
import { loadHostPause, resumeBackfill } from '../host-status';
import type { BackfillStore } from './store';

/** The switch's storage key. Same cs_* key family; no new permission. */
export const BACKFILL_ENABLED_KEY = 'cs_backfill_enabled_v1';

/**
 * 🔴 **Off** by default. The argument is in the C13 report; the one-line version:
 * the live leg only archives "the conversation the user has open right now", so
 * defaulting it on is imperceptible; the backfill leg takes the user's login and
 * walks **an entire account's history**, writing hundreds or thousands of shards
 * into the local host's stage —
 * that is a different kind of behaviour and needs an explicit "on" first. Once it
 * has been turned on it is persisted, so the user is not asked over and over.
 */
export const BACKFILL_DEFAULT_ENABLED = false;

export async function isBackfillEnabled(store: BackfillStore | null): Promise<boolean> {
  if (!store) return false;
  const raw = await store.load(BACKFILL_ENABLED_KEY);
  // Only a strict true counts. Unreadable / wrong shape ⇒ fall back to the default (off); never "guess that the user agreed".
  return raw === true ? true : BACKFILL_DEFAULT_ENABLED;
}

/**
 * The explicit switch.
 * C13 wrote "the function is the interface, this task builds no UI" — with the
 * result that **no production code called it** and the user could not turn it on
 * at all. C18 supplied the call site: the checkbox in entrypoints/popup/main.ts.
 */
export async function setBackfillEnabled(store: BackfillStore | null, on: boolean): Promise<boolean> {
  if (!store) return false;
  await store.save(BACKFILL_ENABLED_KEY, on === true);
  return on === true;
}

/** How many debts one tick may clear at most. */
export const DEFAULT_TICK_DETAILS = 1;

/**
 * Why one tick did or did not run.
 * Every value is an **explicit, assertable** outcome — "something silently
 * happened to nothing" is not allowed.
 */
export type TickReason =
  /** The previous tick had not finished (the single-flight lock). */
  | 'already-running'
  /** No storage.local ⇒ no stop-and-resume ⇒ do not run. */
  | 'no-store'
  /** The switch is off (the default state). */
  | 'disabled'
  /**
   * 🔴 W2 · The last delivery found this machine's host unreachable, so this leg
   * is paused, and this heartbeat's `hello` still did not succeed. Not one debt
   * was moved; it carries on from the breakpoint once the host answers.
   * Its difference from 'halted' is described under host-unavailable in
   * lib/backfill/types.ts.
   */
  | 'host-paused'
  /**
   * 🔴 C30 · There are no backfill targets at all ⇒ we do not even know "where to
   * start from".
   * This is **not** a port problem: the channel may be perfectly connected (a
   * platform page is open, the ping answers), but the registry
   * (cs_backfill_targets_v1) is empty — only a real archived capture by the live
   * leg writes down platform/origin/account scope. Before C30 this outcome was
   * conflated with 'no-http-port', so whoever diagnosed it went off chasing the
   * port, and the port was never broken.
   * 📌 A wrong reason is harder to investigate than no reason.
   */
  | 'no-targets'
  /** No http port injected ⇒ there will never be network activity (see the long note below). */
  | 'no-http-port'
  /**
   * 🔴 W76 · **The registry has targets and none of them may run right now.**
   *
   * The alarm's walk (see `runAlarmTickBody`) skips a target that has no live tab,
   * a target held by a permanent halt that still applies, and a target still inside
   * a transient backoff — and it must keep walking past all three, or the first
   * platform in the registry takes every tick (W72 §1, the defect this names).
   * When nothing along the whole walk was runnable, no run happened, so this is the
   * tick's own outcome.
   *
   * Why it is not `no-http-port`: a live tab was found and deliberately not used,
   * so the channel is fine. Why it is not `no-targets`: the registry is not empty.
   * `schedule.skipped` in the same trace names the platform and the reason code for
   * each one passed over, so this outcome is never the only thing on record.
   * 📌 A wrong reason is harder to investigate than no reason.
   */
  | 'no-runnable-target'
  /** runBackfill really was called. */
  | 'ran';

export interface TickResult {
  ran: boolean;
  reason: TickReason;
  report: RunReport | null;
}

/**
 * The five gates that answer "why will this kick not move".
 * 'already-running' and 'ran' are not among them: the first is a transient
 * concurrency state, the second is not a gate.
 */
export type TickBlockReason = Extract<
  TickReason,
  'no-store' | 'disabled' | 'host-paused' | 'no-targets' | 'no-http-port'
>;

/**
 * 🔴 The **single authority** for gate ordering.
 *
 * Why it was extracted: C18's popup has to tell the user "can it actually run right
 * now, and which gate is it stuck on". If the popup wrote those same ifs again,
 * the two would drift — and on the day they drifted, the popup would say
 * "archiving" while not one conversation was being fetched. So tickBackfill and
 * the popup share this one function, and the order is identical by construction.
 *
 * The values are thunks rather than booleans: it preserves tickBackfill's original
 * **laziness** (when the switch is off, the host-pause store is never read), making
 * the behaviour byte-identical to C13.
 */
export async function tickBlockReason(gate: {
  hasStore: boolean;
  isEnabled: () => boolean | Promise<boolean>;
  /**
   * 🔴 W2 · The pause state for "the delivery exit is unreachable". It replaced
   * C12's download-stall guard: the reason to pause then was "downloads will not
   * finish writing", the only reason now is "this machine's host is not there".
   * The check still comes before the http port: while paused we should not even
   * ask whether to send a request.
   */
  isHostPaused: () => boolean | Promise<boolean>;
  hasHttp: boolean;
  /**
   * 🔴 C30 · Whether there are any **backfill targets**.
   * Omitted ⇒ treated as "there are", which is the live leg's real situation: it is
   * holding a target already and never consults the registry. So existing call
   * sites need not change a character, and the behaviour is unchanged.
   * The alarm's path and the popup must pass it explicitly — their targets can only
   * come from the registry.
   */
  hasTargets?: boolean;
}): Promise<TickBlockReason | null> {
  if (!gate.hasStore) return 'no-store';
  if (!(await gate.isEnabled())) return 'disabled';
  if (await gate.isHostPaused()) return 'host-paused';
  // 🔴 The order matches what runAlarmTick really does: read the registry for a
  //    target first, and only then build a channel. With no target we cannot even
  //    answer "which origin should the channel be built for", so this gate comes
  //    before the port.
  if (gate.hasTargets === false) return 'no-targets';
  if (!gate.hasHttp) return 'no-http-port';
  return null;
}

export interface TickDeps {
  store: BackfillStore | null;
  platform: string;
  origin: string;
  scope: string;
  /**
   * 🔴 The network port for fetching bodies / enumerating. **Deliberately has no
   * default.**
   * Without an injection it returns 'no-http-port' immediately and the engine does
   * not even touch it — so "will the backfill leg send a request" is decided
   * explicitly in one place by the wiring, rather than hidden in a default
   * parameter. In C13's output **nowhere** injected a real fetch.
   */
  http?: HttpPort;
  /**
   * The W2 gate. Omitted ⇒ the production implementation (lib/host-status.ts's pause
   * record + `hello`).
   * Tests can inject a fake so that both "host absent ⇒ pause" and "hello succeeds
   * ⇒ resume" can be asserted without really starting a local process.
   */
  host?: HostGate;
  /** The archive exit: the same write-down function as the live leg, so the on-disk logic does not fork. */
  sink?: BackfillOptions['sink'];
  maxDetails?: number;
  pace?: BackfillOptions['pace'];
  clock?: BackfillOptions['clock'];
  /**
   * 🔴 W16 · The injected source of randomness for the jittered gaps and the
   * day's cap draw. Omitted ⇒ the production `Math.random`; neither of
   * background.ts's two call sites sets it, so the shipped behaviour is not
   * decided anywhere in a test.
   */
  random?: BackfillOptions['random'];
  shouldAbort?: () => boolean;
}

/**
 * The single-flight lock. An MV3 SW is a single-threaded event loop, and a
 * module-level variable is valid for the SW's lifetime; once the SW is reclaimed
 * the lock disappears naturally — which is fine, because all progress lives in
 * storage, and the lock only stops two runs in the same lifetime from racing each
 * other over the same debts.
 */
let inFlight = false;

/** For tests only: an SW being reclaimed ⇒ module state resets. */
export function resetTickLockForTest(): void {
  inFlight = false;
}

/**
 * W2 · The shape of the host gate.
 *  · `paused()` — whether we are currently in the "exit unreachable" pause state
 *    (reads that record out of storage);
 *  · `resume()` — try one `hello`; success ⇒ clear the pause and return true.
 */
export interface HostGate {
  paused(): Promise<boolean>;
  resume(): Promise<boolean>;
}

/** The production implementation: the pause record + one real `hello` (§10's resume condition). */
export function productionHostGate(store: BackfillStore | null): HostGate {
  return {
    paused: async () => (await loadHostPause(store)) !== null,
    resume: async () => (await resumeBackfill(store)).resumed,
  };
}

/**
 * Run one backfill. **This is the only runtime entry point**, and the check order
 * is deliberate:
 *   single-flight → storage → switch → host pause (try to wake it first) → the
 *   remaining gates → really run.
 *
 * 🔴 W2 · The "hello first, then decide" step is the resume path §10 requires:
 *    while paused it does not give up outright but asks whether the host is there;
 *    if it answers, the pause is cleared and work continues from the **same debt**
 *    (the debts were never touched from beginning to end).
 */
export async function tickBackfill(deps: TickDeps): Promise<TickResult> {
  if (inFlight) return { ran: false, reason: 'already-running', report: null };
  inFlight = true;
  try {
    const gate = deps.host ?? productionHostGate(deps.store);
    // With the switch off, not even a hello is sent: no consent, no periodic behaviour.
    if (deps.store && await isBackfillEnabled(deps.store) && await gate.paused()) {
      const resumed = await gate.resume();
      if (!resumed) return { ran: false, reason: 'host-paused', report: null };
    }

    // 🔴 The gates go through tickBlockReason — **the same decision the popup makes**.
    const blocked = await tickBlockReason({
      hasStore: deps.store !== null,
      isEnabled: () => isBackfillEnabled(deps.store),
      isHostPaused: () => gate.paused(),
      hasHttp: deps.http !== undefined,
    });
    if (blocked) return { ran: false, reason: blocked, report: null };
    const store = deps.store!;
    const http = deps.http!;

    const report = await runBackfill({
      platform: deps.platform,
      origin: deps.origin,
      scope: deps.scope,
      store,
      http,
      clock: deps.clock,
      pace: deps.pace,
      random: deps.random,
      maxDetails: deps.maxDetails ?? DEFAULT_TICK_DETAILS,
      shouldAbort: deps.shouldAbort,
      sink: deps.sink,
    });
    return { ran: true, reason: 'ran', report };
  } finally {
    inFlight = false;
  }
}
