/**
 * W86b · **The wake serves whoever has waited longest; registry order is only a
 * tie-break, so no sequence of captures can starve a target.**
 *
 * ## The defect this file pins, from the code at `835d4f1`
 *
 * W86 replaced the positional cursor (`{served: <index>}`) with an identity
 * (`{platform, scope}`) and `cursorStartIndex` then walked the registry **from
 * the row after that identity**. That is still a decision about *order*: which
 * row a wake reaches is decided by where the identity currently sits, and
 * `rememberTarget` is a move-to-front — every live capture moves one row to the
 * head and shifts the rows above it down by one.
 *
 * So the sequence "capture the row that was just served before each wake"
 * degenerates. On `835d4f1`, with `[A, B, C]` all runnable:
 *
 *   | wake | capture before the wake | registry | start | served |
 *   |------|-------------------------|----------|-------|--------|
 *   | 1    | —                       | `[A,B,C]`| 0     | **A**  |
 *   | 2    | —                       | `[A,B,C]`| 1     | **B**  |
 *   | 3    | B (just served)         | `[B,A,C]`| 1     | **A**  |
 *   | 4    | A (just served)         | `[A,B,C]`| 1     | **B**  |
 *   | 5…   | alternate B, A          | —        | 1     | A, B, A, B, … |
 *
 * From wake 3 the cursor identity sits at index 0 every time, so the walk always
 * starts at index 1 — and `C`, registered and runnable throughout, is never that
 * row. One capture is enough to miss a cycle. A removal is the same defect from
 * the other side: `cursorStartIndex` returns `0` when the identity is absent, the
 * head takes the wake, and a row that is never the head waits forever.
 *
 * ## What decides a wake now
 *
 * The cursor is a **map from identity to the sequence number of the wake that
 * last served it** (`{served: {'<platform>\0<scope>': <n>}}`). A wake walks the
 * runnable targets in **least-recently-served-first** order — never-seen targets
 * join at the back, ties broken by registry order — and stamps the identity it
 * really served. Registry order is a tie-break, not the schedule, so no prepend,
 * removal or permutation can move a target out of its turn.
 *
 * ## How "who was served" is observed, and why it is observed that way
 *
 * **Each target's own `archived` set** (`loadState(...).archived.length`), read
 * before and after every tick: the scope whose count rose is the one that tick
 * served. Deliberately not `schedule.served`, for the two reasons
 * `tests/w76-fair-tick-scheduling.test.ts` sets out at length — the trace names a
 * platform and may not name a scope, and a test whose only evidence is a field
 * this revision introduced cannot show the defect it claims to have found. The
 * archive count exists in both revisions.
 *
 * ## The reorderings are performed by the real `rememberTarget` / `forgetTarget`
 *
 * A live capture is simulated by calling the shipped function that a live capture
 * calls, on the real store — never by writing an array literal into the key. The
 * registry the next wake reads is therefore whatever the product would have put
 * there, and a change to the dedup rule or the prepend moves this file with it.
 *
 * ## Why every target is a scope of one reachable platform
 *
 * A port is per origin, so one live tab makes every `chatgpt` scope runnable and
 * the walk's *order* is the only thing under test. `chatgpt`'s list/detail pair is
 * the one the synthetic server answers faithfully.
 *
 * 🔴 W86b-A, B, C, D, E and H are red on `835d4f1` — as is the second case of
 *    W86b-F, which pins the per-entry rule — the pasted runs are in the report.
 *    The cases marked **guard** pass on `835d4f1` as well: they are the fallback
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
/**
 * Enough conversations that no scope runs out of debts inside these tests — the
 * property loop below serves some scope a dozen times.
 */
const IDS = Array.from(
  { length: 64 },
  (_unused, i) => `c1111111-0000-4000-8000-${String(i + 1).padStart(12, '0')}`,
);
/** Five scopes of one reachable platform. The registry order is set per case. */
const SCOPES = ['acct-w86b-a', 'acct-w86b-b', 'acct-w86b-c', 'acct-w86b-d', 'acct-w86b-e'] as const;
const [A, B, C] = SCOPES;
/**
 * The literal cursor key, and the literal identity-key join, spelled out rather
 * than imported: this file must test the byte and the shape that ship, not a
 * constant the fix chose.
 */
const CURSOR_KEY = 'cs_backfill_cursor_v1';
const idKey = (scope: string): string => `${PLATFORM}\0${scope}`;

