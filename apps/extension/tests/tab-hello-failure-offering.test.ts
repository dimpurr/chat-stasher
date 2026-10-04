/**
 * W36 · **`installTabHello` offers every failure to `onFailure` before
 * swallowing it — and nothing it does ever disturbs the page.**
 *
 * ## What this file pins
 *
 * W27's own suite (`tests/w27-tab-hello.test.ts`) covers the scheduling
 * half of the module — the jitter band and the visibility re-announce —
 * but not the failure-offering semantics W36 added to the same call:
 *
 *   · a hello that throws **synchronously** and a hello that returns a
 *     **rejected promise** (the shape the runtime API actually has) are
 *     both offered to `onFailure`, because the call runs inside
 *     `Promise.resolve().then(...)`, which turns either shape into the
 *     same rejection;
 *   · a hello that succeeds — including one that returns a plain,
 *     non-promise value — is not a failure, and `onFailure` stays silent;
 *   · **no throw escapes `installTabHello`** in any case: a failed hello
 *     is swallowed before it can reach the page, which is the rule the
 *     load-time hello always followed;
 *   · a failing hello does **not** stop the interval: `scheduleNext`
 *     re-arms **before** `announce`, so a tab keeps re-announcing itself
 *     across any number of failures;
 *   · **every** failure is offered. There is no once-per-page gate in this
 *     module — the gate lives in `lib/page-link.ts`, which decides what
 *     is worth saying; this module only guarantees the failure reaches the
 *     caller rather than being dropped.
 *
 * Synthetic fixtures only: a fake clock, an injected draw source, and a
 * `VisibilityTarget` stub built to the module's own interface. No DOM, no
 * browser, no network.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  TAB_HELLO_MAX_INTERVAL_MS,
  TAB_HELLO_MIN_INTERVAL_MS,
  installTabHello,
  type VisibilityTarget,
} from '../lib/backfill/tab-hello';

beforeEach(() => {
  // Only the timers this file reasons about — the hello interval — are
  // faked. Microtasks are not: a hello settles through
  // `Promise.resolve().then(...)`, and `drain()` below is what lets it.
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'Date'],
  });
});

afterEach(() => {
  vi.useRealTimers();
});

/** Let every pending microtask run, so an awaited chain has really started before the clock moves. */
async function drain(): Promise<void> {
  for (let i = 0; i < 10; i += 1) await Promise.resolve();
}

/**
 * Install, asserting the one promise every path must hold: **no throw
 * escapes `installTabHello`** — a failed hello must never disturb the
 * page, whatever shape the failure arrives in.
 */
function installQuietly(options: Parameters<typeof installTabHello>[0]): void {
  expect(() => installTabHello(options)).not.toThrow();
}

/**
 * A `VisibilityTarget` stub, built to the module's own interface: the
 * listener it registers is kept so a test can fire "the tab's visibility
 * changed" exactly as a tab switch would.
 */
function makeVisibility(initial: string): {
  target: VisibilityTarget;
  setVisibility(state: string): void;
} {
  const listeners: Array<() => void> = [];
  let state = initial;
  const target: VisibilityTarget = {
    get visibilityState(): string {
      return state;
    },
    addEventListener(type: 'visibilitychange', listener: () => void): void {
      if (type === 'visibilitychange') listeners.push(listener);
    },
  };
  return {
    target,
    setVisibility(next: string): void {
      state = next;
      for (const fn of [...listeners]) fn();
    },
  };
}

/** What Chrome throws when a content script's extension context is gone. */
function deadContext(): Error {
  return new Error('Extension context invalidated.');
}

