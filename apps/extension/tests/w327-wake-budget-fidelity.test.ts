/**
 * W327 · Exercise the shipped wake budget as a cycle, not a rate equation.
 *
 * The real background entry point handles each one-shot alarm, runs the real
 * engine and pacer, and re-arms only after the tick settles. The browser clock,
 * alarms, platform and native-host coordination endpoint are deterministic
 * fixtures. Every transport request consumes virtual time, so a re-arm cannot
 * accidentally be measured from the beginning of the tick.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import { withI18n } from './i18n-harness';
import { withChatGptLeaseIdentity } from './chatgpt-lease-fixtures';
import { createSyntheticHost } from './synthetic-native-host';
import { BACKFILL_ALARM_NAME } from '../lib/backfill/alarm';
import { DAY_SLOW_INTERVAL_FACTOR, DAY_SLOW_KEY, daySlowPlan } from '../lib/backfill/day-slow';
import { stateKey } from '../lib/backfill/types';
import { SPEED_PLANS } from '../lib/backfill/speed';

const ORIGIN = 'https://chatgpt.com';
const START = new Date(2026, 9, 1, 12, 0, 0, 0).getTime();
const ENABLED_KEY = 'cs_backfill_enabled_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';
const IDS = [
  'd1111111-0000-4000-8000-000000000001',
  'd2222222-0000-4000-8000-000000000002',
  'd3333333-0000-4000-8000-000000000003',
  'd4444444-0000-4000-8000-000000000004',
  'd5555555-0000-4000-8000-000000000005',
  'd6666666-0000-4000-8000-000000000006',
  'd7777777-0000-4000-8000-000000000007',
  'd8888888-0000-4000-8000-000000000008',
];

const stored: Record<string, unknown> = {};
const alarmBook = new Map<string, { dueAt: number; delayInMinutes?: number; periodInMinutes?: number }>();
const alarmListeners: Array<(alarm: { name?: string }) => void> = [];
const runtimeListeners: Array<(m: any, s: any, r: (v?: unknown) => void) => unknown> = [];
let now = START;
const requestEvents: Array<{ kind: 'enumerate' | 'detail'; at: number; end: number; status: number }> = [];
let rateLimitDetail = false;
let alarmRandomValue = 1;
const cycleRandom = () => alarmRandomValue;
let deliveryHost = createSyntheticHost({ up: true });

const clock = {
  now: () => now,
  async sleep(ms: number) { now += ms; },
};

/** Deterministic model of the native host's coordination contract, on this clock. */
const host = (() => {
  let owner: string | null = null;
  let nextDetail = 0;
  let cooldownUntil = 0;
  let detailCount = 0;
  let detailDay = '';
  let active = 2;
  const messages: Array<Record<string, unknown>> = [];
  const detailTokens: Array<{ at: number; granted: boolean; waitMs: number }> = [];
  return {
    messages,
    detailTokens,
    sendNativeMessage: async (_name: string, raw: unknown) => {
      const message = raw as Record<string, unknown>;
      messages.push(message);
      if (message.type !== 'coordination') return deliveryHost.sendNativeMessage(_name, raw);
      const mode = String(message.mode);
      let granted = true;
      let waitMs = 0;
      if (mode === 'claim') {
        waitMs = Math.max(cooldownUntil - now, owner && owner !== message.install_id ? 120_000 : 0);
        granted = waitMs === 0;
        if (granted) owner = String(message.install_id);
      } else if (mode === 'token' && message.segment === 'detail') {
        const today = new Date(now).toISOString().slice(0, 10);
        if (detailDay !== today) { detailDay = today; detailCount = 0; }
        waitMs = Math.max(cooldownUntil - now, nextDetail - now, detailCount >= 400 ? 60_000 : 0);
        granted = owner === message.install_id && waitMs <= 0;
        detailTokens.push({ at: now, granted, waitMs });
        if (granted) {
          nextDetail = now + (active > 1 ? 45_000 : 20_000);
          detailCount += 1;
        }
      } else if (mode === 'token') {
        granted = owner === message.install_id && cooldownUntil <= now;
      } else if (mode === 'rate_limit') {
        const retry = typeof message.retry_after_ms === 'number' ? message.retry_after_ms : 0;
        cooldownUntil = Math.max(cooldownUntil, now + Math.max(60_000, retry));
        waitMs = cooldownUntil - now;
      } else if (mode === 'release' && owner === message.install_id) {
        owner = null;
      }
      return {
        protocol: 1, type: 'coordination', ok: true,
        request_id: String(message.request_id), granted,
        active_installs: active, gentle: active > 1,
        cooldown_until: cooldownUntil, wait_ms: Math.max(0, waitMs),
      };
    },
    reset() {
      owner = null; nextDetail = 0; cooldownUntil = 0; detailCount = 0; detailDay = ''; active = 2;
      messages.length = 0;
      detailTokens.length = 0;
      deliveryHost = createSyntheticHost({ up: true });
    },
    async probeClaim() {
      return this.sendNativeMessage('fixture-host', {
        type: 'coordination', mode: 'claim', platform: 'chatgpt',
        install_id: 'w327-probe-install', request_id: 'w327-cooldown-probe',
      });
    },
  };
})();

