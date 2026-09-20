/**
 * W51 · **The tab registry recovers without anyone looking at the browser.**
 *
 * ## The defect this file pins, measured on a real browser (2026-09-20)
 *
 * Backfill reported `no-http-port` for ~41 minutes while the chatgpt tab was
 * open and logged in. Reloading that one tab fixed it instantly. What the
 * measurements showed:
 *
 *  1. `cs_backfill_tabs_v1` had rows for the two tabs opened most recently, and
 *     **not** for chatgpt — while `cs_backfill_targets_v1` still listed it. A
 *     target with no channel, and a channel pointing at a platform with nothing
 *     to fetch.
 *  2. The registry is keyed by `tabId`. Chrome had given that tab a new id, so
 *     the row was stale: a `grok.com` row pointed at an id `chrome.tabs.query({})`
 *     did not list, while a grok tab was open under a different id.
 *  3. The stale row is never cleaned: `pickLiveTab` returns at the first
 *     answering row, so a dead row behind a healthy same-origin row is never
 *     pinged, never struck, never forgotten.
 *  4. The live tab cannot re-announce either. W27's re-hello is a page
 *     `setTimeout` on a 4–6 min jittered interval, and Chrome throttles that
 *     toward hourly when the tab is hidden. W27 pins recovery on
 *     `visibilitychange` — a human returning to the tab, which never happens
 *     in unattended operation.
 *
 * Dead rows also accumulate toward `MAX_TAB_ENTRIES`, because `rememberTab`
 * dedups by `tabId` and a changed id appends rather than replaces.
 *
 * ## What is asserted, and what is deliberately not
 *
 *  · A stale row whose tabId no longer exists is **pruned**, and that prune is
 *    not a ping miss — `pickLiveTab`'s two-strike rule is a different fact.
 *  · A live tab that is not in the registry is found by one `tabs.query({})`
 *    sweep and registered through `rememberTab` (so repeats cannot duplicate).
 *  · `no-http-port` is still reported faithfully when there genuinely is no
 *    platform tab. A sweep that looked and found nothing is distinguishable
 *    in the tick trace from a tick that never swept.
 *  · Any prune goes through `writeRegistry` (the W33-C mirror). Writing
 *    `store.save` directly would leave a stale mirror that silently undoes it.
 *
 * 🔴 The sweep is driven through the alarm's real entry point, not through
 *    `resolveHttpPort`: that helper is also the popup's `transportWired` probe,
 *    and a sweep there would fan out to every tab on every failed resolution.
 *    No network, no real browser profile, no real conversation data.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { handleBackfillMessage, type TabEntry, type TabQueryRow } from '../lib/backfill/tab-port';
import { memoryStore } from '../lib/backfill/store';

const ORIGIN = 'https://chatgpt.com';
const OTHER_ORIGIN = 'https://grok.com';
const SCOPE = 'acct-fixture-1';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
];

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
const alarmBook = new Map<string, { periodInMinutes?: number }>();
/** Tabs whose content script will answer a ping / fetch. */
const liveTabs = new Map<number, string>();
/**
 * What `tabs.query({})` returns. Distinct from `liveTabs` on purpose: a tab can
 * be open (query lists it) while its content script is gone (sendMessage throws),
 * and a registry row can name an id query does not list.
 */
const queriedTabs: TabQueryRow[] = [];
const contentFetches: string[] = [];
const pingedIds: number[] = [];
/** How many pings this tabId has received this test (1-based after increment). */
const pingCounts = new Map<number, number>();
/**
 * The Nth ping to this tabId (and every later one) throws. The sweep's first
 * answer can succeed while the alarm's retry ping fails — the D1 shape.
 */
const pingFailsFrom = new Map<number, number>();
let queryCalls = 0;

function syntheticPageFetch(url: string) {
  contentFetches.push(url);
  const u = new URL(url);
  if (u.pathname === '/backend-api/conversations') {
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({ items: IDS.map((id) => ({ id })), total: IDS.length }),
    });
  }
  return Promise.resolve({
    status: 200,
    text: async () => JSON.stringify({
      mapping: { n1: { id: 'n1', message: { content: { parts: ['synthetic'] } } } },
      current_node: 'n1',
      account_id: SCOPE,
    }),
  });
}

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in store ? store[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
  alarms: {
    create(name: string, info: any) { alarmBook.set(name, info); },
    async clear(name: string) { return alarmBook.delete(name); },
    async get(name: string) { return alarmBook.get(name) ?? undefined; },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    async query() {
      queryCalls += 1;
      return queriedTabs.map((t) => ({ ...t }));
    },
    async sendMessage(tabId: number, message: unknown) {
      if ((message as { type?: string } | null)?.type === 'cs-backfill-ping') {
        pingedIds.push(tabId);
        const n = (pingCounts.get(tabId) ?? 0) + 1;
        pingCounts.set(tabId, n);
        const failFrom = pingFailsFrom.get(tabId);
        if (failFrom !== undefined && n >= failFrom) {
          throw new Error('simulated ping failure');
        }
      }
      const origin = liveTabs.get(tabId);
      if (!origin) throw new Error('Could not establish connection. Receiving end does not exist.');
      const pending = handleBackfillMessage(message, origin, syntheticPageFetch as any);
      if (!pending) return undefined;
      return await pending;
    },
  },
};

