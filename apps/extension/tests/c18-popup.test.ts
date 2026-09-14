/**
 * C18 · The popup — the first entry point to the backfill leg a user can really open.
 *
 * Three things are verified here, none of them vaguely:
 *  1. turning the switch on **persists**: after a browser restart (module state cleared, storage
 *     kept) it is still on;
 *  2. 🔴 with the switch on but no fetch channel, the wording must say **NOT running** and spell
 *     out what is missing, and **the whole text must contain no '%' character**; nothing like
 *     "archiving" may appear;
 *  3. in the paused state an alert appears, and the switch **stays operable**.
 *
 * Zero network and zero logged-in state throughout: only a fake browser.* and pure functions.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import type { BackfillHeader } from '../lib/backfill/types';
import { stateKey } from '../lib/backfill/types';

// ---------------------------------------------------------------------------
// A fake browser. Storage survives vi.resetModules() — that is the model of a "browser restart".
// ---------------------------------------------------------------------------
const store: Record<string, unknown> = {};

const fakeBrowser: any = {
  runtime: { id: 'mock-extension-id' },
  storage: {
    local: {
      async get(query: Record<string, unknown> | null) {
        if (query === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(query)) out[k] = k in store ? store[k] : query[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
};

beforeEach(() => {
  for (const k of Object.keys(store)) delete store[k];
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
});

/**
 * A debt set that "has run a little, but the API gave no total" — exactly the kind
 * that may not show a percentage.
 *
 * 🔴 W18 · What goes into `storage.local` at `stateKey(...)` is now the **header**
 *    (the cursor, the counters, the halt record) and no longer the ids: those live
 *    in IndexedDB (`lib/backfill/debt-store.ts`). The three owed and two archived
 *    conversations are still the fixture's facts — they are just carried as the
 *    two numbers the header holds, which is what the popup has always been able to
 *    see without opening the debt store.
 */
function seedState(platform = 'chatgpt', scope = 'acct-1'): BackfillHeader {
  const state: BackfillHeader = {
    v: 2,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 40, complete: false },
    pendingCount: 3,
    archivedCount: 2,
    detailToday: { day: '2026-08-17', count: 2 },
    halted: null,
  };
  store[stateKey(platform, scope)] = state;
  return state;
}