interface TargetRow { platform: string; origin: string; scope: string }
const chatgpt = (scope: string): TargetRow => ({ platform: PLATFORM, origin: ORIGIN, scope });

const store: Record<string, unknown> = {};
const sessionStore: Record<string, unknown> = {};
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
    session: {
      async get(defaults: Record<string, unknown>) {
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in sessionStore ? sessionStore[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(sessionStore, values); },
      async remove(keys: string[]) { for (const k of keys) delete sessionStore[k]; },
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
 *    `rememberTarget` on the real store. This is exactly what the content
 *    script's capture path calls, and it is the only mechanism that reorders the
 *    registry: the row moves to the head and the rows above it shift down by one.
 *
 * It takes a **scope**, not a row: every target in this file is a `chatgpt` row,
 * and a row-shaped parameter would let a scope string be passed where a row was
 * meant — an all-`undefined` write that changes nothing and makes the case
 * silently vacuous.
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

/** The scope names served by `n` consecutive wakes. */
async function servedSequence(mod: any, rows: TargetRow[], n: number): Promise<Array<string | null>> {
  const out: Array<string | null> = [];
  for (let i = 0; i < n; i += 1) out.push((await servedByOneTick(mod, rows))?.scope ?? null);
  return out;
}

/** The raw byte at the cursor key — what the next wake will read. */
function cursorByte(): any {
  return store[CURSOR_KEY];
}

/** Its `served` map as a plain object, for a readable assertion. */
function stamps(): Record<string, number> {
  return { ...cursorByte()?.served };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  for (const k of Object.keys(sessionStore)) delete sessionStore[k];
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
// W86b-A · the capture-before-every-wake table from the review
// ===========================================================================

describe('W86b-A · a capture between every wake reorders the registry under the cursor', () => {
  it('🔴 capturing the just-served row before each wake must not reduce the rotation to two scopes', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(11);
    const mod = await bootBackground();

    const served: Array<string | null> = [];
    let previous: string | null = null;
    for (let i = 0; i < 6; i += 1) {
      // The user switches back to the tab they were just looking at, so that
      // scope is re-registered and `rememberTarget` prepends it. The registry
      // holds the same three rows throughout — nothing was added and nothing was
      // removed, it was only reordered — so there is no "new target" to excuse a
      // missing turn.
      if (i >= 2 && previous !== null) await captureScope(previous);
      const s = (await servedByOneTick(mod, rows))?.scope ?? null;
      served.push(s);
      previous = s;
    }
    const counts: number[] = [];
    for (const row of rows) counts.push(await archived(row));
    console.log('[W86b-A] served per tick:', JSON.stringify(served), '· archived per scope:', JSON.stringify(counts));
    console.log('[W86b-A] registry now:', JSON.stringify((await registry()).map((r) => r.scope)), '· cursor:', JSON.stringify(cursorByte()));

    // 🔴 On `835d4f1` this is [A,B,A,B,A,B] with counts [3,3,0]: the cursor's
    //    identity sits at index 0 from wake 3 on, so the walk always starts at
    //    index 1 and C — registered and runnable the whole time — is never it.
    expect(served).toEqual([A, B, C, A, B, C]);
    // Each runnable target is served twice; ADR-033: each serve clears 2 bodies ⇒ 4 per scope.
    expect(counts).toEqual([4, 4, 4]);
  });
});

// ===========================================================================
// W86b-B · one capture is enough to miss a cycle
// ===========================================================================

describe('W86b-B · one capture of the row the next wake is owed', () => {
  it('🔴 a capture before the fifth wake must not push the owed row behind another', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(21);
    const mod = await bootBackground();

    const served = await servedSequence(mod, rows, 4);
    console.log('[W86b-B] first four:', JSON.stringify(served), '· cursor:', JSON.stringify(cursorByte()));
    expect(served).toEqual([A, B, C, A]);

    // B is the row the next wake is owed, and the user captures it.
    await captureScope(B);
    const after = await servedSequence(mod, rows, 2);
    console.log('[W86b-B] after capturing B (registry:', JSON.stringify((await registry()).map((r) => r.scope)), '):', JSON.stringify(after));

    // 🔴 On `835d4f1` the next two are C then B — the walk starts at the row after
    //    the cursor's position, and the capture moved B to the head so the row
    //    after A is C. B's turn was skipped, not delayed.
    expect(after).toEqual([B, C]);
  });
});

// ===========================================================================
// W86b-C · a removal plus a prepend on every wake
// ===========================================================================

describe('W86b-C · the row served is dropped from the registry and prepended again', () => {
  it('🔴 a remove-then-re-register between every wake must not starve the rows that are never the head', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(31);
    const mod = await bootBackground();

    const served: Array<string | null> = [];
    for (let i = 0; i < 6; i += 1) {
      const s = (await servedByOneTick(mod, rows))?.scope ?? null;
      served.push(s);
      // A re-scope: the row really leaves the registry and really comes back, at
      // the head. Both halves go through the shipped mutators.
      if (s !== null) {
        await forgetScope(s);
        await captureScope(s);
      }
    }
    console.log('[W86b-C] served per tick:', JSON.stringify(served), '· registry:', JSON.stringify((await registry()).map((r) => r.scope)));

    // 🔴 On `835d4f1` this is [A,B,A,B,A,B]: the served row is prepended, so the
    //    cursor's identity is at index 0 on the next wake and the walk starts at
    //    index 1 — C sits at index 2 and is never reached.
    expect(served).toEqual([A, B, C, A, B, C]);
  });
});

