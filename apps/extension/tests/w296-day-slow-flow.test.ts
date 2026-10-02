/**
 * W296 · **The flow: a 429 seen by a real round arms the platform brake, and the
 * next round obeys it.**
 *
 * `w296-day-slow.test.ts` is the pure half — the predicate, the window, the record
 * and the plan transform, all as functions. This file is the other half, and it is
 * separate because it boots the **real entry point**: a `chat-captured` message
 * arrives at background's registered listener, the leg runs through
 * `kickBackfill` → `coordinatedTick` → the request gateway → the engine, and the
 * assertions are about what the run left in `storage.local`.
 *
 * ## What is real here and what is faked, said plainly
 *
 *  · **Real**: `entrypoints/background.ts` imported and booted, with every one of
 *    its own gates, registries and the real gateway that arms the brake; the real
 *    `runBackfill` behind it; the real `browserLocalStore()` write path.
 *  · **Faked**: the browser (storage/tabs/alarms/runtime), the native host (the
 *    synthetic coordination responder), and the network — the transport is a pure
 *    function in this file. No request leaves the process.
 *
 * ## Why the third case counts requests instead of reading a number
 *
 * "The plan is slower" is only worth anything if the slower plan is what the round
 * actually used: `daySlowPlan` is asserted in the pure suite, and here the count of
 * **detail requests on the wire** is the observable. That number is the product of
 * the per-tick budget (`maxDetails`) and nothing else, so it distinguishes four
 * bodies (`faster`) from one (`faster`, slowed) without reading any internal.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { withChatGptLeaseIdentity } from './chatgpt-lease-fixtures';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import { DAY_SLOW_KEY, localMidnightAfter } from '../lib/backfill/day-slow';
import type { CapturedFetch } from '../lib/contract';
import type { HttpResponse } from '../lib/backfill/engine';
import type { BackfillRequestInit } from '../lib/backfill/enumerate';

const ORIGIN = 'https://chatgpt.com';
const LIST_PATH = '/backend-api/conversations';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
  'c3333333-0000-4000-8000-000000000003',
  'c4444444-0000-4000-8000-000000000004',
  'c5555555-0000-4000-8000-000000000005',
  'c6666666-0000-4000-8000-000000000006',
];

/** The local-day window this suite runs in: local noon, so no advance used here crosses midnight. */
const START = new Date(2026, 9, 1, 12, 0, 0, 0).getTime();

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: unknown, s: unknown, r: (v?: unknown) => void) => unknown> = [];
let host: SyntheticHost;
/** Every URL the injected transport was asked for, in order. */
let requests: string[] = [];

let runtimeNow = START;
const runtimeClock = {
  now: () => runtimeNow,
  sleep: async (ms: number) => { runtimeNow += ms; },
};

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: unknown) { runtimeListeners.push(fn as never); } },
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
    create() {},
    async clear() { return false; },
    async get() { return undefined; },
    onAlarm: { addListener() {} },
  },
  tabs: {
    async sendMessage() { throw new Error('the test transport is injected through the seam'); },
  },
};

