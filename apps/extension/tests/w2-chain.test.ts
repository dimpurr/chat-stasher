/**
 * W2 · The end-to-end synthetic chain: page hook → postMessage → bridge → background → outbox → local host.
 *
 * This replaces chain.test.ts. The old version's whole chain ended at `chrome.downloads`
 * (the real filesystem), and that channel, along with the `downloads` permission, has been
 * deleted; what is unchanged is the **shape** of the case: load the real entrypoint
 * source, really run it, and swap only the browser APIs and the host for stubs.
 *
 * 🔴 Every criterion corresponds one-for-one with the old version; only the landing point
 *    moved from "is that file on disk" to "which payload did the host receive and ack" —
 *    the latter is what counts under spec §1.
 *
 * Zero real network, zero logged-in state, zero real files throughout.
 *
 * 🔴 W616 · **No case below waits on a clock.** Every wait is on the state the
 *    assertions next read — the host's deliveries, or the fake runtime's own
 *    in-flight count — so a case ends the moment its work is done and cannot pass
 *    by outlasting a sleep. Neither of the two fixed sleeps this replaces was a
 *    bound the chain honoured: the delivery from the first case was not in the
 *    host yet when its 100 ms ran, and it then landed *during* the second case,
 *    where it read as a stray delivery against "delivers nothing at all". Both
 *    were load-dependent, and measured 3 failures in 6 runs under half-core load
 *    before this change against 0 in 6 after.
 */

import { describe, it, expect, vi, beforeAll, beforeEach } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { HOOK_STATUS_MESSAGE, type CapturedFetch } from '../lib/contract';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

const runtimeListeners: Array<{
  fn: (msg: any, sender: any, sendResponse: (r: any) => void) => any;
}> = [];
const localValues: Record<string, unknown> = {};

/**
 * 🔴 What the fake runtime is still busy with, counted where the fake can see it.
 *
 *    `runtime` is a message whose listener has not answered yet, `native` a
 *    `sendNativeMessage` the host has not returned from. Both are zero exactly
 *    when the chain has nothing left to do, which is the condition every case
 *    below actually wants — and, unlike a sleep, it is a property of the work
 *    rather than of the machine's load.
 */
let messagesInFlight = 0;
let nativeInFlight = 0;
/** How many `HOOK_STATUS_MESSAGE` reports the page has made. See `awaitPageStartupRound`. */
let hookStatusReports = 0;

/**
 * 🔴 How long a drain waits before it calls the work unfinished.
 *
 *    It is a *bound*, not an expectation: on an idle machine the chain settles in
 *    single-digit milliseconds, so this is slack for a loaded CI runner rather
 *    than a duration the case depends on. A case that cannot reach quiescence
 *    inside it fails here, named, instead of quietly racing on.
 */
const DRAIN_TIMEOUT_MS = 4_000;

/**
 * 🔴 W876 · **Machine-load headroom for the whole file.**
 *
 *    Each wait below is bounded by `DRAIN_TIMEOUT_MS`, but one case can take
 *    several of them in a row — the capture case drains the startup round, then
 *    the delivery, then the round trip — and it is their sum, not any single
 *    bound, that a loaded runner pays. Vitest's 5 s default then measures the
 *    machine instead of the chain and kills the case with its own generic
 *    message before a drain can name what it was waiting for. This only stops a
 *    busy runner from deciding the verdict: the waits, the assertions and what
 *    each case exercises are unchanged, and a chain that cannot reach quiescence
 *    still fails — named, at `DRAIN_TIMEOUT_MS`, well inside this budget. Same
 *    arrangement as tests/w86b-least-recently-served.test.ts (W88/W223).
 */
const LOAD_TIMEOUT_MS = 30_000;

let host: SyntheticHost;

/** Each test simulates a fresh extension: clear all registered listeners/records. */
function resetMocks() {
  runtimeListeners.length = 0;
  for (const key of Object.keys(localValues)) delete localValues[key];
  // A counter left over from a previous case would make every later drain wait
  // for work that no longer exists, so it is cleared with the rest of the state.
  messagesInFlight = 0;
  nativeInFlight = 0;
  hookStatusReports = 0;
  vi.unstubAllGlobals();
}