// ===========================================================================
// W86b-D · what is stored, and what is dropped from it
// ===========================================================================

describe('W86b-D · the stored cursor is a map of identities', () => {
  it('🔴 each serve stamps its own identity, and an identity that leaves the registry is pruned', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(41);
    const mod = await bootBackground();

    const served = await servedSequence(mod, rows, 2);
    console.log('[W86b-D] served:', JSON.stringify(served), '· cursor:', JSON.stringify(cursorByte()));
    expect(served).toEqual([A, B]);
    // 🔴 On `835d4f1` this byte is `{platform, scope}` — one identity, and no record
    //    at all of how long anyone has waited. 🔴 W86c: the map is the whole
    //    registered rotation, dense `0..k-1` — C (not yet served this round) is the
    //    oldest rank, B (just served) the newest.
    expect(stamps()).toEqual({ [idKey(C)]: 0, [idKey(A)]: 1, [idKey(B)]: 2 });

    // A is dropped for good (re-scoped onto another row, or collapsed). The two
    // wakes after it serve C and B; the map must not keep carrying A.
    await forgetScope(A);
    const after = await servedSequence(mod, rows, 2);
    console.log('[W86b-D] after dropping A:', JSON.stringify(after), '· cursor:', JSON.stringify(cursorByte()));
    expect(after).toEqual([C, B]);
    expect(stamps()).toEqual({ [idKey(C)]: 0, [idKey(B)]: 1 });
    expect(Object.keys(stamps())).not.toContain(idKey(A));
  });
});

// ===========================================================================
// W86b-E · a stored map is honoured, and it is what decides
// ===========================================================================

describe('W86b-E · a stored map decides who goes first', () => {
  it('🔴 the identity that waited longest is served, wherever the registry puts it', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(51);
    // A profile that has been running: A and C were served recently, B long ago.
    store[CURSOR_KEY] = { served: { [idKey(A)]: 2, [idKey(B)]: 1, [idKey(C)]: 3 } };

    const mod = await bootBackground();
    const first = await servedByOneTick(mod, rows);
    console.log('[W86b-E] served:', first?.scope, '· cursor:', JSON.stringify(cursorByte()));
    // 🔴 On `835d4f1` this byte is not a cursor at all — no `platform`, no `scope` —
    //    so the walk starts at the head and serves A, which waited 1 turn behind B.
    expect(first?.scope).toBe(B);
    // 🔴 W86c: the stored {A:2,B:1,C:3} is renormalised to dense ranks as soon as
    // it is written back — A:0 (longest waiting), C:1, B:2 (just served).
    expect(stamps()).toEqual({ [idKey(A)]: 0, [idKey(C)]: 1, [idKey(B)]: 2 });

    // And it is a schedule, not a one-off: the next wake takes the next-longest.
    expect((await servedByOneTick(mod, rows))?.scope).toBe(A);
  });
});

// ===========================================================================
// W86b-F · a byte that is not a map is not a cursor
// ===========================================================================

