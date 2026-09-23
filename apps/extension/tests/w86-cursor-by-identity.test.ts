/**
 * W86 · **The tick cursor names the target it served, not the slot it sat in.**
 *
 * ## The defect this file pins, from the code at `588ab22`
 *
 * `cs_backfill_cursor_v1` held `{ served: <index> }` — a position in
 * `cs_backfill_targets_v1` (`lib/backfill/alarm.ts` line 249 at `588ab22`; read
 * back at `entrypoints/background.ts:1609-1610`, written at `:1661` and `:1796`).
 * That array is not a fixed list. `rememberTarget` **prepends** — "Record one
 * target (deduplicated by platform+scope, most recent first)" — so every live
 * capture moves one row to the head and shifts the rows above it down by one.
 * An index read back after that names a *different row*, and the walk starts one
 * row too early.
 *
 * The consequence is not cosmetic, and it is the same class W76 removed from the
 * other end. Let the registry be `[A, B, C]` and let a live capture re-register
 * the least-recently-captured scope between every pair of wakes — the ordinary
 * act of switching to another platform's tab. Then on `588ab22`:
 *
 *   | wake | cursor | registry        | start | served |
 *   |------|--------|-----------------|-------|--------|
 *   | 1    | —      | `[A,B,C]`       | 0     | **A**  |
 *   | 2    | 0      | `[C,A,B]`       | 1     | **A**  |
 *   | 3    | 1      | `[B,C,A]`       | 2     | **A**  |
 *   | 4    | 2      | `[A,B,C]`       | 0     | **A**  |
 *
 * `A` takes every wake and `B` and `C` are never served at all. W79 recorded the
 * reordering itself happening live ("a claude live-capture prepend persists"), and
 * named the exposure without being able to conclude it: *"under repeated
 * reordering a target can be served twice in a row while another waits a full
 * extra cycle (or worse — prove or refute with a test)"*. This file is that test.
 * Case W86-A refutes the weaker half and proves the stronger one.
 *
 * A removal is the same bug from the other side: the index then names the row one
 * *later* than the row behind the served target, so that row is passed over for a
 * whole pass (W86-B).
 *
 * ## How "who was served" is observed, and why it is observed that way
 *
 * **Each target's own `archived` set** (`loadState(...).archived.length`), read
 * before and after every tick: the scope whose count rose is the one that tick
 * served. Deliberately not `schedule.served`, for the two reasons
 * `tests/w76-fair-tick-scheduling.test.ts` sets out at length — the trace names a
 * platform and may not name a scope, and a test whose only evidence is a field the
 * fix introduced cannot show the defect it claims to have found. The archive count
 * exists in both revisions.
 *
 * ## The reorderings are performed by the real `rememberTarget` / `forgetTarget`
 *
 * A live capture is simulated by calling the shipped function that a live capture
 * calls, on the real store — not by writing an array literal into the key. The
 * registry the next wake reads is therefore whatever the product would have put
 * there, and a change to the dedup rule or the prepend moves this test with it.
 *
 * ## Why every target is a scope of one reachable platform
 *
 * A port is per origin, so one live tab makes every `chatgpt` scope runnable and
 * the walk's *order* is the only thing under test. `chatgpt`'s list/detail pair is
 * the one the synthetic server answers faithfully, so no fixture stands in for a
 * platform's own enumerate shape.
 *
 * 🔴 A, B, C, D, E and F are red on `588ab22`; the pasted runs are in the report.
 *    The cases marked **guard** pass on `588ab22` as well; they are the fallback
 *    sides of the change, and they are labelled rather than dressed up as reds.
 *    Zero real network, zero real browser profile, no conversation text.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { handleBackfillMessage, rememberTab, type TabQueryRow } from '../lib/backfill/tab-port';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

const PLATFORM = 'chatgpt';
const ORIGIN = 'https://chatgpt.com';
/** Enough conversations that no scope runs out of debts inside these tests. */
const IDS = Array.from(
  { length: 8 },
  (_unused, i) => `c1111111-0000-4000-8000-00000000000${i + 1}`,
);
/** Four scopes of one reachable platform. The registry order is set per case. */
const SCOPES = ['acct-w86-a', 'acct-w86-b', 'acct-w86-c', 'acct-w86-d'] as const;
const [A, B, C, D] = SCOPES;
/**
 * The literal cursor key. Spelled out rather than imported, so this file tests the
 * key that ships and not a constant the fix chose.
 */