/**
 * The browser this chain runs against. Installed for the whole file, because
 * `@wxt-dev/browser` reads `globalThis.browser` or `globalThis.chrome` **once,
 * when it is first imported** and `@wxt-dev/i18n` then reads through that
 * captured object — so which of the two fakes is in place at import time is part
 * of what these cases exercise, and it must not depend on which case happens to
 * be the first one to import anything.
 */
function installFakes(): void {
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineContentScript', (cfg: any) => cfg);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
}

/**
 * 🔴 W616 · **Load the real entrypoints once, before any case's clock starts.**
 *
 *    Each case below imports them itself, because each one has to *run* a fresh
 *    service worker and a fresh pair of content scripts. The import was where the
 *    transform cost of `entrypoints/background.ts` and its dependency graph went —
 *    inside the first case's 5 s vitest budget, where a loaded runner spent most
 *    of it and the case died as `Test timed out in 5000ms` before reaching a
 *    single assertion. The modules are the same modules and run the same code;
 *    only the one-time transform moves out of the per-case clock.
 *
 *    🔴 It does not move what is exercised. Nothing is stubbed or replaced here:
 *    `loadBackground()`, `loadBridge()` and `loadMainHook()` still import the same
 *    paths and still call the exported `main()`, and those calls are what
 *    register a worker and install a hook per case.
 */
beforeAll(async () => {
  installFakes();
  await import('../entrypoints/background');
  await import('../entrypoints/dw-bridge.content');
  await import('../entrypoints/dw-fetch-main.content');
});

beforeEach(() => {
  resetMocks();
  host = createSyntheticHost({ up: true });
  installFakes();
});

/**
 * 🔴 W616 · **Wait for the chain to have nothing left in flight.**
 *
 *    Replaces every fixed sleep in this file. Because `handleCaptured` is only
 *    released to its caller through `sendResponse`, "no runtime message in
 *    flight" is reached *after* the outbox write, the drain, the matching ack and
 *    the worker's own `getEntry` read — so the outbox assertion below reads a
 *    settled outbox rather than one caught mid-transaction.
 */
async function drainChain(): Promise<void> {
  await vi.waitFor(
    () => {
      expect(messagesInFlight, 'runtime messages still in flight').toBe(0);
      expect(nativeInFlight, 'native-host requests still in flight').toBe(0);
    },
    { interval: 1, timeout: DRAIN_TIMEOUT_MS },
  );
}

/**
 * 🔴 W616 · **The capture case's own first step: wait for the hook to speak.**
 *
 *    `drainChain()` can only see a message once one exists, and the page hook
 *    posts its capture from a fire-and-forget tail: `hookedFetch` returns the
 *    response to the page *before* `maybeCapture` has read the cloned body
 *    (lib/page-hook.ts). So the awaited `fetch` above returns with nothing in
 *    flight at all, and a drain taken there would finish immediately and assert
 *    against a chain that has not started. Waiting for the payload to reach the
 *    host closes that gap on the fact itself, and it is the same fact the
 *    assertion below reads — the delivery count is the payload's arrival, not a
 *    guess about how long the path takes.
 */
async function awaitDeliveredPayload(count: number): Promise<void> {
  await vi.waitFor(() => expect(host.deliveries).toHaveLength(count), {
    interval: 1,
    timeout: DRAIN_TIMEOUT_MS,
  });
}

