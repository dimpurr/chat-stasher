/**
 * C32-COLDSTART · The cold-start sentence "what do you have to do before it starts".
 *
 * The scene on a real machine: a new user turns the switch on and also opens a page on a
 * supported platform, so the fetch channel is up (transportWired = true) — but the backfill
 * target registry is empty, because registration only happens on the kick from "the live leg
 * really archived one conversation once".
 *
 * 🔴 This is **deliberate design**, not a bug (lib/backfill/alarm.ts:80-87):
 *    when the alarm wakes, the SW is brand new with no tab and no account; the answer is not to
 *    guess one, but to use only "the account the user really did use". docs-dev/privacy.md:112 —
 *    the extension has no host permissions and fetches only inside the user's own login.
 * ⇒ So this file **touches no decision logic** and pins one thing: that guidance sentence has to
 *   exist, has to spell out the concrete action and the reason, and **must not fire on a healthy
 *   machine**.
 *
 * Zero network and zero logged-in state throughout: only a fake browser.* and pure functions.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';

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
 * Compute the popup down the real machine's chain: switch → target registry → tickBlockReason → renderPopup.
 * 🔴 It uses the **same** tickBlockReason entrypoints/popup/main.ts calls, and does not write a
 *    second gate decision in the test — a second copy would be marking its own homework.
 */
async function popupNow(opts: { enabled: boolean; hasHttp: boolean; hasTargets: boolean }) {
  const { browserLocalStore } = await import('../lib/backfill/store');
  const { setBackfillEnabled, tickBlockReason } = await import('../lib/backfill/schedule');
  const { renderPopup, popupText, NO_FAILURES } = await import('../lib/popup-view');

  await setBackfillEnabled(browserLocalStore(), opts.enabled);
  if (opts.hasTargets) {
    store['cs_backfill_targets_v1'] = [
      { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'acct-fixture-1', at: 1 },
    ];
  }
  const { loadTargets } = await import('../lib/backfill/alarm');
  const targets = await loadTargets(browserLocalStore());

  const block = await tickBlockReason({
    hasStore: true,
    isEnabled: () => opts.enabled,
    isHostPaused: () => false,
    hasHttp: opts.hasHttp,
    hasTargets: targets.length > 0,
  });

  const view = renderPopup({
    enabled: opts.enabled,
    block,
    state: null,
    target: null,
    failures: NO_FAILURES,
    lastTick: null,
  });
  return { block, view, text: popupText(view) };
}

/** The **concrete actions** the guidance must spell out. The wording may change; these must not vanish. */
const ACTION_PHRASES = ['send a message', 'open a conversation'];
/** The **reason** the guidance must spell out — it restores what looks like a defect into a privacy promise. */
const REASON_PHRASES = ['do not guess your account', 'host permissions'];
/** 🔴 Time promises that must never appear: we have no rate model, and inventing one is a lie. */
const FORBIDDEN_PROMISES = ['within minutes', 'in a few minutes', 'shortly', 'soon', 'starts right away', 'estimated', 'time remaining'];

describe('C32-COLDSTART · with a channel and no target, the whole thing has to be said', () => {
  it('🔴 the counter-proof: transport up and not one target ⇒ it must spell out the one thing to do and why', async () => {
    const { block, view, text } = await popupNow({
      enabled: true, hasHttp: true, hasTargets: false,
    });
    console.log('[C32-EVIDENCE cold start · channel but no target]\n' + text);

    // The premise is unchanged: this is still 'no-targets', and not one character of the gate decision moved.
    expect(block).toBe('no-targets');
    expect(view.running).toContain('NOT running');
    expect(text).not.toContain('Running: archiving');

    // 1 · The concrete action. A platitude like "please wait" does not count.
    expect(view.missing).not.toBeNull();
    for (const phrase of ACTION_PHRASES) expect(view.missing!).toContain(phrase);

    // 2 · Why it has to be this way — a privacy promise, not a "system limitation".
    for (const phrase of REASON_PHRASES) expect(view.missing!).toContain(phrase);

    // 3 · No promising what we cannot do.
    for (const promise of FORBIDDEN_PROMISES) expect(text).not.toContain(promise);
    expect(text).not.toContain('%');
  });

  it('🟢 healthy-path guard: with a target really present, that guidance **may not appear at all**', async () => {
    const { block, view, text } = await popupNow({
      enabled: true, hasHttp: true, hasTargets: true,
    });
    console.log('[C32-EVIDENCE healthy machine · target and channel]\n' + text);

    expect(block).toBeNull();
    expect(view.missing).toBeNull();
    // 🔴 This repository's rule: a newly added hint is verified once on a healthy machine for "it stays quiet".
    // Otherwise it becomes permanent noise, and by the time the user really needs it they have stopped reading.
    for (const phrase of [...ACTION_PHRASES, ...REASON_PHRASES]) {
      expect(text).not.toContain(phrase);
    }
  });

  it('🟢 guard two: stuck on another gate, this sentence is not allowed either — it would be the **wrong** advice', async () => {
    // The switch is off: what is needed here is to turn the switch on, not to send a message.
    const off = await popupNow({ enabled: false, hasHttp: true, hasTargets: false });
    expect(off.block).toBe('disabled');
    for (const phrase of [...ACTION_PHRASES, ...REASON_PHRASES]) {
      expect(off.text).not.toContain(phrase);
    }

    // A target but no channel: what is needed here is to leave a platform page open, not to send a message.
    const noPort = await popupNow({ enabled: true, hasHttp: false, hasTargets: true });
    expect(noPort.block).toBe('no-http-port');
    for (const phrase of REASON_PHRASES) expect(noPort.text).not.toContain(phrase);
  });
});
