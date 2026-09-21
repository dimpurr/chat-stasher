/**
 * W43 · **A page where the capture hook did not install must say so.**
 *
 * ## What this file is protecting
 *
 * Until W43, a page whose hook never ran produced an empty stage — and so did a
 * page where the user simply opened no conversation. The extension had no
 * surface at all that could tell those two apart, which is the project's first
 * invariant (an unknown must never be recorded as empty) broken in the one place
 * the extension itself is the observer. The 2026-09-19 acceptance measured
 * exactly that state on two platforms and could not read it as anything.
 *
 * Three things are asserted here, and they are three different claims:
 *
 *  1. **The vocabulary is closed and total.** Every reason in
 *     `HOOK_OBSERVATIONS` has a sentence, and `isHookStatusMessage` accepts
 *     exactly the two shapes a page's report can have.
 *  2. **The record is per origin and last-word-wins among top frames.**
 *     Observations merge by reason, a non-record in storage is never read as
 *     one, and the one thing that clears a record is a *top frame* whose hook
 *     *verified* — evidence, not acknowledgement. A child frame's observation
 *     is a statement about that frame and is not sent.
 *  3. **The bridge says it out loud.** On a page where the hook answered, one
 *     positive observation leaves; on a page where it did not and the fallback
 *     was refused, one failure observation leaves, exactly once, carrying an
 *     origin and a reason code and nothing else.
 *
 * ## And the frame shape the fix is for
 *
 * The measured hole behind the fix: an `about:blank` / `about:srcdoc` subframe of
 * a matched origin got **neither** content script, and its `location.origin` is
 * the string `"null"` even though its environment origin is the inherited one.
 * With the content scripts installed there (`matchOriginAsFallback`) the old code
 * still could not work: `postMessage(data, "null")` **throws** `SyntaxError` in
 * Chromium (measured), so the hook could not post a capture, answer a probe, or
 * report its own failure, and `parsed.origin !== pageOrigin` refused every
 * request the frame made. The last group below is that frame, modelled on what
 * the browser actually does.
 *
 * The fake page is deliberately the same shape as `w12-csp-handshake.test.ts`'s:
 * `postMessage` is delivered as a task, an appended inline script executes
 * synchronously unless the test refuses it. The differences are the two facts
 * this file needs — a `targetOrigin` that behaves like the browser's (an invalid
 * origin throws, a non-matching one drops) and an environment origin that is not
 * `location.origin`.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import {
  CAPTURE_MESSAGE,
  HOOK_OBSERVATIONS,
  HOOK_REASON_DID_NOT_RUN,
  HOOK_REASON_DID_NOT_TAKE,
  HOOK_REASON_WAS_REPLACED,
  HOOK_REPORT_MESSAGE,
  HOOK_SELF_CHECK_INTERVAL_MS,
  HOOK_STATUS_MESSAGE,
  isHookStatusMessage,
  MAIN_READY_MESSAGE,
  PLATFORMS,
  type HookObservation,
} from '../lib/contract';
import {
  hookStatusKey,
  hookStatusOf,
  looksLikeHookStatus,
  mergeHookObservation,
  recordHookStatus,
  type HookStatusRecord,
} from '../lib/hook-status';
import { NO_FAILURES, renderPopup, type PopupModel } from '../lib/popup-view';
import { FALLBACK_HOOK_VERIFICATION_WARNING } from '../lib/fallback-verification';
import type { BackfillStore } from '../lib/backfill/store';

const ORIGIN = 'https://chatgpt.com';
const SESSION_ID = 'a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const API_PATH = `/backend-api/conversation/${SESSION_ID}`;
/** The ChatGPT row's declared shape, so a capture is not refused for the wrong reason. */
const CONVERSATION_BODY = JSON.stringify({ mapping: {}, current_node: 'node-1' });

// ---------------------------------------------------------------------------
// The record (no browser)
// ---------------------------------------------------------------------------

/** A store with the port's four members and nothing else, like `browserLocalStore`'s. */
function memoryStore(initial: Record<string, unknown> = {}) {
  const rows = new Map<string, unknown>(Object.entries(initial));
  const store: BackfillStore = {
    async load(key) { return rows.has(key) ? rows.get(key) : null; },
    async save(key, value) { rows.set(key, value); },
    async remove(key) { rows.delete(key); },
    async keys() { return [...rows.keys()]; },
  };
  return { store, rows };
}

function record(overrides: Partial<HookStatusRecord> = {}): HookStatusRecord {
  return {
    origin: ORIGIN,
    platform: 'chatgpt',
    reasons: [{ reason: HOOK_REASON_DID_NOT_RUN, at: 1_000 }],
    at: 1_000,
    ...overrides,
  };
}

/** A model with the fields `renderPopup` needs, so only the field under test varies. */
function model(overrides: Partial<PopupModel> = {}): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    ...overrides,
  };
}