describe('W36 · every hello failure is offered to onFailure, and nothing else happens', () => {
  it('the load-time hello fires at once, without waiting for the first interval', async () => {
    const hello = vi.fn(() => 'ok');
    const onFailure = vi.fn();
    installQuietly({ hello, onFailure, random: () => 0 });

    await drain();
    expect(hello).toHaveBeenCalledTimes(1);
    expect(onFailure).not.toHaveBeenCalled();
    // The repetition is scheduled but has not fired: the first hello was
    // not an interval wait in disguise, and the next wake is the one
    // pending timer.
    expect(vi.getTimerCount()).toBe(1);
  });

  it('a hello that throws synchronously is offered to onFailure', async () => {
    const error = deadContext();
    const hello = vi.fn(() => { throw error; });
    const onFailure = vi.fn();
    installQuietly({ hello, onFailure, random: () => 0 });

    await drain();
    expect(onFailure).toHaveBeenCalledTimes(1);
    expect(onFailure).toHaveBeenCalledWith(error);
  });

  it('a hello that returns a rejected promise is offered to onFailure — the shape the runtime API has', async () => {
    const error = deadContext();
    const hello = vi.fn(() => Promise.reject(error));
    const onFailure = vi.fn();
    installQuietly({ hello, onFailure, random: () => 0 });

    await drain();
    expect(onFailure).toHaveBeenCalledTimes(1);
    expect(onFailure).toHaveBeenCalledWith(error);
  });

  it('a hello that succeeds is not a failure — and a plain value is a success', async () => {
    // A plain, non-promise return value: accepted, and not a rejection.
    const plain = vi.fn(() => 42);
    const onFailure = vi.fn();
    installQuietly({ hello: plain, onFailure, random: () => 0 });
    await drain();
    expect(plain).toHaveBeenCalledTimes(1);
    expect(onFailure).not.toHaveBeenCalled();

    // A resolved promise is the other half of "not a failure".
    const resolved = vi.fn(async () => 'resolved');
    installQuietly({ hello: resolved, onFailure, random: () => 0 });
    await drain();
    expect(resolved).toHaveBeenCalledTimes(1);
    expect(onFailure).not.toHaveBeenCalled();
  });

  it('a failing hello does not stop the interval: the next hello still fires', async () => {
    const error = deadContext();
    const hello = vi.fn(() => { throw error; });
    const onFailure = vi.fn();
    installQuietly({ hello, onFailure, random: () => 0 });
    await drain();
    expect(hello).toHaveBeenCalledTimes(1);
    expect(onFailure).toHaveBeenCalledTimes(1);

    // One millisecond short of the drawn interval, nothing has fired
    // again — the interval is a real wait, not a busy loop.
    await vi.advanceTimersByTimeAsync(TAB_HELLO_MIN_INTERVAL_MS - 1);
    await drain();
    expect(hello).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(1);
    await drain();
    expect(hello).toHaveBeenCalledTimes(2);
    expect(onFailure).toHaveBeenCalledTimes(2);
    expect(onFailure).toHaveBeenNthCalledWith(2, error);
    // 🔴 Re-armed **before** the hello went out: the wake for the round
    //    after next is already pending, on every path through the
    //    callback — a tab keeps re-announcing itself across failures.
    expect(vi.getTimerCount()).toBe(1);
  });

  it('a failing hello at the ceiling of the band repeats the same way', async () => {
    const error = deadContext();
    const hello = vi.fn(() => Promise.reject(error));
    const onFailure = vi.fn();
    installQuietly({ hello, onFailure, random: () => 1 });
    await drain();

    await vi.advanceTimersByTimeAsync(TAB_HELLO_MAX_INTERVAL_MS);
    await drain();
    expect(hello).toHaveBeenCalledTimes(2);
    expect(onFailure).toHaveBeenCalledTimes(2);
    expect(vi.getTimerCount()).toBe(1);
  });

  it('becoming visible announces at once; going hidden announces nothing', async () => {
    const hello = vi.fn(() => 'ok');
    const onFailure = vi.fn();
    const visibility = makeVisibility('visible');
    installQuietly({ hello, onFailure, random: () => 0, visibility: visibility.target });
    await drain();
    expect(hello).toHaveBeenCalledTimes(1); // the load-time hello

    visibility.setVisibility('hidden');
    await drain();
    expect(hello, 'hiding is not news this registry has any use for').toHaveBeenCalledTimes(1);

    visibility.setVisibility('visible');
    await drain();
    expect(hello, 'a tab the user came back to checks in right now').toHaveBeenCalledTimes(2);
    expect(onFailure).not.toHaveBeenCalled();
  });

  it('every failure is offered — there is no once-per-page gate in this module', async () => {
    // The gate that says a stale link once lives in lib/page-link.ts; this
    // module's contract is the opposite: hand the caller every failure and
    // let it decide. So the count matches the number of failures, exactly.
    let failures = 0;
    const hello = vi.fn(() => Promise.reject(new Error(`failure ${(failures += 1)}`)));
    const onFailure = vi.fn();
    installQuietly({ hello, onFailure, random: () => 0 });
    await drain();

    // The load-time failure, then three interval failures.
    for (let round = 0; round < 3; round += 1) {
      await vi.advanceTimersByTimeAsync(TAB_HELLO_MIN_INTERVAL_MS);
      await drain();
    }
    expect(hello).toHaveBeenCalledTimes(4);
    expect(onFailure).toHaveBeenCalledTimes(4);
    for (let i = 0; i < 4; i += 1) {
      expect((onFailure.mock.calls[i]![0] as Error).message).toBe(`failure ${i + 1}`);
    }
  });
});