let runtimeNow = 1_700_000_000_000;
const runtimeClock = {
  now: () => runtimeNow,
  sleep: async (ms: number) => { runtimeNow += ms; },
};

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock, random: () => 0 });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

async function alarmTick(mod: any): Promise<void> {
  alarmListeners[0]!({ name: 'cs-backfill-tick' });
  await mod.backfillTickSettled();
}

async function trace(): Promise<any> {
  const { loadLastTick } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  return await loadLastTick(browserLocalStore());
}

async function registry(): Promise<TabEntry[]> {
  const { loadTabs } = await import('../lib/backfill/tab-port');
  const { browserLocalStore } = await import('../lib/backfill/store');
  const s = browserLocalStore();
  if (!s) throw new Error('this suite runs against a fake browser with storage.local; it must not be null');
  return loadTabs(s);
}

async function enabledWithTarget(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
  store['cs_backfill_targets_v1'] = [{ platform: 'chatgpt', origin: ORIGIN, scope: SCOPE, at: 1 }];
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  alarmBook.clear();
  liveTabs.clear();
  queriedTabs.length = 0;
  contentFetches.length = 0;
  pingedIds.length = 0;
  pingCounts.clear();
  pingFailsFrom.clear();
  queryCalls = 0;
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  (globalThis as any).indexedDB = new IDBFactory();
});

// ===========================================================================
// W51-A · through the alarm's real entry point
// ===========================================================================

describe('W51-A · a tick that is about to concede no-http-port sweeps once', () => {
  it('🔴 a stale row whose tabId no longer exists is pruned, and the live tab is registered', async () => {
    await enabledWithTarget();
    const { rememberTab } = await import('../lib/backfill/tab-port');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const s = browserLocalStore()!;
    // The measured shape: a registry row for this origin pointing at an id
    // Chrome no longer has, while the real tab is open under a new id.
    await rememberTab(s, { tabId: 740386466, origin: ORIGIN, at: 1 });
    liveTabs.set(999, ORIGIN);
    queriedTabs.push({ id: 999 });

    const mod = await bootBackground();
    await alarmTick(mod);

    const rows = await registry();
    console.log('[W51-A] after recovery sweep, registry tabIds:', rows.map((t) => t.tabId), 'queryCalls:', queryCalls, 'tick:', (await trace())?.reason);
    expect(rows.map((t) => t.tabId), 'the gone id is not kept').not.toContain(740386466);
    expect(rows.map((t) => t.tabId), 'the live tab the query found is now the channel').toContain(999);
    expect(queryCalls, 'the sweep belongs once per tick, not once per origin resolution').toBe(1);
    expect((await trace())?.reason, 'the recovered tab is a real fetch channel, not a pretend run').not.toBe('no-http-port');
    expect(contentFetches.length, 'a recovered tab is used, not merely registered').toBeGreaterThan(0);
  });

  it('🔴 the sweep does not duplicate a row that is already in the registry', async () => {
    await enabledWithTarget();
    const { rememberTab } = await import('../lib/backfill/tab-port');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await rememberTab(browserLocalStore()!, { tabId: 5, origin: ORIGIN, at: 1 });
    liveTabs.set(5, ORIGIN);
    queriedTabs.push({ id: 5 });

    const mod = await bootBackground();
    await alarmTick(mod);

    const rows = await registry();
    console.log('[W51-A] registry after a tick whose tab was already known:', rows.map((t) => t.tabId), 'queryCalls:', queryCalls);
    expect(rows.map((t) => t.tabId)).toEqual([5]);
    // A healthy registered tab is a port: the tick must not sweep "just in case".
    expect(queryCalls, 'a tick that already has a port does not sweep').toBe(0);
  });

  it('🔴 with no open platform page it still returns no-http-port faithfully (and the sweep says it looked)', async () => {
    await enabledWithTarget();
    // Query ran, and it found nothing — the C19 case with the tab closed, now
    // with a query that can actually answer. contentFetches stays empty: we
    // looked, we did not pretend to run.
    const mod = await bootBackground();
    await alarmTick(mod);

    const rec = await trace();
    console.log('[W51-A] empty sweep trace:', JSON.stringify({
      reason: rec?.reason,
      tabSweep: rec?.tabSweep,
      fetches: contentFetches.length,
      queryCalls,
    }));
    expect(rec?.reason).toBe('no-http-port');
    expect(contentFetches).toEqual([]);
    expect(queryCalls).toBe(1);
    expect(rec?.tabSweep, 'we looked, and found nothing — not "we never looked"').toEqual({
      looked: true,
      queried: 0,
      pruned: 0,
      pinged: 0,
      registered: 0,
      deferred: 0,
    });
    expect(await mod.backfillRuntimeStatus()).toMatchObject({
      transportWired: false,
      lastTickReason: 'no-http-port',
    });
  });

  it('🔴 a tick that never reached the port gate does not pretend to have swept', async () => {
    // No target: the alarm writes `no-targets` and never asks whether a tab
    // could fetch. That is the other half of "looked and found nothing".
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);

    const mod = await bootBackground();
    await alarmTick(mod);

    const rec = await trace();
    console.log('[W51-A] never-swept trace:', JSON.stringify({ reason: rec?.reason, tabSweep: rec?.tabSweep, queryCalls }));
    expect(rec?.reason).toBe('no-targets');
    expect(queryCalls, 'no registered target ⇒ no sweep').toBe(0);
    expect(rec?.tabSweep ?? null, 'absent/null: this tick never swept').toBeNull();
  });
});

