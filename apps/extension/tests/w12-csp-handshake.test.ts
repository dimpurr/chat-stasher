/**
 * W12 · The MAIN-world readiness handshake, without the inline script that
 * chat.deepseek.com's Content Security Policy refuses to execute.
 *
 * The old handshake proved "the MAIN-world hook is installed" by injecting an
 * inline `<script>` that read `window.fetch[PAGE_HOOK_FETCH_MARKER]` and posted
 * the answer back. On DeepSeek every page load produced
 * `Executing inline script violates the following Content Security Policy
 * directive 'script-src …'` in `chrome://extensions` — and it proved nothing the
 * tokenized probe had not already proved, because the hook's probe listener is
 * registered by `installPageFetchHook` itself.
 *
 * So these cases are about what "the probe was answered" is allowed to mean, and
 * about the CSP failure that has to stay visible now that the verifier is gone:
 *
 *  (a) MAIN answers ⇒ **nothing** is injected.
 *  (b) MAIN is silent ⇒ the fallback still runs, and if it does not answer either
 *      the fixed warning is emitted — exactly once, no matter how long we watch.
 *  (c) the property that makes (a) safe: an answered probe implies an installed
 *      hook, so a hook that fell over before patching `fetch` must stay silent.
 *  (d) the fallback that *is* blocked (append succeeds, execution does not) is the
 *      case that must warn rather than pass for working.
 */

import { afterEach, describe, expect, it, vi, beforeEach } from 'vitest';
import { withI18n } from './i18n-harness';
import {
  MAIN_PROBE_MESSAGE,
  MAIN_READY_MESSAGE,
  PAGE_HOOK_FETCH_MARKER,
  PAGE_HOOK_VERSION,
} from '../lib/contract';
import { FALLBACK_HOOK_VERIFICATION_WARNING } from '../lib/fallback-verification';

const ORIGIN = 'https://chat.deepseek.com';

type Listener = (event: { source: unknown; origin: string; data: unknown }) => void;

interface FakePage {
  win: any;
  /** Everything the page world posted, in order. */
  posted: any[];
  /** The `textContent` of every inline <script> the bridge created and appended. */
  injectedSources: string[];
  /** How many scripts were created, whatever became of them. */
  createdScripts: number;
  /** Model the page's CSP: false ⇒ the script is appended but never executes. */
  allowInlineScripts: boolean;
  /** Deliver every message the page world has posted so far, and any it posts in response. */
  pump(): void;
}

/**
 * One fake page world, faithful in the two places the handshake depends on:
 *
 *  · `postMessage` is delivered **as a task**, and this fake keeps its own task
 *    queue instead of draining it on every microtask. That is what a browser
 *    does: every `document_start` content script — isolated and MAIN, whichever
 *    order they were injected in — runs to completion, and only then are the
 *    messages that were posted during that phase dispatched. A fake that
 *    delivered synchronously would answer a probe from a bridge that ran before
 *    the listener existed, and a fake that drained on every microtask would let
 *    the `await import()` in the harness itself reorder the two worlds. Either
 *    one would make the ordering this handshake rests on untestable.
 *  · an appended inline <script> **executes synchronously** (unless the test
 *    models a CSP block), because that is what `injectPageScript` is for.
 */
function makeFakePage(hook: {
  installPageFetchHook: (options: unknown) => void;
  PAGE_HOOK_OPTIONS: unknown;
}): FakePage {
  const listeners: Listener[] = [];
  const tasks: Array<() => void> = [];
  const page: FakePage = {
    win: null,
    posted: [],
    injectedSources: [],
    createdScripts: 0,
    allowInlineScripts: false,
    pump() {
      // FIFO, and a task may queue another: the handshake is two round trips.
      while (tasks.length > 0) {
        const task = tasks.shift()!;
        task();
      }
    },
  };

  const win: any = {
    location: { origin: ORIGIN, href: `${ORIGIN}/` },
    // The hook reads `window.fetch[marker]` before doing anything else, so it has
    // to be a function even when nothing has been patched yet.
    fetch: async () => new Response('{}', { status: 200 }),
    addEventListener(name: string, fn: Listener) {
      if (name === 'message') listeners.push(fn);
    },
    postMessage(data: unknown, targetOrigin: string) {
      if (targetOrigin !== win.location.origin) return;
      page.posted.push(data);
      tasks.push(() => {
        for (const fn of [...listeners]) {
          fn({ source: win, origin: win.location.origin, data });
        }
      });
    },
  };
  page.win = win;

  vi.stubGlobal('document', {
    documentElement: {
      appendChild(script: { textContent: string }) {
        page.injectedSources.push(script.textContent);
        if (!page.allowInlineScripts) return;
        // The only inline payload the bridge builds is the fallback hook, i.e. a
        // serialised `installPageFetchHook` call. Running the module's own copy
        // of that function is what the browser's script engine would have run,
        // and it is deliberately not `eval`.
        if (!script.textContent.includes('installPageFetchHook')) return;
        hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);
      },
    },
    createElement(tag: string) {
      page.createdScripts += 1;
      expect(tag).toBe('script');
      return { textContent: '', remove() { /* detached again by the bridge */ } };
    },
    // 🔴 W27 · Every real page's `document` has these two, and the bridge now
    //    listens for the tab becoming visible (lib/backfill/tab-hello.ts). This
    //    fake is a page, so it has them too; neither is ever fired here, because
    //    this file is about the MAIN-world handshake and not about that listener.
    visibilityState: 'visible' as const,
    addEventListener() { /* the visibility listener is not what this file tests */ },
  });
  vi.stubGlobal('window', win);
  return page;
}