const CURSOR_KEY = 'cs_backfill_cursor_v1';

interface TargetRow { platform: string; origin: string; scope: string }
const chatgpt = (scope: string): TargetRow => ({ platform: PLATFORM, origin: ORIGIN, scope });

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
const alarmBook = new Map<string, { periodInMinutes?: number }>();
/** The platform tabs that are "open" right now. Delete one and its ping naturally fails. */
const liveTabs = new Map<number, string>();
/** What `tabs.query({})` returns — the recovery sweep's only view of the browser. */
const queriedTabs: TabQueryRow[] = [];
const contentFetches: string[] = [];
let host: SyntheticHost;

/**
 * A synthetic server: a pure function, never the network. One page of
 * conversations, then the empty page that confirms the list is finished.
 */
function syntheticPageFetch(url: string) {
  contentFetches.push(url);
  const u = new URL(url);
  if (u.pathname === '/backend-api/conversations') {
    const offset = Number(u.searchParams.get('offset') ?? '0');
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({
        items: IDS.slice(offset).map((id) => ({ id })),
        total: IDS.length,
      }),
    });
  }
  const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
  return Promise.resolve({
    status: 200,
    text: async () => JSON.stringify({
      mapping: { n1: { id: 'n1', message: { content: { parts: ['synthetic'] } } } },
      current_node: 'n1',
      account_id: id,
    }),
  });
}

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
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
    async query() { return queriedTabs.map((t) => ({ ...t })); },
    async sendMessage(tabId: number, message: unknown) {
      const origin = liveTabs.get(tabId);
      if (!origin) throw new Error('Could not establish connection. Receiving end does not exist.');
      const pending = handleBackfillMessage(message, origin, syntheticPageFetch as any);
      if (!pending) return undefined;
      return await pending;
    },
  },
};

/** A fake clock through background's test seam, so the test does not really sleep. */
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

/** A service-worker reclaim: the module goes away, `storage.local` does not. */
async function restartServiceWorker(): Promise<any> {
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  return bootBackground();
}

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

/** Seed the registry directly: array order **is** the walk's order. */
function seedTargets(rows: TargetRow[]): void {
  store['cs_backfill_targets_v1'] = rows.map((row) => ({ ...row, at: 1 }));
}

/** Give the chatgpt origin a live, logged-in tab: the row `resolveHttpPort` needs. */
async function openTab(tabId: number): Promise<void> {
  const { browserLocalStore } = await import('../lib/backfill/store');
  liveTabs.set(tabId, ORIGIN);
  queriedTabs.push({ id: tabId });
  await rememberTab(browserLocalStore(), { tabId, origin: ORIGIN, at: 1 });
}

/** **How many conversations this target has actually archived.** */
async function archived(row: TargetRow): Promise<number> {
  const { loadState } = await import('../lib/backfill/engine');
  const { browserLocalStore } = await import('../lib/backfill/store');
  const state = await loadState(browserLocalStore()!, row.platform, row.scope);
  return state?.archived.length ?? 0;
}

/**
 * 🔴 A live capture of an already-registered scope, performed by the shipped
 *    `rememberTarget` on the real store. This is exactly what the content script's
 *    capture path calls, and it is the only mechanism that reorders the registry:
 *    the row moves to the head and the rows above it shift down by one.
 *
 * Both mutators take a **scope**, not a row: every target in this file is a
 * `chatgpt` row, and a row-shaped parameter would let a scope string be passed
 * where a row was meant — an all-`undefined` write that changes nothing and makes
 * the case silently vacuous.
 */
async function captureScope(scope: string): Promise<void> {
  const { rememberTarget } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await rememberTarget(browserLocalStore(), { ...chatgpt(scope), at: 1 });
}

/** The registry as storage holds it — read, never assumed, so drift shows up here. */
async function registry(): Promise<TargetRow[]> {
  const { loadTargets } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  return (await loadTargets(browserLocalStore())).map((t) => ({
    platform: t.platform, origin: t.origin, scope: t.scope,
  }));
}

/** Capture the least-recently-captured row: the tab the user has not used in a while. */
async function captureTail(): Promise<void> {
  const rows = await registry();
  await captureScope(rows[rows.length - 1]!.scope);
}

/** Forget a scope, the way a re-scoped or collapsed row is dropped. */
async function forgetScope(scope: string): Promise<void> {
  const { forgetTarget } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await forgetTarget(browserLocalStore(), PLATFORM, scope);
}