describe('W86b-F · a malformed or pre-W86 byte is an empty map, never a crash', () => {
  it('guard · every shape this key has ever held, and every shape a partial write can make, starts at the head', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(61);

    const notCursors: unknown[] = [
      // The two shapes that really are in the wild on an upgrading profile.
      { served: 1 },
      { platform: PLATFORM, scope: B },
      // A map field of the wrong type, or no map field at all.
      { served: 'not-a-map' },
      { served: 7 },
      { served: null },
      { served: [] },
      {},
      // Not an object.
      'not-a-cursor',
      42,
      null,
    ];
    for (const byte of notCursors) {
      store[CURSOR_KEY] = byte;
      delete sessionStore[`${CURSOR_KEY}:session`];
      // A fresh worker, so no in-memory map can answer for the byte.
      const fresh = await restartServiceWorker();
      const served = await servedByOneTick(fresh, rows);
      console.log('[W86b-F] byte', JSON.stringify(byte), '⇒ served', served?.scope, '· cursor now:', JSON.stringify(cursorByte()));
      expect(served?.scope, `byte ${JSON.stringify(byte)} served nobody`).toBe(A);
      // The bad byte is replaced by a real dense map, so the leg cannot stay stuck
      // at the head on the strength of one corrupt value (🔴 W86c: it names the
      // whole rotation, `0..k-1`).
      expect(stamps(), `byte ${JSON.stringify(byte)} was not replaced`).toEqual({
        [idKey(B)]: 0, [idKey(C)]: 1, [idKey(A)]: 2,
      });
    }
  });

  it('🔴 a single unreadable stamp is dropped on its own; the rest of the map still schedules', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(62);
    // A and C hold usable stamps; B's is not a stamp. B is therefore never-served.
    // 🔴 W86c §1: never-served ranks at the *back* now — so B waits behind the two
    //   rows whose stamps are intact, and is served within the rotation (`k` = 3
    //   wakes), instead of jumping the queue in front of them.
    store[CURSOR_KEY] = { served: { [idKey(A)]: 5, [idKey(B)]: 'junk', [idKey(C)]: 5 } };

    const mod = await bootBackground();
    // A and C (equal 5s, tie by registry order) are served first.
    const served = await servedSequence(mod, rows, 3);
    console.log('[W86b-F2] served:', JSON.stringify(served), '· cursor:', JSON.stringify(cursorByte()));
    // 🔴 On `835d4f1` the whole value is rejected — it has no `platform`/`scope` —
    //    so the walk starts at the head and serves A every time.
    expect(served).toEqual([A, C, B]);
  });
});

// ===========================================================================
// W86b-G · the write is best-effort, the in-memory map is not
// ===========================================================================

describe('W86b-G · a cursor write that keeps failing still rotates', () => {
  it('guard · the rotation is carried in memory while storage.local refuses the key', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(71);
    // The synthetic full quota: only the cursor key is refused, so what this case
    // observes is the cursor's own best-effort failure and nothing else.
    const realSet = fakeBrowser.storage.local.set;
    try {
      fakeBrowser.storage.local.set = async (values: Record<string, unknown>) => {
        if (Object.prototype.hasOwnProperty.call(values, CURSOR_KEY)) {
          throw new Error('synthetic: storage.local refused the write');
        }
        await realSet(values);
      };

      const mod = await bootBackground();
      const served = await servedSequence(mod, rows, 6);
      console.log('[W86b-G] served per tick:', JSON.stringify(served), '· cursor key:', JSON.stringify(store[CURSOR_KEY]));
      // The write really did fail — without this the case could pass because storage
      // worked, which would prove nothing about the path it is here for.
      expect(store[CURSOR_KEY]).toBeUndefined();
      // The rotation is still a rotation for as long as this worker lives (W76b).
      expect(served).toEqual([A, B, C, A, B, C]);
    } finally {
      // 🔴 The override is scoped to this case: it is a shared fixture object, and
      //    leaving it blocked would silently make every later test (which needs the
      //    cursor to persist) a cursor-write-failure test.
      fakeBrowser.storage.local.set = realSet;
    }
  });

  it('🔴 survives a fresh worker on every wake when the local cursor write keeps failing', async () => {
    const rows = SCOPES.slice(0, 3).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(72);
    const realSet = fakeBrowser.storage.local.set;
    fakeBrowser.storage.local.set = async (values: Record<string, unknown>) => {
      if (Object.prototype.hasOwnProperty.call(values, CURSOR_KEY)) {
        throw new Error('synthetic: storage.local refused the cursor write');
      }
      await realSet(values);
    };
    try {
      const served: Array<string | null> = [];
      let mod = await bootBackground();
      for (let wake = 0; wake < rows.length; wake += 1) {
        served.push(...await servedSequence(mod, rows, 1));
        mod = await restartServiceWorker();
      }
      console.log('[W86b-G restart] served per fresh worker:', JSON.stringify(served));
      expect(store[CURSOR_KEY]).toBeUndefined();
      expect(served).toEqual([A, B, C]);
    } finally {
      fakeBrowser.storage.local.set = realSet;
    }
  });
});