describe('W43 · the closed vocabulary has a sentence for every member', () => {
  it('🔴 renders a distinct note for each reason — a new reason cannot ship unsaid', () => {
    for (const reason of HOOK_OBSERVATIONS) {
      const notes = renderPopup(model({ hookStatus: [record({ reasons: [{ reason, at: 5 }] })] })).notes;
      const joined = notes.join('\n');
      expect(joined, `no note rendered for ${reason}`).toContain(ORIGIN);
      expect(joined, `reason code leaked into the sentence: ${reason}`).not.toContain(reason);
    }
  });

  it('names the reason sentence differently for each member — they are different facts', () => {
    const sentenceFor = (reason: HookObservation): string =>
      renderPopup(model({ hookStatus: [record({ reasons: [{ reason, at: 5 }] })] })).notes.join('\n');
    const sentences = HOOK_OBSERVATIONS.map(sentenceFor);
    expect(new Set(sentences).size).toBe(HOOK_OBSERVATIONS.length);
  });

  it('says nothing when nothing was observed, and nothing when the field was not read', () => {
    expect(renderPopup(model({ hookStatus: [] })).notes.join('\n')).not.toContain(ORIGIN);
    // An existing call site that never passes the field changes no character.
    expect(renderPopup(model()).notes.join('\n')).not.toContain(ORIGIN);
  });

  it('states the observation and does not name a cause', () => {
    const joined = renderPopup(model({ hookStatus: [record()] })).notes.join('\n').toLowerCase();
    // The 2026-09-19 measurement left the cause on one platform unsettled, so no
    // sentence here may assert one — this is the assertion that keeps a future
    // edit from writing "Trusted Types" (or any other guess) into the popup.
    // "usual reason" / "already open when the extension was loaded" are the
    // cause the pre-W46 `notInstalled` copy named; an observation records a
    // fact and never names a reason.
    for (const guess of [
      'trusted types',
      'content security policy',
      'csp',
      'usual reason',
      'already open when the extension was loaded',
    ]) {
      expect(joined).not.toContain(guess);
    }
  });
});

describe('W43 · the hook-status message guard', () => {
  const base = { type: HOOK_STATUS_MESSAGE, origin: ORIGIN, observedAt: 1_000 };

  it('accepts the positive observation and every reason in the set', () => {
    expect(isHookStatusMessage({ ...base, reason: null })).toBe(true);
    for (const reason of HOOK_OBSERVATIONS) {
      expect(isHookStatusMessage({ ...base, reason })).toBe(true);
    }
  });

  it('refuses a wrong type, an empty origin, a missing time, and an unknown reason', () => {
    expect(isHookStatusMessage({ ...base, reason: null, type: 'chat-captured' })).toBe(false);
    expect(isHookStatusMessage({ ...base, reason: null, origin: '' })).toBe(false);
    expect(isHookStatusMessage({ type: HOOK_STATUS_MESSAGE, origin: ORIGIN, reason: null })).toBe(false);
    expect(isHookStatusMessage({ ...base, reason: 'hook-was-something' })).toBe(false);
    // 🔴 `undefined` is not the positive observation. A page cannot clear a
    //    record by saying nothing; only the explicit `null` does that.
    expect(isHookStatusMessage({ ...base, reason: undefined })).toBe(false);
    expect(isHookStatusMessage(null)).toBe(false);
  });
});

