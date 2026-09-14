// @vitest-environment jsdom
/**
 * W32 · "Open dashboard", pressed twice in one tick, in a **real DOM**.
 *
 * ## The defect this file exists for
 *
 * `onOpenDashboard` (entrypoints/popup/main.ts) disables the button as its
 * first statement — before the first `await` — precisely so that a second click
 * cannot start a second dashboard. §6.5 says the host *cannot* deduplicate one:
 * every `open_dashboard` it receives starts a new child process, and each one
 * lives until its own idle timeout. The guard is one statement, and nothing
 * tested it: move it below the `await` and every existing suite stays green.
 *
 * So this file drives the popup's own module, mounted on the popup's own
 * markup, with a host that answers late:
 *
 *   1. two clicks in the same tick ⇒ exactly one `open_dashboard` request on the
 *      wire, and at most one tab;
 *   2. the button is already disabled when the first click returns — that is
 *      the mechanism, asserted directly rather than inferred from (1);
 *   3. after a `nack`, the button is usable again and the note says why it
 *      failed (a control that fails silently would be the worse bug).
 *
 * ## Why jsdom, and why only here
 *
 * `vitest.config.ts` keeps `environment: 'node'` for the whole suite; the
 * docblock above selects jsdom for this file alone. A real DOM is the point:
 * `HTMLElement.click()` on a disabled control dispatches nothing (HTML's
 * "actually disabled" rule, verified against jsdom 30), which is what makes the
 * second click a no-op rather than a second request. A hand-rolled DOM stub
 * would have to implement that rule itself, and then the test would be asserting
 * its own stub instead of the browser's behaviour.
 *
 * `main.ts` is imported, not copied: the click listener this file pulls is the
 * one the shipped popup registers. Nothing here is a second implementation of
 * the guard.
 *
 * Zero network, zero logged-in state: a fake `browser.*` around the shared
 * synthetic host (tests/synthetic-native-host.ts), exactly as the other popup
 * suites do.
 */

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import {
  createSyntheticHost,
  SYNTHETIC_DASHBOARD_URL,
  type SyntheticHostOptions,
} from './synthetic-native-host';

const POPUP_HTML = readFileSync(
  resolve(__dirname, '..', 'entrypoints', 'popup', 'index.html'),
  'utf8',
);

/**
 * Put the popup's real body into the document.
 *
 * The `<script type="module">` tag is dropped: this test imports `main.ts`
 * itself, and it has to control when that happens (after the fakes are in
 * place). Everything else — every id `main.ts` reaches for — is the markup the
 * browser loads.
 */
function mountPopup(): void {
  const start = POPUP_HTML.indexOf('<body>') + '<body>'.length;
  const body = POPUP_HTML.slice(start, POPUP_HTML.lastIndexOf('</body>'));
  document.body.innerHTML = body.replace(/<script[\s\S]*?<\/script>/g, '');
}

/** `storage.local`, in memory. `get(null)` means "everything" (§ browserLocalSnapshot). */
function memoryArea() {
  const data: Record<string, unknown> = {};
  return {
    async get(query: Record<string, unknown> | null) {
      if (query === null) return { ...data };
      const out: Record<string, unknown> = {};
      for (const key of Object.keys(query)) out[key] = key in data ? data[key] : query[key];
      return out;
    },
    async set(values: Record<string, unknown>) {
      Object.assign(data, values);
    },
    async remove(keys: string | string[]) {
      for (const key of ([] as string[]).concat(keys)) delete data[key];
    },
  };
}

/** Let every already-queued microtask and macrotask turn run. */
async function settle(turns = 8): Promise<void> {
  for (let i = 0; i < turns; i += 1) await new Promise((r) => setTimeout(r, 0));
}

/** The same, but bounded and loud: a popup that never paints must not hang the suite. */
async function until(ready: () => boolean, what: string, turns = 200): Promise<void> {
  for (let i = 0; i < turns; i += 1) {
    if (ready()) return;
    await new Promise((r) => setTimeout(r, 0));
  }
  throw new Error(`timed out waiting for ${what}`);
}

interface Booted {
  host: ReturnType<typeof createSyntheticHost>;
  /** Every `browser.tabs.create` URL, in order. */
  opened: string[];
  /** Every request the popup **sent**, recorded before the host is allowed to answer. */
  sent: Array<Record<string, unknown>>;
  /** The `open_dashboard` requests among them. */
  dashboardRequests: () => Array<Record<string, unknown>>;
  button: HTMLButtonElement;
  note: HTMLElement;
  /** Let the host's held `open_dashboard` answer through. */
  release: () => void;
}