// ===========================================================================
// W86b-H · the property, over random reorder / newcomer / eviction sequences
// ===========================================================================

/** The registry cap — the "n wakes" bound the walk guarantees. */
const CAP = 8;

/**
 * One run of the anti-starvation property: `WAKES` **real-work** wakes, with a
 * random registry mutation performed by the shipped mutators before each wake.
 * A wake is observed by "which scope's archive actually grew", for **every**
 * scope then in the registry — the base population *and* brand-new scopes alike
 * — so a turn spent on a newcomer is counted as a real-work wake, not hidden.
 *
 * The registration **set** is no longer held fixed, and that is the whole point
 * of W86c:
 *
 *  · a **brand-new scope** is registered sometimes (§1): a newcomer must join the
 *    rotation at the back and be materialised there, never jump the queue in
 *    front of the rows already waiting;
 *  · a row is **evicted for good** sometimes: a row that leaves is exempt from the
 *    assertion, but the run must exercise the `throughout` side so the assertion
 *    cannot quietly pass because every starved row happened to leave (the 🩸 hole
 *    the pre-W86b version of this loop fell into on `835d4f1`);
 *  · move-to-front and remove-then-re-prepend still happen, as before.
 *
 * The bound asserted is the one W86c states while at least one cursor mirror
 * accepts writes: every scope that is registered for `n` consecutive real-work
 * wakes is served within those `n`. If both local and session writes fail, a fresh
 * worker can repeat a stale cursor until one write succeeds. `n` is the registry
 * cap (`MAX_TARGET_ENTRIES`, 8), which is the strongest safe form of "served
 * within `m` real-work wakes for `m` registered runnable rows" — `m` varies with
 * the churn, so a single hard upper bound keeps every row covered.
 */
async function propertyRun(
  seed0: number,
  WAKES: number,
  base: TargetRow[],
  mod: any,
): Promise<{ served: Array<string | null>; ops: string[]; registered: string[][] }> {
  /** A deterministic LCG: the sequence is fixed, so a failure is reproducible. */
  let seed = seed0;
  const rnd = (): number => {
    seed = (seed * 1103515245 + 12345) % 2147483648;
    return seed / 2147483648;
  };
  const pick = (xs: readonly string[]): string => xs[Math.floor(rnd() * xs.length)]!;
  const ops: string[] = [];
  const registered: string[][] = [];
  const served: Array<string | null> = [];
  const counts: Record<string, number> = {};
  for (const r of base) counts[r.scope] = await archived(r);
  let fresh = 0;

  /** Which now-registered scope's archive grew since our last read? `null` = none. */
  const servedThisWake = async (): Promise<string | null> => {
    const env = await registry();
    let grown: string | null = null;
    for (const t of env) {
      if (!(t.scope in counts)) counts[t.scope] = await archived(chatgpt(t.scope));
      const c = await archived(chatgpt(t.scope));
      const before = counts[t.scope];
      counts[t.scope] = c;
      if (before !== undefined && c > before) grown = t.scope;
    }
    return grown;
  };

  for (let i = 0; i < WAKES; i += 1) {
    const before = (await registry()).map((r) => r.scope);
    const roll = rnd();
    if (roll < 0.25) {
      // A brand-new identity (§1): joins the registry, possibly evicting the tail.
      await captureScope(`acct-w86c-new-${fresh++}`);
      ops.push(`new ${before.length}`);
    } else if (roll < 0.4 && before.length > 1) {
      const s = pick(before);
      await forgetScope(s); // evicted for good (the throughout side)
      ops.push(`evict ${s}`);
    } else if (roll < 0.65) {
      const s = served[served.length - 1] ?? pick(before);
      await captureScope(s);
      ops.push(`prepend-served ${s}`);
    } else if (roll < 0.85) {
      const s = pick(before);
      await captureScope(s);
      ops.push(`prepend ${s}`);
    } else {
      const s = pick(before);
      await forgetScope(s);
      await captureScope(s);
      ops.push(`remove+prepend ${s}`);
    }
    // Snapshot every identity before the tick, including a newcomer first
    // registered on this wake, so serving it now is observed as a real serve.
    const afterMutation = await registry();
    for (const target of afterMutation) {
      if (!(target.scope in counts)) counts[target.scope] = await archived(chatgpt(target.scope));
    }
    registered.push(afterMutation.map((r) => r.scope));
    alarmListeners[0]!({ name: 'cs-backfill-tick' });
    await mod.backfillTickSettled();
    const s = await servedThisWake();
    served.push(s);
    ops.push(`-> ${s ?? 'none'}`);
  }
  return { served, ops, registered };
}