// ===========================================================================
// W51-B · the sweep itself (prune ≠ miss, rememberTab, writeRegistry)
// ===========================================================================

describe('W51-B · pruning a gone id is not a missed ping, and it writes through the mirror', () => {
  it('🔴 forgetMissingTabs drops only ids the query no longer lists, and keeps remaining order', async () => {
    const { rememberTab, loadTabs, pickLiveTab, forgetMissingTabs } = await import('../lib/backfill/tab-port');
    const s = memoryStore();
    await rememberTab(s, { tabId: 1, origin: ORIGIN, at: 1 });
    await rememberTab(s, { tabId: 2, origin: ORIGIN, at: 2 });
    await rememberTab(s, { tabId: 3, origin: ORIGIN, at: 3 });
    expect((await loadTabs(s)).map((t) => t.tabId)).toEqual([3, 2, 1]);

    const pruned = await forgetMissingTabs(s, new Set([3, 1]));
    expect(pruned).toBe(1);
    expect((await loadTabs(s)).map((t) => t.tabId), 'remaining rows keep registry order').toEqual([3, 1]);

    const asked: number[] = [];
    await pickLiveTab(s, ORIGIN, async (id) => {
      asked.push(id);
      return { ok: true };
    });
    expect(asked, 'pickLiveTab still walks remaining rows in registry order').toEqual([3]);
  });

  it('🔴 a prune is not a ping miss: remaining rows keep the miss count they already had', async () => {
    const { rememberTab, loadTabs, pickLiveTab, forgetMissingTabs } = await import('../lib/backfill/tab-port');
    const s = memoryStore();
    await rememberTab(s, { tabId: 7, origin: ORIGIN, at: 1 });
    await rememberTab(s, { tabId: 8, origin: OTHER_ORIGIN, at: 2 });
    await pickLiveTab(s, ORIGIN, async () => { throw new Error('dead'); });
    expect((await loadTabs(s)).find((t) => t.tabId === 7)!.misses).toBe(1);

    await forgetMissingTabs(s, new Set([7]));
    const remaining = (await loadTabs(s)).find((t) => t.tabId === 7)!;
    expect(remaining.misses, 'pruning a different id does not strike this one').toBe(1);
    expect((await loadTabs(s)).map((t) => t.tabId)).toEqual([7]);
  });

  it('🔴 a prune writes through the W33 mirror, so a forgotten id can re-register inside the skip window', async () => {
    const { rememberTab, loadTabs, forgetMissingTabs } = await import('../lib/backfill/tab-port');
    const { TAB_HELLO_MIN_INTERVAL_MS } = await import('../lib/backfill/tab-hello');
    const s = memoryStore();
    const t0 = 1_700_000_000_000;
    await rememberTab(s, { tabId: 7, origin: ORIGIN, at: t0 });
    const writesAfterHello = s.writes;

    await forgetMissingTabs(s, new Set());
    expect(await loadTabs(s)).toEqual([]);

    // Well inside the skip window: if the prune had written storage without
    // updating the mirror, this hello would be skipped and the tab would stay
    // unregistered — the W33-C failure returning.
    await rememberTab(s, { tabId: 7, origin: ORIGIN, at: t0 + 1000 });
    expect(t0 + 1000).toBeLessThan(t0 + TAB_HELLO_MIN_INTERVAL_MS / 2);
    expect((await loadTabs(s)).map((t) => t.tabId), 'pruned, then back').toEqual([7]);
    expect(s.writes, 're-registering after a prune is a write, not a skipped hello').toBeGreaterThan(writesAfterHello);
  });

  it('🔴 sweepUnregisteredTabs pings unknown ids, registers whoever answers, and skips known / discarded / frozen', async () => {
    const { rememberTab, loadTabs, BACKFILL_PING_MESSAGE, sweepUnregisteredTabs } = await import('../lib/backfill/tab-port');
    const s = memoryStore();
    await rememberTab(s, { tabId: 1, origin: ORIGIN, at: 1 });

    const asked: number[] = [];
    const ping = async (id: number) => {
      asked.push(id);
      if (id === 2) return { ok: true, origin: ORIGIN };
      if (id === 3) return { ok: true, origin: OTHER_ORIGIN };
      throw new Error('Could not establish connection. Receiving end does not exist.');
    };
    const report = await sweepUnregisteredTabs(
      s,
      async () => [
        { id: 1 },
        { id: 2 },
        { id: 3 },
        { id: 4, discarded: true },
        { id: 5, frozen: true },
        { id: 6 },
      ],
      ping,
      9,
    );

    expect(BACKFILL_PING_MESSAGE).toBe('cs-backfill-ping');
    expect(asked.sort((a, b) => a - b), 'known / discarded / frozen are not pinged').toEqual([2, 3, 6]);
    expect(report).toMatchObject({ looked: true, queried: 6, pruned: 0, pinged: 3, registered: 2, deferred: 0 });
    expect(report.looked).toBe(true);
    if (report.looked) {
      expect([...report.origins].sort()).toEqual([ORIGIN, OTHER_ORIGIN].sort());
      expect([...report.pingedIds].sort((a, b) => a - b)).toEqual([2, 3, 6]);
      expect(report.recovered.map((r) => r.tabId).sort((a, b) => a - b)).toEqual([2, 3]);
    }
    expect((await loadTabs(s)).map((t) => t.tabId).sort((a, b) => a - b)).toEqual([1, 2, 3]);
    // rememberTab moved each new row to the front; the known row is still there once.
    expect((await loadTabs(s)).filter((t) => t.tabId === 1)).toHaveLength(1);
  });

  it('🔴 a query that throws is "we could not look", not "we looked and found nothing"', async () => {
    const { sweepUnregisteredTabs } = await import('../lib/backfill/tab-port');
    const s = memoryStore();
    const report = await sweepUnregisteredTabs(
      s,
      async () => { throw new Error('tabs.query unavailable'); },
      async () => ({ ok: true, origin: ORIGIN }),
    );
    expect(report).toEqual({ looked: false });
  });
});