const fakeBrowser: any = {
  runtime: {
    id: 'w327-test-extension',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    sendNativeMessage: host.sendNativeMessage,
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...stored };
        const out: Record<string, unknown> = {};
        for (const key of Object.keys(defaults)) out[key] = key in stored ? stored[key] : defaults[key];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(stored, values); },
      async remove(keys: string[]) { for (const key of keys) delete stored[key]; },
    },
  },
  alarms: {
    create(name: string, info: { delayInMinutes?: number; periodInMinutes?: number }) {
      alarmBook.set(name, { ...info, dueAt: now + (info.delayInMinutes ?? info.periodInMinutes ?? 0) * 60_000 });
    },
    async clear(name: string) { return alarmBook.delete(name); },
    async get(name: string) { return alarmBook.get(name); },
    onAlarm: { addListener(fn: (alarm: { name?: string }) => void) { alarmListeners.push(fn); } },
  },
  tabs: { async sendMessage() { throw new Error('W327 uses the injected same-origin transport'); } },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
};

function transport() {
  return withChatGptLeaseIdentity(async (url: string) => {
    const start = now;
    const path = new URL(url).pathname;
    // Each platform response takes seven virtual seconds, independently of the
    // pacing delay before it. That duration must be included before rearming.
    now += 7_000;
    if (path === '/backend-api/conversations') {
      requestEvents.push({ kind: 'enumerate', at: start, end: now, status: 200 });
      const offset = Number(new URL(url).searchParams.get('offset') ?? '0');
      return { status: 200, text: JSON.stringify({ items: IDS.slice(offset).map((id) => ({ id })), total: IDS.length }) };
    }
    if (rateLimitDetail) {
      rateLimitDetail = false;
      requestEvents.push({ kind: 'detail', at: start, end: now, status: 429 });
      return { status: 429, retryAfter: '60', text: '' };
    }
    const id = decodeURIComponent(path.replace('/backend-api/conversation/', ''));
    requestEvents.push({ kind: 'detail', at: start, end: now, status: 200 });
    return {
      status: 200,
      text: JSON.stringify({
        mapping: { n1: { id: 'n1', message: { content: { parts: [`synthetic ${id}`] } } } },
        current_node: 'n1', account_id: 'acct-fixture-1',
      }),
    };
  });
}

async function bootWorker(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  // 20–45 s pacing: choose the upper edge, and use the production preset so W296
  // will transform the plan after the 429. Two detail bodies make the host's
  // active-install 45 s interval observable within the same tick.
  mod.configureBackfillPace({ clock, random: cycleRandom, preset: 'standard' });
  mod.configureBackfillTransport(transport());
  await mod.default();
  await mod.backgroundSetupSettled();
  return mod;
}

async function fireTick(mod: any): Promise<void> {
  const armed = alarmBook.get(BACKFILL_ALARM_NAME);
  expect(armed).toBeDefined();
  if (!armed) return;
  expect(armed.dueAt).toBeGreaterThanOrEqual(now + 60_000);
  now = armed.dueAt;
  alarmBook.delete(BACKFILL_ALARM_NAME); // a one-shot is removed before its handler runs
  const listener = alarmListeners.find((fn) => fn !== undefined);
  if (!listener) throw new Error('background did not register the alarm listener');
  listener({ name: BACKFILL_ALARM_NAME });
  await mod.backfillTickSettled();
}

beforeEach(() => {
  for (const key of Object.keys(stored)) delete stored[key];
  alarmBook.clear();
  alarmListeners.length = 0;
  runtimeListeners.length = 0;
  requestEvents.length = 0;
  now = START;
  rateLimitDetail = false;
  host.reset();
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (callback: any) => callback);
  vi.resetModules();
  stored[ENABLED_KEY] = true;
  stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'default', at: START }];
});