describe('W86b-H / W86c · every row that stays registered is served within n wakes', () => {
  it('🔴 random newcomer, eviction, move-to-front and re-scope sequences cannot starve a registered row', async () => {
    const rows = SCOPES.map(chatgpt);
    await enableBackfill();
    await openTab(81);

    const n = CAP;
    const WAKES = 24;
    /** Two fixed streams, so the property is not a statement about one lucky seed. */
    // Seed 0 registers a newcomer before the first wake and that prepended row
    // is served immediately; its before-tick baseline must make it observable.
    const SEEDS = [0, 0x51eed, 0xbeef1];
    for (const seed of SEEDS) {
      // 🔴 Each seed is an independent world: the previous seed's newcomers are
      //    still in the registry (their evictions were for-good, not for the next
      //    run), so re-seed, drop any stale cursor and boot a fresh worker.
      seedTargets(rows);
      delete store[CURSOR_KEY];
      delete sessionStore[`${CURSOR_KEY}:session`];
      const mod = await restartServiceWorker();
      const { served, ops, registered } = await propertyRun(seed, WAKES, rows, mod);
      console.log(`[W86b-H/W86c seed ${seed}] served:`, JSON.stringify(served));
      console.log(`[W86b-H/W86c seed ${seed}] ops:`, JSON.stringify(ops));

      // Worth asserting on: every wake was a real-work wake, both newscomers and
      // evictions actually happened, and the registry never ran empty (so a
      // null-serve cannot excuse a window the defect would have shrunk).
      expect(served.filter((s) => s === null)).toEqual([]);
      if (seed === 0) expect(served[0]).toMatch(/^acct-w86c-new-/);
      expect(ops.some((o) => o.startsWith('new '))).toBe(true);
      expect(ops.some((o) => o.startsWith('evict '))).toBe(true);
      expect(ops.some((o) => o.startsWith('remove+prepend '))).toBe(true);
      for (const set of registered) expect(set.length).toBeGreaterThan(0);

      const scopes = new Set(registered.flat());
      const violations: string[] = [];
      for (let i = 0; i + n <= WAKES; i += 1) {
        const window = served.slice(i, i + n);
        for (const scope of scopes) {
          const throughout = registered.slice(i, i + n).every((set) => set.includes(scope));
          if (!throughout) continue;
          if (!window.includes(scope)) {
            violations.push(`seed ${seed} wake ${i}: ${scope} was registered through wake ${i + n - 1} but not served in ${JSON.stringify(window)}`);
          }
        }
      }
      console.log(`[W86b-H/W86c seed ${seed}] windows checked:`, WAKES - n + 1, '· violations:', JSON.stringify(violations));
      // 🔴 On `835d4f1` this is non-empty: a positional start plus a move-to-front
      //    leaves rows that are never the row after the cursor; with brand-new
      //    scopes joining (which the old loop excluded), their front-of-queue -1
      //    rank leaves a long-lived row out of every window too.
      expect(violations).toEqual([]);

      // Reclaim once the churn has settled. The persisted map must cover exactly
      // the registered identities and contain dense ranks, even when session
      // storage was the newer mirror selected by the fresh worker.
      await restartServiceWorker();
      const finalRows = await registry();
      const saved = cursorByte() as { served?: Record<string, number> } | undefined;
      const identities = finalRows.map((row) => idKey(row.scope));
      expect(Object.keys(saved?.served ?? {}).sort()).toEqual(identities.sort());
      expect(Object.values(saved?.served ?? {}).sort((a, b) => a - b)).toEqual(
        identities.map((_identity, i) => i),
      );
    }
  });
});

