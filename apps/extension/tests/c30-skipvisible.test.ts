/**
 * C30 · 「跳过」必须是可观察的。
 *
 * C29 精确复现了时序：有 transport、没有 target ⇒ 闹钟每次醒来都静默跳过。
 * 但 C29 只把那个假象【钉住】（断言 popup 说「正在归档」、断言 reason 是
 * 'no-http-port'），它把错的现状写成了期望。本文件反过来：
 *
 *  1. 🔴 有 transport 但没有 target 时，popup 【不许】宣称正在归档；
 *  2. 🔴 这一跳为什么什么都没做，必须【写进存储】，而不是只留在内存里；
 *  3. 🔴 结局必须具名到能分辨：没目标 ≠ 没通道；
 *  4. 🔴 健康路径守卫：目标确实存在 + 通道活着 ⇒ 仍然说「正在归档」。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { handleBackfillMessage } from '../lib/backfill/tab-port';
import type { BackfillRuntimeStatus } from '../lib/popup-view';

const ORIGIN = 'https://chatgpt.com';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
];

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
const alarmBook = new Map<string, { periodInMinutes?: number }>();
const liveTabs = new Map<number, string>();
const changeListeners: Array<(c: any) => void> = [];
const contentFetches: string[] = [];

function syntheticPageFetch(url: string) {
  contentFetches.push(url);
  const u = new URL(url);
  if (u.pathname === '/backend-api/conversations') {
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({ items: IDS.map((id) => ({ id })), total: IDS.length }),
    });
  }
  return Promise.resolve({
    status: 200,
    text: async () => JSON.stringify({
      mapping: { n1: { id: 'n1', message: { content: { parts: ['synthetic'] } } } },
      current_node: 'n1',
      account_id: 'acct-fixture-1',
    }),
  });
}

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
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
  downloads: {
    // 真实浏览器会异步回一条 complete；不回的话落盘那一步会一直等下去。
    async download() {
      const id = 1;
      setTimeout(() => { for (const fn of changeListeners) fn({ id, state: { current: 'complete' } }); }, 0);
      return id;
    },
    onChanged: { addListener(fn: any) { changeListeners.push(fn); } },
    async removeFile() {},
    async erase() {},
  },
  alarms: {
    create(name: string, info: any) { alarmBook.set(name, info); },
    async clear(name: string) { return alarmBook.delete(name); },
    async get(name: string) { return alarmBook.get(name) ?? undefined; },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    async sendMessage(tabId: number, message: unknown) {
      const origin = liveTabs.get(tabId);
      if (!origin) throw new Error('Could not establish connection. Receiving end does not exist.');
      const pending = handleBackfillMessage(message, origin, syntheticPageFetch as any);
      if (!pending) return undefined;
      return await pending;
    },
  },
};

let runtimeNow = 1_700_000_000_000;
const runtimeClock = {
  now: () => runtimeNow,
  sleep: async (ms: number) => { runtimeNow += ms; },
};

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

async function dispatch(message: unknown, tabId?: number): Promise<any> {
  const sender = tabId === undefined ? { id: 's' } : { id: 's', tab: { id: tabId } };
  return await new Promise((resolve) => {
    const ret = runtimeListeners[0]!(message, sender, resolve);
    if (ret !== true) resolve(undefined);
  });
}

/** Popup 生产路径的等价物：与 entrypoints/popup/main.ts 读的是同一批事实。 */
async function popupNow(transportWired: boolean) {
  const { tickBlockReason } = await import('../lib/backfill/schedule');
  const { loadTargets, loadLastTick } = await import('../lib/backfill/alarm');
  const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
  const { renderPopup, pickBackfillState, collectFailures, popupText } =
    await import('../lib/popup-view');
  const s = browserLocalStore();
  const targets = await loadTargets(s);
  const block = await tickBlockReason({
    hasStore: s !== null,
    isEnabled: () => true,
    isDownloadPaused: () => false,
    hasHttp: transportWired,
    hasTargets: targets.length > 0,
  });
  const snapshot = await browserLocalSnapshot();
  const view = renderPopup({
    enabled: true,
    block,
    guard: null,
    state: pickBackfillState(snapshot),
    target: null,
    failures: collectFailures(snapshot),
    lastTick: await loadLastTick(s),
  });
  return { block, view, text: popupText(view) };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  contentFetches.length = 0;
  alarmBook.clear();
  liveTabs.clear();
  changeListeners.length = 0;
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C30-SKIPVISIBLE · 跳过必须可观察，且原因必须具名', () => {
  it('🔴 反证：有 transport 但一个回溯目标都没有时，popup 不得宣称正在归档', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');

    await setBackfillEnabled(browserLocalStore(), true);
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    // 真机快照：有开关、有 tabs、没有 targets。
    expect(store).toHaveProperty('cs_backfill_tabs_v1');
    expect(store).not.toHaveProperty('cs_backfill_targets_v1');

    const statusReply: BackfillRuntimeStatus = await dispatch({ type: 'cs-backfill-status' });
    expect(statusReply.transportWired).toBe(true);

    const { block, view, text } = await popupNow(statusReply.transportWired);
    console.log('[C30-EVIDENCE 没有目标]\n' + text);

    // 🔴 核心反证：transport 一个人撑不起「正在归档」这句话。
    expect(view.running).not.toContain('正在归档');
    expect(text).not.toContain('正在归档');
    expect(view.running).toContain('未在运行');
    // 🔴 用户读完必须知道「他要做什么才会开始」。
    expect(view.missing).not.toBeNull();
    expect(view.missing!).toContain('实时归档');
    expect(block).toBe('no-targets');
  });

  it('🔴 闹钟每一次跳过都要写进存储，且结局具名为 no-targets（不是 no-http-port）', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { BACKFILL_LAST_TICK_KEY, loadLastTick } = await import('../lib/backfill/alarm');

    await setBackfillEnabled(browserLocalStore(), true);
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    alarmListeners[0]!({ name: 'cs-backfill-tick' });
    await mod.backfillTickSettled();

    expect(mod.lastBackfillTick()).toEqual({ ran: false, reason: 'no-targets', report: null });
    expect(contentFetches).toEqual([]);            // 行为没变：仍然一条都不枚举

    // 🔴 (a)：这一跳什么都没做，以及为什么，必须留在存储里。
    expect(store).toHaveProperty(BACKFILL_LAST_TICK_KEY);
    const rec = await loadLastTick(browserLocalStore());
    expect(rec).toMatchObject({ ran: false, reason: 'no-targets', targets: 0 });
    expect(typeof rec!.at).toBe('number');

    // popup 要把这条记录说出来（「闹钟真的醒过、真的什么都没做」）。
    const { text } = await popupNow(true);
    expect(text).toContain('闹钟');
    expect(text).not.toContain('正在归档');
  });

  it('🔴 两种结局说的话必须不一样：没有目标 vs 没有通道', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);

    const noTargets = await popupNow(true);
    store['cs_backfill_targets_v1'] = [
      { platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1', at: 1 },
    ];
    const noPort = await popupNow(false);

    expect(noTargets.block).toBe('no-targets');
    expect(noPort.block).toBe('no-http-port');
    expect(noTargets.view.running).not.toBe(noPort.view.running);
    expect(noTargets.view.missing).not.toBe(noPort.view.missing);
    expect(noPort.view.missing!).toContain('页面');
  });

  it('🟢 健康路径守卫：目标确实存在且通道活着时，popup 仍然说「正在归档」', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);
    store['cs_backfill_targets_v1'] = [
      { platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1', at: 1 },
    ];
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);

    const statusReply: BackfillRuntimeStatus = await dispatch({ type: 'cs-backfill-status' });
    expect(statusReply.transportWired).toBe(true);

    const { block, view, text } = await popupNow(statusReply.transportWired);
    console.log('[C30-EVIDENCE 健康路径]\n' + text);
    expect(block).toBeNull();
    expect(view.running).toContain('正在归档');
    expect(view.missing).toBeNull();

    // 闹钟这一跳是真的跑了 —— 存储里的记录也必须是 ran。
    alarmListeners[0]!({ name: 'cs-backfill-tick' });
    await mod.backfillTickSettled();
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    const { loadLastTick } = await import('../lib/backfill/alarm');
    expect(await loadLastTick(browserLocalStore())).toMatchObject({ ran: true, reason: 'ran' });
  });
});
