/**
 * W91b · **A stable build must not delete or surface a dev build's leftover
 * experimental state.**
 *
 * W91 made a stable build *ignore* Perplexity/Kimi rows: `loadTargets` /
 * `loadTabs` filter them out on read, and the engine/tick refuse an experimental
 * origin. But every write path then read that already-filtered list and saved it
 * back, so a stable write **deleted** the leftover row, and
 * `rememberTarget`'s `slice(0, MAX_TARGET_ENTRIES)` let an experimental row
 * consume (and be evicted from) a stable slot. The popup's
 * `registeredStateKeys` still fed the raw registry to `pickBackfillState` /
 * `collectFailures`, and clear-failures wrote over an experimental ledger.
 *
 * The fix is "filter on read, merge the leftover rows back on write". These
 * tests pin the invariant from the outside: after any stable write, the leftover
 * Perplexity/Kimi rows are still in `storage.local`, byte-for-byte, and a
 * dev→stable→dev round trip finds them again.
 *
 * 🔴 The suite pins the build channel to `dev` (`vitest.config.ts`), so this file
 *    mocks `currentReleaseChannel` to `'stable'` and drives the dev channel
 *    explicitly where the round trip needs it. Every write/read helper also takes
 *    an explicit `channel`, which is the production seam W91 added and W91b
 *    threads through the writers.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';

/*
 * Mock the active build channel to `stable`, which is what a real stable build
 * bakes in. The platform table and the read filters are the real ones; only the
 * "which channel am I" answer changes, so a stable write path exercises the same
 * filter a shipped stable build does.
 */
vi.mock('../lib/contract', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/contract')>();
  return { ...actual, currentReleaseChannel: () => 'stable' as const };
});

import {
  BACKFILL_TARGETS_KEY,
  MAX_TARGET_ENTRIES,
  forgetNonOrganizationTargets,
  forgetTarget,
  loadTargets,
  rememberOrganizationScopedTarget,
  rememberTarget,
} from '../lib/backfill/alarm';
import {
  BACKFILL_TABS_KEY,
  MAX_TAB_ENTRIES,
  forgetMissingTabs,
  forgetTab,
  loadTabs,
  pickLiveTab,
  rememberTab,
  resetTabRegistryMirrorForTest,
} from '../lib/backfill/tab-port';
import { backfillStateEntries, collectFailures, pickBackfillState } from '../lib/popup-view';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { headerOf, initialState, stateKey, type BackfillHeader } from '../lib/backfill/types';
import { clearFailures } from '../lib/backfill/failures';

const PERPLEXITY_ORIGIN = 'https://www.perplexity.ai';
const KIMI_ORIGIN = 'https://www.kimi.com';
const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';
const CHATGPT_ORIGIN = 'https://chatgpt.com';
const CLAUDE_ORIGIN = 'https://claude.ai';

/** A dev build's leftover rows, byte-for-byte what storage held before the stable write. */
const LEFTOVER_TARGET = { platform: 'perplexity', origin: PERPLEXITY_ORIGIN, scope: 'dev-acct', at: 11 };
const LEFTOVER_TAB = { tabId: 71, origin: PERPLEXITY_ORIGIN, at: 12, misses: 0 };

function stableTarget(platform: string, scope: string, at: number) {
  const origin = platform === 'deepseek' ? DEEPSEEK_ORIGIN : CHATGPT_ORIGIN;
  return { platform, origin, scope, at };
}

async function rawRows(store: BackfillStore, key: string): Promise<unknown[]> {
  const raw = await store.load(key);
  return Array.isArray(raw) ? raw : [];
}

/** Byte-for-byte: compare the serialized row, not just deep equality. */
function containsRowExact(rows: readonly unknown[], row: unknown): boolean {
  const want = JSON.stringify(row);
  return rows.some((r) => JSON.stringify(r) === want);
}

function stableHeader(platform: string, scope: string, archivedCount = 0): BackfillHeader {
  const state = initialState(platform, scope);
  state.archived = Array.from({ length: archivedCount }, (_, i) => `synthetic-${i}`);
  return headerOf(state);
}

beforeEach(() => {
  resetTabRegistryMirrorForTest();
});