/**
 * One alarm tick, and **which target it served** — read from the archives, not
 * from the trace. `null` when no target's archive moved (a tick that ran nobody).
 */
async function servedByOneTick(mod: any, rows: TargetRow[]): Promise<TargetRow | null> {
  const before: number[] = [];
  for (const row of rows) before.push(await archived(row));
  alarmListeners[0]!({ name: 'cs-backfill-tick' });
  await mod.backfillTickSettled();
  const moved: TargetRow[] = [];
  for (let i = 0; i < rows.length; i += 1) {
    if ((await archived(rows[i]!)) !== before[i]) moved.push(rows[i]!);
  }
  // One tick may serve at most one platform (W76 design rule 3), so more than one
  // moving count is itself a failure — said here rather than left to whichever
  // assertion happens to read the counts next.
  expect(moved.length, `one tick served ${JSON.stringify(moved.map((m) => m.scope))}`)
    .toBeLessThanOrEqual(1);
  return moved[0] ?? null;
}

/** The cursor as the walk will read it next, or the raw byte if it is not an object. */
function cursorByte(): unknown {
  return store[CURSOR_KEY];
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  alarmBook.clear();
  liveTabs.clear();
  queriedTabs.length = 0;
  contentFetches.length = 0;
  runtimeNow = 1_700_000_000_000;
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

// ===========================================================================
// W86-A · a live capture between wakes reorders the registry under the cursor
// ===========================================================================

describe('W86-A · the rotation survives a registry that reorders under it', () => {
  it('🔴 a capture between every wake must not let one scope take them all', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(11);
    const mod = await bootBackground();

    const served: Array<string | null> = [];
    for (let i = 0; i < 6; i += 1) {
      served.push((await servedByOneTick(mod, rows))?.scope ?? null);
      // The user switches to another tab and captures there: that scope is
      // re-registered and `rememberTarget` prepends it. The registry holds the same
      // three rows throughout — nothing was added and nothing was removed, it was
      // only reordered, so there is no "new target" to excuse a missing turn.
      await captureTail();
    }
    const counts: number[] = [];
    for (const row of rows) counts.push(await archived(row));
    console.log('[W86-A] served per tick:', JSON.stringify(served), 'archived per scope:', JSON.stringify(counts));

    // 🔴 On `588ab22` this is [A,A,A,A,A,A] with counts [6,0,0] — one scope takes
    //    every wake and the other two are never reached, from a reorder alone.
    expect(served).toEqual([A, B, C, A, B, C]);
    // Each runnable target is served within one full cycle, twice over.
    expect(counts).toEqual([2, 2, 2]);
  });
});

// ===========================================================================
// W86-B · a removal between wakes must not skip the row behind the served one
// ===========================================================================

describe('W86-B · a removal between wakes does not pass a row over', () => {
  it('🔴 a scope dropped ahead of the cursor must not shift the walk onto the row after its successor', async () => {
    const rows = SCOPES.map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(21);
    const mod = await bootBackground();

    const first = await servedByOneTick(mod, rows);
    const second = await servedByOneTick(mod, rows);
    console.log('[W86-B1] first two:', JSON.stringify([first?.scope, second?.scope]), '· cursor:', JSON.stringify(cursorByte()));
    expect([first?.scope, second?.scope]).toEqual([A, B]);

    // A is dropped (re-scoped, or collapsed onto another row). The row behind the
    // target that was served last is C, and C is what the rotation owes the next
    // wake — B and C are still registered and still runnable.
    await forgetScope(A);
    const third = await servedByOneTick(mod, rows);
    console.log('[W86-B1] after dropping A:', third?.scope, '· registry:', JSON.stringify((await registry()).map((r) => r.scope)));
    // 🔴 On `588ab22` this is D: the cursor's index 1 named B before the removal and
    //    names D after it, so C is passed over for a whole pass.
    expect(third?.scope).toBe(C);
  });

  it('🔴 dropping the target the cursor names falls back to the head instead of re-serving the row behind it', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(31);
    const mod = await bootBackground();

    const first = await servedByOneTick(mod, rows);
    expect(first?.scope).toBe(A);
    await forgetScope(A);

    const second = await servedByOneTick(mod, rows);
    console.log('[W86-B2] after dropping the served row:', second?.scope, '· cursor:', JSON.stringify(cursorByte()));
    // 🔴 On `588ab22` this is C: index 0 named A, the removal slid B into slot 0, and
    //    `(0 + 1) % 2` names C — so B, the only row the rotation had left to serve,
    //    is skipped. The identity is gone, so the walk starts at the head; the head
    //    still examines every row, which is why this fallback cannot strand anyone.
    expect(second?.scope).toBe(B);
    // And the fallback is written down: the next wake has a real cursor again.
    expect(cursorByte()).toEqual({ platform: PLATFORM, scope: B });
  });
});

