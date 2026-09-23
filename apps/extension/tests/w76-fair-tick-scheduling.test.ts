/**
 * W76 · **Every platform gets a turn: the alarm's tick is a round-robin, not a
 * queue the registry head always wins.**
 *
 * ## The defect this file pins (W72 §1, measured live 2026-09-23)
 *
 * The live registry is `deepseek, claude, chatgpt, perplexity, kimi, grok,
 * gemini` — `cs_backfill_targets_v1` is ordered most-recently-captured-first —
 * and every observed tick landed on deepseek. The walk stopped at the first
 * target whose result was not `no-http-port` (`background.ts:1375` at `566e4e0`),
 * so the head platform with an open tab took **every** tick and the six behind it
 * were never served. A head that is permanently halted, or waiting out a
 * transient backoff, returns the same non-`no-http-port` shape and took every
 * tick too — a dead platform blocked every live one, forever.
 *
 * ## How "who was served" is observed, and why it is observed that way
 *
 * **Each target's own `archived` set** (`loadState(...).archived.length`), read
 * before and after every tick: the scope whose count rose is the one that tick
 * served. Deliberately not `schedule.served`, for two independent reasons.
 *
 *  · `schedule.served` names a **platform**, and the trace may not name a scope —
 *    an account id is exactly what this project must never write down (the W76
 *    contract's privacy rule; the same reason `TickSchedule` carries platform ids
 *    and reason codes and nothing else). So a fixture with several targets of one
 *    platform could not use it to tell them apart even if it wanted to.
 *  · A test whose only evidence is a field *the fix introduced* cannot show the
 *    defect it claims to have found. The archive count exists in both revisions,
 *    so the red runs below fail for the scheduling reason and not for a missing
 *    key.
 *
 * ## Why the runnable targets are three *scopes of one platform*
 *
 * A **port is per origin**, so "this platform has a tab and that one does not" is
 * the only way to make one target tabless while another is reachable — and the
 * walk keys on a target's **position in the registry**, not on the platform it
 * names, so a rotation among several scopes of one reachable origin is the same
 * walk exercising the same starvation. The alternative would need a synthetic
 * server per platform's own enumerate shape (a DeepSeek cursor, a Perplexity
 * envelope, a Gemini RPC), which would put the fixture, not the scheduler, under
 * test. `chatgpt`'s list/detail pair is the one the synthetic server below can
 * answer faithfully; `grok` appears only as a *tabless* head, which is never run
 * and so needs no server.
 *
 * 🔴 Cases A, B, C, D and E1/E2 are red on `566e4e0`; the pasted runs are in the
 *    report. E3 is a guard for behaviour the base revision also had, and is called
 *    out as such rather than dressed up as a red. Zero real network, zero real
 *    browser profile, no conversation text.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { handleBackfillMessage, rememberTab, type TabQueryRow } from '../lib/backfill/tab-port';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

const ORIGIN = 'https://chatgpt.com';
const PLATFORM = 'chatgpt';
const GROK_ORIGIN = 'https://grok.com';
const GROK = 'grok';
/** Enough conversations that no scope runs out of debts inside these tests. */
const IDS = Array.from(
  { length: 8 },
  (_unused, i) => `c1111111-0000-4000-8000-00000000000${i + 1}`,
);
/** Three targets of one reachable platform, in registry order. `[0]` is the head. */
const SCOPES = ['acct-w76-a', 'acct-w76-b', 'acct-w76-c'] as const;
/**
 * The literal cursor key. Spelled out rather than imported so this file tests the
 * key that ships and not a constant the fix chose — a rename, or a key written
 * somewhere else, must not be invisible here.
 */
const CURSOR_KEY = 'cs_backfill_cursor_v1';

interface TargetRow { platform: string; origin: string; scope: string }
const chatgpt = (scope: string): TargetRow => ({ platform: PLATFORM, origin: ORIGIN, scope });
const grok = (scope: string): TargetRow => ({ platform: GROK, origin: GROK_ORIGIN, scope });

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

