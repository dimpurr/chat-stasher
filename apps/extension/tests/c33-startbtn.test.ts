/**
 * C33-STARTBTN · 「开始回溯这个平台」——【显式知情同意】那条登记入口。
 *
 * ## 缺陷现场（真机复现）
 * 新用户把开关打开、也开着一个受支持平台的页面（取数通道是通的），
 * 但回溯【永远不会开始】：唯一登记回溯目标的地方是 kickBackfill，
 * 而它要求**先有一次实时对话被捕获**。
 *
 * 🔴 那个限制是【有意的设计】，本文件一个字都没去改它
 *    （lib/backfill/alarm.ts:80-87：闹钟醒来时 SW 是全新的，没有 tab 也没有账号，
 *      唯一不用编的信息就是实时腿现成攥着的那一个；docs/privacy.md —— 扩展没有
 *      任何 host 权限）。
 * ⇒ 本单加的是**另一条登记入口**：用户在 Popup 上点一下，明确说「就补这个平台」。
 *   「不去猜」这条原则不变 —— 变的只是多了一个「用户自己说」的来源。
 *
 * ## 这里钉死四件事
 *  1. 🔴 反证：有通道、无目标时，点了按钮之后 cs_backfill_targets_v1 里出现一条目标；
 *  2. 已经有目标时按钮【不出现】（否则它是永远挂着的噪声）；
 *  3. 没有通道时按钮【不出现】（点了也没用）；
 *  4. 登记之后闹钟那一跳【不再报 no-targets】——用真实的 runAlarmTick 走一遍。
 *
 * 全程零真实网络、零登录态：只有假的 browser.* 和一个合成 fetch。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { handleBackfillMessage } from '../lib/backfill/tab-port';

const ORIGIN = 'https://chatgpt.com';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
];

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const downloadCalls: Array<{ id: number; filename: string }> = [];
const changeListeners: Array<(d: any) => void> = [];
const alarmBook = new Map<string, unknown>();
/** 现在"开着"的平台标签页。删掉一个，ping 自然就不通了。 */
const liveTabs = new Map<number, string>();
const contentFetches: string[] = [];

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
      mapping: { n1: { id: 'n1', message: { content: { parts: [`synthetic ${id}`] } } } },
      current_node: 'n1',
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
    async download(opts: any) {
      const id = downloadCalls.length + 1;
      downloadCalls.push({ id, filename: opts.filename });
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
    onAlarm: { addListener() {} },
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

/** 假时钟走 background 的测试接缝，免得测试真的睡满 20 秒。 */
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

/** 像 Popup 那样派发一条消息（Popup 没有 sender.tab）。 */
async function dispatch(message: unknown): Promise<any> {
  return await new Promise((resolve) => {
    const ret = runtimeListeners[0]!(message, { id: 'popup' }, resolve);
    if (ret !== true) resolve(undefined);
  });
}

/** 内容脚本报到：这就是「有一个开着的平台页面」在生产里的样子。 */
async function tabHello(tabId: number): Promise<void> {
  const { BACKFILL_TAB_HELLO_MESSAGE } = await import('../lib/backfill/tab-port');
  liveTabs.set(tabId, ORIGIN);
  await new Promise((resolve) => {
    const ret = runtimeListeners[0]!(
      { type: BACKFILL_TAB_HELLO_MESSAGE, origin: ORIGIN },
      { id: 'cs', tab: { id: tabId } },
      resolve,
    );
    if (ret !== true) resolve(undefined);
  });
}

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

/**
 * Popup 生产路径的等价物：与 entrypoints/popup/main.ts 读的是同一批事实、
 * 同一个 tickBlockReason、同一个 renderPopup。不在测试里另写一份判断。
 */
async function popupNow(mod: any) {
  const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
  const { tickBlockReason, isBackfillEnabled } = await import('../lib/backfill/schedule');
  const { loadTargets, loadLastTick } = await import('../lib/backfill/alarm');
  const { renderPopup, popupText, collectFailures, pickBackfillState } =
    await import('../lib/popup-view');

  const s = browserLocalStore();
  const runtime = await mod.backfillRuntimeStatus();
  const enabled = await isBackfillEnabled(s);
  const targets = await loadTargets(s);
  const snapshot = await browserLocalSnapshot();
  const block = await tickBlockReason({
    hasStore: s !== null,
    isEnabled: () => enabled,
    isDownloadPaused: () => false,
    hasHttp: runtime.transportWired,
    hasTargets: targets.length > 0,
  });
  const view = renderPopup({
    enabled,
    block,
    guard: null,
    state: pickBackfillState(snapshot),
    target: null,
    failures: collectFailures(snapshot),
    lastTick: await loadLastTick(s),
    liveTarget: runtime.liveTarget ?? null,
    targetCount: targets.length,
  });
  return { runtime, targets, block, view, text: popupText(view) };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  downloadCalls.length = 0;
  changeListeners.length = 0;
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

describe('C33-STARTBTN · 显式知情同意的那条登记入口', () => {
  it('🔴 反证：有通道、无目标 ⇒ 点了按钮之后 cs_backfill_targets_v1 里出现一条目标', async () => {
    const { BACKFILL_TARGETS_KEY } = await import('../lib/backfill/alarm');
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');

    await enableBackfill();
    const mod = await bootBackground();
    await tabHello(7);                       // 用户开着一个已登录的 chatgpt 页面

    const before = await popupNow(mod);
    console.log('[C33-EVIDENCE 点击前]\n' + before.text);
    // 前提：通道是通的、目标是空的 —— 这正是真机上那个死结。
    expect(before.runtime.transportWired).toBe(true);
    expect(before.targets).toEqual([]);
    expect(before.block).toBe('no-targets');
    expect(store[BACKFILL_TARGETS_KEY]).toBeUndefined();
    // 🔴 按钮必须【出现】，否则用户永远解不开这个死结。
    expect(before.view.startBackfill.visible).toBe(true);
    console.log('[C33-EVIDENCE 按钮文案]', before.view.startBackfill.label);

    // === 点一下 ===（走真实的 runtime.onMessage 入口，不直接调内部函数）
    const reply = await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });
    console.log('[C33-EVIDENCE 点击回执]', JSON.stringify(reply));
    expect(reply?.ok).toBe(true);

    const written = store[BACKFILL_TARGETS_KEY] as any[];
    console.log('[C33-EVIDENCE 登记表]', JSON.stringify(written));
    expect(Array.isArray(written)).toBe(true);
    expect(written.length).toBe(1);
    expect(written[0]).toMatchObject({ platform: 'chatgpt', origin: ORIGIN });
    expect(typeof written[0].scope).toBe('string');
    expect(written[0].scope.length).toBeGreaterThan(0);

    // 🔴 点完【立刻】重画一次就该看得见变化，不需要用户手动刷新。
    const after = await popupNow(mod);
    console.log('[C33-EVIDENCE 点击后]\n' + after.text);
    expect(after.targets.length).toBe(1);
    expect(after.block).not.toBe('no-targets');
    expect(after.view.startBackfill.visible).toBe(false);
  });

  it('🟢 守卫一：已经有目标时按钮【不出现】（不许变成永远挂着的噪声）', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    await tabHello(7);
    const { rememberTarget } = await import('../lib/backfill/alarm');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await rememberTarget(browserLocalStore(), {
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1', at: 1,
    });

    const now = await popupNow(mod);
    console.log('[C33-EVIDENCE 已有目标]\n' + now.text);
    expect(now.runtime.transportWired).toBe(true);
    expect(now.targets.length).toBe(1);
    expect(now.view.startBackfill.visible).toBe(false);
    expect(now.text).not.toContain(now.view.startBackfill.label);
  });

  it('🟢 守卫二：没有通道时按钮【不出现】（点了也没用）', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    // 一个平台页面都没开着 ⇒ 没有可用取数通道。

    const now = await popupNow(mod);
    console.log('[C33-EVIDENCE 无通道]\n' + now.text);
    expect(now.runtime.transportWired).toBe(false);
    expect(now.runtime.liveTarget ?? null).toBeNull();
    expect(now.targets).toEqual([]);
    expect(now.view.startBackfill.visible).toBe(false);
    expect(now.text).not.toContain(now.view.startBackfill.label);
  });

  it('🔴 登记之后，闹钟那一跳不再报 no-targets（走真实的 runAlarmTick）', async () => {
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');
    await enableBackfill();
    const mod = await bootBackground();
    await tabHello(7);

    // 登记之前：闹钟醒来什么都做不了，理由就是 no-targets。
    const dry = await mod.runAlarmTick();
    console.log('[C33-EVIDENCE 登记前的闹钟一跳]', dry.reason);
    expect(dry.reason).toBe('no-targets');

    await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });

    const wet = await mod.runAlarmTick();
    console.log('[C33-EVIDENCE 登记后的闹钟一跳]', wet.reason,
      '| 内容脚本代发的 URL:', contentFetches,
      '| 落盘:', downloadCalls.filter((d) => !d.filename.endsWith('.part')).map((d) => d.filename));
    expect(wet.reason).not.toBe('no-targets');
    // 通道也确实是通的 ⇒ 这一跳真的跑了，而不是换了一种卡住的说法。
    expect(wet.reason).toBe('ran');
    expect(contentFetches.length).toBeGreaterThan(0);
  });

  it('接线守卫：popup 的 HTML 里真的有这个按钮，main.ts 真的挂了它并在点完之后重画', async () => {
    const html = readFileSync(new URL('../entrypoints/popup/index.html', import.meta.url), 'utf8');
    const main = readFileSync(new URL('../entrypoints/popup/main.ts', import.meta.url), 'utf8');
    // 🔴 真机装的是构建产物：HTML 里没有这个元素，按钮就永远不会出现在屏幕上。
    expect(html).toContain('id="start-backfill"');
    expect(main).toContain("getElementById('start-backfill')");
    // 点完必须自己重画 —— 「立刻反映」不许靠用户手动关掉再打开。
    expect(main).toMatch(/onStartBackfill[\s\S]*refresh\(\)/);
  });
});
