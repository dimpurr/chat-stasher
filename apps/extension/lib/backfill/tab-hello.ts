/**
 * W27 · **The tab re-announces itself, so a registry that lost it can find it
 * again.**
 *
 * ## The defect this file exists for
 * `lib/backfill/tab-port.ts` keeps the registry of tabs the backfill leg may fetch
 * through, and the content script wrote it exactly once — on page load. Two
 * ordinary events therefore emptied it of a page that was still open and still
 * perfectly able to answer:
 *  · reloading the extension tears every content script down, and the hello that
 *    had registered them is a thing of the past (it is not re-sent — the page is
 *    not reloaded);
 *  · a service worker restart can leave the registry holding entries for tabs that
 *    no longer exist.
 * Observed in a real browser: from then on the tick reported `no-http-port` until
 * the user happened to reload that tab.
 *
 * ## What this does about it
 * One cheap message, repeated: the same `cs-backfill-tab-hello` the page load
 * already sends, on a jittered interval and whenever the tab becomes visible.
 *
 * 🔴 **Idempotent by construction, not by care here.** Background's handler is
 *    `rememberTab` (entrypoints/background.ts), which dedups by `tabId` and moves
 *    the entry to the front — a repeat hello from a known tab refreshes when it was
 *    last seen and adds nothing. So this file may send as often as it likes without
 *    growing the registry (`MAX_TAB_ENTRIES`), and it does not need to know whether
 *    it is already registered.
 *    One consequence is worth stating rather than leaving to be discovered: because
 *    a hello moves its tab to the front, the registry's order now leans toward
 *    "whichever tabs announced most recently" instead of "whichever tab loaded most
 *    recently". That is the helpful direction — a hidden tab's timer is throttled,
 *    so tabs the user is actually working in drift to the front, which is also
 *    where the first candidate for the next fetch comes from.
 *
 * ## 🔴 Why this is not an alarm, and why that is the point
 * Nothing here asks the browser to wake the tab up: there is no `alarms` use and
 * no new permission (`alarms` is not even reachable from a content script). A plain
 * `setTimeout` in a page is **throttled by the browser when the tab is hidden** —
 * heavily, up to once an hour under intensive throttling — and that is accepted
 * rather than fought:
 *  · a hidden tab is still *live* for this registry's purpose. The hello is not
 *    what makes a tab answer a fetch; the registry entry is, and it stays. Message
 *    handling is not throttled, so a hidden, backed-off tab serves the backfill
 *    exactly as before;
 *  · what throttling delays is only *re-registration after a loss*, and the fix for
 *    that is the `visibilitychange` path below — a user coming back to the tab is
 *    the moment the page becomes interesting again, and it is free to notice.
 * ⇒ the interval is a **floor on how often we try, never a promise about when the
 *   hello lands**. A tab that is never looked at again may re-register late; it is
 *   never kept awake for our benefit.
 *
 * ## 🔴 Why 4–6 minutes
 * 🔴 **W310 · This band is deliberately NOT scaled with the tick.** The bound
 * that motivated it was the tick's: with `BACKFILL_TICK_DELAY_MIN_MINUTES = 5`
 * the shortest gap between two backfill ticks was 300 s, so a 4-minute floor had
 * a tab re-announce itself inside the gap *before* the tick that would look for
 * it — the registry was healed before it was read. W310 lowered the tick floor to
 * 1 minute (see alarm.ts), so that particular inequality no longer holds: on a
 * fast day a lost registration can now be read by a couple of ticks before the
 * page checks in again. That is accepted rather than chased, for two reasons:
 *  · the ticks it costs are `no-http-port` skips, and a skip **consumes no
 *    rotation slot** — the walk keeps going past the missing platform, so a slow
 *    heal delays nothing but that platform's own turn (alarm.ts's W86c bound
 *    still holds for every row that *is* registered);
 *  · the hello is a message that *wakes the MV3 service worker*, and shrinking
 *    the band to stay inside a 60 s gap would multiply those wake-ups with no
 *    request saved. With a one-minute tick already waking the worker, the cheap
 *    side of the trade is to let a heal span a few ticks.
 * The band itself is unchanged and still earns its shape: the floor is a bound
 * on how often a page talks to the worker, the ceiling means a run of unlucky
 * draws still re-announces at least once per tick on average, and the 1.5× width
 * keeps two consecutive hellos from looking like a metronome. Both ends are far
 * below any rate that could matter for a single in-process message.
 *
 * The draw uses `uniformBetween`, the leg's single source of randomness (see
 * lib/backfill/random.ts): it is clamped before scaling, so **no draw can ever land
 * below the documented 4-minute floor**, whatever the source of randomness does.
 */