// ===========================================================================
// W51-C · three defects the first recovery commit left open
// ===========================================================================

describe('W51-C · a retry cannot spend two strikes in one tick (D1)', () => {
  it('🔴 a silent registered tab is not forgotten when the recovered tab\'s re-ping fails', async () => {
    await enabledWithTarget();
    const { rememberTab, TAB_PING_MISSES_BEFORE_FORGET } = await import('../lib/backfill/tab-port');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const s = browserLocalStore()!;
    const silent = 10;
    const recovered = 11;
    await rememberTab(s, { tabId: silent, origin: ORIGIN, at: 1 });
    liveTabs.set(recovered, ORIGIN);
    queriedTabs.push({ id: silent }, { id: recovered });
    // Sweep ping (first) succeeds and registers `recovered`. The alarm's retry
    // pickLiveTab tries `recovered` first; that second ping fails, and without
    // the fix the loop walks on to `silent` and spends strike 2 → forgetTab.
    pingFailsFrom.set(recovered, 2);

    const mod = await bootBackground();
    await alarmTick(mod);

    const rows = await registry();
    console.log('[W51-C D1] registry after a same-tick retry whose recovered ping failed:', rows.map((t) => ({ tabId: t.tabId, misses: t.misses ?? 0 })), 'pings:', [...pingedIds]);
    expect(TAB_PING_MISSES_BEFORE_FORGET).toBe(2);
    expect(rows.map((t) => t.tabId), 'the silent tab is still listed by query; two strikes in one tick must not forget it').toContain(silent);
    expect(rows.find((t) => t.tabId === silent)?.misses, 'exactly one strike this tick').toBe(1);
    expect(rows.map((t) => t.tabId), 'the sweep still registered the answering tab').toContain(recovered);
  });
});

