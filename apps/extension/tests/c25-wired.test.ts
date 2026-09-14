/**
 * C25 · The backfill leg in the production background must be reachable.
 *
 * These assertions enter only through defineBackground's real callbacks; runBackfill is the
 * one network/engine boundary and is replaced with a spy here so the test sends no real
 * request. Alarms, storage events, target registration and the tab port still go through
 * background's production code.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';

const runBackfillSpy = vi.fn(async (_opts: any) => ({
  stopped: 'queue-empty',
  enumeratedPages: 0,
  newDebts: 0,
  archivedThisRun: [],
  failedThisRun: [],
  skippedAlreadyArchived: 0,
  skippedAlreadyPending: 0,
  progress: 'stub',
  halted: null,
  paceTrace: { enumerate: [], detail: [] },
  state: {},
}));

vi.mock('../lib/backfill/engine', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/backfill/engine')>();
  return { ...actual, runBackfill: (opts: any) => runBackfillSpy(opts) };
});

const ORIGIN = 'https://chatgpt.com';
const ENABLED_KEY = 'cs_backfill_enabled_v1';
const TARGETS_KEY = 'cs_backfill_targets_v1';
const TABS_KEY = 'cs_backfill_tabs_v1';

const stored: Record<string, unknown> = {};
const alarmBook = new Map<string, { periodInMinutes?: number }>();
const alarmCreates: string[] = [];
const alarmClears: string[] = [];
const runtimeListeners: Array<(message: any, sender: any, sendResponse: (value: any) => void) => any> = [];
const alarmListeners: Array<(alarm: { name?: string }) => void> = [];
const startupListeners: Array<() => void> = [];
const storageChangeListeners: Array<(changes: Record<string, { newValue?: unknown }>, area: string) => void> = [];

const fakeBrowser: any = {
  runtime: {
    id: 'c25-test-extension',
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    onStartup: { addListener(fn: any) { startupListeners.push(fn); } },
  },
  storage: {
    onChanged: { addListener(fn: any) { storageChangeListeners.push(fn); } },
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...stored };
        const result: Record<string, unknown> = {};
        for (const key of Object.keys(defaults)) result[key] = key in stored ? stored[key] : defaults[key];
        return result;
      },
      async set(values: Record<string, unknown>) {
        const changes: Record<string, { newValue?: unknown }> = {};
        for (const [key, value] of Object.entries(values)) {
          stored[key] = value;
          changes[key] = { newValue: value };
        }
        for (const listener of storageChangeListeners) listener(changes, 'local');
      },
    },
  },
  alarms: {
    create(name: string, info: { periodInMinutes?: number }) {
      alarmBook.set(name, info);
      alarmCreates.push(name);
    },
    async clear(name: string) {
      alarmBook.delete(name);
      alarmClears.push(name);
      return true;
    },
    async get(name: string) { return alarmBook.get(name); },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    async sendMessage(_tabId: number, message: any) {
      if (message?.type === 'cs-backfill-ping') return { ok: true, origin: ORIGIN };
      throw new Error('network must not be reached in C25');
    },
  },
  action: {
    async setBadgeText() {},
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
};

function seedTargetAndLiveTab(): void {
  stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
  stored[TABS_KEY] = [{ tabId: 25, origin: ORIGIN, at: 1 }];
}

async function boot(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  // main() is synchronous now (MV3: listeners before any await); its async setup
  // is awaited through the exported handle instead.
  expect(mod.default()).toBeUndefined();
  await mod.backgroundSetupSettled();
  return mod;
}

async function waitForAsyncEvent(): Promise<void> {
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
}

beforeEach(async () => {
  for (const key of Object.keys(stored)) delete stored[key];
  alarmBook.clear();
  alarmCreates.length = 0;
  alarmClears.length = 0;
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  startupListeners.length = 0;
  storageChangeListeners.length = 0;
  runBackfillSpy.mockClear();
  vi.resetModules();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (callback: any) => callback);
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C25 · background wiring', () => {
  it('the alarm is created when the switch goes on and cleared at once via the storage change when it goes off', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { BACKFILL_ALARM_NAME, BACKFILL_SAFETY_ALARM_NAME } = await import('../lib/backfill/alarm');

    await setBackfillEnabled(browserLocalStore(), true);
    await boot();
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
    // 🔴 W16 · Two alarms now, and the create order is the one syncBackfillAlarm
    //    writes: the jittered tick first, then the watchdog. The criterion this
    //    case guards — "switch on ⇒ the wiring really creates the alarm(s), and
    //    the storage change really clears them" — is unchanged; the list grew
    //    because the design grew a second alarm, not because the assertion was
    //    loosened.
    expect(alarmCreates).toEqual([BACKFILL_ALARM_NAME, BACKFILL_SAFETY_ALARM_NAME]);

    await setBackfillEnabled(browserLocalStore(), false);
    await waitForAsyncEvent();
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
    // 🔴 Both, or a disabled leg would keep waking up on the watchdog's period.
    expect(alarmBook.has(BACKFILL_SAFETY_ALARM_NAME)).toBe(false);
    expect(alarmClears).toContain(BACKFILL_ALARM_NAME);
    expect(alarmClears).toContain(BACKFILL_SAFETY_ALARM_NAME);
  });

  it('main() is synchronous and registers onStartup / onAlarm before its async setup runs', async () => {
    const mod: any = await import('../entrypoints/background');
    const startupBefore = startupListeners.length;
    const alarmBefore = alarmListeners.length;
    const returned = mod.default();
    // Checked before awaiting anything: a listener added after an await would
    // miss the event that woke a reclaimed worker.
    expect(returned).toBeUndefined();
    expect(startupListeners.length).toBe(startupBefore + 1);
    expect(alarmListeners.length).toBe(alarmBefore + 1);
    await mod.backgroundSetupSettled();
  });

  it('starting with the default (off) creates no alarm, and an alarm event runs no backfill either', async () => {
    const mod = await boot();
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');

    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
    expect(alarmCreates).toEqual([]);

    alarmListeners[0]!({ name: BACKFILL_ALARM_NAME });
    await mod.backfillTickSettled();
    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('disabled');
  });

  it('a matching onAlarm walks from the production entry point through one backfill tick, without any real network', async () => {
    stored[ENABLED_KEY] = true;
    seedTargetAndLiveTab();
    const mod = await boot();
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');

    alarmListeners[0]!({ name: BACKFILL_ALARM_NAME });
    await mod.backfillTickSettled();

    expect(runBackfillSpy).toHaveBeenCalledTimes(1);
    expect(runBackfillSpy.mock.calls[0]![0].http).toEqual(expect.any(Function));
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
  });

  it('switch on but no port available returns no-http-port explicitly, without calling the engine or throwing', async () => {
    stored[ENABLED_KEY] = true;
    stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
    const mod = await boot();
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');

    alarmListeners[0]!({ name: BACKFILL_ALARM_NAME });
    await expect(mod.backfillTickSettled()).resolves.toMatchObject({
      ran: false,
      reason: 'no-http-port',
    });
    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('no-http-port');
  });
});