describe('W43 · the per-origin record', () => {
  it('replaces a repeated reason instead of appending, so its size is bounded by the vocabulary', () => {
    const first = mergeHookObservation(null, {
      origin: ORIGIN, platform: 'chatgpt', reason: HOOK_REASON_DID_NOT_RUN, at: 10,
    });
    const second = mergeHookObservation(first, {
      origin: ORIGIN, platform: 'chatgpt', reason: HOOK_REASON_DID_NOT_RUN, at: 20,
    });
    expect(second.reasons).toHaveLength(1);
    expect(second.reasons[0]!.at).toBe(20);
    expect(second.at).toBe(20);

    const third = mergeHookObservation(second, {
      origin: ORIGIN, platform: 'chatgpt', reason: HOOK_REASON_WAS_REPLACED, at: 30,
    });
    expect(third.reasons).toHaveLength(2);
    // Ordered by the closed set, not by the order things were noticed in.
    expect(third.reasons.map((row) => row.reason)).toEqual([
      HOOK_REASON_DID_NOT_RUN,
      HOOK_REASON_WAS_REPLACED,
    ]);
  });

  it('does not merge two origins into one row', () => {
    const other = mergeHookObservation(
      record({ origin: 'https://gemini.google.com', platform: 'gemini' }),
      { origin: ORIGIN, platform: 'chatgpt', reason: HOOK_REASON_DID_NOT_RUN, at: 50 },
    );
    expect(other.origin).toBe(ORIGIN);
    expect(other.platform).toBe('chatgpt');
    expect(other.reasons).toHaveLength(1);
  });

  it('🔴 clears on the positive observation — evidence, not acknowledgement', async () => {
    const memory = memoryStore();
    await recordHookStatus(memory.store, {
      origin: ORIGIN, platform: 'chatgpt', reason: HOOK_REASON_DID_NOT_RUN, at: 10,
    });
    expect(memory.rows.has(hookStatusKey(ORIGIN))).toBe(true);

    await recordHookStatus(memory.store, { origin: ORIGIN, platform: 'chatgpt', reason: null, at: 20 });
    // `remove`, not an empty record: nothing is left to filter out later, and a
    // reader cannot mistake a leftover row for a live observation.
    expect(memory.rows.has(hookStatusKey(ORIGIN))).toBe(false);
  });

  it('never repairs junk into a record, and never lets a key disagree with its own row', () => {
    const junk = { [hookStatusKey(ORIGIN)]: { origin: ORIGIN }, [hookStatusKey(ORIGIN) + 'x']: record() };
    expect(hookStatusOf(junk)).toEqual([]);
    const mismatched = { [hookStatusKey('https://gemini.google.com')]: record() };
    expect(hookStatusOf(mismatched)).toEqual([]);
    expect(looksLikeHookStatus(record())).toBe(true);
    // A record with no reasons is not a record: there is nothing it observed.
    expect(looksLikeHookStatus({ ...record(), reasons: [] })).toBe(false);
  });

  it('reads every record back, newest first, and nothing when the snapshot is unreadable', () => {
    const older = record({ at: 10, reasons: [{ reason: HOOK_REASON_DID_NOT_RUN, at: 10 }] });
    const newer = record({
      origin: 'https://gemini.google.com', platform: 'gemini', at: 20,
      reasons: [{ reason: HOOK_REASON_WAS_REPLACED, at: 20 }],
    });
    const snapshot = {
      [hookStatusKey(older.origin)]: older,
      [hookStatusKey(newer.origin)]: newer,
      unrelated: 1,
    };
    expect(hookStatusOf(snapshot).map((r) => r.origin)).toEqual([newer.origin, older.origin]);
    expect(hookStatusOf(null)).toEqual([]);
  });

  it('is a no-op without a store, and does not throw when the store refuses', async () => {
    await expect(recordHookStatus(null, {
      origin: ORIGIN, platform: 'chatgpt', reason: HOOK_REASON_DID_NOT_RUN, at: 1,
    })).resolves.toBeUndefined();

    const refusing: BackfillStore = {
      async load() { return null; },
      async save() { throw new Error('quota'); },
      async remove() { throw new Error('quota'); },
      async keys() { return []; },
    };
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => { /* asserted below */ });
    await expect(recordHookStatus(refusing, {
      origin: ORIGIN, platform: 'chatgpt', reason: HOOK_REASON_DID_NOT_RUN, at: 1,
    })).resolves.toBeUndefined();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it('a full cycle: observe, observe again, then a verified page withdraws it', async () => {
    const memory = memoryStore();
    const observe = (reason: HookObservation, at: number) =>
      recordHookStatus(memory.store, { origin: ORIGIN, platform: 'chatgpt', reason, at });

    await observe(HOOK_REASON_DID_NOT_RUN, 1);
    await observe(HOOK_REASON_WAS_REPLACED, 2);
    const shown = hookStatusOf(Object.fromEntries(memory.rows));
    expect(shown).toHaveLength(1);
    expect(shown[0]!.reasons.map((row) => row.reason)).toEqual([
      HOOK_REASON_DID_NOT_RUN,
      HOOK_REASON_WAS_REPLACED,
    ]);

    await recordHookStatus(memory.store, { origin: ORIGIN, platform: 'chatgpt', reason: null, at: 3 });
    expect(hookStatusOf(Object.fromEntries(memory.rows))).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// The page world
// ---------------------------------------------------------------------------

interface Posted {
  data: any;
  targetOrigin: string;
}

interface FakePage {
  win: any;
  posted: Posted[];
  injectedSources: string[];
  createdScripts: number;
  /** false ⇒ an appended inline script is never executed (a CSP `script-src` refusal). */
  allowInlineScripts: boolean;
  /** true ⇒ assigning the script's `textContent` throws, as Trusted Types requires. */
  trustedTypes: boolean;
  /** What the page's own `fetch` returns for the captured path. */
  respond(body?: string): void;
  /** Deliver every message the page world has posted, and anything posted in response. */
  pump(): void;
}

interface FakePageOptions {
  /** Overrides `useLocationOriginOnly` — the URL-less frame's reading. */
  environmentOrigin?: string;
  locationOrigin?: string;
  /**
   * 🔴 W46 · Model a nested browsing context: `window.top` is some other
   *    window, so `isTopFrame()` is false. The default is a top frame
   *    (`window.top` unset, which the bridge treats as the top document — the
   *    node test environment has no `top` at all).
   */
  nested?: boolean;
}

/**
 * One fake page world, faithful in the three places this file depends on:
 *
 *  · `postMessage` is delivered **as a task**, so every `document_start` content
 *    script — isolated and MAIN, in whichever order they were injected — runs to
 *    completion before the messages posted during that phase are dispatched;
 *  · `targetOrigin` behaves like the browser's: a string that is not a valid
 *    origin **throws** `SyntaxError` (which is what `"null"` is, measured in
 *    Chromium on an `about:blank` frame) and a valid origin that is not this
 *    document's environment origin is dropped without a word;
 *  · an appended inline <script> executes synchronously, unless the test models
 *    a CSP refusal (`allowInlineScripts: false`) or Trusted Types
 *    (`trustedTypes: true`), which are different refusals and are modelled as
 *    such.
 */
function makeFakePage(hook: {
  installPageFetchHook: (options: unknown) => void;
  PAGE_HOOK_OPTIONS: unknown;
}, options: FakePageOptions = {}): FakePage {
  const listeners: Array<(event: unknown) => void> = [];
  const tasks: Array<() => void> = [];
  const locationOrigin = options.locationOrigin ?? ORIGIN;
  const environmentOrigin = options.environmentOrigin ?? locationOrigin;
  let body = CONVERSATION_BODY;

  const page: FakePage = {
    win: null,
    posted: [],
    injectedSources: [],
    createdScripts: 0,
    allowInlineScripts: false,
    trustedTypes: false,
    respond(next?: string) { if (next !== undefined) body = next; },
    pump() {
      while (tasks.length > 0) tasks.shift()!();
    },
  };

  class FakeXhr {
    open(): void { /* patched by the hook; nothing else needs it */ }
    send(): void { /* see above */ }
  }

  const win: any = {
    location: { origin: locationOrigin, href: `${locationOrigin}/` },
    // 🔴 The fact the whole frame case turns on: the environment's origin is not
    //    the URL's. Set to `undefined` by a test that wants the pre-W43 reading.
    origin: environmentOrigin === 'none' ? undefined : environmentOrigin,
    XMLHttpRequest: FakeXhr,
    fetch: async () => new Response(body, { status: 200 }),
    // 🔴 W43c · The hook's re-check is a page timer (`window.setInterval`), so the
    //    fake window carries one. Vitest's fake clock is installed in `beforeEach`,
    //    so this is the faked timer and `settle` is what advances it.
    setInterval: setInterval as unknown,
    addEventListener(name: string, fn: (event: unknown) => void) {
      if (name === 'message') listeners.push(fn);
    },
    postMessage(data: unknown, targetOrigin: string) {
      if (!/^[a-z][a-z0-9+.-]*:\/\//.test(targetOrigin)) {
        // Chromium, measured on an `about:blank` subframe: `"null"` is not a URL.
        throw new SyntaxError(
          "Failed to execute 'postMessage' on 'Window': Invalid target origin"
          + ` '${targetOrigin}' in a call to 'postMessage'.`,
        );
      }
      page.posted.push({ data, targetOrigin });
      if (targetOrigin !== environmentOrigin) return;
      tasks.push(() => {
        for (const fn of [...listeners]) fn({ source: win, origin: environmentOrigin, data });
      });
    },
  };
  page.win = win;

  vi.stubGlobal('document', {
    documentElement: {
      appendChild(script: { textContent: string }) {
        page.injectedSources.push(script.textContent);
        if (!page.allowInlineScripts) return;
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
    visibilityState: 'visible' as const,
    addEventListener() { /* the visibility listener is not what this file tests */ },
  });
  // A nested frame has a `top` that is not itself. Leaving it unset is the
  // top-document case the bridge already treats as `isTopFrame() === true`.
  if (options.nested) win.top = { /* a parent browsing context, not this window */ };
  vi.stubGlobal('window', win);
  vi.stubGlobal('top', options.nested ? win.top : win);
  return page;
}

/** The messages the bridge sent to background, in order. */
function stubRuntime(): any[] {
  const sent: any[] = [];
  const api = withI18n({
    runtime: {
      id: 'mock-extension-id',
      onMessage: { addListener() { /* the backfill port, not this handshake */ } },
      async sendMessage(message: unknown) { sent.push(message); return undefined; },
    },
  });
  vi.stubGlobal('browser', api);
  vi.stubGlobal('chrome', api);
  return sent;
}

async function loadBridge(): Promise<void> {
  const mod: any = await import('../entrypoints/dw-bridge.content');
  mod.default.main();
}

async function pageHook() {
  const mod = await import('../lib/page-hook');
  return {
    installPageFetchHook: mod.installPageFetchHook as (options: unknown) => void,
    PAGE_HOOK_OPTIONS: mod.PAGE_HOOK_OPTIONS as unknown,
  };
}

/** Let the clock run, then dispatch whatever the page world posted. */
async function settle(page: FakePage, ms = 1): Promise<void> {
  await vi.advanceTimersByTimeAsync(ms);
  page.pump();
}

function hookStatusReports(sent: any[]): any[] {
  return sent.filter((message) => message?.type === HOOK_STATUS_MESSAGE);
}

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers();
  vi.stubGlobal('defineContentScript', (cfg: unknown) => cfg);
  vi.stubGlobal('defineBackground', (cb: unknown) => cb);
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('W43 · the bridge reports what the page said about its own hook', () => {
  it('🔴 a page whose hook answered sends the positive observation, and nothing else', async () => {
    const page = makeFakePage(await pageHook());
    const sent = stubRuntime();

    // MAIN first, so the probe is answered by the real hook and the fallback is
    // never injected — the shape of a healthy page on any platform.
    const mainMod: any = await import('../entrypoints/dw-fetch-main.content');
    mainMod.default.main();
    await loadBridge();
    await settle(page);

    const reports = hookStatusReports(sent);
    expect(reports).toHaveLength(1);
    expect(reports[0]).toMatchObject({
      type: HOOK_STATUS_MESSAGE,
      origin: ORIGIN,
      // 🔴 The positive observation, and it is `null` and not `undefined`: only
      //    the explicit value withdraws a record.
      reason: null,
    });
    expect(typeof reports[0].observedAt).toBe('number');
    // No capture, no id, no token: this message is metadata about our own code.
    expect(Object.keys(reports[0]).sort()).toEqual(['observedAt', 'origin', 'reason', 'type']);
  });

  it('🔴 a page where Trusted Types refuses the fallback sends one named failure, and never silence', async () => {
    const page = makeFakePage(await pageHook());
    page.trustedTypes = true;
    const sent = stubRuntime();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => { /* asserted below */ });

    // The bridge alone: no MAIN-world copy ran, which is the measured state.
    await loadBridge();
    await settle(page, 1_000);

    const reports = hookStatusReports(sent);
    expect(reports).toHaveLength(1);
    expect(reports[0]).toMatchObject({ origin: ORIGIN, reason: HOOK_REASON_DID_NOT_RUN });
    // The console line the record is the durable counterpart of.
    expect(warn.mock.calls.flat().join(' ')).toContain(FALLBACK_HOOK_VERIFICATION_WARNING);

    // Watching longer does not produce a second report: a page that keeps failing
    // has to stay legible.
    await settle(page, 10_000);
    expect(hookStatusReports(sent)).toHaveLength(1);
    warn.mockRestore();
  });

  it('a verified fallback is a hook that is installed, so it is reported as the positive observation', async () => {
    const page = makeFakePage(await pageHook());
    // The append works and the script runs, but only after the probe timed out:
    // the fallback path, which the bridge verifies with its own token.
    page.allowInlineScripts = true;
    const sent = stubRuntime();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => { /* asserted below */ });

    await loadBridge();
    // 🔴 Two steps, not one, and the order is the point: the fallback is injected
    //    by a timer, and the token it posts in answer is delivered as a **task**.
    //    Advancing the clock past the verification timer without dispatching that
    //    task would report a failure the browser never sees.
    await settle(page, 101);
    expect(warn.mock.calls.flat().join(' ')).not.toContain(FALLBACK_HOOK_VERIFICATION_WARNING);
    await settle(page, 500);
    expect(warn.mock.calls.flat().join(' ')).not.toContain(FALLBACK_HOOK_VERIFICATION_WARNING);

    // The fallback hook answered its probe, so the positive observation leaves.
    const reports = hookStatusReports(sent);
    expect(reports).toHaveLength(1);
    expect(reports[0].reason).toBeNull();
    warn.mockRestore();
  });
});

describe('W43 · a URL-less same-origin frame', () => {
  /**
   * The frame the fix is for: `location.origin` is the string `"null"`, the
   * environment origin is the inherited one, and a request to the platform is
   * same-origin as far as the browser is concerned.
   */
  const frameOptions = { locationOrigin: 'null', environmentOrigin: ORIGIN };

  it('🔴 installs the hook and captures, where the URL-derived origin made both impossible', async () => {
    const page = makeFakePage(await pageHook(), frameOptions);
    const hook = await pageHook();
    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);
    // The patch really took: the page's own function is ours now.
    expect(String(page.win.fetch)).not.toContain('[native code]');

    // A relative path, because that is what a real app fetches with, and it is
    // the case the URL-derived base made impossible: `new URL('/api',
    // 'about:blank')` throws, and the throw was swallowed as "not a candidate".
    page.win.fetch(API_PATH).catch(() => { /* the page's own call is not our concern */ });
    await settle(page, 0);

    const captures = page.posted.filter((entry) => entry.data?.type === CAPTURE_MESSAGE);
    expect(captures).toHaveLength(1);
    // 🔴 And it was addressed to the **environment** origin. With
    //    `location.origin` this post threw `SyntaxError` — measured in Chromium,
    //    and modelled by this fake — so no capture left the frame at all.
    expect(captures[0]!.targetOrigin).toBe(ORIGIN);
    expect(captures[0]!.data.payload.url).toContain(SESSION_ID);
  });

  it('🔴 still refuses a genuinely opaque document: no platform matches "null"', async () => {
    // A sandboxed frame with no origin of its own reports `"null"` from the
    // environment origin too. The fix must not have opened that door.
    const page = makeFakePage(await pageHook(), { locationOrigin: 'null', environmentOrigin: 'null' });
    const hook = await pageHook();
    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);

    page.win.fetch(API_PATH).catch(() => { /* the page's own call is not our concern */ });
    await vi.advanceTimersByTimeAsync(1);

    expect(page.posted.filter((entry) => entry.data?.type === CAPTURE_MESSAGE)).toHaveLength(0);
    // Nothing is posted either — including the install-time readiness signal.
    expect(page.posted).toEqual([]);
  });
});

describe('W43 · the hook reports a patch that did not take', () => {
  it('🔴 a global that refuses the wrapper is reported, and the probe is left unanswered', async () => {
    const page = makeFakePage(await pageHook());
    const sent = stubRuntime();
    const nativeFetch = page.win.fetch;
    // 🔴 The shape the read-back exists for: an assignment that neither takes nor
    //    throws (a swallowing setter — what a read-only global does in the
    //    sloppy-mode IIFE this file compiles to in the browser). "I assigned it"
    //    and "the page now uses my function" are two different facts.
    Object.defineProperty(page.win, 'fetch', {
      configurable: true,
      get: () => nativeFetch,
      set: () => { /* swallowed */ },
    });

    const hook = await pageHook();
    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);
    await loadBridge();
    // The hook's own report is posted at install time and relayed as a task; the
    // bridge's fallback failure comes later, from a timer.
    await settle(page, 0);
    await settle(page, 500);

    // The positive observation can never be sent — the probe has no listener —
    // and the failure is named instead. Both facts are named, in the order they
    // were observed: the refused patch first, then the verification that failed.
    expect(hookStatusReports(sent).map((message) => message.reason)).toEqual([
      HOOK_REASON_DID_NOT_TAKE,
      HOOK_REASON_DID_NOT_RUN,
    ]);
  });

  it('🔴 a wrapper replaced after the install is reported instead of answered', async () => {
    const page = makeFakePage(await pageHook());
    const nativeFetch = page.win.fetch;
    const hook = await pageHook();
    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);
    const sent = stubRuntime();
    await loadBridge();

    // The page restores its own function afterwards — capture is over for this
    // document, and answering a probe would say the opposite.
    page.win.fetch = nativeFetch;
    await postProbe(page);

    const reports = hookStatusReports(sent);
    expect(reports.map((message) => message.reason)).toEqual([HOOK_REASON_WAS_REPLACED]);
    // 🔴 And it was not answered: an answer here would tell the bridge this page
    //    is captured while the page's own calls go straight past us. (The old
    //    form of this line tested for a message name no code has ever posted, so
    //    it could not fail; the token is what makes it an assertion.)
    expect(probeAnswers(page)).toEqual([]);
  });
});

/**
 * W43b · **"Installed" means both live globals are still the wrappers this hook
 * installed.**
 *
 * The shape these two cases model is the measured one, and it is the shape the
 * file above could not fail on: `window.fetch` is ours while
 * `XMLHttpRequest.prototype.open` is the page's own function again. Capture on
 * such a page goes through XHR, so nothing is staged and no backfill target is
 * registered — and because the probe only asked about `fetch`, the page answered
 * it, which sent `reason: null` to background and **withdrew** the record the
 * hook had just written. The user saw the same empty popup as a user who had
 * opened nothing, which is the ambiguity this whole change exists to remove.
 *
 * Both cases assert the same three things about the half-install: it is recorded,
 * the positive observation is not sent for it, and the probe is left unanswered
 * (an answer is the isolated side's only proof that capture is live, and
 * `MAIN_READY_MESSAGE` is that answer).
 */
describe('W43b · a page whose XHR half is not ours is never reported as healthy', () => {
  it('🔴 a page that replaces XMLHttpRequest after the install is reported, not answered', async () => {
    const page = makeFakePage(await pageHook());
    // The page's own two functions, before the hook touches them — what a page
    // that restores its constructor puts back.
    const pageOpen = page.win.XMLHttpRequest.prototype.open;
    const pageSend = page.win.XMLHttpRequest.prototype.send;

    const hook = await pageHook();
    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);
    // Both wrappers took a moment ago, so this is a regression from a working
    // install rather than an install that never happened.
    expect(String(page.win.fetch)).not.toContain('[native code]');

    const sent = stubRuntime();
    await loadBridge();

    // 🔴 The measured half-install itself: `fetch` stays ours, and the constructor
    //    the page will use from now on is its own again.
    class RestoredXhr { /* the page's replacement, with its own prototype */ }
    Object.defineProperty(RestoredXhr.prototype, 'open', { value: pageOpen, configurable: true });
    Object.defineProperty(RestoredXhr.prototype, 'send', { value: pageSend, configurable: true });
    page.win.XMLHttpRequest = RestoredXhr;

    await postProbe(page);

    const reports = hookStatusReports(sent);
    expect(reports.map((message) => message.reason)).toEqual([HOOK_REASON_WAS_REPLACED]);
    // 🔴 A half-install is never the positive observation. `null` is what removes
    //    the origin's record, so sending it here would delete the record this page
    //    just earned — the exact sequence the review measured: written, then
    //    withdrawn moments later.
    expect(reports.some((message) => message.reason === null)).toBe(false);
    // 🔴 And it was not answered: on this page the user's own conversation request
    //    goes straight past us.
    expect(probeAnswers(page)).toEqual([]);
  });

  it('🔴 an XHR whose setter swallows the patch is recorded at install, and still not healthy', async () => {
    const page = makeFakePage(await pageHook());
    const pageOpen = page.win.XMLHttpRequest.prototype.open;
    // 🔴 The install-time shape: the assignment neither takes nor throws — what a
    //    read-only prototype does in the sloppy-mode function the built hook
    //    compiles to — so the fetch half below it still installs.
    Object.defineProperty(page.win.XMLHttpRequest.prototype, 'open', {
      configurable: true,
      get: () => pageOpen,
      set: () => { /* swallowed */ },
    });

    const hook = await pageHook();
    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);

    // 🔴 The installer's own observation, before any probe exists: the XHR half did
    //    not take and that is written down rather than assumed. Deleting
    //    `if (xhrHookExpected && !xhrPatched)` in `lib/page-hook.ts` turns this
    //    assertion red, and nothing else in the suite notices.
    expect(
      page.posted.filter((entry) => entry.data?.type === HOOK_REPORT_MESSAGE)
        .map((entry) => entry.data.reason),
    ).toEqual([HOOK_REASON_DID_NOT_TAKE]);
    // The install continued past the refused half: a refusal is not a reason to
    // abandon the wrapper that did work.
    expect(String(page.win.fetch)).not.toContain('[native code]');

    const sent = stubRuntime();
    await loadBridge();
    await settle(page, 0);
    await postProbe(page);

    const reports = hookStatusReports(sent);
    expect(reports.map((message) => message.reason)).toEqual([HOOK_REASON_DID_NOT_TAKE]);
    expect(reports.some((message) => message.reason === null)).toBe(false);
    expect(probeAnswers(page)).toEqual([]);
  });
});