/**
 * Boot the popup against the popup's own markup and a synthetic host.
 *
 * 🔴 The host's §6.5 answer is **held** until `release()` — a real host launches
 *    a process and waits for the dashboard to print its URL, so a stub that
 *    answers in the same tick would let a broken guard look correct.
 *
 * The request is recorded **before** the gate: "how many `open_dashboard`
 * requests did the popup actually send" has to be answerable while the first one
 * is still in flight, which is exactly the moment the second click arrives.
 */
async function boot(options: SyntheticHostOptions = {}): Promise<Booted> {
  mountPopup();
  const host = createSyntheticHost(options);
  const opened: string[] = [];
  const sent: Array<Record<string, unknown>> = [];
  let release!: () => void;
  const held = new Promise<void>((r) => {
    release = r;
  });

  const fake = {
    runtime: {
      id: 'w32-dashboard-click-test',
      lastError: undefined,
      // Answers background's status query: the popup asks on open, and a
      // listener that never answers would leave the paint waiting.
      sendMessage: async () => ({
        transportWired: false,
        lastTickReason: null,
        liveTarget: null,
      }),
      sendNativeMessage: async (name: string, message: unknown) => {
        const request = message as Record<string, unknown>;
        sent.push(request);
        if (request.type === 'open_dashboard') await held;
        return host.sendNativeMessage(name, message);
      },
    },
    storage: { local: memoryArea() },
    tabs: {
      async create({ url }: { url: string }) {
        opened.push(url);
        return { id: 1 };
      },
    },
  };
  const browser = withI18n(fake);
  vi.stubGlobal('browser', browser);
  vi.stubGlobal('chrome', browser);

  // The import runs the popup: it attaches every listener in the markup above
  // and asks for the summary once (§6.4/§10), which is what enables the button.
  await import('../entrypoints/popup/main.ts');

  const button = document.getElementById('open-dashboard') as HTMLButtonElement;
  const note = document.getElementById('dashboard-note') as HTMLElement;
  // 🔴 The markup's button is enabled and empty. A paint writes the label, and
  //    the *first* paint disables the button (the summary is still unasked), so
  //    "labelled and enabled" means at least one paint has happened **and** the
  //    §6.4 answer has arrived. Waiting only for `!button.disabled` would return
  //    on the first check — the raw button is already enabled — and the click
  //    would then race the popup's own opening sequence, whose later repaint
  //    clears the note this test asserts on.
  await until(
    () => button.textContent.length > 0 && !button.disabled,
    'the popup to paint the button usable (the summary was answered)',
  );
  return {
    host,
    opened,
    sent,
    dashboardRequests: () => sent.filter((r) => r.type === 'open_dashboard'),
    button,
    note,
    release,
  };
}

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
});

afterEach(async () => {
  // A module imported by one test keeps running its own async chain; let it run
  // out before the next test replaces the body, so nothing here leaks forward.
  await settle();
});

describe('W32 · the dashboard button cannot be pressed twice into one tick', () => {
  /**
   * The mechanism, on its own — no await between the click and the assertion.
   * Moving the disable below the first `await` in `onOpenDashboard` fails here
   * and nowhere else, which is what makes this the guard's own test rather than
   * an inference from the request count below.
   */
  it('the button is already disabled when the first click returns', async () => {
    const { button } = await boot();

    expect(button.disabled).toBe(false);
    button.click();
    expect(button.disabled).toBe(true);
  });

  it('two clicks in the same tick send exactly one open_dashboard and open at most one tab', async () => {
    const { host, opened, dashboardRequests, button, release } = await boot();

    // Same tick, no await between them: the second click is the user's second
    // click arriving while the first request is still in flight.
    button.click();
    button.click();

    // Let the first request reach the host; the host has not answered yet.
    await settle(1);
    expect(dashboardRequests()).toHaveLength(1);
    expect(opened.length).toBe(0);

    release();
    await settle();

    expect(dashboardRequests()).toHaveLength(1);
    expect(host.dashboardCount()).toBe(1);
    expect(opened).toEqual([SYNTHETIC_DASHBOARD_URL]);
    // Back to a usable control once the round trip is over.
    expect(button.disabled).toBe(false);
  });

  it('a nack gives the button back, with the host reason on the note', async () => {
    const { host, opened, dashboardRequests, button, note, release } = await boot({
      dashboardNack: {
        kind: 'config',
        retryable: false,
        detail: 'no [native_host] destination',
      },
    });

    button.click();
    button.click();
    await settle(1);
    expect(dashboardRequests()).toHaveLength(1);

    release();
    await settle();

    expect(opened).toEqual([]);
    // 🔴 "Usable again" is the point: a `config` nack is fixed by editing the
    //    config, and the user must be able to press the button afterwards.
    expect(button.disabled).toBe(false);
    expect(note.hidden).toBe(false);
    expect(note.textContent).toContain('no [native_host] destination');
  });
});
