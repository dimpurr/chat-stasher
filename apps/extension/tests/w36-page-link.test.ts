/**
 * W36 · The page's dead link to the extension, and the difference between it and
 * a sleeping worker.
 *
 * Measured in a real Chromium (W36's e2e suite, `e2e/reload-stale-page.spec.ts`):
 * after `chrome.runtime.reload()` on a page left open, the page's `window.fetch`
 * is **still the previous build's wrapper**, the page's request still succeeds,
 * and the capture is dropped against a context that no longer exists — with no
 * outbox entry and no console line. The empty `catch` that produced that silence
 * is what this file's subject replaces: the drop is still not the page's problem,
 * but it is no longer nobody's.
 *
 * The distinction the whole file rests on, and why an unknown failure is not it:
 *
 *   · `Extension context invalidated` — this document's scripts belong to an
 *     extension that is gone. Only a page reload recovers it, and until then
 *     every capture from this document is lost. **This must be said.**
 *   · `Could not establish connection. Receiving end does not exist.` — the
 *     ordinary case: the worker is asleep and is woken by the message. The
 *     content script is right to say nothing, and the hello repeats anyway
 *     (lib/backfill/tab-hello.ts).
 *   · anything else — an unknown. Answered "quiet", which is what every caller
 *     did before this existed: a shape this module does not recognise must not be
 *     able to make the extension noisier than it was.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  createStaleLinkWarningGate,
  isInvalidatedContextError,
  STALE_PAGE_LINK_WARNING,
} from '../lib/page-link';
import { CAPTURE_MESSAGE, PLATFORMS } from '../lib/contract';
import { withI18n } from './i18n-harness';

afterEach(() => {
  vi.restoreAllMocks();
});

/** What Chrome throws, in the shape `runtime.sendMessage` rejects with. */
function invalidated(): Error {
  return new Error('Extension context invalidated.');
}

describe('W36 · a stale page link is a named fact, and a quiet one is not a fact', () => {
  it('recognises the invalidated-context failure, in the shapes it arrives in', () => {
    expect(isInvalidatedContextError(invalidated())).toBe(true);
    // A raw string is also a shape a rejection can carry.
    expect(isInvalidatedContextError('Uncaught Error: Extension context invalidated.')).toBe(true);

    // 🔴 The ordinary case, which must stay quiet.
    expect(isInvalidatedContextError(
      new Error('Could not establish connection. Receiving end does not exist.'),
    )).toBe(false);
    // Unknowns are quiet, deliberately.
    expect(isInvalidatedContextError(new Error('something else entirely'))).toBe(false);
    expect(isInvalidatedContextError(undefined)).toBe(false);
    expect(isInvalidatedContextError(null)).toBe(false);
    expect(isInvalidatedContextError({})).toBe(false);
  });

  it('says it once, and only for a stale link', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const gate = createStaleLinkWarningGate();

    // A sleeping worker, twice: nothing is said.
    expect(gate(new Error('Could not establish connection. Receiving end does not exist.'))).toBe(false);
    expect(gate(new Error('Could not establish connection. Receiving end does not exist.'))).toBe(false);
    expect(warn).not.toHaveBeenCalled();

    // The stale link: said, with the fixed metadata-only line.
    expect(gate(invalidated())).toBe(true);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn).toHaveBeenCalledWith(STALE_PAGE_LINK_WARNING);

    // 🔴 Two occasions can name the same fact (a lost delivery, then the periodic
    //    hello failing for the same reason). A page left open for a week must not
    //    become a wall of identical lines.
    expect(gate(invalidated())).toBe(false);
    expect(gate(new Error('Extension context invalidated.'))).toBe(false);
    expect(warn).toHaveBeenCalledTimes(1);
  });

  it('the warning carries no page, request or conversation data', () => {
    expect(STALE_PAGE_LINK_WARNING).toBe(
      '[chat-stasher] this page’s link to the extension is stale'
      + ' (the extension was reloaded or updated); reload this page to capture again',
    );
    // The fixed line is the only thing said, so nothing captured can reach it.
    expect(STALE_PAGE_LINK_WARNING).not.toContain('http');
    expect(STALE_PAGE_LINK_WARNING).not.toContain('chat-stasher/inbox');
  });
});

// ---------------------------------------------------------------------------
// The wiring: the **bridge** is what has to call the gate, and the bridge is
// where the empty catch used to be. A unit test and not an e2e one, because the
// isolated world's console is not visible to the e2e driver (measured: a marker
// logged by the bridge at document_start never reaches `page.on('console')`),
// while here the whole path — page message → bridge → `sendMessage` → rejection
// → warning — runs against a fake runtime that rejects the way Chrome does.
// ---------------------------------------------------------------------------

const ORIGIN = 'https://chatgpt.com';