describe('W91b-1 · every stable target write keeps the experimental row', () => {
  it('🔴 rememberTarget / forgetTarget / scoped writers preserve a leftover perplexity row', async () => {
    const operations: Array<{ name: string; run: (store: BackfillStore) => Promise<unknown> }> = [
      {
        name: 'rememberTarget',
        run: (s) => rememberTarget(s, stableTarget('deepseek', 'stable-new', 20), 'stable'),
      },
      {
        name: 'forgetTarget',
        run: (s) => forgetTarget(s, 'claude', 'default'),
      },
      {
        name: 'rememberOrganizationScopedTarget',
        run: (s) => rememberOrganizationScopedTarget(
          s,
          { platform: 'claude', origin: CLAUDE_ORIGIN, scope: 'org-1', at: 21 },
          (sc) => sc.startsWith('org-'),
          'stable',
        ),
      },
      {
        name: 'forgetNonOrganizationTargets',
        run: (s) => forgetNonOrganizationTargets(s, 'claude', (sc) => sc.startsWith('org-')),
      },
    ];

    for (const op of operations) {
      const store = memoryStore();
      await store.save(BACKFILL_TARGETS_KEY, [
        LEFTOVER_TARGET,
        { platform: 'claude', origin: CLAUDE_ORIGIN, scope: 'default', at: 1 },
      ]);

      await op.run(store);

      const rows = await rawRows(store, BACKFILL_TARGETS_KEY);
      expect(containsRowExact(rows, LEFTOVER_TARGET), `${op.name} dropped the leftover row`).toBe(true);
      // And the dev channel still finds it, in storage and through the reader.
      expect((await loadTargets(store, 'dev')).map((t) => t.platform)).toContain('perplexity');
    }
  });

  it('🔴 a full stable registry plus one new stable target does not evict the experimental row', async () => {
    const store = memoryStore();
    const fullStable = Array.from({ length: MAX_TARGET_ENTRIES }, (_, i) =>
      stableTarget(i % 2 === 0 ? 'deepseek' : 'chatgpt', `s${i}`, i + 1));
    await store.save(BACKFILL_TARGETS_KEY, [LEFTOVER_TARGET, ...fullStable]);

    await rememberTarget(store, stableTarget('deepseek', 'brand-new', 100), 'stable');

    const rows = await rawRows(store, BACKFILL_TARGETS_KEY);
    // The experimental row neither consumed a stable slot nor was evicted.
    expect(containsRowExact(rows, LEFTOVER_TARGET)).toBe(true);
    expect(rows).toHaveLength(MAX_TARGET_ENTRIES + 1);
    const active = await loadTargets(store, 'stable');
    expect(active).toHaveLength(MAX_TARGET_ENTRIES);
    expect(active.some((t) => t.scope === 'brand-new')).toBe(true);
    expect(active.some((t) => t.platform === 'perplexity')).toBe(false);
  });
});

