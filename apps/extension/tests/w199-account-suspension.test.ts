/**
 * W199 · W128 step 2, the half that is **not** the halt: the suspension.
 *
 * A run that proves the account changed stops twice over, and the two records say two
 * different things (`AccountSuspension`, lib/backfill/types.ts):
 *
 *  · the `account-changed` halt says **what happened**, and it expires like any transient
 *    record — that is the right rule for a platform that refused a request;
 *  · the suspension says **what has to happen before this scope runs again**, and it does
 *    not expire. Coming back on a clock would mean issuing the same request under the same
 *    wrong account, i.e. a slow poll of somebody else's data.
 *
 * What this file pins, and each one is a way the mechanism can be wrong:
 *
 *  1. 🔴 **a suspended scope is not served**, and it stays unserved after the halt's own
 *     backoff has run out — the case a halt alone gets wrong.
 *  2. 🔴 **an unreadable suspension fails closed.** Something is on the record; we cannot
 *     say what; no request goes out. The opposite choice (treat it as absent) is one
 *     `?? undefined` away and is the one that would fetch under an unaccountable session.
 *  3. 🔴 **a suspension is lifted by an observation that agrees with it**, and the lift
 *     clears the halt too — otherwise the popup keeps saying "the account changed" for up to
 *     the ladder's two hours *after* the user switched back.
 *  4. 🔴 **the lift also clears the retry marker**, because a scope with a spent-attempt
 *     marker and no halt on disk is read by `scopeRetryDue` as "do not ask again" — the lift
 *     would deadlock the very scope it was meant to free.
 *  5. 🔴 **nothing incomparable is ever accused.** A different salt, and a scope that never
 *     had a lease, both leave every record byte-identical.
 *
 * Zero real network, zero real browser profile, no conversation text: the storage is a plain
 * object, the http port is a pure function, and the write-down channel is a synthetic host.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { handleBackfillMessage, rememberTab, type TabQueryRow } from '../lib/backfill/tab-port';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import { ACCOUNT_SALT_KEY } from '../lib/account-fingerprint';
import { memoryStore } from '../lib/backfill/store';
import { accountSuspensionHolds, readAccountSuspension, stateKey, type BackfillHeader } from '../lib/backfill/types';

const ORIGIN = 'https://grok.com';
const PLATFORM = 'grok';
const ACCOUNT_A = 'acct-fixture-1';
const ACCOUNT_B = 'acct-fixture-2';
const C1 = 'cc111111-0000-4000-8000-000000000001';

interface TargetRow { platform: string; origin: string; scope: string }
const grok = (scope: string): TargetRow => ({ platform: PLATFORM, origin: ORIGIN, scope });

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
const alarmBook = new Map<string, unknown>();
const liveTabs = new Map<number, string>();
const queriedTabs: TabQueryRow[] = [];
/** Every request the leg really made, so "it issued nothing" is an observation. */
const contentFetches: string[] = [];
let host: SyntheticHost;

/** A synthetic grok backend: a pure function, never the network. */
function syntheticGrokFetch(url: string) {
  contentFetches.push(url);
  const u = new URL(url);
  if (u.pathname === '/rest/app-chat/conversations') {
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({ conversations: [{ conversationId: C1 }] }),
    });
  }
  if (/\/response-node$/.test(u.pathname)) {
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({ responseNodes: [{ responseId: 'r1', sender: 'human' }], inflightResponses: [] }),
    });
  }
  return Promise.resolve({
    status: 200,
    text: async () => JSON.stringify({ responses: [{ responseId: 'r1', message: 'synthetic', sender: 'human' }] }),
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
      const pending = handleBackfillMessage(message, origin, syntheticGrokFetch as any);
      if (!pending) return undefined;
      return await pending;
    },
  },
};

let runtimeNow = 1_700_000_000_000;
const runtimeClock = { now: () => runtimeNow, sleep: async (ms: number) => { runtimeNow += ms; } };

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock, random: () => 0, preset: 'standard' });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

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

function seedTargets(rows: TargetRow[]): void {
  store['cs_backfill_targets_v1'] = rows.map((row) => ({ ...row, at: 1 }));
}