async function writeHalt(row: TargetRow, reason: string, clock?: typeof runtimeClock): Promise<void> {
  const { recordBackfillHalt } = await import('../lib/backfill/engine');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await recordBackfillHalt(browserLocalStore(), {
    platform: row.platform, scope: row.scope, reason: reason as never, detail: 'synthetic',
    ...(clock ? { clock } : {}),
  });
}

/**
 * One alarm tick, and **which target it served** — read from the archives, not
 * from the trace. `null` when no target's archive moved (a tick that ran nobody).
 */
async function serveOne(mod: any, rows: TargetRow[]): Promise<{ served: TargetRow | null; rec: any }> {
  const before: number[] = [];
  for (const row of rows) before.push(await archived(row));
  alarmListeners[0]!({ name: 'cs-backfill-tick' });
  await mod.backfillTickSettled();
  const moved: TargetRow[] = [];
  for (let i = 0; i < rows.length; i += 1) {
    if (await archived(rows[i]!) !== before[i]) moved.push(rows[i]!);
  }
  const { loadLastTick } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  const rec = await loadLastTick(browserLocalStore());
  // One tick may serve at most one platform (W76 design rule 3), so more than one
  // moving count is itself a failure — said here rather than left to whichever
  // assertion happens to read the counts next.
  expect(moved.length, `one tick served ${JSON.stringify(moved.map((m) => m.scope))}`)
    .toBeLessThanOrEqual(1);
  return { served: moved[0] ?? null, rec };
}