// ===========================================================================
// W86-C · an unreadable, foreign or positional byte is not a target
// ===========================================================================

describe('W86-C · a cursor that does not name a registered target starts at the head', () => {
  it('🔴 the pre-W86 positional shape is not an identity: it is read as no cursor', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(41);
    // An upgrading profile still has the old shape at this key. It names slot 1 of
    // a registry that has been reordered since, which is exactly the fact this
    // revision refuses to guess at.
    store[CURSOR_KEY] = { served: 1 };

    const mod = await bootBackground();
    const first = await servedByOneTick(mod, rows);
    console.log('[W86-C1] served:', first?.scope, '· cursor now:', JSON.stringify(cursorByte()));
    // 🔴 On `588ab22` this byte is a real position and the walk starts at C.
    expect(first?.scope).toBe(A);
    // The stale byte is replaced by a real identity, so the leg cannot stay skewed.
    expect(cursorByte()).toEqual({ platform: PLATFORM, scope: A });

    // And it is honoured, not merely written: the next wake moves on to B.
    expect((await servedByOneTick(mod, rows))?.scope).toBe(B);
  });

  it('guard · an identity the registry no longer holds falls back to the head', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(51);
    // A scope that was renamed, or evicted by MAX_TARGET_ENTRIES. Well-formed, and
    // names nothing.
    store[CURSOR_KEY] = { platform: PLATFORM, scope: 'acct-w86-not-registered' };

    const mod = await bootBackground();
    const first = await servedByOneTick(mod, rows);
    console.log('[W86-C2] served:', first?.scope, '· cursor now:', JSON.stringify(cursorByte()));
    expect(first?.scope).toBe(A);
    expect(cursorByte()).toEqual({ platform: PLATFORM, scope: A });
  });

  it('guard · a half-written or wrongly-typed identity falls back to the head', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(61);

    // Each of these is a shape a partial write or a corrupt byte can produce; none
    // of them names a target, so none of them is a position to serve from.
    const notCursors: unknown[] = [
      { platform: PLATFORM },
      { scope: A },
      { platform: 7, scope: A },
      { platform: null, scope: A },
      'not-a-cursor',
      42,
    ];
    for (const byte of notCursors) {
      store[CURSOR_KEY] = byte;
      // A fresh worker, so the in-memory position cannot answer for the byte.
      const fresh = await restartServiceWorker();
      const served = await servedByOneTick(fresh, rows);
      console.log('[W86-C3] byte', JSON.stringify(byte), '⇒ served', served?.scope, '· cursor now:', JSON.stringify(cursorByte()));
      expect(served?.scope, `byte ${JSON.stringify(byte)} served nobody`).toBe(A);
      expect(cursorByte(), `byte ${JSON.stringify(byte)} was not replaced`).toEqual({ platform: PLATFORM, scope: A });
    }
  });
});

// ===========================================================================
// W86-D · the identity crosses a reclaim, and so does the reorder
// ===========================================================================

describe('W86-D · the stored identity is looked up after a reclaim, not trusted', () => {
  it('🔴 a reorder between the serve and the reclaim must not re-serve the stored target', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(71);
    let mod = await bootBackground();

    const before = [
      (await servedByOneTick(mod, rows))?.scope,
      (await servedByOneTick(mod, rows))?.scope,
    ];
    console.log('[W86-D] before the reclaim:', JSON.stringify(before), '· cursor:', JSON.stringify(cursorByte()));
    expect(before).toEqual([A, B]);

    // The MV3 reclaim: the module — and every in-memory byte — is gone; storage is
    // not. Then the registry reorders before the next wake, which is the whole
    // point: the stored cursor has to survive *and* still be found where it now is.
    mod = await restartServiceWorker();
    await captureScope(C);
    const after = await servedByOneTick(mod, rows);
    console.log('[W86-D] after the reclaim + reorder:', after?.scope, '· registry:', JSON.stringify((await registry()).map((r) => r.scope)));
    // 🔴 On `588ab22` this is B again: the stored index 1 named B when it was
    //    written, and names B again after the reorder, so B is served twice in a row
    //    while C waits.
    expect(after?.scope).toBe(C);
  });
});