/**
 * The probe's token, and the only thing that distinguishes a probe **answer** from
 * the best-effort install-time signal: both carry `MAIN_READY_MESSAGE`, and only
 * the answer echoes the token it was asked with (the signal posts `token: null`).
 * So "the probe was answered" is a test on the type *and* the token.
 */
const PROBE_TOKEN = 'probe-token-1234';

function probeAnswers(page: FakePage): any[] {
  return page.posted.filter(
    (entry) => entry.data?.type === MAIN_READY_MESSAGE && entry.data.token === PROBE_TOKEN,
  );
}

/**
 * Ask the page world to verify itself, the way the bridge's probe does.
 *
 * The token only has to be long enough to clear the listener's own guard; the
 * answer is checked by the token's return, not by its value.
 */
async function postProbe(page: FakePage): Promise<void> {
  const { MAIN_PROBE_MESSAGE } = await import('../lib/contract');
  page.win.postMessage({ type: MAIN_PROBE_MESSAGE, token: PROBE_TOKEN }, ORIGIN);
  page.pump();
}

/**
 * W43c · **The hook asks about itself again, after the handshake.**
 *
 * Everything the two groups above test happens in a document's first moments:
 * the read-back at install and the answer to the probe. The measured failure is
 * the one those cannot see, on a real logged-in page after a full reload:
 *
 * ```
 * fetchNative=false   xhrNative=true   hookWhole=false   cs_hook_v1:* {}
 * ```
 *
 * The live XHR half is not ours, `fetch` is, and nothing writes a record. The
 * install-time read-back cannot be silent about a patch that never took (it
 * reports `did-not-take` on the spot), and a page that breaks the half before
 * the probe is reported by the listener (measured in the e2e harness). What is
 * left is a page that takes the transport back **after** it answered a probe —
 * which is the shape below, and which these cases pin as recorded.
 *
 * The three cases are different claims about the same re-check:
 *
 *  1. it reports the half-install the handshake already blessed;
 *  2. a healthy page stays silent, however many times it runs (so the new
 *     timer cannot file a failure against a page that is fine);
 *  3. a hook that is *still* broken repeats the observation, which is what
 *     keeps the record alive when another *top frame* on the origin verifies —
 *     `lib/hook-status.ts` lets the newest top-frame word win, and the newest
 *     word has to keep being the true one. A child frame cannot speak.
 */