// ---------------------------------------------------------------------------
// 1 · Switch persistence
// ---------------------------------------------------------------------------
describe('C18-1 · turning the switch on persists, and it is still on after a restart', () => {
  it('off by default; after setBackfillEnabled(true), reloading the module still reads on', async () => {
    const first = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');

    // 🔴 The default itself was not changed.
    expect(first.BACKFILL_DEFAULT_ENABLED).toBe(false);
    expect(await first.isBackfillEnabled(browserLocalStore())).toBe(false);

    // That one click in the popup does exactly this.
    await first.setBackfillEnabled(browserLocalStore(), true);
    expect(store[first.BACKFILL_ENABLED_KEY]).toBe(true);

    // "A browser restart": module state cleared, storage.local kept.
    vi.resetModules();
    const second = await import('../lib/backfill/schedule');
    const { browserLocalStore: store2 } = await import('../lib/backfill/store');
    expect(await second.isBackfillEnabled(store2())).toBe(true);

    // Turning it off has to persist immediately too.
    await second.setBackfillEnabled(store2(), false);
    vi.resetModules();
    const third = await import('../lib/backfill/schedule');
    const { browserLocalStore: store3 } = await import('../lib/backfill/store');
    expect(await third.isBackfillEnabled(store3())).toBe(false);
  });

  it('🔴 it can persist in the Chrome shape too (only globalThis.chrome, no browser)', async () => {
    vi.unstubAllGlobals();
    vi.stubGlobal('chrome', fakeBrowser); // browser is deliberately not stubbed — this is Chrome MV3
    vi.resetModules();
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { setBackfillEnabled, isBackfillEnabled } = await import('../lib/backfill/schedule');
    expect(browserLocalStore()).not.toBeNull();
    await setBackfillEnabled(browserLocalStore(), true);
    expect(await isBackfillEnabled(browserLocalStore())).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 2 · 🔴 on, but with no port
// ---------------------------------------------------------------------------
describe('C18-2 · on, but the fetch channel is not connected', () => {
  it('the wording must say "NOT running", spell out what is missing, and the whole text must contain no percent sign', async () => {
    seedState();
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const { setBackfillEnabled, isBackfillEnabled, tickBlockReason } =
      await import('../lib/backfill/schedule');
    const { renderPopup, popupText, pickBackfillState, collectFailures } = await import('../lib/popup-view');

    // The user turned the switch on in the popup.
    await setBackfillEnabled(browserLocalStore(), true);

    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => isBackfillEnabled(browserLocalStore()),
      isHostPaused: () => false,
      // 🔴 This is the value in a production build: no code injects an http port.
      hasHttp: false,
    });
    expect(block).toBe('no-http-port');

    const snapshot = await browserLocalSnapshot();
    const state = pickBackfillState(snapshot);
    const view = renderPopup({
      enabled: true,
      block,
      state,
      target: state ? { platform: state.platform, scope: state.scope } : null,
      // C20: it goes through the real aggregation function rather than a hardcoded value — this case's fixture has no failures, so it is empty.
      failures: collectFailures(snapshot),
    });
    const out = popupText(view);

    // 🔴 The three criteria.
    expect(view.running).toContain('NOT running');
    expect(view.missing).not.toBeNull();
    expect(view.missing!).toContain('fetch channel');
    expect(out).not.toContain('%');

    // 🔴 The things that must never appear.
    for (const forbidden of ['Running: archiving', 'backfilling now', 'is now running', 'estimated remaining']) {
      expect(out).not.toContain(forbidden);
    }

    // The switch itself must honestly show as ON; it must not pretend to be off because nothing can run.
    expect(view.toggle.checked).toBe(true);
    expect(view.status).toContain('the switch is ON');
  });

  it('the gate decision is the same function tickBackfill uses — really running one tick reaches the same conclusion', async () => {
    seedState();
    const { browserLocalStore } = await import('../lib/backfill/store');
    const schedule = await import('../lib/backfill/schedule');
    await schedule.setBackfillEnabled(browserLocalStore(), true);
    schedule.resetTickLockForTest();

    const tick = await schedule.tickBackfill({
      store: browserLocalStore(),
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope: 'acct-1',
      // No http injection — consistent with a production build.
    });
    expect(tick).toEqual({ ran: false, reason: 'no-http-port', report: null });
  });

  it('with the switch off, the wording says "the switch is off" rather than a missing port', async () => {
    const { tickBlockReason } = await import('../lib/backfill/schedule');
    const { renderPopup, popupText, NO_FAILURES } = await import('../lib/popup-view');
    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => false,
      isHostPaused: () => false,
      hasHttp: false,
    });
    expect(block).toBe('disabled');
    const out = popupText(renderPopup({
      enabled: false, block, state: null, target: null, failures: NO_FAILURES,
    }));
    expect(out).toContain('NOT running');
    expect(out).toContain('the switch is off');
    expect(out).not.toContain('%');
  });
});

// ---------------------------------------------------------------------------
// 3 · The host-paused state (W2 replaced C12's download-stall state)
// ---------------------------------------------------------------------------
describe('C18-3 · the host-paused state', () => {
  it('it shows the pause line and a named reason, and the switch stays operable', async () => {
    seedState();
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const { setBackfillEnabled, isBackfillEnabled, tickBlockReason } =
      await import('../lib/backfill/schedule');
    const { HOST_PAUSE_KEY, HOST_UNAVAILABLE } = await import('../lib/host-status');
    const { renderPopup, popupText, pickBackfillState, collectFailures } = await import('../lib/popup-view');

    await setBackfillEnabled(browserLocalStore(), true);
    // Really record the pause: this is exactly the record the backfill leg writes when a delivery fails.
    store[HOST_PAUSE_KEY] = { reason: HOST_UNAVAILABLE, at: 1_700_000_000_000, detail: 'timeout' };
    const { loadHostPause } = await import('../lib/host-status');
    const pause = await loadHostPause(browserLocalStore());

    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => isBackfillEnabled(browserLocalStore()),
      isHostPaused: () => pause !== null,
      hasHttp: false,
    });
    expect(block).toBe('host-paused');

    const snapshot = await browserLocalSnapshot();
    const state = pickBackfillState(snapshot);
    const view = renderPopup({
      enabled: true, block, state,
      hostPause: pause,
      target: state ? { platform: state.platform, scope: state.scope } : null,
      failures: collectFailures(snapshot),
    });
    const out = popupText(view);

    expect(view.status).toContain('host is unreachable');
    expect(view.running).toContain('NOT running');
    // 🔴 A pause is a **named** outcome: it says which machine's host cannot be reached, that no debt was lost, and when it was noticed.
    expect(view.pause).not.toBeNull();
    expect(view.pause!).toContain('PAUSED');
    expect(view.pause!).toContain(HOST_UNAVAILABLE);
    expect(view.pause!).toContain('untouched');
    // 🔴 A pause does not take away the user's right to flip the switch.
    expect(view.toggle.disabled).toBe(false);
    expect(view.toggle.checked).toBe(true);
    expect(out).not.toContain('%');
  });
});