/**
 * 🔴 W616 · **Let the page finish booting, on the message it always sends.**
 *
 *    `loadBridge()` leaves one piece of its own work pending: the one-shot
 *    verification armed by `MAIN_FALLBACK_TIMEOUT_MS`, which either finds the page
 *    hook already installed (the probe came back, `reportHookStatus(null)`) or
 *    finds it missing (`noteFallbackUnverified` ⇒ `HOOK_REASON_DID_NOT_RUN`). Both
 *    outcomes end in exactly one `HOOK_STATUS_MESSAGE`, so waiting for one waits
 *    for the round itself rather than for the 100 ms that happens to bound it — and
 *    the case would still pass if that constant moved.
 *
 *    🔴 It is not cosmetic. `resetMocks()` takes `window` and `browser` away for
 *    the next case, and that timer reads both; a case that ended before it fired
 *    left a callback behind that threw `window is not defined` out of a finished
 *    test, which vitest reports as an uncaught exception in the file. Draining the
 *    round here is what keeps the worker clean, and it is the same thing the old
 *    fixed sleeps were buying by accident.
 */
async function awaitPageStartupRound(): Promise<void> {
  await vi.waitFor(() => expect(hookStatusReports).toBeGreaterThan(0), {
    interval: 1,
    timeout: DRAIN_TIMEOUT_MS,
  });
  await drainChain();
}

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() { /* startup badge refresh is a no-op here */ } },
    onMessage: {
      addListener(fn: any) {
        runtimeListeners.push({ fn });
      },
    },
    sendNativeMessage: (h: string, m: unknown) => {
      nativeInFlight += 1;
      return host.sendNativeMessage(h, m).finally(() => { nativeInFlight -= 1; });
    },
    async sendMessage(msg: any): Promise<any> {
      messagesInFlight += 1;
      if (msg?.type === HOOK_STATUS_MESSAGE) hookStatusReports += 1;
      return new Promise((resolve, reject) => {
        let settled = false;
        /**
         * 🔴 W876 · The last-resort bound below is owed only while the message is
         *    unanswered, so its handle is kept here and dropped the moment any
         *    listener settles the message. A passing test therefore leaves no
         *    fixed timer behind, and the bound can never fire against work that
         *    already finished.
         */
        let fallback: ReturnType<typeof setTimeout> | undefined;
        const doSettle = (fn: () => void) => {
          if (settled) return;
          settled = true;
          messagesInFlight -= 1;
          if (fallback !== undefined) { clearTimeout(fallback); fallback = undefined; }
          fn();
        };
        const doResolve = (v: any) => doSettle(() => resolve(v));
        const doReject = (err: unknown) => doSettle(() => reject(err));
        /**
         * 🔴 W616 · **Only a listener that owns the message may settle it.**
         *
         * A browser calls every listener and the *first* `sendResponse` wins; a
         * listener that returns neither `true` nor a promise is simply not
         * answering and settles nothing. That distinction is not academic here:
         * `loadBridge()` registers the bridge's own `onMessage` listener on this
         * fake runtime (`isTopFrame()` is true under node's stub window), and for
         * a `chat-captured` message it returns `undefined` — so the old reading of
         * this mock resolved every message with `undefined` the moment the bridge
         * declined it, and the capture's real answer from `handleCaptured` was
         * thrown away. A drain built on that could never see the delivery.
         */
        let channelKept = false;
        for (const { fn } of runtimeListeners) {
          try {
            const ret = fn(msg, { id: 'mock-sender', url: null }, doResolve);
            if (ret && typeof ret.then === 'function') {
              channelKept = true;
              ret.then(doResolve).catch(doReject);
            } else if (ret === true) {
              channelKept = true;
            }
            // ret === true => async, waits for sendResponse() (called above)
          } catch (err) {
            doReject(err);
            return;
          }
        }
        // Nobody kept the channel open, so nothing will ever answer — which is
        // the answer, and the reason no listener had to call sendResponse.
        if (!channelKept) doResolve(undefined);
        /**
         * 🔴 W616 · **A bound, and the one shape it still has to cover.**
         *
         * Everything above settles a message the way a browser would. What is left
         * is the defect this cannot settle honestly: a listener that returned
         * `true` and then never called `sendResponse`, which a real browser
         * reports as a closed channel. It **rejects** rather than resolving
         * `undefined`, so it can never be mistaken for an answer and let a case
         * pass on the chain having gone quiet.
         *
         * 🔴 W876 · **It cannot fire in a passing test, by construction.** It is
         *    armed only while the message is still unanswered (`if (!settled)`),
         *    it is cleared the instant a listener answers (`doSettle`), and it is
         *    one beat longer than `DRAIN_TIMEOUT_MS` — so a chain that cannot
         *    reach quiescence fails first, named, inside the drain, and this bound
         *    is left to catch only the listener that never answers at all, keeping
         *    a defective one from hanging the worker forever.
         */
        if (!settled) {
          fallback = setTimeout(() => doReject(new Error(
            `no listener answered the runtime message within ${DRAIN_TIMEOUT_MS + 1_000}ms: ${String(msg?.type)}`,
          )), DRAIN_TIMEOUT_MS + 1_000);
          fallback.unref?.();
        }
      });
    },
  },
  action: {
    async setBadgeText() {},
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
  storage: {
    local: {
      async get(query: Record<string, unknown> | null) {
        if (query === null) return { ...localValues };
        return Object.fromEntries(Object.entries(query).map(([key, fallback]) => [key, key in localValues ? localValues[key] : fallback]));
      },
      async set(values: Record<string, unknown>) { Object.assign(localValues, values); },
      async remove(keys: string[]) { for (const key of keys) delete localValues[key]; },
    },
  },
};

