/**
 * C29 · 还原真机实测现象：通道接上、闹钟在跳、SW在醒 —— 但【枚举】从来没启动。
 *
 * 根因复现：
 * 1. 用户在 Popup 打开「自动回溯历史对话」开关；
 * 2. 用户打开 https://chatgpt.com/ 页面，内容脚本报到（cs-backfill-tab-hello），
 *    rememberTab 将标签页记入 cs_backfill_tabs_v1；
 * 3. 但用户尚未在页面上触发任何实时对话抓取（即没有 chat-captured 消息），
 *    因此 rememberTarget 从未被调用，cs_backfill_targets_v1 保持为空；
 * 4. 此时 Popup 查 transportWired 为 true，渲染显示「正在归档 —— 每 5 分钟自动清 1 笔账」；
 *    ⚠️ C30 已修：Popup 现在还会读目标登记表，这一步改说「未在运行 —— 还没有任何回溯目标」；
 * 5. 闹钟 cs-backfill-tick 触发，SW 唤醒并执行 runAlarmTick()；
 * 6. runAlarmTick() 读取 loadTargets() 为空，命中 targets.length === 0 分支，
 *    返回 { ran: false, reason: ... , report: null }，
 *    没有任何枚举请求发出，存储里永远不会创建欠账集合。
 *    ⚠️ C30 已修两件事：结局具名成 'no-targets'（端口没坏，是没目标），
 *       并且每一跳都会把「什么都没做以及为什么」写进 cs_backfill_lasttick_v1。
 *    🔴 行为（跳过、不枚举）一个字都没改 —— 本文件下半段的断言仍然逐字有效。
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
const contentFetches: string[] = [];

function syntheticPageFetch(url: string) {
  contentFetches.push(url);
  const u = new URL(url);
  if (u.pathname === '/backend-api/conversations') {
    return Promise.resolve({
      status: 200,
      text: async () => JSON.stringify({
        items: IDS.map((id) => ({ id })),
        total: IDS.length,
      }),
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
    async download() { return 1; },
    onChanged: { addListener() {} },
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

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  contentFetches.length = 0;
  alarmBook.clear();
  liveTabs.clear();
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C29-NOENUM · 仅打开标签页但未发生过实时对话时，闹钟醒来不枚举的真机复现', () => {
  it('复现 12:55 ~ 13:12 完整事实：Popup显示正在归档，但闹钟每次跳动都因 targets 为空而静默跳过', async () => {
    const { setBackfillEnabled, tickBlockReason } = await import('../lib/backfill/schedule');
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const { renderPopup, pickBackfillState, collectFailures } = await import('../lib/popup-view');

    // 1. ~12:55: Popup 中开启回溯开关
    await setBackfillEnabled(browserLocalStore(), true);

    // 2. 12:56: 打开一个 ChatGPT 页面
    liveTabs.set(101, ORIGIN);
    const mod = await bootBackground();

    // 内容脚本报到 (cs-backfill-tab-hello)
    const helloReply = await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 101);
    expect(helloReply).toEqual({ ok: true });

    // 此时存储中仅有开关与 tabs 登记（对应真机 298 字节快照）
    expect(store).toHaveProperty('cs_backfill_enabled_v1', true);
    expect(store).toHaveProperty('cs_backfill_tabs_v1');
    expect(store).not.toHaveProperty('cs_backfill_targets_v1');

    // 3. Popup 查询状态：transportWired 为 true
    const statusReply: BackfillRuntimeStatus = await dispatch({ type: 'cs-backfill-status' });
    expect(statusReply.transportWired).toBe(true);

    // Popup 计算闸门与渲染。
    // 🔴 C30 修正：这里【原来】只传 hasHttp —— 那正是缺陷本体的一半：
    //    拿"通道通不通"去回答"有没有活要干"。现在 Popup（entrypoints/popup/main.ts）
    //    会一并读回溯目标登记表，所以这一段照着生产路径写。
    const { loadTargets } = await import('../lib/backfill/alarm');
    const { browserLocalStore: storeFn } = await import('../lib/backfill/store');
    const targets = await loadTargets(storeFn());
    expect(targets).toEqual([]);      // 时序没变：登记表就是空的

    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => true,
      isDownloadPaused: () => false,
      hasHttp: statusReply.transportWired,
      hasTargets: targets.length > 0,
    });
    // 🔴 C30 之前这里是 toBeNull()（闸门全过 ⇒ Popup 宣称正在归档）。
    //    时序一个字没改，改的是它现在【说得出】卡在哪一道。
    expect(block).toBe('no-targets');

    const snapshot = await browserLocalSnapshot();
    const view = renderPopup({
      enabled: true,
      block,
      guard: null,
      state: pickBackfillState(snapshot),
      target: null,
      failures: collectFailures(snapshot),
    });

    // 🔴 C30 之后：progress 说的还是同一句实话（一条都没枚举过），
    //    而 running 不再与它自相矛盾 —— 它现在说「未在运行」并说清用户要做什么。
    expect(view.running).not.toContain('正在归档');
    expect(view.running).toContain('未在运行');
    expect(view.progress).toBe(
      '进度：还没有开始 —— 存储里还没有这个平台的欠账集合，也就是说一条都还没有枚举过。',
    );

    // 4. 12:59:39 & 13:09:39: 闹钟响了 (alarmListeners)
    expect(alarmListeners.length).toBeGreaterThan(0);
    alarmListeners[0]!({ name: 'cs-backfill-tick' });
    await mod.backfillTickSettled();

    // 🔴 核心断言：runAlarmTick 因 targets.length === 0 跳过。
    //    C30 之前这里报的是 'no-http-port' —— 端口根本没坏，那句话把排查带偏了。
    expect(mod.lastBackfillTick()).toEqual({
      ran: false,
      reason: 'no-targets',
      report: null,
    });
    // 🔴 C30 · 这一跳什么都没做，但它【留下了痕迹】（不再是静默跳过）。
    const { loadLastTick } = await import('../lib/backfill/alarm');
    expect(await loadLastTick(storeFn())).toMatchObject({
      ran: false, reason: 'no-targets', targets: 0,
    });

    // 🔴 没有发任何枚举请求
    expect(contentFetches).toEqual([]);

    // 🔴 存储里没有生成任何平台的欠账账本
    const allKeys = Object.keys(store);
    expect(allKeys.some((k) => k.startsWith('cs_backfill_v1:'))).toBe(false);
  });
});