async function openTab(tabId: number): Promise<void> {
  const { browserLocalStore } = await import('../lib/backfill/store');
  liveTabs.set(tabId, ORIGIN);
  queriedTabs.push({ id: tabId });
  await rememberTab(browserLocalStore(), { tabId, origin: ORIGIN, at: 1 });
}

function storedHeader(scope: string): BackfillHeader {
  return store[stateKey(PLATFORM, scope)] as BackfillHeader;
}

beforeEach(async () => {
  vi.stubGlobal('indexedDB', new IDBFactory());
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', withI18n(fakeBrowser));
  host = createSyntheticHost();
  for (const k of Object.keys(store)) delete store[k];
  liveTabs.clear();
  queriedTabs.length = 0;
  contentFetches.length = 0;
  alarmBook.clear();
  alarmListeners.length = 0;
  runtimeListeners.length = 0;
  runtimeNow = 1_700_000_000_000;
  vi.resetModules();
  // WXT's auto-import, as every suite that loads `entrypoints/background` stubs it: the
  // module's default export is its setup callback, and this is the identity so the call
  // is what installs the listeners.
  vi.stubGlobal('defineBackground', (cb: unknown) => cb);
});

// ---------------------------------------------------------------------------
// The suspension record's own reader — the three answers that are not two.
// ---------------------------------------------------------------------------
describe('W199-H · reading a suspension is three answers, and the third fails closed', () => {
  it('absent, holds, unreadable are three different answers', () => {
    expect(accountSuspensionHolds({})).toBe('absent');
    expect(accountSuspensionHolds({ suspended: null })).toBe('absent');
    expect(accountSuspensionHolds({
      suspended: { at: 1, reason: 'account-changed', observed: { value: 'v', saltId: 's', source: 'response-body-platform-uid' } },
    })).toBe('holds');
    // A record this build cannot read is `unreadable` — NOT `absent`, which is the reading
    // that would let a request go out under an account nothing can vouch for.
    expect(accountSuspensionHolds({ suspended: { at: 'not-a-number' } })).toBe('unreadable');
    // …and a cause this build does not know is not rounded into the one it does.
    expect(accountSuspensionHolds({ suspended: { at: 1, reason: 'something-new' } })).toBe('unreadable');
    expect(accountSuspensionHolds({ suspended: 'a string' })).toBe('unreadable');
  });

  it('🔴 the reader drops what it cannot validate rather than half-building a record', () => {
    expect(readAccountSuspension({ at: 1, reason: 'account-changed' })).toEqual({
      at: 1, reason: 'account-changed',
    });
    // An unrecognised `source` is not a lease, so it is not carried.
    expect(readAccountSuspension({
      at: 1, reason: 'account-changed',
      lease: { value: 'v', saltId: 's', source: 'not-a-real-source' },
    })).toEqual({ at: 1, reason: 'account-changed' });
  });
});