function stubRuntime(): void {
  const api = withI18n({
    runtime: {
      id: 'mock-extension-id',
      onMessage: { addListener() { /* the backfill port, not this handshake */ } },
      async sendMessage() { return undefined; },
    },
  });
  vi.stubGlobal('browser', api);
  vi.stubGlobal('chrome', api);
}

async function loadBridge(): Promise<void> {
  const mod: any = await import('../entrypoints/dw-bridge.content');
  mod.default.main();
}

async function loadMainHook(): Promise<void> {
  const mod: any = await import('../entrypoints/dw-fetch-main.content');
  mod.default.main();
}

/** Let the clock run, then dispatch whatever the page world posted. */
async function settle(page: FakePage, ms: number): Promise<void> {
  await vi.advanceTimersByTimeAsync(ms);
  page.pump();
}

/** The page world's copy of the hook, handed to the fake before any script runs. */
async function pageHook() {
  const mod = await import('../lib/page-hook');
  return {
    installPageFetchHook: mod.installPageFetchHook as (options: unknown) => void,
    PAGE_HOOK_OPTIONS: mod.PAGE_HOOK_OPTIONS as unknown,
  };
}

/** The hook's answer to the bridge's probe: an echo of the token it invented. */
function probeAnswers(page: FakePage): any[] {
  return page.posted.filter(
    (m) => m?.type === MAIN_READY_MESSAGE && typeof m.token === 'string',
  );
}

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers();
  vi.stubGlobal('defineContentScript', (cfg: unknown) => cfg);
  vi.stubGlobal('defineBackground', (cb: unknown) => cb);
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe('W12 · MAIN world answers: nothing is injected any more', () => {
  it('🔴 MAIN runs first ⇒ the probe is answered and no inline script is ever created', async () => {
    const page = makeFakePage(await pageHook());
    stubRuntime();

    await loadMainHook();            // the MAIN-world content script
    await loadBridge();              // the isolated bridge
    await settle(page, 1);

    // The answer really happened, and it carries the token the bridge invented —
    // it is not the hook's own install-time `token: null` broadcast.
    const answers = probeAnswers(page);
    expect(answers).toHaveLength(1);
    expect(page.posted.some((m) => m?.type === MAIN_READY_MESSAGE && m.token === null)).toBe(true);

    // 🔴 The whole point: no <script> was created or appended for verification.
    //    The old handshake injected one here, and DeepSeek's CSP logged it.
    expect(page.createdScripts).toBe(0);
    expect(page.injectedSources).toEqual([]);

    // ...and it stays that way past every deadline the old code had.
    await settle(page, 1_000);
    expect(page.createdScripts).toBe(0);
    expect(page.injectedSources).toEqual([]);
  });

  it('🔴 the bridge runs first ⇒ same outcome (the probe waits for the hook, it does not race it)', async () => {
    const page = makeFakePage(await pageHook());
    stubRuntime();

    await loadBridge();              // the isolated bridge first
    await loadMainHook();            // the MAIN-world content script second
    // Both content scripts have now run; only now are the posted messages
    // dispatched, exactly as a browser does it.
    await settle(page, 1);

    expect(probeAnswers(page)).toHaveLength(1);
    expect(page.createdScripts).toBe(0);

    await settle(page, 1_000);
    expect(page.createdScripts).toBe(0);
    expect(page.injectedSources).toEqual([]);
  });
});

