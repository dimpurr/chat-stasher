/**
 * C25 · 生产 background 的回溯腿必须可达。
 *
 * 这些断言只从 defineBackground 的真实回调进入；runBackfill 是唯一的
 * 网络/引擎边界，在这里替换成 spy，避免测试发送真实请求。闹钟、storage
 * 事件、目标登记和标签页端口仍然走 background 的生产代码。
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';

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
  downloads: {
    async download() { return 1; },
    onChanged: { addListener() {} },
    async removeFile() {},
    async erase() {},
  },
};

function seedTargetAndLiveTab(): void {
  stored[TARGETS_KEY] = [{ platform: 'chatgpt', origin: ORIGIN, scope: 'fixture', at: 1 }];
  stored[TABS_KEY] = [{ tabId: 25, origin: ORIGIN, at: 1 }];
}

async function boot(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  await mod.default();
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
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (callback: any) => callback);
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C25 · background wiring', () => {
  it('开关开时创建闹钟，切关时通过 storage change 立即清掉', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');

    await setBackfillEnabled(browserLocalStore(), true);
    await boot();
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
    expect(alarmCreates).toEqual([BACKFILL_ALARM_NAME]);

    await setBackfillEnabled(browserLocalStore(), false);
    await waitForAsyncEvent();
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
    expect(alarmClears).toContain(BACKFILL_ALARM_NAME);
  });

  it('默认关闭时启动不创建闹钟，闹钟事件也不跑回溯', async () => {
    const mod = await boot();
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');

    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
    expect(alarmCreates).toEqual([]);

    alarmListeners[0]!({ name: BACKFILL_ALARM_NAME });
    await mod.backfillTickSettled();
    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('disabled');
  });

  it('匹配的 onAlarm 从生产入口走到回溯一跳，但不发真实网络', async () => {
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

  it('开关打开但拿不到端口时显式返回 no-http-port，不调用引擎且不抛错', async () => {
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