// ---------------------------------------------------------------------------
// 4 · The progress wording must come from C11; a second copy is not allowed
// ---------------------------------------------------------------------------
describe('C18-4 · the progress wording obeys C11\'s rules', () => {
  it('a percentage appears only with a trustworthy denominator, and it matches formatProgress byte for byte', async () => {
    const { renderPopup, NO_FAILURES } = await import('../lib/popup-view');
    const { formatProgress, progressOfHeader } = await import('../lib/backfill/progress');
    const trusted: BackfillHeader = {
      v: 2, platform: 'chatgpt', scope: 'acct-1',
      totalKnown: 10, totalSource: 'response-total',
      enumCursor: { offset: 10, complete: true },
      pendingCount: 1, archivedCount: 3,
      detailToday: { day: '2026-08-17', count: 3 }, halted: null,
    };
    const view = renderPopup({
      enabled: true, block: 'no-http-port', state: trusted,
      target: { platform: 'chatgpt', scope: 'acct-1' }, failures: NO_FAILURES,
    });
    expect(view.progress).toBe(`Progress: ${formatProgress(progressOfHeader(trusted))}`);
    expect(view.progress).toContain('30%');
  });

  it('with an untrustworthy denominator the progress line contains no percent sign', async () => {
    const { renderPopup, NO_FAILURES } = await import('../lib/popup-view');
    const untrusted: BackfillHeader = {
      v: 2, platform: 'chatgpt', scope: 'acct-1',
      totalKnown: null, totalSource: 'unknown',
      enumCursor: { offset: 0, complete: false },
      pendingCount: 1, archivedCount: 0,
      detailToday: { day: '', count: 0 }, halted: null,
    };
    const view = renderPopup({
      enabled: true, block: 'no-http-port', state: untrusted,
      target: { platform: 'chatgpt', scope: 'acct-1' }, failures: NO_FAILURES,
    });
    expect(view.progress).not.toContain('%');
    expect(view.progress).toContain('total unknown');
  });

  it('pickBackfillState accepts only sets whose key and value agree', async () => {
    const { pickBackfillState } = await import('../lib/popup-view');
    const good = seedState('chatgpt', 'acct-1');
    // A record under a key that names a different platform/scope than the record
    // itself does is never displayed...
    expect(pickBackfillState({ ...store, 'cs_backfill_v2:bogus:x': good })).toEqual(good);
    // ...and neither is a pre-W18 record (a whole state, ids and all) that happens
    // to sit under a v2 key: showing its stale numbers as live progress is the
    // "unknown recorded as known" mistake at the UI layer.
    expect(pickBackfillState({
      'cs_backfill_v2:chatgpt:acct-1': {
        v: 1, platform: 'chatgpt', scope: 'acct-1', totalKnown: null, totalSource: 'unknown',
        enumCursor: { offset: 0, complete: false }, pending: ['d1'], archived: [],
        detailToday: { day: '', count: 0 }, halted: null,
      },
    })).toBeNull();
    expect(pickBackfillState({ cs_count: 3 })).toBeNull();
    expect(pickBackfillState(null)).toBeNull();
  });
});
