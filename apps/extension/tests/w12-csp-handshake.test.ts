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
 *      hook, so a hook that could not complete its install must stay silent.
 *      🔴 W43b · "Installed" now means **both** halves: a page whose XHR patch did
 *      not take is still not answered from, even though its `fetch` half is.
 *  (d) the fallback that *is* blocked (append succeeds, execution does not) is the
 *      case that must warn rather than pass for working.
 */

import { afterEach, describe, expect, it, vi, beforeEach } from 'vitest';
import { withI18n } from './i18n-harness';
import {
  HOOK_REASON_DID_NOT_TAKE,
  HOOK_REPORT_MESSAGE,
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
  /**
   * 🔴 W36 · Model **Trusted Types enforcement**: true ⇒ assigning the script's
   * `textContent` throws, exactly as Chrome does on a page whose policy carries
   * `require-trusted-types-for 'script'` (`HTMLScriptElement.text` is a
   * TrustedScript sink). This is a different failure from `allowInlineScripts:
   * false`: there the script is appended and merely never executes, and the
   * bridge's own verification sees the difference.
   */
  trustedTypes: boolean;
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
    trustedTypes: false,
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
      const script: { textContent: string; remove(): void } = {
        textContent: '',
        remove() { /* detached again by the bridge */ },
      };
      if (page.trustedTypes) {
        // Chrome's own refusal, verbatim (measured 2026-09-19 on a page served
        // with `require-trusted-types-for 'script'`): the assignment throws
        // before anything can be appended or executed.
        Object.defineProperty(script, 'textContent', {
          set() {
            throw new TypeError(
              "Failed to set the 'textContent' property on 'HTMLScriptElement':"
              + " This document requires 'TrustedScript' assignment.",
            );
          },
        });
      }
      return script;
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

  it('🔴 a page that enforces Trusted Types refuses the assignment ⇒ the fallback failure is named, not thrown', async () => {
    // 🔴 W36 · The third refusal, and the one the first real-Chrome acceptance ran
    //    into (measured 2026-09-19 on gemini.google.com): `script.textContent =
    //    source` is a TrustedScript sink, so on a page whose policy carries
    //    `require-trusted-types-for 'script'` the assignment **throws**. The other
    //    two cases model "appended but not executed" — this one models the browser,
    //    where the refusal happens before anything can be appended at all.
    //
    //    The property under test is the one this file's whole fallback machinery
    //    exists for: an inline script this extension could not get into the page
    //    must be a **named** fact. A throw escapes `injectPageScript` before
    //    `fallbackScriptAppended` is assigned, so the warning below is never
    //    reached — the failure is silent exactly where it cannot be fixed.
    const page = makeFakePage(await pageHook());
    page.trustedTypes = true;
    stubRuntime();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    await loadBridge();
    await settle(page, 1);             // the probe goes unanswered
    await settle(page, 150);           // the fallback is attempted here — and refused

    const warnings = warn.mock.calls.filter((c) => c[0] === FALLBACK_HOOK_VERIFICATION_WARNING);
    expect(warnings).toHaveLength(1);

    // Nothing was appended (the refusal came first), so the two states stay apart:
    // "this page refused to run our script" is not "our script ran and did nothing".
    expect(page.injectedSources).toEqual([]);

    // Still once per page, however long we watch.
    await settle(page, 5_000);
    expect(warn.mock.calls.filter((c) => c[0] === FALLBACK_HOOK_VERIFICATION_WARNING)).toHaveLength(1);
  });
});

describe('W12 · what an answered probe is allowed to mean', () => {
  it('🔴 a hook whose XHR patch cannot take does not answer the probe', async () => {
    const page = makeFakePage(await pageHook());
    stubRuntime();

    // A page that froze `XMLHttpRequest.prototype`: neither the `open` nor the
    // `send` assignment can take.
    // 🔴 W43b · This case's premise changed with the fix, and it is worth naming
    //    which way: the XHR assignment is now guarded the way the fetch assignment
    //    and the WebSocket patch already were, so a refusal of one half is a
    //    **recorded** observation instead of an exception thrown at
    //    `document_start` that abandoned the other half. What the case exists for is
    //    unchanged — the probe is not answered, because an answer is the isolated
    //    side's only proof that capture is live, and on this page the XHR requests
    //    go straight past us.
    const Xhr = function Xhr() { /* never constructed */ } as unknown as { prototype: object };
    Object.freeze(Xhr.prototype);
    page.win.XMLHttpRequest = Xhr;

    const { installPageFetchHook, PAGE_HOOK_OPTIONS } = await import('../lib/page-hook');
    expect(() => installPageFetchHook(PAGE_HOOK_OPTIONS)).not.toThrow();

    // The refusal is the hook's own report, posted into the page world for the
    // bridge to record; the fetch half is in place and is not what is broken.
    expect(page.posted).toContainEqual(
      expect.objectContaining({ type: HOOK_REPORT_MESSAGE, reason: HOOK_REASON_DID_NOT_TAKE }),
    );
    expect(page.win.fetch[PAGE_HOOK_FETCH_MARKER]).toBe(PAGE_HOOK_VERSION);

    // ...and the probe must get no answer. Registering the listener before the
    // patch — which is where it used to live — answered from a page that was still
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