import { systemRandom, uniformBetween, type RandomFn } from './random';

/**
 * The floor of the hello interval: a bound on how often a page talks to the
 * worker, not an inequality against the tick gap (W310 — see the note above).
 */
export const TAB_HELLO_MIN_INTERVAL_MS = 4 * 60_000;
/** The ceiling of the hello interval: the tab re-announces at least once per tick on average. */
export const TAB_HELLO_MAX_INTERVAL_MS = 6 * 60_000;

/** Draw the wait before the next hello, in milliseconds. Never below the floor, never above the ceiling. */
export function drawHelloDelayMs(random: RandomFn = systemRandom): number {
  return uniformBetween(random, TAB_HELLO_MIN_INTERVAL_MS, TAB_HELLO_MAX_INTERVAL_MS);
}

/**
 * The slice of `document` this needs. Narrowed to two members so a test can drive
 * "the tab became visible" without a DOM, and so this module cannot quietly grow a
 * dependency on the rest of it.
 */
export interface VisibilityTarget {
  readonly visibilityState: string;
  addEventListener(type: 'visibilitychange', listener: () => void): void;
}

export interface TabHelloOptions {
  /**
   * Send one hello. Its rejection is swallowed here on purpose: the background
   * worker may simply be asleep, which is the normal case, and a failed hello must
   * never disturb the page (the same rule the load-time hello always followed).
   */
  hello: () => unknown;
  /** The draw source. Production passes nothing and gets `Math.random`. */
  random?: RandomFn;
  /** `document`, or null where there is none (a test running under node). */
  visibility?: VisibilityTarget | null;
  /**
   * 🔴 W36 · Called with every failure, **before** it is swallowed.
   *
   * Swallowing stays the default: "the worker was asleep" is the ordinary case
   * and the caller is right to say nothing about it. But one failure is not
   * ordinary — `Extension context invalidated` means this document's scripts
   * belong to an extension that is gone, and no later hello will ever succeed.
   * That is the state a page left open across an extension reload is in, and it
   * is the state a real machine reported as `no-http-port` for days with the
   * platform page sitting open on screen. The caller decides what to say; this
   * module only guarantees the failure is *offered* to it rather than dropped.
   * Omitted ⇒ the pre-W36 behaviour, byte for byte.
   */
  onFailure?: (error: unknown) => void;
}

/**
 * Announce this tab now, again on a jittered interval, and again whenever the tab
 * becomes visible.
 *
 * Called once per page load by `entrypoints/dw-bridge.content.ts`; the immediate
 * hello it sends is the same one that file used to send by hand, so a page that is
 * never re-visited behaves exactly as before.
 *
 * 🔴 Becoming visible sends an **extra** hello and does **not** reschedule the
 *    interval: the tab that has been in the background for an hour has missed
 *    many draws, and the honest thing is to check in right now rather than to
 *    restart a clock. It cannot flood: `visibilitychange` fires on an actual state
 *    change, and each one costs one message.
 */
export function installTabHello(options: TabHelloOptions): void {
  const random = options.random ?? systemRandom;
  const visibility = options.visibility ?? null;

  const announce = (): void => {
    // `Promise.resolve().then(...)` rather than a bare try/catch: a hello that
    // returns a rejected promise must be caught too, and that is the shape the
    // runtime API has.
    void Promise.resolve()
      .then(() => options.hello())
      .catch((err: unknown) => {
        // The page is not disturbed either way; the caller is offered the failure
        // so that a *dead* link can be told apart from a sleeping worker (W36,
        // see onFailure). Not offering it is the pre-W36 behaviour.
        options.onFailure?.(err);
      });
  };

  const scheduleNext = (): void => {
    setTimeout(() => {
      // 🔴 Re-armed **before** the hello goes out: the next wake is scheduled on
      //    every path through this callback. `announce` cannot throw (it swallows
      //    its own failures), and the chain then survives even an edit that makes
      //    it throw — the tab stops re-announcing itself only if the page goes
      //    away, which is the one case where there is nothing left to announce.
      scheduleNext();
      announce();
    }, drawHelloDelayMs(random));
  };

  // The load-time hello, then the repetitions.
  announce();
  scheduleNext();

  if (visibility) {
    const target = visibility;
    target.addEventListener('visibilitychange', () => {
      // Only on becoming visible. Hiding is not an event this registry has any use
      // for — the entry does not change, and the tab is still there.
      if (target.visibilityState === 'visible') announce();
    });
  }
}