// ===========================================================================
// W86c §1 · a constant stream of newcomers must not starve a registered row
// ===========================================================================
//
// The Grok run that advertised the defect: eight runnable chatgpt scopes share
// one tab, `V` is kept registered (move-to-front) and a brand-new scope is
// prepended before every wake. On the break, each newcomer ranks `-1` — ahead of
// every stamp — so it takes the wake, `V` was never served, and any window of
// `n` fails. Under the fix, `V` must be served within every `n` real-work wakes.

describe('W86c §1 · brand-new scopes cannot jump the queue', () => {
  it('🔴 a fresh scope before every wake leaves V (registered throughout) unserved on the break', async () => {
    const n = CAP; // the registry cap, and the "n wakes" bound
    const V = 'acct-w86c-V';
    // V plus seven long-lived peers, all runnable on one origin.
    const peers = Array.from({ length: n - 1 }, (_u, i) => `acct-w86c-peer-${i}`);
    const seed = [V, ...peers].map(chatgpt);
    seedTargets(seed);
    await enableBackfill();
    await openTab(910);
    const mod = await bootBackground();

    const WAKES = 4 * n; // generously more than a few "n" windows
    let prevV = await archived(chatgpt(V));
    const servedV: boolean[] = [];
    for (let i = 0; i < WAKES; i += 1) {
      // Keep V registered (move-to-front, so the cap never evicts it) and drop a
      // brand-new scope before the wake.
      await captureScope(V);
      await captureScope(`acct-w86c-fresh-${i}`);
      alarmListeners[0]!({ name: 'cs-backfill-tick' });
      await mod.backfillTickSettled();
      const nowV = await archived(chatgpt(V));
      servedV.push(nowV > prevV); // whether THIS real-work wake served V
      prevV = nowV;
    }
    const firstServed = servedV.indexOf(true);
    console.log('[W86c-§1] servedV:', JSON.stringify(servedV), '· first at', firstServed);

    // Every window of n consecutive real-work wakes must contain a serve of V.
    // 🔴 On the break, `firstServed` is 0 and nothing after it is ever served again
    //    — the second window (wakes 1..n) is all `false`.
    for (let i = 0; i + n <= servedV.length; i += 1) {
      expect(
        servedV.slice(i, i + n).some(Boolean),
        `wake ${i}: V was registered through wake ${i + n - 1} but not served in that window`,
      ).toBe(true);
    }
  });
});

// ===========================================================================
// W86c §2 · a stored MAX_SAFE_INTEGER stamp round-trips
// ===========================================================================
//
// The review's storage hole: `readTickCursor` accepts `Number.MAX_SAFE_INTEGER`
// (it is a safe integer), but the old `nextServeStamp` stored `max+1` = 2^53,
// which is *not*, so the next worker dropped it. Registry `[A, B]` with a huge
// stamp held only by `A` then left `A` unserved forever while fresh workers kept
// re-serving the never-stamped `B`.

describe('W86c §2 · stamps are dense and bounded, so nothing round-trips into starvation', () => {
  it('🔴 a stored MAX_SAFE_INTEGER stamp is renormalised; the other row is not starved by a huge stamp', async () => {
    const rows = SCOPES.slice(0, 2).map(chatgpt); // A, B
    seedTargets(rows);
    store[CURSOR_KEY] = { served: { [idKey(A)]: Number.MAX_SAFE_INTEGER } };
    await enableBackfill();
    await openTab(920);

    // A fresh worker reads the ceiling byte out of storage, as a real worker after
    // a reclaim does.
    const mod = await bootBackground();
    const first = await servedByOneTick(mod, rows);
    console.log('[W86c-§2] served:', first?.scope, '· cursor:', JSON.stringify(cursorByte()));
    // 🔴 On the break this is B: the huge stamp is a valid rank but the old walk put
    //    the never-stamped B (-1) ahead of it, and wrote B's stamp as 2^53. A — the
    //    row whose only flaw was the huge stamp — must actually be served.
    expect(first?.scope).toBe(A);
    // and the saved byte stayed readable (a dense rank, not a 2^53 that the next
    // worker would drop — the round-trip that IS the defect).
    const srv: Record<string, number> = ((cursorByte() as any)?.served) ?? {};
    for (const v of Object.values(srv)) {
      expect(Number.isSafeInteger(v), `stamp ${v} is not a safe integer`).toBe(true);
    }
  });
});