/** The one capture payload the bridge will relay: it must pass isCapturedFetchShape. */
function capturePayload(): Record<string, unknown> {
  return {
    url: `${ORIGIN}/backend-api/conversation/a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: { n1: { id: 'n1' } }, current_node: 'n1' }),
    pageUrl: `${ORIGIN}/c/a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d`,
    capturedAt: Date.now(),
  };
}

/**
 * The bridge, loaded against a fake page world and a fake runtime that can be
 * made to reject **for one message kind at a time** — because the two occasions
 * this file is about go through the same `sendMessage` and a fake that fails both
 * at once would let either path's fix answer for the other's test.
 */
async function loadBridgeWithRuntime(reject: {
  /** The capture delivery (`chat-captured`) fails with this. */
  delivery?: Error;
  /** The tab hello fails with this. */
  hello?: Error;
}): Promise<{ posted: unknown[]; deliver(): Promise<void> }> {
  const listeners: Array<(event: { source: unknown; origin: string; data: unknown }) => void> = [];
  const posted: unknown[] = [];
  const win = {
    location: { origin: ORIGIN, href: `${ORIGIN}/c/x` },
    fetch: async () => new Response('{}', { status: 200 }),
    addEventListener(name: string, fn: (event: never) => void) {
      if (name === 'message') listeners.push(fn as never);
    },
    postMessage(data: unknown) { posted.push(data); },
  };
  vi.stubGlobal('window', win);
  vi.stubGlobal('document', {
    documentElement: { appendChild() { /* the fallback script is not under test */ } },
    createElement: () => ({ textContent: '', remove() { /* detached again */ } }),
    visibilityState: 'visible' as const,
    addEventListener() { /* the visibility listener is not what this file tests */ },
  });
  const api = withI18n({
    runtime: {
      id: 'mock-extension-id',
      onMessage: { addListener() { /* the backfill port, not this path */ } },
      async sendMessage(message: { type?: string }) {
        if (message?.type === 'chat-captured' && reject.delivery) throw reject.delivery;
        if (message?.type !== 'chat-captured' && reject.hello) throw reject.hello;
        return undefined;
      },
    },
  });
  vi.stubGlobal('browser', api);
  vi.stubGlobal('chrome', api);

  const mod = await import('../entrypoints/dw-bridge.content');
  (mod.default as { main(): void }).main();
  // The load-time hello is announced during `main()`, so let it settle before a
  // case starts measuring.
  for (let i = 0; i < 5; i += 1) await Promise.resolve();

  return {
    posted,
    async deliver(): Promise<void> {
      // The bridge's handler is registered on `window`'s message event; deliver
      // the page's own message the way the browser does (a task later).
      for (const fn of [...listeners]) {
        fn({ source: win, origin: ORIGIN, data: { type: CAPTURE_MESSAGE, payload: capturePayload() } });
      }
      // Let the delivery promise chain settle.
      for (let i = 0; i < 5; i += 1) await Promise.resolve();
    },
  };
}

function staleWarnings(warn: { mock: { calls: unknown[][] } }): unknown[][] {
  return warn.mock.calls.filter((call) => call[0] === STALE_PAGE_LINK_WARNING);
}

beforeEach(() => {
  vi.resetModules();
  vi.stubGlobal('defineContentScript', (config: unknown) => config);
  vi.stubGlobal('defineBackground', (callback: unknown) => callback);
});

describe('W36 · the bridge names a delivery it could not make', () => {
  it('a capture dropped against an invalidated context is said out loud, once', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    const bridge = await loadBridgeWithRuntime({
      delivery: new Error('Extension context invalidated.'),
    });
    expect(staleWarnings(warn)).toHaveLength(0);   // the hello succeeded; nothing to say yet
    await bridge.deliver();

    const warnings = staleWarnings(warn);
    expect(warnings).toHaveLength(1);
    // Metadata only: the call carries the fixed line and nothing else.
    expect(warnings[0]).toHaveLength(1);

    // A second capture on the same stale page does not repeat it.
    await bridge.deliver();
    expect(staleWarnings(warn)).toHaveLength(1);
  });

  it('a page with no traffic still finds out: the periodic hello names it too', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    // The load-time hello fails because the context is gone — which is the state
    // a page left open across a reload is in even when nothing is ever captured
    // on it again (the W27 defect, seen from the other side).
    await loadBridgeWithRuntime({ hello: new Error('Extension context invalidated.') });

    expect(staleWarnings(warn)).toHaveLength(1);
  });

  it('an ordinary failure stays quiet — a sleeping worker is not a fact', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    const asleep = new Error('Could not establish connection. Receiving end does not exist.');
    const bridge = await loadBridgeWithRuntime({ delivery: asleep, hello: asleep });
    await bridge.deliver();

    expect(staleWarnings(warn)).toHaveLength(0);
  });

  it('a delivery that succeeds says nothing at all', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    const bridge = await loadBridgeWithRuntime({});
    await bridge.deliver();

    expect(staleWarnings(warn)).toHaveLength(0);
  });
});