function liveCapture(): CapturedFetch {
  const sid = 'aaaaaaaa-1111-2222-3333-444444444444';
  return {
    url: `${ORIGIN}/backend-api/conversation/${sid}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-fixture-1' }),
    pageUrl: `${ORIGIN}/c/${sid}`,
    capturedAt: START,
  };
}

/** The injected transport: a pure function, recorded, with no network behind it. */
function transport(handler: (url: string, init?: BackfillRequestInit) => HttpResponse) {
  return async (url: string, init?: BackfillRequestInit): Promise<HttpResponse> => {
    requests.push(url);
    return handler(url, init);
  };
}

const always = (status: number) => transport(() => ({ status, text: '' }));

/** A healthy synthetic backend: a list page, then one body per conversation id. */
const healthy = () => transport((url) => {
  const u = new URL(url);
  if (u.pathname === LIST_PATH) {
    const offset = Number(u.searchParams.get('offset') ?? '0');
    return { status: 200, text: JSON.stringify({ items: IDS.slice(offset).map((id) => ({ id })), total: IDS.length }) };
  }
  const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
  return {
    status: 200,
    text: JSON.stringify({
      mapping: { n1: { id: 'n1', message: { content: { parts: [`synthetic ${id}`] } } } },
      current_node: 'n1',
      account_id: 'acct-fixture-1',
    }),
  };
});

const detailRequests = () => requests.filter((url) => new URL(url).pathname.startsWith('/backend-api/conversation/'));

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

async function dispatch(message: unknown, tabId?: number): Promise<void> {
  const sender = tabId === undefined ? { id: 's' } : { id: 's', tab: { id: tabId } };
  await new Promise<void>((resolve) => {
    const done: (v?: unknown) => void = () => resolve();
    const ret = runtimeListeners[0]!(message, sender, done);
    if (ret !== true) resolve();
  });
}

/** The fields of `lastBackfillTick()` this suite reads. */
interface RoundTick {
  ran?: boolean;
  reason?: string;
  report?: { stopped?: unknown; halted?: { reason?: unknown } | null } | null;
}

/** One real round: a capture arrives, the leg runs, the tick settles. */
async function runRound(mod: any, tabId = 42): Promise<RoundTick | null> {
  await dispatch({ type: 'chat-captured', payload: liveCapture() }, tabId);
  await mod.backfillTickSettled();
  return mod.lastBackfillTick() as RoundTick | null;
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  requests = [];
  runtimeNow = START;
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
});

describe('W296-D · a 429 on a real round arms the brake at the gateway', () => {
  it('🔴 the round stops on the 429 and storage carries the platform brake until the next local midnight', async () => {
    const mod = await bootBackground();
    mod.configureBackfillTransport(withChatGptLeaseIdentity(always(429)));

    const tick = await runRound(mod);
    expect(tick?.report).toMatchObject({ halted: { reason: 'rate-limited' } });

    const record = store[DAY_SLOW_KEY] as { v: number; platforms: Record<string, { until: number; at: number }> };
    expect(record).toBeDefined();
    expect(Object.keys(record.platforms)).toEqual(['chatgpt']);
    const brake = record.platforms.chatgpt!;
    // The window really is "the rest of the local day", computed from the clock the
    // tick itself runs on rather than from the wall clock.
    expect(brake.until).toBe(localMidnightAfter(brake.at));
    expect(brake.at).toBeGreaterThanOrEqual(START);
    expect(brake.at).toBeLessThanOrEqual(runtimeNow);
    expect(brake.until).toBeGreaterThan(brake.at);
  });

  it('🔴 a 403 is a refusal but not this brake: no record is written', async () => {
    const mod = await bootBackground();
    mod.configureBackfillTransport(withChatGptLeaseIdentity(always(403)));

    const tick = await runRound(mod);
    expect(tick?.report).toMatchObject({ halted: { reason: 'refused-unknown' }, state: { suspended: { reason: 'request-refused' } } });
    expect(store[DAY_SLOW_KEY]).toBeUndefined();
  });

  it('🔴 a healthy round writes no brake at all', async () => {
    const mod = await bootBackground();
    mod.configureBackfillTransport(withChatGptLeaseIdentity(healthy()));

    const tick = await runRound(mod);
    expect(tick?.report?.halted ?? null).toBeNull();
    expect(store[DAY_SLOW_KEY]).toBeUndefined();
  });
});

describe('W296-E · the next round obeys the brake', () => {
  it('🔴 a `faster` install fetches four bodies a round, and one once the brake is armed', async () => {
    store['cs_backfill_speed_v1'] = 'faster';
    const mod = await bootBackground();
    mod.configureBackfillTransport(withChatGptLeaseIdentity(healthy()));

    requests = [];
    await runRound(mod);
    const unbraked = detailRequests().length;
    console.log('[W296-E] details fetched with no brake:', unbraked);
    expect(unbraked).toBeGreaterThan(1);

    // Arm the brake exactly as the gateway would, then let the next round run.
    store[DAY_SLOW_KEY] = {
      v: 1,
      platforms: { chatgpt: { until: localMidnightAfter(runtimeNow), at: runtimeNow } },
    };
    requests = [];
    await runRound(mod);
    const braked = detailRequests().length;
    console.log('[W296-E] details fetched with the brake armed:', braked);
    expect(braked).toBe(1);
    expect(braked).toBeLessThan(unbraked);
  });
});