async function loadBackground() {
  const mod = await import('../entrypoints/background');
  const bg = (mod as any).default;
  await bg(); // simulate service worker startup
}

async function loadBridge() {
  const mod: any = await import('../entrypoints/dw-bridge.content');
  mod.default.main();
}

async function loadMainHook() {
  const mod: any = await import('../entrypoints/dw-fetch-main.content');
  mod.default.main();
  return mod.default;
}

function makeFakeWindow() {
  const listeners: Record<string, Array<(e: any) => void>> = {};
  return {
    listeners,
    location: { origin: 'https://chat.deepseek.com' },
    addEventListener(name: string, fn: (e: any) => void) {
      (listeners[name] ??= []).push(fn);
    },
    dispatchEvent(e: any) {
      for (const fn of listeners[e.type] ?? []) fn(e);
      return true;
    },
    postMessage(data: unknown, targetOrigin: string) {
      if (targetOrigin !== this.location.origin) return;
      for (const fn of listeners.message ?? []) fn({ source: this, origin: this.location.origin, data });
    },
    fetch: null as any,
  };
}

describe('W2 · synthetic chain: page → bridge → background → outbox → host', { timeout: LOAD_TIMEOUT_MS }, () => {
  it('one real capture ends with a matching ack, and the payload is byte-for-byte a conforming inbox bundle', async () => {
    await loadBackground();

    const fakeWin = makeFakeWindow();
    vi.stubGlobal('window', fakeWin);
    await loadBridge();

    const rawBody = JSON.stringify({
      session_id: 'c622b5dd-0000-4000-8000-00000000abcd',
      message: { content: 'synthetic answer, never printed in reports', reasoning_content: '' },
    });
    fakeWin.fetch = async () =>
      new Response(rawBody, { status: 200, headers: { 'content-type': 'application/json' } });
    await loadMainHook();

    const fakeUrl = 'https://chat.deepseek.com/api/v0/chat/session/c622b5dd-0000-4000-8000-00000000abcd';
    await (fakeWin.fetch as any)(fakeUrl);

    // 🔴 W616 · Wait for the three facts the assertions below read, in that order.
    //    The page's own startup round has to be over (no callback left running),
    //    the delivery has to have reached the host (`awaitDeliveredPayload`), and
    //    the capture's own round trip has to have finished (`drainChain`) — the
    //    outbox assertion at the end of this case reads a settled outbox, not one
    //    caught between the write-ahead `enqueue` and the matching ack.
    //
    //    Measured per case, this is where the dead time went: 551–1137 ms here
    //    before (almost all of it the one-time module transform, charged to this
    //    case) and 107–115 ms after.
    await awaitPageStartupRound();
    await awaitDeliveredPayload(1);
    await drainChain();

    expect(host.deliveries).toHaveLength(1);
    const delivery = host.deliveries[0]!;
    console.log('[W2-CHAIN] name the host received:', delivery.name, '· payload bytes:', delivery.payload.length);

    // §6.2: the name must conform to the spec.
    expect(delivery.name).toBe('deepseek-c622b5dd-0000-4000-8000-00000000abcd.json');
    // §6.2: sha256 is the SHA-256 of the payload's UTF-8 bytes — the host computed it itself and checked.
    expect(delivery.sha256).toMatch(/^[0-9a-f]{64}$/);

    const doc = JSON.parse(delivery.payload);
    expect(doc.schema).toBe('chat-stasher/inbox@2');
    expect(doc.platform).toBe('deepseek');
    expect(doc.sessionId).toBe('c622b5dd-0000-4000-8000-00000000abcd');
    // C3: this synthetic body carries no account fields, so the ADR-002 chain
    // falls through to an explicit 'default' marker — never silently missing.
    expect(doc.identity).toEqual({ level: 'default', value: '' });
    // raw.bytes = size of the ORIGINAL response.
    expect(doc.raw.bytes).toBe(Buffer.byteLength(rawBody, 'utf8'));

    // After the ack the outbox is empty: no entries are left behind as "sent but still here".
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
  });

  it('non-conversation traffic (another path / another origin / anything but GET) delivers nothing at all', async () => {
    await loadBackground();

    const fakeWin = makeFakeWindow();
    vi.stubGlobal('window', fakeWin);
    await loadBridge();

    fakeWin.fetch = async () => new Response('{}', { status: 200 });
    await loadMainHook();

    const nonSessionUrls = [
      'https://chat.deepseek.com/api/v0/users/me',
      'https://chat.deepseek.com/api/v0/payments/session/sess-x',
      'https://example.com/api/v0/chat/anything', // not DeepSeek origin
    ];
    for (const u of nonSessionUrls) await (fakeWin.fetch as any)(u);
    // 🔴 W616 · Wait for the channel to go quiet rather than for a duration.
    //
    //    The traffic under test needs no wait of its own: the hook's capture
    //    decision for a request that is not a platform candidate is taken inside
    //    `maybeCapture` before its first `await` (lib/page-hook.ts returns as soon
    //    as the origin/method/path check fails), so awaiting the three fetches has
    //    already run every decision this traffic can start, and none of them posts.
    //    What is drained is the rest of the page's own startup round — the tab
    //    hello, and the bridge's one-shot fallback verification
    //    (`MAIN_FALLBACK_TIMEOUT_MS`) — which `loadBridge` leaves pending. Neither
    //    can deliver: the first asks the background to remember a tab, and the
    //    second reports `HOOK_REASON_DID_NOT_RUN`, whose handler writes a
    //    hook-status record and returns before the `chat-captured` branch is
    //    reached.
    await awaitPageStartupRound();

    expect(host.deliveries).toEqual([]);
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    console.log('[W2-CHAIN-EVIDENCE] deliveries produced by non-conversation traffic:', host.deliveries.length);
  });

  it('with the host absent, the same capture stays in the outbox (write-ahead, end to end)', async () => {
    host = createSyntheticHost({ up: false });
    await loadBackground();

    const payload: CapturedFetch = {
      url: 'https://chat.deepseek.com/api/v0/chat/session/aaaa1111-bbbb-4000-8000-00000000ffff',
      method: 'POST',
      status: 200,
      text: JSON.stringify({ session_id: 'aaaa1111-bbbb-4000-8000-00000000ffff', message: { content: 'x' } }),
      capturedAt: Date.now(),
    };
    const { handleCaptured } = await import('../entrypoints/background');
    const result = await handleCaptured(payload);

    expect(result).toMatchObject({ saved: false, status: 'queued' });
    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    const doc = JSON.parse(entries[0]!.payload);
    expect(doc.sessionId).toBe('aaaa1111-bbbb-4000-8000-00000000ffff');
    // The name is already settled in the outbox — the host will store it as a shard under that name when it returns.
    expect(entries[0]!.name).toBe('deepseek-aaaa1111-bbbb-4000-8000-00000000ffff.json');
  });
});