describe('W12 · MAIN world silent: the fallback still runs, and says so exactly once', () => {
  it('🔴 no answer ⇒ the fallback is injected, and a blocked one warns exactly once however long we watch', async () => {
    const page = makeFakePage(await pageHook());
    page.allowInlineScripts = false;   // DeepSeek's CSP: appended, never executed
    stubRuntime();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    await loadBridge();
    // Nothing in the MAIN world: the probe goes unanswered.
    await settle(page, 1);
    expect(page.createdScripts).toBe(0);

    // The capability window closes: the fallback is injected here.
    await settle(page, 150);
    expect(page.createdScripts).toBe(1);
    expect(page.injectedSources).toHaveLength(1);
    expect(page.injectedSources[0]).toContain('installPageFetchHook');

    // The fallback's own verification window closes with no answer to the
    // re-probe: one warning, naming the fixed constant.
    await settle(page, 150);
    const warnings = warn.mock.calls.filter((c) => c[0] === FALLBACK_HOOK_VERIFICATION_WARNING);
    expect(warnings).toHaveLength(1);

    // 🔴 Once per page, not once per deadline: nothing repeats.
    await settle(page, 5_000);
    const later = warn.mock.calls.filter((c) => c[0] === FALLBACK_HOOK_VERIFICATION_WARNING);
    expect(later).toHaveLength(1);
  });

  it('🔴 the fallback that does execute is verified by its own answer, so it does not warn', async () => {
    const page = makeFakePage(await pageHook());
    page.allowInlineScripts = true;    // the inline script runs
    stubRuntime();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    await loadBridge();
    await settle(page, 1);
    await settle(page, 150);           // fallback injected, and it runs
    await settle(page, 150);           // its re-probe is answered; no deadline should fire
    await settle(page, 5_000);

    expect(page.createdScripts).toBe(1);
    // The injected copy of the hook answered the bridge's token from the page
    // world — verification without a second <script> reading `window.fetch`.
    expect(probeAnswers(page).length).toBeGreaterThanOrEqual(1);
    expect(warn.mock.calls.filter((c) => c[0] === FALLBACK_HOOK_VERIFICATION_WARNING)).toHaveLength(0);
  });
});

describe('W12 · what an answered probe is allowed to mean', () => {
  it('🔴 a hook that throws before it patches fetch does not answer the probe', async () => {
    const page = makeFakePage(await pageHook());
    stubRuntime();

    // A page that froze `XMLHttpRequest.prototype`: the XHR patch assignment in
    // `installPageFetchHook` throws before the fetch wrapper is ever assigned, so
    // the hook did NOT install. `installPageFetchHook` has always let that throw
    // escape (the WebSocket patch below it is guarded for the same reason).
    const Xhr = function Xhr() { /* never constructed */ } as unknown as { prototype: object };
    Object.freeze(Xhr.prototype);
    page.win.XMLHttpRequest = Xhr;

    const { installPageFetchHook, PAGE_HOOK_OPTIONS } = await import('../lib/page-hook');
    expect(() => installPageFetchHook(PAGE_HOOK_OPTIONS)).toThrow();

    // The marker is absent, so the hook is not installed...
    expect(page.win.fetch[PAGE_HOOK_FETCH_MARKER]).not.toBe(PAGE_HOOK_VERSION);

    // ...and the probe must get no answer, because an answer is now the only
    // proof the isolated side has. Registering the listener before the patch —
    // which is where it used to live — answered from a page that was still
    // talking to the original `fetch`.
    page.win.postMessage({ type: MAIN_PROBE_MESSAGE, token: 'tok-0123456789' }, ORIGIN);
    await settle(page, 1);
    expect(probeAnswers(page)).toEqual([]);
  });

  it('a hook that does install answers a probe carrying the token it was given', async () => {
    const hook = await pageHook();
    const page = makeFakePage(hook);
    stubRuntime();

    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);
    expect(page.win.fetch[PAGE_HOOK_FETCH_MARKER]).toBe(PAGE_HOOK_VERSION);

    const token = 'tok-0123456789';
    page.win.postMessage({ type: MAIN_PROBE_MESSAGE, token }, ORIGIN);
    await settle(page, 1);

    const answers = probeAnswers(page);
    expect(answers).toHaveLength(1);
    expect(answers[0].token).toBe(token);
  });
});