describe('W51-C · a sweep burst cannot evict a live listed row (D2)', () => {
  it('🔴 twelve answering unknown tabs do not drop twelve live grok rows', async () => {
    const { rememberTab, loadTabs, sweepUnregisteredTabs, MAX_TAB_ENTRIES } = await import('../lib/backfill/tab-port');
    const s = memoryStore();
    const grokIds: number[] = [];
    for (let i = 1; i <= MAX_TAB_ENTRIES; i += 1) {
      grokIds.push(i);
      await rememberTab(s, { tabId: i, origin: OTHER_ORIGIN, at: i });
    }
    const chatgptIds = Array.from({ length: MAX_TAB_ENTRIES }, (_, i) => MAX_TAB_ENTRIES + 1 + i);
    const asked: number[] = [];
    const report = await sweepUnregisteredTabs(
      s,
      async () => [...grokIds, ...chatgptIds].map((id) => ({ id })),
      async (id) => {
        asked.push(id);
        if (chatgptIds.includes(id)) return { ok: true, origin: ORIGIN };
        return { ok: true, origin: OTHER_ORIGIN };
      },
      0,
    );

    const remaining = (await loadTabs(s)).map((t) => t.tabId);
    console.log('[W51-C D2] after a 12+12 sweep, registry:', remaining, 'pinged:', asked.length, 'report:', report);
    for (const id of grokIds) {
      expect(remaining, 'a row whose tabId the query just listed as live is not sliced off by a burst of rememberTab').toContain(id);
    }
    expect(remaining, 'MAX_TAB_ENTRIES is unchanged; the fix is not to raise the ceiling').toHaveLength(MAX_TAB_ENTRIES);
  });
});

describe('W51-C · the ping fan-out is capped (D3)', () => {
  it('🔴 a sweep that hits the cap is distinguishable from one that pinged everyone it wanted to', async () => {
    const { sweepUnregisteredTabs, MAX_TAB_ENTRIES, TAB_SWEEP_PING_CAP } = await import('../lib/backfill/tab-port');
    expect(TAB_SWEEP_PING_CAP, 'the cap is the registry ceiling, not a new larger burst').toBe(MAX_TAB_ENTRIES);
    const s = memoryStore();
    const extra = 8;
    const n = MAX_TAB_ENTRIES + extra;
    const asked: number[] = [];
    const query = async () => Array.from({ length: n }, (_, i) => ({ id: i + 1 }));
    const ping = async (id: number) => {
      asked.push(id);
      throw new Error('Could not establish connection. Receiving end does not exist.');
    };

    const report = await sweepUnregisteredTabs(s, query, ping, 0);

    console.log('[W51-C D3] pinged ids:', asked, 'report:', report);
    expect(asked.length, 'the fan-out is the cap, not the window').toBe(MAX_TAB_ENTRIES);
    expect(asked, 'now=0 takes a prefix, so the test can name which ids were pinged').toEqual(
      Array.from({ length: MAX_TAB_ENTRIES }, (_, i) => i + 1),
    );
    expect(report).toMatchObject({
      looked: true,
      queried: n,
      pruned: 0,
      pinged: MAX_TAB_ENTRIES,
      registered: 0,
      deferred: extra,
    });

    asked.length = 0;
    const rotated = await sweepUnregisteredTabs(s, query, ping, MAX_TAB_ENTRIES);
    expect(rotated).toMatchObject({ looked: true, pinged: MAX_TAB_ENTRIES, deferred: extra });
    expect(asked, 'a later tick is not stuck on the same never-answering prefix').toContain(n);

    await enabledWithTarget();
    for (let i = 1; i <= n; i += 1) queriedTabs.push({ id: i });
    const mod = await bootBackground();
    await alarmTick(mod);
    const rec = await trace();
    console.log('[W51-C D3] persisted tabSweep:', rec?.tabSweep);
    expect(rec?.tabSweep).toMatchObject({
      looked: true,
      queried: n,
      pinged: MAX_TAB_ENTRIES,
      registered: 0,
      deferred: extra,
    });
    expect(rec?.tabSweep && 'deferred' in rec.tabSweep, 'the trace carries the cap as a count, not as tab ids').toBe(true);
  });
});