describe('W91b-2 · every stable tab write keeps the experimental row', () => {
  it('🔴 rememberTab / forgetTab / setTabMisses / forgetMissingTabs preserve a leftover perplexity tab', async () => {
    const operations: Array<{ name: string; run: (store: BackfillStore) => Promise<unknown> }> = [
      {
        name: 'rememberTab',
        run: (s) => rememberTab(s, { tabId: 90, origin: DEEPSEEK_ORIGIN, at: 30 }, 'stable'),
      },
      {
        name: 'forgetTab',
        run: (s) => forgetTab(s, 8, 'stable'),
      },
      {
        name: 'forgetMissingTabs',
        run: (s) => forgetMissingTabs(s, new Set([8]), 'stable'),
      },
      {
        // A single failed ping is `setTabMisses`; the tab survives with misses=1.
        name: 'setTabMisses (via pickLiveTab, one miss)',
        run: (s) => pickLiveTab(
          s,
          DEEPSEEK_ORIGIN,
          async () => { throw new Error('synthetic ping timeout'); },
          50,
          'stable',
        ),
      },
      {
        // Seeded with one miss already, so the failed ping is the second and
        // takes the `forgetTab` branch.
        name: 'forgetTab (via pickLiveTab, two misses)',
        run: async (s) => {
          const rows = await rawRows(s, BACKFILL_TABS_KEY);
          await s.save(BACKFILL_TABS_KEY, rows.map((r) =>
            (r as { tabId?: number }).tabId === 8 ? { ...(r as object), misses: 1 } : r));
          return pickLiveTab(s, DEEPSEEK_ORIGIN, async () => { throw new Error('synthetic ping timeout'); }, 50, 'stable');
        },
      },
    ];

    for (const op of operations) {
      const store = memoryStore();
      await store.save(BACKFILL_TABS_KEY, [LEFTOVER_TAB, { tabId: 8, origin: DEEPSEEK_ORIGIN, at: 2 }]);
      resetTabRegistryMirrorForTest();

      await op.run(store);

      const rows = await rawRows(store, BACKFILL_TABS_KEY);
      expect(containsRowExact(rows, LEFTOVER_TAB), `${op.name} dropped the leftover row`).toBe(true);
      expect((await loadTabs(store, 'dev')).map((t) => t.tabId)).toContain(LEFTOVER_TAB.tabId);
    }
  });

  it('🔴 a full stable tab registry plus one new stable tab does not evict the experimental row', async () => {
    const store = memoryStore();
    const fullStable = Array.from({ length: MAX_TAB_ENTRIES }, (_, i) => ({
      tabId: i + 1,
      origin: i % 2 === 0 ? DEEPSEEK_ORIGIN : CHATGPT_ORIGIN,
      at: i + 1,
    }));
    await store.save(BACKFILL_TABS_KEY, [LEFTOVER_TAB, ...fullStable]);

    await rememberTab(store, { tabId: 999, origin: DEEPSEEK_ORIGIN, at: 200 }, 'stable');

    const rows = await rawRows(store, BACKFILL_TABS_KEY);
    expect(containsRowExact(rows, LEFTOVER_TAB)).toBe(true);
    expect(rows).toHaveLength(MAX_TAB_ENTRIES + 1);
    const active = await loadTabs(store, 'stable');
    expect(active).toHaveLength(MAX_TAB_ENTRIES);
    expect(active.some((t) => t.tabId === 999)).toBe(true);
  });
});

describe('W91b-3 · the popup helpers omit experimental state in stable', () => {
  function snapshot(): Record<string, unknown> {
    const perp = stableHeader('perplexity', 'dev-acct', 99);
    perp.failures = [{ shortId: 'deadbeef', platform: 'perplexity', reason: 'not-saved', at: 5 }];
    const ds = stableHeader('deepseek', 'acct', 5);
    ds.failures = [{ shortId: 'cafebabe', platform: 'deepseek', reason: 'not-saved', at: 6 }];
    return {
      [stateKey('perplexity', 'dev-acct')]: perp,
      [stateKey('deepseek', 'acct')]: ds,
      [BACKFILL_TARGETS_KEY]: [
        LEFTOVER_TARGET,
        { platform: 'deepseek', origin: DEEPSEEK_ORIGIN, scope: 'acct', at: 1 },
      ],
    };
  }

  it('🔴 pickBackfillState / collectFailures / backfillStateEntries skip the experimental platform in stable', () => {
    const snap = snapshot();

    expect(pickBackfillState(snap, 'stable')?.platform).toBe('deepseek');
    expect(pickBackfillState(snap, 'dev')?.platform).toBe('perplexity');

    expect(collectFailures(snap, 'stable').entries.map((e) => e.platform)).toEqual(['deepseek']);
    // Dev aggregates both ledgers, newest first.
    expect(collectFailures(snap, 'dev').entries.map((e) => e.platform)).toEqual(['deepseek', 'perplexity']);

    expect(backfillStateEntries(snap, 'stable').map((e) => e.state.platform)).toEqual(['deepseek']);
    expect(backfillStateEntries(snap, 'dev').map((e) => e.state.platform).sort()).toEqual(['deepseek', 'perplexity']);
  });

  it('🔴 a registry holding only an experimental row does not fall back to showing its header', () => {
    const perp = stableHeader('perplexity', 'dev-acct', 7);
    const snap = {
      [stateKey('perplexity', 'dev-acct')]: perp,
      [BACKFILL_TARGETS_KEY]: [LEFTOVER_TARGET],
    };
    expect(pickBackfillState(snap, 'stable')).toBeNull();
    expect(backfillStateEntries(snap, 'stable')).toEqual([]);
    expect(pickBackfillState(snap, 'dev')?.platform).toBe('perplexity');
  });

  it('🔴 clear-failures in stable leaves an experimental ledger untouched', async () => {
    const store = memoryStore();
    const perp = stableHeader('perplexity', 'dev-acct', 3);
    perp.failures = [{ shortId: 'deadbeef', platform: 'perplexity', reason: 'not-saved', at: 5 }];
    const ds = stableHeader('deepseek', 'acct', 3);
    ds.failures = [{ shortId: 'cafebabe', platform: 'deepseek', reason: 'not-saved', at: 6 }];
    await store.save(stateKey('perplexity', 'dev-acct'), perp);
    await store.save(stateKey('deepseek', 'acct'), ds);

    // The popup's clear-failures loop, with the active channel passed in.
    const snap = JSON.parse(JSON.stringify(store.data)) as Record<string, unknown>;
    for (const { key, state } of backfillStateEntries(snap, 'stable')) {
      clearFailures(state);
      await store.save(key, state);
    }

    const perpAfter = await store.load(stateKey('perplexity', 'dev-acct')) as BackfillHeader;
    expect(perpAfter.failures).toHaveLength(1);
    expect(perpAfter.failuresDropped).toBe(0);
    const dsAfter = await store.load(stateKey('deepseek', 'acct')) as BackfillHeader;
    expect(dsAfter.failures).toHaveLength(0);
  });
});