describe('W327 · wake budget fidelity on the real tick path', () => {
  it('rearms after request completion, carries its anchor over worker restart, and composes day-slow with Retry-After', async () => {
    const firstWorker = await bootWorker();
    await fireTick(firstWorker);

    const firstDetails = requestEvents.filter((event) => event.kind === 'detail');
    expect(firstDetails).toHaveLength(2);
    expect(firstDetails[1]!.at - firstDetails[0]!.at).toBe(45_000);
    // Each fake request lasts 7 s; the first alarm is armed after the second one
    // ends, then gets its own whole-minute delay.
    const nextAlarm = alarmBook.get(BACKFILL_ALARM_NAME)!;
    expect(nextAlarm.dueAt).toBe(requestEvents.at(-1)!.end + 120_000);
    expect(nextAlarm.delayInMinutes).toBe(2);
    const savedHeader = stored[stateKey('chatgpt', 'default')] as { lastFetchAt?: { detail?: number | null } };
    expect(savedHeader.lastFetchAt?.detail).toBe(firstDetails.at(-1)!.at);
    expect(host.detailTokens.slice(0, 2)).toEqual([
      { at: firstDetails[0]!.at, granted: true, waitMs: 0 },
      { at: firstDetails[1]!.at, granted: true, waitMs: 0 },
    ]);

    // Reclaim the worker exactly between one-shot ticks. Storage and the browser
    // alarm survive; module state and event listeners do not.
    alarmListeners.length = 0;
    runtimeListeners.length = 0;
    vi.resetModules();
    const secondWorker = await bootWorker();
    expect((stored[stateKey('chatgpt', 'default')] as { lastFetchAt?: { detail?: number | null } }).lastFetchAt?.detail)
      .toBe(savedHeader.lastFetchAt?.detail);
    rateLimitDetail = true;
    await fireTick(secondWorker);
    expect(requestEvents.at(-1)).toMatchObject({ kind: 'detail', status: 429 });
    expect(stored[DAY_SLOW_KEY]).toBeDefined();
    const limitReport = host.messages.find((message) => message.mode === 'rate_limit');
    expect(limitReport?.retry_after_ms).toBe(60_000);
    const cooldown = await host.probeClaim() as { granted?: boolean; wait_ms?: number };
    expect(cooldown.granted).toBe(false);
    expect(cooldown.wait_ms).toBe(60_000);
    const daySlow = stored[DAY_SLOW_KEY] as { platforms: Record<string, { at: number; until: number }> };
    expect(daySlow.platforms.chatgpt!.at).toBe(requestEvents.at(-1)!.end);
    expect(daySlow.platforms.chatgpt!.until).toBeGreaterThan(daySlow.platforms.chatgpt!.at);
    expect(new Date(daySlow.platforms.chatgpt!.at).getDate()).toBe(new Date(START).getDate());
    const slowed = daySlowPlan(SPEED_PLANS.standard);
    expect(slowed.pace.detail.minIntervalMs).toBe(20_000 * DAY_SLOW_INTERVAL_FACTOR);
    expect(slowed.pace.detail.minIntervalMs + slowed.pace.detail.jitterMs!).toBe(90_000);
    expect(host.detailTokens.every((token) => token.granted)).toBe(true);

    // The 429's host cooldown is shorter than this alarm's next wake. Prove the
    // cooldown is held immediately, then let the next one-minute wake exercise
    // the stored Retry-After retry. A further one-minute wake makes W296's
    // doubled 20–45 s pacing observable: the next request must start 90 s after
    // the resumed request began.
    alarmRandomValue = 0;
    await fireTick(secondWorker);
    const afterLimit = requestEvents.filter((event) => event.kind === 'detail');
    expect(afterLimit.at(-1)).toMatchObject({ status: 200 });
    expect(alarmBook.get(BACKFILL_ALARM_NAME)!.dueAt)
      .toBe(requestEvents.at(-1)!.end + 60_000);
    expect(host.detailTokens.at(-1)).toMatchObject({ granted: true, waitMs: 0 });
    expect(new Date(afterLimit.at(-1)!.at).getDate()).toBe(new Date(START).getDate());

    const resumedDetail = afterLimit.at(-1)!;
    await fireTick(secondWorker);
    const finalDetail = requestEvents.filter((event) => event.kind === 'detail').at(-1)!;
    expect(finalDetail.status).toBe(200);
    expect(finalDetail.at - resumedDetail.at).toBe(67_000);
    expect(host.detailTokens.at(-1)).toMatchObject({ granted: true, waitMs: 0 });
    expect(new Date(finalDetail.at).getDate()).toBe(new Date(START).getDate());

    // This one-minute alarm's lower-edge pace fits its 67 s wake-plus-request
    // gap. Its re-arm is also one minute; set the next pacing draw to the upper
    // edge so that tick waits the remaining 23 s of W296's 90 s interval.
    await fireTick(secondWorker);
    const pacedDetail = requestEvents.filter((event) => event.kind === 'detail').at(-1)!;
    expect(pacedDetail.status).toBe(200);
    expect(pacedDetail.at - finalDetail.at).toBe(67_000);
    alarmRandomValue = 1;
    await fireTick(secondWorker);
    const slowedDetail = requestEvents.filter((event) => event.kind === 'detail').at(-1)!;
    expect(slowedDetail.status).toBe(200);
    expect(slowedDetail.at - pacedDetail.at).toBe(90_000);
    expect(host.detailTokens.at(-1)).toMatchObject({ granted: true, waitMs: 0 });
    expect(new Date(slowedDetail.at).getDate()).toBe(new Date(START).getDate());
  }, 15_000);
});