describe('W43c · the hook re-checks itself after the handshake', () => {
  it('🔴 a page that takes the XHR half back after answering a probe is reported by the next check', async () => {
    const page = makeFakePage(await pageHook());
    // The page's own two functions, before the hook touches them.
    const pageOpen = page.win.XMLHttpRequest.prototype.open;
    const pageSend = page.win.XMLHttpRequest.prototype.send;

    const hook = await pageHook();
    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);
    const sent = stubRuntime();
    await loadBridge();

    // The handshake completes on a whole hook: the probe is answered, and the
    // one thing that clears an origin's record is sent — there is nothing wrong
    // with this page yet, and the extension says exactly that.
    await postProbe(page);
    expect(probeAnswers(page)).toHaveLength(1);
    expect(hookStatusReports(sent).map((message) => message.reason)).toEqual([null]);

    // 🔴 The measured half-install, one moment after the handshake blessed it:
    //    `fetch` stays ours and the constructor the page will use from now on is
    //    its own again. No probe follows this — that is the whole point.
    class RestoredXhr {}
    Object.defineProperty(RestoredXhr.prototype, 'open', { value: pageOpen, configurable: true });
    Object.defineProperty(RestoredXhr.prototype, 'send', { value: pageSend, configurable: true });
    page.win.XMLHttpRequest = RestoredXhr;

    await settle(page, 0);
    // Still nothing: this is the silence, and it is not a rendering problem.
    expect(hookStatusReports(sent).map((message) => message.reason)).toEqual([null]);

    // The re-check reads the live global and reports what it sees. Deleting the
    // `window.setInterval(recheckHook, …)` registration in `lib/page-hook.ts`
    // turns this assertion red, and nothing else in the suite notices.
    await settle(page, HOOK_SELF_CHECK_INTERVAL_MS + 1);
    expect(hookStatusReports(sent).map((message) => message.reason))
      .toEqual([null, HOOK_REASON_WAS_REPLACED]);
    // And it is still not answered: nothing about the page became capturable.
    expect(probeAnswers(page)).toHaveLength(1);
  });

  it('🔴 a page whose hook stays whole is never reported, however many checks run', async () => {
    const page = makeFakePage(await pageHook());
    const hook = await pageHook();
    hook.installPageFetchHook(hook.PAGE_HOOK_OPTIONS);
    const sent = stubRuntime();
    await loadBridge();
    await postProbe(page);

    // Ten intervals of a page that is fine. A check that reported anything here
    // would file a failure against every healthy page in the product.
    for (let tick = 0; tick < 10; tick += 1) await settle(page, HOOK_SELF_CHECK_INTERVAL_MS + 1);

    expect(hookStatusReports(sent).map((message) => message.reason)).toEqual([null]);
    expect(page.posted.filter((entry) => entry.data?.type === HOOK_REPORT_MESSAGE)).toEqual([]);
  });

  it('🔴 a repeated report from the page is relayed again after one check, and not before', async () => {
    // No hook here on purpose: what this case pins is the bridge's rule for a
    // page's *own* reports, which the re-check above depends on to be heard more
    // than once.
    const page = makeFakePage(await pageHook());
    const sent = stubRuntime();
    await loadBridge();

    const reportFromPage = (): void => {
      page.win.postMessage({ type: HOOK_REPORT_MESSAGE, reason: HOOK_REASON_DID_NOT_TAKE }, ORIGIN);
      page.pump();
    };
    const didNotTakeRelays = (): number =>
      hookStatusReports(sent).filter((message) => message.reason === HOOK_REASON_DID_NOT_TAKE).length;

    reportFromPage();
    expect(didNotTakeRelays()).toBe(1);

    // 🔴 A second one in the same breath is dropped. The page world can post this
    //    message as often as it likes, and every relayed one is a storage read and
    //    write, so without a floor a page could turn its own record into a write
    //    loop. The floor is the hook's own check interval, so the honest cadence
    //    and the abusive one are the same rate.
    reportFromPage();
    expect(didNotTakeRelays()).toBe(1);

    // 🔴 And after one check interval it goes out again. This is what keeps a
    //    page that is *still* broken audible on its own origin: a later top frame
    //    whose hook verified removes the origin's record (`lib/hook-status.ts`,
    //    newest top-frame word wins), so a repeated observation is what keeps a
    //    still-broken tab current when another tab on the same origin verifies.
    //    The record itself merges the repeat into one row — what repeats is the
    //    observation, and it carries a fresh time.
    await settle(page, HOOK_SELF_CHECK_INTERVAL_MS + 1);
    reportFromPage();
    expect(didNotTakeRelays()).toBe(2);
  });
});