// ---------------------------------------------------------------------------
// The walk: a suspended scope is not served, and does not become servable.
// ---------------------------------------------------------------------------
describe('W199-I · the alarm walk passes a suspended scope over', () => {
  it('🔴 a suspended scope is skipped with its own reason, and issues no request', async () => {
    seedTargets([grok(ACCOUNT_A)]);
    const mod = await bootBackground();
    await enableBackfill();
    await openTab(1);

    // A record with a lease and a halt whose backoff is **already in the past**: this is
    // exactly the state a halt alone would treat as due again.
    store[stateKey(PLATFORM, ACCOUNT_A)] = {
      v: 2,
      platform: PLATFORM,
      scope: ACCOUNT_A,
      totalKnown: null,
      totalSource: 'unknown',
      enumCursor: { offset: 0, complete: false },
      pendingCount: 0,
      archivedCount: 0,
      detailToday: { day: '2023-11-14', count: 0 },
      accountLease: { value: 'fixture-lease-value', saltId: 'fixture-salt', source: 'response-body-platform-uid', at: 1 },
      suspended: {
        at: 1,
        reason: 'account-changed',
        lease: { value: 'fixture-lease-value', saltId: 'fixture-salt', source: 'response-body-platform-uid' },
        observed: { value: 'fixture-other-value', saltId: 'fixture-salt', source: 'response-body-platform-uid' },
      },
      halted: { reason: 'account-changed', at: 1, detail: 'fixture', attempts: 1, retryAt: 1 },
    };

    runtimeNow += 10 * 60_000;
    const result = await mod.runAlarmTick();

    expect(result.report).toBeNull();
    expect(result.ran).toBe(false);
    expect(lastTickSkipped()).toContainEqual({ platform: PLATFORM, reason: 'account-suspended' });
    // 🔴 And nothing was fetched: the skip is a decision before the request, not a report
    //    after one.
    expect(contentFetches).toEqual([]);
  });

  it('🔴 the same scope with the suspension removed IS served — the control', async () => {
    seedTargets([grok(ACCOUNT_A)]);
    const mod = await bootBackground();
    await enableBackfill();
    await openTab(1);
    store[stateKey(PLATFORM, ACCOUNT_A)] = {
      v: 2,
      platform: PLATFORM,
      scope: ACCOUNT_A,
      totalKnown: null,
      totalSource: 'unknown',
      enumCursor: { offset: 0, complete: false },
      pendingCount: 0,
      archivedCount: 0,
      detailToday: { day: '2023-11-14', count: 0 },
      accountLease: { value: 'fixture-lease-value', saltId: 'fixture-salt', source: 'response-body-platform-uid', at: 1 },
      halted: null,
    };

    const result = await mod.runAlarmTick();

    expect(result.ran).toBe(true);
    expect(contentFetches.length).toBeGreaterThan(0);
  });

  it('🔴 an unreadable suspension holds the scope too — the fail-closed half', async () => {
    seedTargets([grok(ACCOUNT_A)]);
    const mod = await bootBackground();
    await enableBackfill();
    await openTab(1);
    store[stateKey(PLATFORM, ACCOUNT_A)] = {
      v: 2,
      platform: PLATFORM,
      scope: ACCOUNT_A,
      totalKnown: null,
      totalSource: 'unknown',
      enumCursor: { offset: 0, complete: false },
      pendingCount: 0,
      archivedCount: 0,
      detailToday: { day: '2023-11-14', count: 0 },
      // Something is on the record and we cannot read it. No request may go out.
      suspended: { at: 'not-a-number', reason: 'account-changed' },
      halted: null,
    };

    const result = await mod.runAlarmTick();

    expect(result.ran).toBe(false);
    expect(lastTickSkipped().some((s) => s.reason === 'account-suspended')).toBe(true);
    expect(contentFetches).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// The registry half: an observation suspends siblings and lifts its own scope.
// ---------------------------------------------------------------------------
describe('W199-J · an observation of the signed-in account moves the scopes of that platform', () => {
  const leaseRecord = (value: string, saltId: string) => ({
    value, saltId, source: 'response-body-platform-uid' as const,
  });

  /** Seed a scope's header directly. `suspendedAt` absent ⇒ not suspended. */
  function seedScope(scope: string, lease: ReturnType<typeof leaseRecord> | undefined, halted: BackfillHeader['halted'] = null) {
    store[stateKey(PLATFORM, scope)] = {
      v: 2,
      platform: PLATFORM,
      scope,
      totalKnown: null,
      totalSource: 'unknown',
      enumCursor: { offset: 0, complete: true },
      pendingCount: 0,
      archivedCount: 0,
      detailToday: { day: '2023-11-14', count: 0 },
      ...(lease ? { accountLease: { ...lease, at: 1 } } : {}),
      ...(halted ? { halted } : { halted: null }),
    } satisfies BackfillHeader;
  }

  it('🔴 a different account suspends the sibling, and the observer is left alone', async () => {
    const mod = await bootBackground();
    seedTargets([grok(ACCOUNT_A), grok(ACCOUNT_B)]);
    seedScope(ACCOUNT_A, leaseRecord('lease-a', 'salt-1'));
    seedScope(ACCOUNT_B, leaseRecord('lease-b', 'salt-1'));

    const result = await mod.applyAccountObservation(
      await browserStore(), PLATFORM, leaseRecord('lease-b', 'salt-1'), 42,
    );

    expect(result.suspended).toEqual([ACCOUNT_A]);
    expect(result.lifted).toEqual([]);
    const a = readAccountSuspension(storedHeader(ACCOUNT_A).suspended)!;
    expect(a.reason).toBe('account-changed');
    expect(a.lease?.value).toBe('lease-a');
    expect(a.observed?.value).toBe('lease-b');
    expect(a.at).toBe(42);
    // The scope the observation came from is untouched.
    expect(storedHeader(ACCOUNT_B).suspended).toBeUndefined();
  });

  it('🔴 a different salt suspends nothing at all — the record is byte-identical', async () => {
    const mod = await bootBackground();
    seedTargets([grok(ACCOUNT_A)]);
    seedScope(ACCOUNT_A, leaseRecord('lease-a', 'salt-1'));
    const before = JSON.stringify(storedHeader(ACCOUNT_A));

    const result = await mod.applyAccountObservation(
      await browserStore(), PLATFORM, leaseRecord('lease-b', 'a-different-installs-salt'), 42,
    );

    expect(result.suspended).toEqual([]);
    expect(JSON.stringify(storedHeader(ACCOUNT_A))).toBe(before);
  });

  it('🔴 a scope that has never had a lease is never accused', async () => {
    const mod = await bootBackground();
    seedTargets([grok(ACCOUNT_A)]);
    seedScope(ACCOUNT_A, undefined);
    const before = JSON.stringify(storedHeader(ACCOUNT_A));

    const result = await mod.applyAccountObservation(
      await browserStore(), PLATFORM, leaseRecord('lease-b', 'salt-1'), 42,
    );

    expect(result.suspended).toEqual([]);
    expect(JSON.stringify(storedHeader(ACCOUNT_A))).toBe(before);
  });

  it('🔴 an agreeing observation lifts the suspension AND the halt it stood for', async () => {
    const mod = await bootBackground();
    seedTargets([grok(ACCOUNT_A)]);
    seedScope(ACCOUNT_A, leaseRecord('lease-a', 'salt-1'), {
      reason: 'account-changed', at: 1, detail: 'fixture', attempts: 1, retryAt: 1,
    });
    // Write the suspension by hand: the shape the run writes when it proves a switch.
    (storedHeader(ACCOUNT_A) as { suspended?: unknown }).suspended = {
      at: 1, reason: 'account-changed', lease: leaseRecord('lease-a', 'salt-1'), observed: leaseRecord('lease-b', 'salt-1'),
    };
    // A spent-attempt marker, which is the trap: with `halted` cleared and this left behind,
    // `scopeRetryDue` answers "do not ask again" and the lift deadlocks the scope.
    (storedHeader(ACCOUNT_A) as { haltRetried?: unknown }).haltRetried = { build: 'a-build', at: 1 };

    const result = await mod.applyAccountObservation(
      await browserStore(), PLATFORM, leaseRecord('lease-a', 'salt-1'), 99,
    );

    expect(result.lifted).toEqual([ACCOUNT_A]);
    const h = storedHeader(ACCOUNT_A);
    expect(h.suspended).toBeUndefined();
    expect(h.halted).toBeNull();
    expect((h as { haltRetried?: unknown }).haltRetried).toBeUndefined();
    // …and the lease itself is untouched: a lift is not a re-take.
    expect(h.accountLease?.value).toBe('lease-a');
  });

  it('🔴 a platform that holds no lease is never walked', async () => {
    const mod = await bootBackground();
    store['cs_backfill_targets_v1'] = [{ platform: 'chatgpt', origin: 'https://chatgpt.com', scope: ACCOUNT_A, at: 1 }];
    store[stateKey('chatgpt', ACCOUNT_A)] = {
      v: 2, platform: 'chatgpt', scope: ACCOUNT_A, totalKnown: null, totalSource: 'unknown',
      enumCursor: { offset: 0, complete: true }, pendingCount: 0, archivedCount: 0,
      detailToday: { day: '2023-11-14', count: 0 },
      accountLease: { value: 'lease-a', saltId: 'salt-1', source: 'response-body-platform-uid', at: 1 },
      halted: null,
    } satisfies BackfillHeader;
    const before = JSON.stringify(store[stateKey('chatgpt', ACCOUNT_A)]);

    const result = await mod.applyAccountObservation(
      await browserStore(), 'chatgpt', leaseRecord('lease-b', 'salt-1'), 42,
    );

    expect(result).toEqual({ suspended: [], lifted: [], unwritten: [] });
    expect(JSON.stringify(store[stateKey('chatgpt', ACCOUNT_A)])).toBe(before);
  });

  it('🔴 a scope whose record cannot be read is named as unwritten, not as "nothing to say"', async () => {
    const mod = await bootBackground();
    seedTargets([grok(ACCOUNT_A)]);
    // A record this build cannot read at all: `openLedger` refuses, so the suspension cannot be
    // written — and the caller must be told that rather than handed an empty list.
    store[stateKey(PLATFORM, ACCOUNT_A)] = { v: 2, platform: PLATFORM, scope: ACCOUNT_A, nope: true };

    const result = await mod.applyAccountObservation(
      await browserStore(), PLATFORM, leaseRecord('lease-b', 'salt-1'), 42,
    );

    expect(result.suspended).toEqual([]);
    expect(result.unwritten).toEqual([ACCOUNT_A]);
  });

  it('🔴 a suspension is not re-written when it is already the one on the record', async () => {
    const mod = await bootBackground();
    seedTargets([grok(ACCOUNT_A)]);
    seedScope(ACCOUNT_A, leaseRecord('lease-a', 'salt-1'));
    const observed = leaseRecord('lease-b', 'salt-1');
    await mod.applyAccountObservation(await browserStore(), PLATFORM, observed, 42);
    const after = JSON.stringify(storedHeader(ACCOUNT_A));
    const second = await mod.applyAccountObservation(await browserStore(), PLATFORM, observed, 100);
    expect(second.suspended).toEqual([]);
    expect(JSON.stringify(storedHeader(ACCOUNT_A))).toBe(after);
  });
});

// ---------------------------------------------------------------------------
// It has to survive a service-worker restart, like every other record here.
// ---------------------------------------------------------------------------
describe('W199-K · the suspension survives a reclaim', () => {
  it('🔴 a suspended scope is still held after the module goes away', async () => {
    seedTargets([grok(ACCOUNT_A)]);
    await bootBackground();
    await enableBackfill();
    await openTab(1);
    store[stateKey(PLATFORM, ACCOUNT_A)] = {
      v: 2, platform: PLATFORM, scope: ACCOUNT_A, totalKnown: null, totalSource: 'unknown',
      enumCursor: { offset: 0, complete: false }, pendingCount: 0, archivedCount: 0,
      detailToday: { day: '2023-11-14', count: 0 },
      accountLease: { value: 'lease-a', saltId: 'salt-1', source: 'response-body-platform-uid', at: 1 },
      suspended: { at: 1, reason: 'account-changed', observed: { value: 'lease-b', saltId: 'salt-1', source: 'response-body-platform-uid' } },
      halted: null,
    } satisfies BackfillHeader;

    const mod = await restartServiceWorker();
    runtimeNow += 60 * 60_000;
    const result = await mod.runAlarmTick();

    expect(result.ran).toBe(false);
    expect(contentFetches).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// The salt the lease is keyed with is the one thing that must never be rebuilt.
// ---------------------------------------------------------------------------
describe('W199-L · the install keeps one salt across the whole mechanism', () => {
  it('🔴 a run creates the salt once and reuses it', async () => {
    const s = memoryStore();
    const { accountLeaseForScope } = await import('../lib/backfill/account-lease');
    const first = await accountLeaseForScope(PLATFORM, ACCOUNT_A, s, 1);
    const second = await accountLeaseForScope(PLATFORM, ACCOUNT_A, s, 2);
    if (first.kind !== 'lease' || second.kind !== 'lease') throw new Error('fixture');
    expect(first.lease.saltId).toBe(second.lease.saltId);
    expect(first.lease.value).toBe(second.lease.value);
    expect(s.data[ACCOUNT_SALT_KEY]).toBeDefined();
  });
});

/**
 * The real store the background module sees — its `storage.local` is {@link store}, and
 * `browserLocalStore()` memoises per area, which is the same object throughout this file.
 */
async function browserStore() {
  const { browserLocalStore } = await import('../lib/backfill/store');
  return browserLocalStore();
}

/** The trace the last walk wrote, which is where `schedule.skipped` survives the tick. */
function lastTickSkipped(): Array<{ platform: string; reason: string }> {
  const record = store['cs_backfill_lasttick_v1'] as { schedule?: { skipped?: unknown } } | undefined;
  const skipped = record?.schedule?.skipped;
  return Array.isArray(skipped) ? (skipped as Array<{ platform: string; reason: string }>) : [];
}