describe('W91b-4 · dev → stable → dev round trip', () => {
  it('🔴 Perplexity and Kimi state is intact after a stable build ran', async () => {
    const store = memoryStore();

    // A dev build captures and queues state for both experimental platforms.
    resetTabRegistryMirrorForTest();
    await rememberTab(store, { tabId: 71, origin: PERPLEXITY_ORIGIN, at: 1 }, 'dev');
    await rememberTab(store, { tabId: 72, origin: KIMI_ORIGIN, at: 2 }, 'dev');
    await rememberTarget(store, { platform: 'perplexity', origin: PERPLEXITY_ORIGIN, scope: 'dev-acct', at: 1 }, 'dev');
    await rememberTarget(store, { platform: 'kimi', origin: KIMI_ORIGIN, scope: 'dev-acct', at: 2 }, 'dev');
    const perpHeader = stableHeader('perplexity', 'dev-acct', 4);
    perpHeader.failures = [{ shortId: 'deadbeef', platform: 'perplexity', reason: 'not-saved', at: 3 }];
    await store.save(stateKey('perplexity', 'dev-acct'), perpHeader);

    // The user switches to a stable build: it serves only its own platforms.
    resetTabRegistryMirrorForTest();
    await rememberTab(store, { tabId: 80, origin: DEEPSEEK_ORIGIN, at: 10 }, 'stable');
    await rememberTarget(store, stableTarget('claude', 'stable-acct', 10), 'stable');
    await forgetMissingTabs(store, new Set([80]), 'stable');
    await forgetTarget(store, 'claude', 'does-not-exist');
    await rememberOrganizationScopedTarget(
      store,
      { platform: 'claude', origin: CLAUDE_ORIGIN, scope: 'org-1', at: 11 },
      (sc) => sc.startsWith('org-'),
      'stable',
    );

    // Back on a dev build, everything the experimental platforms had is there.
    resetTabRegistryMirrorForTest();
    expect((await loadTabs(store, 'dev')).map((t) => t.tabId).sort()).toEqual([71, 72, 80]);
    expect((await loadTargets(store, 'dev')).map((t) => t.platform).sort())
      .toEqual(['claude', 'kimi', 'perplexity']);

    expect((await store.load(stateKey('perplexity', 'dev-acct')) as BackfillHeader).failures).toHaveLength(1);
    expect(pickBackfillState(await snapshotOf(store), 'dev')?.platform).toBe('perplexity');
  });
});

/** The popup reads a snapshot; the memory store exposes its raw map, deep-copied like `browserLocalSnapshot`. */
async function snapshotOf(store: BackfillStore & { data: Record<string, unknown> }): Promise<Record<string, unknown>> {
  return JSON.parse(JSON.stringify(store.data)) as Record<string, unknown>;
}