/**
 * W46 · **A frame's observation is a statement about that frame.**
 *
 * The record is keyed by origin because the user needs one place to look, but
 * a positive report from one document is not evidence that another document's
 * hook is healthy. Only the top frame speaks for the origin: the bridge drops
 * every other frame's report, so `reason: null` from a healthy iframe cannot
 * `remove` a failure the main document wrote down.
 *
 * These two cases are the unit-level form of `e2e/hook-origin-authority.spec.ts`.
 * Deleting `if (!isTopFrame()) return;` in `sendHookStatus` turns both red.
 */
describe('W46 · only the top frame speaks for an origin', () => {
  it('🔴 a healthy subframe does not send the positive observation', async () => {
    const page = makeFakePage(await pageHook(), { nested: true });
    const sent = stubRuntime();

    const mainMod: any = await import('../entrypoints/dw-fetch-main.content');
    mainMod.default.main();
    await loadBridge();
    await settle(page);

    // The hook answered — the child is whole. On the unfixed tree that answer
    // is forwarded as `reason: null` and would clear the origin's record.
    expect(page.posted.some((entry) => entry.data?.type === MAIN_READY_MESSAGE)).toBe(true);
    expect(hookStatusReports(sent)).toEqual([]);
  });

  it('🔴 a subframe whose hook failed does not file a failure for the origin either', async () => {
    const page = makeFakePage(await pageHook(), { nested: true });
    page.trustedTypes = true;
    const sent = stubRuntime();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => { /* asserted by absence of a report */ });

    await loadBridge();
    await settle(page, 1_000);

    // The fallback was refused in this frame. That is a fact about this frame,
    // not about the origin the user is looking at.
    expect(hookStatusReports(sent)).toEqual([]);
    warn.mockRestore();
  });
});
