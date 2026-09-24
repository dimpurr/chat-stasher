/**
 * W90 · **The tick's own re-arm obeys the backfill switch.**
 *
 * ## The defect this file exists for
 *
 * `runAlarmTick` ends in a `finally` that arms the next jittered one-shot
 * (`entrypoints/background.ts`, `rearmBackfillTick`). W87 routed every
 * switch-driven alarm sync through `alarmSyncQueue`, but deliberately left this
 * one writer out: it called `armBackfillTick` directly and never read the switch.
 *
 * So an off committed during an in-flight tick was overwritten by the tick's own
 * `finally`: the switch's queued sync cleared both alarms, and the tick, when it
 * finished, re-created `cs-backfill-tick`. Every later `disabled` tick then did
 * the same, because its own `finally` re-armed again — the switch said off and the
 * worker kept waking for the rest of the day. No platform request is made on those
 * ticks (the `disabled` gate stops them), which is exactly why the defect is easy
 * to miss; the damage is the endless wake-ups, not the traffic.
 *
 * ## What is asserted, and why each one is a different claim
 *
 *  1. **off during an in-flight tick ⇒ nothing is armed after the tick, and no
 *     later `disabled` wake re-arms it.** The tick is parked inside its first
 *     platform request (the injected transport) and the switch is turned off
 *     while it is parked. This is the regression: it is **red** against the
 *     unfixed code, where the `finally` re-creates the alarm.
 *  2. **on, with no interleaving ⇒ re-armed exactly as before**, with a fresh
 *     draw. This is a guard: the fix must not stop the ordinary re-arm. It
 *     passes before and after, which is the point.
 *  3. **on → off → on during a tick ⇒ ends on and armed.** The two toggles are
 *     queued before the tick's re-arm, so the last decision wins.
 *
 * 🔴 The interleaving in 1 and 3 is forced, not raced: the tick is *parked* on a
 *    promise only the test resolves, so "the tick is in flight while the switch
 *    changes" is a fact rather than a hope about scheduling. A test that merely
 *    raced the two would pass on the unfixed code whenever the shorter path won.
 *
 * Everything enters through the real `onAlarm` listener and the real
 * `storage.onChanged` listener; only the network is replaced (the engine's http
 * port is injected) and no request really goes out.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';

import {
  BACKFILL_ALARM_NAME,
  BACKFILL_SAFETY_ALARM_NAME,
  BACKFILL_TICK_DELAY_MAX_MINUTES,
  BACKFILL_TICK_DELAY_MIN_MINUTES,
} from '../lib/backfill/alarm';
import type { RandomFn } from '../lib/backfill/random';

const ORIGIN = 'https://chatgpt.com';
const ENABLED_KEY = 'cs_backfill_enabled_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';

const stored: Record<string, unknown> = {};
const alarmBook = new Map<string, { periodInMinutes?: number; delayInMinutes?: number }>();
const alarmListeners: Array<(alarm: { name?: string }) => void> = [];
const storageChangeListeners: Array<
  (changes: Record<string, { newValue?: unknown }>, area: string) => void
> = [];
const listCalls: string[] = [];

/** A deterministic draw: the boundary values are known, so an exact delay can be asserted. */
const fixed = (v: number): RandomFn => () => v;

/**
 * The park the in-flight tick waits on. One-shot: only the first platform request
 * parks, and only the test resolves it, so the tick cannot leave until told to.
 */
let parkNextHttp = false;
let httpGate: Promise<void> | null = null;
let releaseHttp: (() => void) | null = null;
let httpParked = false;

function armHttpGate(): void {
  parkNextHttp = true;
  httpParked = false;
  httpGate = new Promise<void>((resolve) => { releaseHttp = resolve; });
}

function releaseHttpGate(): void {
  const release = releaseHttp;
  releaseHttp = null;
  parkNextHttp = false;
  release?.();
}

const fakeBrowser: any = {
  runtime: {
    id: 'w90-rearm-test-extension',
    onMessage: { addListener() {} },
    onStartup: { addListener() {} },
  },
  storage: {
    onChanged: { addListener(fn: any) { storageChangeListeners.push(fn); } },
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...stored };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in stored ? stored[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(stored, values); },
      async remove(keys: string[]) { for (const k of keys) delete stored[k]; },
    },
  },
  alarms: {
    create(name: string, info: { periodInMinutes?: number; delayInMinutes?: number }) {
      alarmBook.set(name, info);
    },
    async clear(name: string) {
      const had = alarmBook.delete(name);
      return had;
    },
    async get(name: string) { return alarmBook.get(name); },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: { async sendMessage() { throw new Error('no live tab in W90'); } },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
};

/** The runtime seam's clock. This file never sleeps, so a constant "now" is enough. */
const runtimeClock = { now: () => 1_700_000_000_000, sleep: async () => { /* virtual */ } };