/** The scope names served, tick by tick. */
async function servedSequence(mod: any, rows: TargetRow[], n: number): Promise<Array<string | null>> {
  const out: Array<string | null> = [];
  for (let i = 0; i < n; i += 1) out.push((await serveOne(mod, rows)).served?.scope ?? null);
  return out;
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
// W76-A · the head no longer takes every tick
// ===========================================================================

describe('W76-A · a runnable head stops starving the targets behind it', () => {
  it('🔴 over 6 ticks with 3 runnable targets, each is served exactly twice', async () => {
    const rows = SCOPES.map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(11);
    const mod = await bootBackground();

    const served = await servedSequence(mod, rows, 6);
    const counts: number[] = [];
    for (const row of rows) counts.push(await archived(row));
    console.log('[W76-A] served per tick:', JSON.stringify(served), 'archived per scope:', JSON.stringify(counts));

    // ⌈6/3⌉ = ⌊6/3⌋ = 2, so this is an exact figure and not a range.
    expect(counts).toEqual([2, 2, 2]);
    // And it is a rotation, not a coincidence of totals: the order wraps.
    expect(served).toEqual([
      SCOPES[0], SCOPES[1], SCOPES[2], SCOPES[0], SCOPES[1], SCOPES[2],
    ]);
  });

  it('the trace names the platform served and every target passed over, with a reason code', async () => {
    // 🔴 A tabless head: this is also W76-D's property, asserted here through the
    //    record the tick writes. `grok` is the head and has no tab at all.
    const rows = [grok('acct-w76-grok'), chatgpt(SCOPES[0]), chatgpt(SCOPES[1])];
    seedTargets(rows);
    await enableBackfill();
    await openTab(22);
    const mod = await bootBackground();

    const { served, rec } = await serveOne(mod, rows);
    console.log('[W76-A] schedule:', JSON.stringify(rec?.schedule));
    expect(served?.scope).toBe(SCOPES[0]);
    expect(rec?.schedule).toEqual({
      served: PLATFORM,
      skipped: [{ platform: GROK, reason: 'no-http-port' }],
    });
  });
});

// ===========================================================================
// W76-B · a permanently halted head does not consume the tick
// ===========================================================================

describe('W76-B · a permanently halted target does not eat the tick', () => {
  it('🔴 a halted head with an open tab is passed over; the runnable target behind it is served every tick', async () => {
    const dead = chatgpt(SCOPES[0]);
    const alive = chatgpt(SCOPES[1]);
    const rows = [dead, alive];
    seedTargets(rows);
    await enableBackfill();
    await openTab(31);
    // A permanent stop this build wrote, so it still applies: not re-decidable.
    await writeHalt(dead, 'unsupported-platform');

    const mod = await bootBackground();
    const served = await servedSequence(mod, rows, 4);
    console.log('[W76-B] served per tick:', JSON.stringify(served));

    // The head is dead and the platform behind it is alive: the alive one is
    // served every tick, and the dead one is never run at all.
    expect(served).toEqual([SCOPES[1], SCOPES[1], SCOPES[1], SCOPES[1]]);
    expect(await archived(dead)).toBe(0);
    expect(await archived(alive)).toBe(4);

    const { rec } = await serveOne(mod, rows);
    console.log('[W76-B] schedule:', JSON.stringify(rec?.schedule));
    expect(rec?.schedule).toEqual({
      served: PLATFORM,
      skipped: [{ platform: PLATFORM, reason: 'halted' }],
    });
  });
});

// ===========================================================================
// W76-C · a head inside its backoff does not consume the tick
// ===========================================================================

describe('W76-C · a target waiting out a transient backoff does not eat the tick', () => {
  it('🔴 a backoff-waiting head is passed over as waiting-retry; the runnable target is served', async () => {
    const waiting = chatgpt(SCOPES[0]);
    const alive = chatgpt(SCOPES[1]);
    const rows = [waiting, alive];
    seedTargets(rows);
    await enableBackfill();
    await openTab(41);
    // A transient stop whose backoff has not elapsed *on this test's clock* — the
    // one clock the scheduler and the engine both read.
    await writeHalt(waiting, 'transport-error', runtimeClock);

    const mod = await bootBackground();
    const served = await servedSequence(mod, rows, 3);
    console.log('[W76-C] served per tick:', JSON.stringify(served));

    expect(served).toEqual([SCOPES[1], SCOPES[1], SCOPES[1]]);
    expect(await archived(waiting)).toBe(0);
    expect(await archived(alive)).toBe(3);

    const { rec } = await serveOne(mod, rows);
    console.log('[W76-C] schedule:', JSON.stringify(rec?.schedule));
    // 🔴 `waiting-retry`, not `halted`: a waiting round is not a stop, and the
    //    trace is the only place that distinction is visible.
    expect(rec?.schedule).toEqual({
      served: PLATFORM,
      skipped: [{ platform: PLATFORM, reason: 'waiting-retry' }],
    });
  });

  it('nothing runnable is reported as no-runnable-target, not as a missing port', async () => {
    const waiting = chatgpt(SCOPES[0]);
    const rows = [waiting];
    seedTargets(rows);
    await enableBackfill();
    await openTab(51);
    await writeHalt(waiting, 'transport-error', runtimeClock);

    const mod = await bootBackground();
    const held = await serveOne(mod, rows);
    console.log('[W76-C] while waiting:', JSON.stringify(held.rec?.schedule), 'reason:', held.rec?.reason);
    // A tab was open, so the port is fine; the registry is not empty, so this is
    // not `no-targets`. Reporting either would send a reader after a channel or a
    // registry that was never broken.
    expect(held.served).toBeNull();
    expect(held.rec?.ran).toBe(false);
    expect(held.rec?.reason).toBe('no-runnable-target');
    expect(await archived(waiting)).toBe(0);

    // Roll the clock past the backoff: the same target must come back on its own.
    runtimeNow += 24 * 60 * 60 * 1000;
    const resumed = await serveOne(mod, rows);
    console.log('[W76-C] after the backoff:', JSON.stringify(resumed.rec?.schedule));
    expect(resumed.served?.scope).toBe(SCOPES[0]);
    expect(resumed.rec?.schedule).toMatchObject({ served: PLATFORM, skipped: [] });
  });
});

// ===========================================================================
// W76-D · a target with no tab is skipped, as before
// ===========================================================================

describe('W76-D · a target with no tab is skipped and does not consume the tick', () => {
  it('🔴 a tabless head is skipped as no-http-port; the two runnable targets behind it alternate', async () => {
    const rows = [grok('acct-w76-grok'), chatgpt(SCOPES[0]), chatgpt(SCOPES[1])];
    seedTargets(rows);
    await enableBackfill();
    // Only the chatgpt origin has a tab; grok has none.
    await openTab(61);
    const mod = await bootBackground();

    const served = await servedSequence(mod, rows, 4);
    console.log('[W76-D] served per tick:', JSON.stringify(served));

    expect(served).toEqual([SCOPES[0], SCOPES[1], SCOPES[0], SCOPES[1]]);
    expect(await archived(rows[0]!)).toBe(0);
    expect(await archived(rows[1]!)).toBe(2);
    expect(await archived(rows[2]!)).toBe(2);
  });
});

// ===========================================================================
// W76-E · the cursor is storage-backed, and an unreadable one falls back safe
// ===========================================================================

describe('W76-E · the cursor survives a service-worker restart and fails safe', () => {
  it('🔴 the rotation position is remembered across a worker restart', async () => {
    const rows = SCOPES.map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(71);

    let mod = await bootBackground();
    const before = await servedSequence(mod, rows, 2);
    console.log('[W76-E] before the restart:', JSON.stringify(before), 'cursor:', JSON.stringify(store[CURSOR_KEY]));

    // The MV3 reclaim: the module (and every in-memory byte) is gone; storage is not.
    mod = await restartServiceWorker();
    const after = await servedSequence(mod, rows, 3);
    console.log('[W76-E] after the restart:', JSON.stringify(after));

    // The walk resumed where it left off, not at the head: after B comes C, not A.
    expect(before).toEqual([SCOPES[0], SCOPES[1]]);
    expect(after).toEqual([SCOPES[2], SCOPES[0], SCOPES[1]]);
  });

  it('🔴 an unreadable cursor starts at the head rather than skipping a platform forever', async () => {
    const rows = SCOPES.slice(0, 2).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(81);
    // A byte at the cursor key that is not a cursor. It must not be read as
    // "serve nobody", and it must not be read as a position either.
    store[CURSOR_KEY] = 'not-a-cursor';

    const mod = await bootBackground();
    const first = await serveOne(mod, rows);
    console.log('[W76-E] after a garbage cursor:', JSON.stringify(first.rec?.schedule), 'cursor:', JSON.stringify(store[CURSOR_KEY]));
    // The head is served: the safe fallback is the old behaviour, not a skip loop.
    expect(first.served?.scope).toBe(SCOPES[0]);
    // …and the garbage is replaced by a real cursor, so the leg cannot stay stuck
    // at the head forever on the strength of one bad byte.
    expect(store[CURSOR_KEY]).toMatchObject({ served: 0 });

    // The next tick really does move on — the cursor is honoured, not merely written.
    const second = await serveOne(mod, rows);
    expect(second.served?.scope).toBe(SCOPES[1]);
  });

  it('a cursor past the end of a shrunk registry wraps instead of skipping the walk', async () => {
    const rows = SCOPES.slice(0, 2).map(chatgpt);
    seedTargets(rows);
    await enableBackfill();
    await openTab(91);
    // Three targets were served last time; the registry now holds two.
    store[CURSOR_KEY] = { served: 99 };

    const mod = await bootBackground();
    const { served, rec } = await serveOne(mod, rows);
    console.log('[W76-E] cursor past the end:', JSON.stringify(rec?.schedule));
    // Whoever is served, somebody is: an out-of-range cursor is a position to wrap,
    // never a reason for the walk to find nothing.
    expect(served).not.toBeNull();
    expect(rec?.schedule?.served).toBe(PLATFORM);
  });
});
