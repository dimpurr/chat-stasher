import { describe, expect, it } from 'vitest';
import {
  BACKFILL_ALARM_NAME,
  BACKFILL_SAFETY_ALARM_NAME,
  BACKFILL_SAFETY_PERIOD_MINUTES,
  syncBackfillAlarm,
  type AlarmsApi,
} from '../lib/backfill/alarm';

type AlarmInfo = { periodInMinutes?: number; delayInMinutes?: number };

function failingAlarms() {
  const live = new Map<string, AlarmInfo>();
  let failure: { operation: 'get' | 'create' | 'clear'; name: string; afterEffect: boolean } | undefined;
  const calls: string[] = [];

  function failNext(
    operation: 'get' | 'create' | 'clear',
    name: string,
    afterEffect = false,
  ) {
    failure = { operation, name, afterEffect };
  }

  function maybeFail(operation: 'get' | 'create' | 'clear', name: string, afterEffect: boolean) {
    if (failure?.operation !== operation || failure.name !== name || failure.afterEffect !== afterEffect) return;
    failure = undefined;
    throw new Error(`${operation} failed for ${name}`);
  }

  const api: AlarmsApi = {
    async get(name) {
      calls.push(`get:${name}`);
      maybeFail('get', name, false);
      return live.get(name);
    },
    async create(name, info) {
      calls.push(`create:${name}`);
      maybeFail('create', name, false);
      live.set(name, info);
      maybeFail('create', name, true);
    },
    async clear(name) {
      calls.push(`clear:${name}`);
      maybeFail('clear', name, false);
      const removed = live.delete(name);
      maybeFail('clear', name, true);
      return removed;
    },
  };

  return { api, live, calls, failNext };
}

function expectBothArmed(live: Map<string, AlarmInfo>) {
  expect([...live.keys()].sort()).toEqual([BACKFILL_ALARM_NAME, BACKFILL_SAFETY_ALARM_NAME].sort());
  expect(live.get(BACKFILL_ALARM_NAME)).toEqual({ delayInMinutes: 1 });
  expect(live.get(BACKFILL_SAFETY_ALARM_NAME)).toEqual({ periodInMinutes: BACKFILL_SAFETY_PERIOD_MINUTES });
}

describe('syncBackfillAlarm failure recovery', () => {
  it('a failed alarm lookup rejects instead of reporting a success, then a retry arms both alarms', async () => {
    const alarms = failingAlarms();
    alarms.failNext('get', BACKFILL_ALARM_NAME);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).rejects.toThrow('get failed');
    expect(alarms.live.size).toBe(0);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).resolves.toBe('created');
    expectBothArmed(alarms.live);
    expect(await syncBackfillAlarm(alarms.api, true, () => 0)).toBe('kept');
    expectBothArmed(alarms.live);
  });

  it('a failed tick create rejects instead of reporting created, then a retry arms both alarms', async () => {
    const alarms = failingAlarms();
    alarms.failNext('create', BACKFILL_ALARM_NAME);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).rejects.toThrow('create failed');
    expect(alarms.live.size).toBe(0);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).resolves.toBe('created');
    expectBothArmed(alarms.live);
    expect(await syncBackfillAlarm(alarms.api, true, () => 0)).toBe('kept');
    expectBothArmed(alarms.live);
  });

  it('a tick create that commits before rejecting is recovered without duplicating either alarm', async () => {
    const alarms = failingAlarms();
    alarms.failNext('create', BACKFILL_ALARM_NAME, true);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).rejects.toThrow('create failed');
    expect(alarms.live.size).toBe(1);
    expect(alarms.live.has(BACKFILL_ALARM_NAME)).toBe(true);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).resolves.toBe('created');
    expectBothArmed(alarms.live);
    expect(await syncBackfillAlarm(alarms.api, true, () => 0)).toBe('kept');
    expectBothArmed(alarms.live);
  });

  it('a failed watchdog lookup after the tick was created rejects, then a retry arms the watchdog', async () => {
    const alarms = failingAlarms();
    alarms.failNext('get', BACKFILL_SAFETY_ALARM_NAME);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).rejects.toThrow('get failed');
    expect([...alarms.live.keys()]).toEqual([BACKFILL_ALARM_NAME]);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).resolves.toBe('created');
    expectBothArmed(alarms.live);
    expect(await syncBackfillAlarm(alarms.api, true, () => 0)).toBe('kept');
    expectBothArmed(alarms.live);
  });

  it('a failed watchdog create rejects instead of reporting created, then a retry converges to both alarms', async () => {
    const alarms = failingAlarms();
    alarms.failNext('create', BACKFILL_SAFETY_ALARM_NAME);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).rejects.toThrow('create failed');
    expect([...alarms.live.keys()]).toEqual([BACKFILL_ALARM_NAME]);

    await expect(syncBackfillAlarm(alarms.api, true, () => 0)).resolves.toBe('created');
    expectBothArmed(alarms.live);
    expect(await syncBackfillAlarm(alarms.api, true, () => 0)).toBe('kept');
    expectBothArmed(alarms.live);
  });

  it.each([
    ['tick clear', BACKFILL_ALARM_NAME],
    ['watchdog clear after the tick was cleared', BACKFILL_SAFETY_ALARM_NAME],
  ] as const)('%s failure rejects instead of reporting cleared, then a retry clears both', async (_label, failedName) => {
    const alarms = failingAlarms();
    alarms.live.set(BACKFILL_ALARM_NAME, { delayInMinutes: 1 });
    alarms.live.set(BACKFILL_SAFETY_ALARM_NAME, { periodInMinutes: BACKFILL_SAFETY_PERIOD_MINUTES });
    alarms.failNext('clear', failedName);

    await expect(syncBackfillAlarm(alarms.api, false)).rejects.toThrow('clear failed');

    await expect(syncBackfillAlarm(alarms.api, false)).resolves.toBe('cleared');
    expect(alarms.live.size).toBe(0);
    expect(await syncBackfillAlarm(alarms.api, false)).toBe('cleared');
    expect(alarms.live.size).toBe(0);
  });

  it('preserves switch-off clearing and the no-get fallback', async () => {
    const alarms = failingAlarms();
    const noGetApi: AlarmsApi = {
      create: alarms.api.create,
      clear: alarms.api.clear,
    };

    expect(await syncBackfillAlarm(noGetApi, true, () => 0)).toBe('created');
    expectBothArmed(alarms.live);
    expect(await syncBackfillAlarm(noGetApi, true, () => 0)).toBe('created');
    expectBothArmed(alarms.live);
    expect(alarms.calls.filter((call) => call.startsWith('get:'))).toHaveLength(0);

    expect(await syncBackfillAlarm(noGetApi, false)).toBe('cleared');
    expect(alarms.live.size).toBe(0);
  });
});
