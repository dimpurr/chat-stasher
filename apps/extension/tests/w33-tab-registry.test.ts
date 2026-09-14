/**
 * W33 · **A repeat tab hello costs nothing, and a hello that carries news still
 * gets through.**
 *
 * ## The defect this file pins
 * W27 made the content script repeat its hello every 4–6 minutes per open platform
 * tab (`lib/backfill/tab-hello.ts`). Its handler in background calls `rememberTab`,
 * which read the whole registry out of `storage.local`, rebuilt it and wrote it
 * back — so an idle browser with three chat tabs open paid a read-modify-write of
 * the same unchanged rows every few minutes, forever.
 *
 * The rule now (lib/backfill/tab-port.ts, `REGISTRY_WRITE_SKIP_MS`): a hello whose
 * tab is already registered, from the same origin, younger than half the hello's
 * floor interval, is a **no-op** — answered `{ok:true}` without touching storage,
 * because the registry already says everything that hello would say. A hello from
 * an unknown tab, from a changed origin, or one that arrives after the window
 * writes at once: those are the ones that carry news.
 *
 * 🔴 What is asserted, and what is deliberately not:
 *   · The decision is made against an in-memory mirror of the registry, so a no-op
 *     costs no read either. These tests count **writes on the store** and, for the
 *     restart case, prove the decision still comes from storage.
 *   · The mirror must never outlive its usefulness: `forgetTab` and a failed-ping
 *     strike both write the registry, and both must leave the mirror agreeing with
 *     what was written — otherwise a lost tab could not re-register. That is pinned
 *     here (`a tab that was forgotten ...`).
 *   · The mirror is per store. Two stores are two registries; answering one from
 *     the other's rows would be a wrong write, not a saved read (`another store's
 *     mirror is never consulted`).
 *   · No timers, no network, no browser profile: the store is the in-memory port
 *     with a write counter, and `at` is passed in rather than read from a clock.
 */

import { describe, it, expect } from 'vitest';
import { memoryStore } from '../lib/backfill/store';
import {
  BACKFILL_TABS_KEY,
  forgetTab,
  loadTabs,
  rememberTab,
  resetTabRegistryMirrorForTest,
  MAX_TAB_ENTRIES,
} from '../lib/backfill/tab-port';
import { TAB_HELLO_MIN_INTERVAL_MS } from '../lib/backfill/tab-hello';

const ORIGIN = 'https://chatgpt.com';
const OTHER_ORIGIN = 'https://chat.deepseek.com';
const TAB = 7;

/** The window the registry uses, restated here so a change to it has to be deliberate. */
const SKIP_WINDOW_MS = TAB_HELLO_MIN_INTERVAL_MS / 2;

/** A plausible wall-clock stamp; `at` is always passed in, never read from a clock here. */
const T0 = 1_700_000_000_000;

describe('W33-A · a repeat hello inside the window is a no-op', () => {
  it('🔴 the second hello rewrites nothing, and the first one still did', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 });
    expect(store.writes, 'the first hello registers the tab, so it writes').toBe(1);

    const returned = await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 + SKIP_WINDOW_MS - 1 });
    expect(store.writes, 'a repeat hello inside the window is not a registry change').toBe(1);
    // It still answers with the registry, so a caller sees the tab it just announced.
    expect(returned.map((t) => t.tabId)).toEqual([TAB]);
    expect((await loadTabs(store)).map((t) => t.tabId)).toEqual([TAB]);

    // And it is a repeat, not a one-off: five more in the same window change nothing.
    for (let i = 0; i < 5; i += 1) {
      await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 + 1000 + i });
    }
    expect(store.writes, 'no number of hellos inside the window costs a write').toBe(1);
  });

  it('🔴 exactly at the window it writes again — the boundary is the constant, not a guess', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 });
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 + SKIP_WINDOW_MS });
    expect(store.writes, 'the row is no longer young, so the hello refreshes it').toBe(2);
    expect(SKIP_WINDOW_MS).toBeLessThan(TAB_HELLO_MIN_INTERVAL_MS);
  });

  it('the registry is capped and ordered exactly as before, on the path that does write', async () => {
    const store = memoryStore();
    for (let i = 0; i < MAX_TAB_ENTRIES + 3; i += 1) {
      // Distinct tabs and distinct origins: each one is news, so each one writes.
      await rememberTab(store, { tabId: i + 1, origin: ORIGIN, at: T0 });
    }
    const rows = await loadTabs(store);
    expect(rows).toHaveLength(MAX_TAB_ENTRIES);
    expect(rows[0]!.tabId, 'most recent first').toBe(MAX_TAB_ENTRIES + 3);
  });
});

describe('W33-B · a hello that carries news writes at once', () => {
  it('🔴 an unknown tab at the same instant is not mistaken for a repeat', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 });
    await rememberTab(store, { tabId: TAB + 1, origin: ORIGIN, at: T0 });
    expect(store.writes).toBe(2);
    expect((await loadTabs(store)).map((t) => t.tabId)).toEqual([TAB + 1, TAB]);
  });

  it('🔴 the same tab on a changed origin writes at once', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 });
    await rememberTab(store, { tabId: TAB, origin: OTHER_ORIGIN, at: T0 + 1 });
    expect(store.writes, 'a moved tab is news even inside the window').toBe(2);
    expect((await loadTabs(store))[0]!.origin).toBe(OTHER_ORIGIN);
  });

  it('🔴 a tab that was forgotten re-registers on the next hello, however soon it comes', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 });
    await forgetTab(store, TAB);
    expect(await loadTabs(store)).toEqual([]);

    // Well inside the window: the mirror would still be holding the old row if the
    // forget had not written through it, and this hello would be skipped — leaving
    // the tab unregistered, which is exactly the W27 failure returning.
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 + 1000 });
    expect((await loadTabs(store)).map((t) => t.tabId), 'forgotten, then back').toEqual([TAB]);
  });
});

describe('W33-C · storage stays the source of truth', () => {
  it('🔴 a worker restart decides from storage, not from an empty memory', async () => {
    const store = memoryStore();
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 });
    const writesBefore = store.writes;

    // A reclaimed worker: module state is gone, storage.local is not.
    resetTabRegistryMirrorForTest();
    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 + 1000 });
    expect(store.writes, 'the young row is still in storage, so the hello is still a repeat').toBe(writesBefore);
    expect((await loadTabs(store)).map((t) => t.tabId)).toEqual([TAB]);
  });

  it('🔴 a cold worker reads a row another worker wrote, and does not write over it', async () => {
    // Seeded as if a previous worker had registered the tab and been reclaimed.
    const store = memoryStore({ [BACKFILL_TABS_KEY]: [{ tabId: TAB, origin: ORIGIN, at: T0 }] });
    resetTabRegistryMirrorForTest();

    await rememberTab(store, { tabId: TAB, origin: ORIGIN, at: T0 + 500 });
    expect(store.writes, 'nothing was rewritten: the row is young and this tab is the same tab').toBe(0);
  });

  it("🔴 another store's mirror is never consulted", async () => {
    const first = memoryStore();
    await rememberTab(first, { tabId: TAB, origin: ORIGIN, at: T0 });
    expect(first.writes).toBe(1);

    // A second, empty registry. The tab is unknown *here*, however well the first
    // store's mirror knows it.
    const second = memoryStore();
    await rememberTab(second, { tabId: TAB, origin: ORIGIN, at: T0 });
    expect(second.writes, 'an unknown tab in this registry is a write here').toBe(1);
    expect((await loadTabs(second)).map((t) => t.tabId)).toEqual([TAB]);
  });
});
