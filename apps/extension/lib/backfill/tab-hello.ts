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
 * The bound that matters is the tick's: `BACKFILL_TICK_DELAY_MIN_MINUTES` is 5, so
 * the shortest gap between two backfill ticks is 300 s. With the floor at **4
 * minutes** a tab has normally re-announced itself inside the gap before the tick
 * that would look for it — the registry is healed **before** it is read, not one
 * tick later. The ceiling is **6 minutes**, so a run of unlucky draws still
 * re-announces at least once per tick on average, and the band is 2× wide like the
 * tick's own jitter: two consecutive hellos differ by up to a factor of 1.5, so the
 * page is not a metronome either. Both ends are far below any rate that could
 * matter for a single in-process message.
 *
 * The draw uses `uniformBetween`, the leg's single source of randomness (see
 * lib/backfill/random.ts): it is clamped before scaling, so **no draw can ever land
 * below the documented 4-minute floor**, whatever the source of randomness does.
 */

import { systemRandom, uniformBetween, type RandomFn } from './random';

/** The floor of the hello interval: shorter than the shortest gap between two backfill ticks. */
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
      .catch(() => {
        /* background asleep / nobody listening: do not disturb the page */
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