async function boot(drawSource: RandomFn): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock, random: drawSource });
  mod.configureBackfillTransport(async () => {
    if (parkNextHttp) {
      parkNextHttp = false;
      httpParked = true;
      await httpGate;
    }
    listCalls.push('list');
    return { status: 200, text: '{"items":[],"total":0}' };
  });
  expect(mod.default()).toBeUndefined();
  await mod.backgroundSetupSettled();
  return mod;
}

function fireAlarm(name: string): void {
  for (const fn of alarmListeners) fn({ name });
}

async function waitForAsyncEvent(): Promise<void> {
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
}

/** Turn the switch off/on the way the popup does: a storage write, then the real change event. */
async function setSwitch(mod: any, on: boolean): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), on);
  for (const fn of storageChangeListeners) fn({ [ENABLED_KEY]: { newValue: on } }, 'local');
  await waitForAsyncEvent();
  await waitForAsyncEvent();
}

beforeEach(() => {
  for (const k of Object.keys(stored)) delete stored[k];
  alarmBook.clear();
  alarmListeners.length = 0;
  storageChangeListeners.length = 0;
  listCalls.length = 0;
  parkNextHttp = false;
  httpGate = null;
  releaseHttp = null;
  httpParked = false;
  vi.resetModules();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

describe('W90 · the tick re-arm reads the switch through the one alarm queue', () => {
  it('🔴 off during an in-flight tick: nothing is armed after the tick, and no later disabled wake re-arms it', async () => {
    stored[ENABLED_KEY] = true;
    stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
    const mod = await boot(fixed(0));

    // The switch is on, so the boot-time sync armed both alarms.
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);

    // Park the tick inside its first platform request, exactly where a real tick
    // sits while the user reaches for the toggle.
    armHttpGate();
    // The one-shot the browser removed when it fired.
    alarmBook.delete(BACKFILL_ALARM_NAME);
    fireAlarm(BACKFILL_ALARM_NAME);
    for (let i = 0; i < 50 && !httpParked; i += 1) await waitForAsyncEvent();
    expect(httpParked).toBe(true);

    // The user turns backfill off while the tick is in flight. The switch's sync
    // clears both alarms — that half worked before W90 too.
    await setSwitch(mod, false);
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
    expect(alarmBook.has(BACKFILL_SAFETY_ALARM_NAME)).toBe(false);

    // Let the tick finish. On the unfixed code its `finally` re-creates the
    // one-shot here even though the switch is off.
    releaseHttpGate();
    await mod.backfillTickSettled();
    await waitForAsyncEvent();
    await waitForAsyncEvent();

    expect(stored[ENABLED_KEY]).toBe(false);
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);

    // …and a later wake with the switch still off must not re-arm it. This is the
    // "every later disabled tick re-arms again" half of the defect.
    await mod.runAlarmTick();
    await waitForAsyncEvent();
    fireAlarm(BACKFILL_SAFETY_ALARM_NAME); // the watchdog: disabled ⇒ no arm
    await mod.backfillTickSettled();
    await waitForAsyncEvent();
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
    expect(alarmBook.has(BACKFILL_SAFETY_ALARM_NAME)).toBe(false);
  });

  it('on, no interleaving: the tick re-arms the next one with a fresh draw, as before', async () => {
    stored[ENABLED_KEY] = true;
    stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
    let draw = 0;
    const mod = await boot(() => draw);

    expect(alarmBook.get(BACKFILL_ALARM_NAME)).toEqual({
      delayInMinutes: BACKFILL_TICK_DELAY_MIN_MINUTES,
    });

    // A new draw for the next gap: if the re-arm reused the old number this fails.
    draw = 1;
    alarmBook.delete(BACKFILL_ALARM_NAME);
    fireAlarm(BACKFILL_ALARM_NAME);
    await mod.backfillTickSettled();
    await waitForAsyncEvent();

    expect(alarmBook.get(BACKFILL_ALARM_NAME)).toEqual({
      delayInMinutes: BACKFILL_TICK_DELAY_MAX_MINUTES,
    });
    expect(listCalls.length).toBeGreaterThan(0);
  });

  it('on → off → on during a tick: the last decision wins and the leg ends armed', async () => {
    stored[ENABLED_KEY] = true;
    stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
    const mod = await boot(fixed(0));

    armHttpGate();
    alarmBook.delete(BACKFILL_ALARM_NAME);
    fireAlarm(BACKFILL_ALARM_NAME);
    for (let i = 0; i < 50 && !httpParked; i += 1) await waitForAsyncEvent();
    expect(httpParked).toBe(true);

    await setSwitch(mod, false);
    await setSwitch(mod, true);

    releaseHttpGate();
    await mod.backfillTickSettled();
    await waitForAsyncEvent();
    await waitForAsyncEvent();

    expect(stored[ENABLED_KEY]).toBe(true);
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
    expect(alarmBook.has(BACKFILL_SAFETY_ALARM_NAME)).toBe(true);
  });
});
