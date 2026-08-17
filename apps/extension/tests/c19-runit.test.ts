/**
 * C19 · 让回溯腿真的自己跑起来。
 *
 * 三件事，一件都不许含糊：
 *  1. 闹钟（chrome.alarms）：开关打开 ⇒ 创建；关掉 ⇒ 清除。默认仍然是关的。
 *  2. http 端口在【生产代码里】被注入 —— 且取数发生在用户已登录的页面上下文里
 *     （内容脚本同源 fetch），不新增任何 host 权限。
 *  3. 🔴 BUG-3：取正文的最小间隔【跨 tick】也生效。
 *
 * 全程零真实网络、零登录态：fetch 是本文件里的一个纯函数。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { runBackfill } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { DEFAULT_DETAIL_PACE, type Clock } from '../lib/backfill/pace';
import { handleBackfillMessage } from '../lib/backfill/tab-port';
import type { CapturedFetch } from '../lib/contract';

const ORIGIN = 'https://chatgpt.com';
const IDS = [
  'c1111111-0000-4000-8000-000000000001',
  'c2222222-0000-4000-8000-000000000002',
  'c3333333-0000-4000-8000-000000000003',
];

/** 合成"服务器"：一个纯函数，绝不碰网络。 */
function syntheticPort() {
  const calls: string[] = [];
  return {
    calls,
    port: async (url: string) => {
      calls.push(url);
      const u = new URL(url);
      if (u.pathname === '/backend-api/conversations') {
        const offset = Number(u.searchParams.get('offset') ?? '0');
        return {
          status: 200,
          text: JSON.stringify({
            items: IDS.slice(offset).map((id) => ({ id })),
            total: IDS.length,
          }),
        };
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
    },
  };
}

/** 假时钟：sleep 不真的等，只把 now 往前推 —— 于是"等了多久"是可断言的数字。 */
function fakeClock(startMs = 1_700_000_000_000): Clock & { readonly at: () => number } {
  let now = startMs;
  return {
    now: () => now,
    sleep: async (ms: number) => { now += ms; },
    at: () => now,
  };
}

describe('C19 任务 3 · BUG-3：取正文的最小间隔必须【跨 tick】生效', () => {
  it('🔴 第二次 tick（新的一次 runBackfill，同一份 storage）必须补足 20 秒', async () => {
    const store = memoryStore();
    const server = syntheticPort();
    const clock = fakeClock();

    const tick = () => runBackfill({
      platform: 'chatgpt',
      origin: ORIGIN,
      scope: 'acct-fixture-1',
      store,
      http: server.port,
      clock,
      maxDetails: 1,   // 与运行时一致：一次 tick 只清 1 笔账
    });

    const r1 = await tick();
    const r2 = await tick();
    const r3 = await tick();

    console.log('[C19-3] tick1 detail 等待序列(ms) =', JSON.stringify(r1.paceTrace.detail));
    console.log('[C19-3] tick2 detail 等待序列(ms) =', JSON.stringify(r2.paceTrace.detail));
    console.log('[C19-3] tick3 detail 等待序列(ms) =', JSON.stringify(r3.paceTrace.detail));

    // 第一次没有"上一次"可言 ⇒ 等 0，这是对的。
    expect(r1.paceTrace.detail).toEqual([0]);
    // 🔴 第二、三次是【新的 runBackfill】，但上次取数的时刻已经落盘 ⇒ 必须补足间隔。
    expect(r2.paceTrace.detail).toEqual([DEFAULT_DETAIL_PACE.minIntervalMs]);
    expect(r3.paceTrace.detail).toEqual([DEFAULT_DETAIL_PACE.minIntervalMs]);

    // 三笔账都清了，一共 3 次取正文，彼此间隔 20 秒。
    expect(store.data['cs_backfill_v1:chatgpt:acct-fixture-1']).toMatchObject({
      archived: IDS,
      pending: [],
    });
  });

  it('闹钟按 5 分钟醒一次时不会被间隔挡住（elapsed 已经远超 20 秒）', async () => {
    const store = memoryStore();
    const server = syntheticPort();
    const clock = fakeClock();

    const tick = () => runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1',
      store, http: server.port, clock, maxDetails: 1,
    });

    await tick();
    await clock.sleep(5 * 60_000);   // 闹钟周期
    const r2 = await tick();
    console.log('[C19-3] 隔 5 分钟之后的 tick2 detail 等待序列(ms) =', JSON.stringify(r2.paceTrace.detail));
    expect(r2.paceTrace.detail).toEqual([0]);
  });

  it('日上限仍然生效（间隔生效不许把已经在跑的日上限弄坏）', async () => {
    const store = memoryStore();
    const server = syntheticPort();
    const clock = fakeClock();

    const r = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture-1',
      store, http: server.port, clock,
      pace: { enumerate: { minIntervalMs: 0, maxPerDay: null }, detail: { minIntervalMs: 20_000, maxPerDay: 2 } },
    });
    console.log('[C19-3] 日上限 2 时的停止原因 =', r.stopped, '已清 =', r.archivedThisRun.length);
    expect(r.stopped).toBe('daily-cap');
    expect(r.archivedThisRun.length).toBe(2);
  });
});

// ===========================================================================
// 任务 1 + 2 · 运行时：闹钟会醒，端口在生产代码里被注入
//
// 🔴 这一段一条断言都不许直接调 runBackfill / tickBackfill。
//    每个用例都从【真实入口】出发：
//      · 实时腿：browser.runtime.onMessage 收到 'chat-captured'
//      · 闹钟：  browser.alarms.onAlarm 触发
//    然后去看回溯腿有没有真的取到数。
//
// 🔴 取数这一段也走【真实的内容脚本处理函数】handleBackfillMessage ——
//    假 tabs.sendMessage 把消息交给它，它再调一个合成 fetch。
//    也就是说 background → tabs.sendMessage → 内容脚本 → 同源 fetch
//    这条链在测试里是真的被走了一遍的，只有最后那个 fetch 是合成的。
// ===========================================================================

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
const downloadCalls: Array<{ id: number; filename: string }> = [];
const changeListeners: Array<(d: any) => void> = [];

/** 闹钟登记簿：assert「开 ⇒ 创建 / 关 ⇒ 清除」就看它。 */
const alarmBook = new Map<string, { periodInMinutes?: number }>();
const alarmLog: string[] = [];

/** 现在"开着"的平台标签页。关掉一个就从这里删，ping 自然就不通了。 */
const liveTabs = new Map<number, string>();
/** 内容脚本代发过的 URL —— 取数确实发生在页面上下文里的证据。 */
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
    create(name: string, info: any) { alarmBook.set(name, info); alarmLog.push(`create ${name} ${JSON.stringify(info)}`); },
    async clear(name: string) { const had = alarmBook.delete(name); alarmLog.push(`clear ${name}`); return had; },
    async get(name: string) { return alarmBook.get(name) ?? undefined; },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    /**
     * 🔴 这里就是「background → 已登录的页面」那一跳。
     * 标签页不在 liveTabs 里 ⇒ 抛错，与真实浏览器里"标签页已关"的行为一致。
     */
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

function liveCapture(): CapturedFetch {
  const sid = 'aaaaaaaa-1111-2222-3333-444444444444';
  return {
    url: `${ORIGIN}/backend-api/conversation/${sid}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-fixture-1' }),
    pageUrl: `${ORIGIN}/c/${sid}`,
    capturedAt: 1_700_000_000_000,
  };
}

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

/** 像内容脚本那样派发一条消息，sender.tab.id 由"浏览器"填。 */
async function dispatch(message: unknown, tabId?: number): Promise<any> {
  const sender = tabId === undefined ? { id: 's' } : { id: 's', tab: { id: tabId } };
  return await new Promise((resolve) => {
    const ret = runtimeListeners[0]!(message, sender, resolve);
    if (ret !== true) resolve(undefined);
  });
}

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  downloadCalls.length = 0;
  changeListeners.length = 0;
  contentFetches.length = 0;
  alarmBook.clear();
  alarmLog.length = 0;
  liveTabs.clear();
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C19 任务 1 · 闹钟：开 ⇒ 创建，关 ⇒ 清除', () => {
  it('🔴 默认（关）下 SW 启动【不会】创建闹钟，只会确保它不存在', async () => {
    const { BACKFILL_DEFAULT_ENABLED } = await import('../lib/backfill/schedule');
    expect(BACKFILL_DEFAULT_ENABLED).toBe(false);   // 🔴 默认值一个字都没改

    await bootBackground();
    console.log('[C19-1] 默认态 SW 启动后的闹钟操作:', alarmLog, '现存闹钟:', [...alarmBook.keys()]);
    expect([...alarmBook.keys()]).toEqual([]);
    expect(alarmLog.every((l) => l.startsWith('clear'))).toBe(true);
  });

  it('开关打开 ⇒ 创建闹钟；关掉 ⇒ 清除', async () => {
    const { syncBackfillAlarm, BACKFILL_ALARM_NAME, BACKFILL_ALARM_PERIOD_MINUTES } =
      await import('../lib/backfill/alarm');

    const created = await syncBackfillAlarm(fakeBrowser.alarms, true);
    console.log('[C19-1] 打开开关 ->', created, '现存闹钟:', [...alarmBook.entries()]);
    expect(created).toBe('created');
    expect(alarmBook.get(BACKFILL_ALARM_NAME)).toEqual({
      periodInMinutes: BACKFILL_ALARM_PERIOD_MINUTES,
    });

    // 再同步一次不该把周期从头计时（每次 SW 醒来都会走这条路）。
    expect(await syncBackfillAlarm(fakeBrowser.alarms, true)).toBe('kept');

    const cleared = await syncBackfillAlarm(fakeBrowser.alarms, false);
    console.log('[C19-1] 关掉开关 ->', cleared, '现存闹钟:', [...alarmBook.keys()]);
    expect(cleared).toBe('cleared');
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(false);
  });

  it('开关已经开着时 SW 启动会把闹钟补上（重启之后不用用户再点一次）', async () => {
    await enableBackfill();
    await bootBackground();
    const mod = await import('../entrypoints/background');
    await (mod as any).syncAlarmWithSwitch();
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');
    console.log('[C19-1] 开关持久为开 + SW 启动 -> 现存闹钟:', [...alarmBook.keys()]);
    expect(alarmBook.has(BACKFILL_ALARM_NAME)).toBe(true);
  });
});

describe('C19 任务 2 · http 端口：生产代码里真的被注入了', () => {
  it('🔴 实时腿的真实消息路径：不调 configureBackfillTransport，也真的取到了正文', async () => {
    await enableBackfill();
    liveTabs.set(42, ORIGIN);                 // 用户开着一个已登录的 chatgpt 页面
    const mod = await bootBackground();
    // 🔴 故意【不】调用 mod.configureBackfillTransport —— 这就是生产状态。

    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 42);
    await mod.backfillTickSettled();

    console.log('[C19-2] tick 结论:', mod.lastBackfillTick()?.reason);
    console.log('[C19-2] 内容脚本在页面上下文里代发的 URL:', contentFetches);
    console.log('[C19-2] 落盘的最终文件:', downloadCalls.filter((d) => !d.filename.endsWith('.part')).map((d) => d.filename));

    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    // 枚举 1 页 + 取 1 条正文，全部经由内容脚本的同源 fetch。
    expect(contentFetches.some((u) => u.includes('/backend-api/conversations'))).toBe(true);
    expect(contentFetches.filter((u) => u.includes('/backend-api/conversation/')).length).toBe(1);
    const s: any = store['cs_backfill_v1:chatgpt:acct-fixture-1'];
    expect(s.archived.length).toBe(1);
    expect(s.pending.length).toBe(IDS.length - 1);
  });

  it('🔴 闹钟路径：没有任何实时捕获，闹钟自己醒来也清得动账', async () => {
    await enableBackfill();
    liveTabs.set(7, ORIGIN);
    const mod = await bootBackground();

    // 先让内容脚本报到 —— 闹钟醒来时 SW 只有这张登记表可用。
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 7);
    // 目标登记：由一次真实捕获留下（这是"用户至少用过一次"的最低前提）。
    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 7);
    await mod.backfillTickSettled();
    const before = (store['cs_backfill_v1:chatgpt:acct-fixture-1'] as any).archived.length;

    // 现在什么捕获都不发，只让闹钟响。
    contentFetches.length = 0;
    runtimeNow += 5 * 60_000;
    expect(alarmListeners.length).toBeGreaterThan(0);
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');
    alarmListeners[0]!({ name: BACKFILL_ALARM_NAME });
    await mod.backfillTickSettled();

    const after = (store['cs_backfill_v1:chatgpt:acct-fixture-1'] as any).archived.length;
    console.log('[C19-2] 闹钟醒来一次:', { reason: mod.lastBackfillTick()?.reason, before, after });
    console.log('[C19-2] 闹钟这一脚代发的 URL:', contentFetches);
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    expect(after).toBe(before + 1);           // 🔴 没有任何实时捕获，账也少了一笔
  });

  it('🔴 没有开着的平台页面 ⇒ 仍然如实返回 no-http-port（不许假装在跑）', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    // 先报到，再把标签页"关掉"。
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 9);
    liveTabs.set(9, ORIGIN);
    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 9);
    await mod.backfillTickSettled();
    liveTabs.delete(9);                        // 用户关掉了页面

    contentFetches.length = 0;
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');
    alarmListeners[0]!({ name: BACKFILL_ALARM_NAME });
    await mod.backfillTickSettled();

    console.log('[C19-2] 页面关掉后闹钟的结论:', mod.lastBackfillTick()?.reason, '代发 URL:', contentFetches);
    expect(mod.lastBackfillTick()?.reason).toBe('no-http-port');
    expect(contentFetches).toEqual([]);        // 一个请求都没发出去
    expect(await mod.backfillRuntimeStatus()).toEqual({
      transportWired: false, lastTickReason: 'no-http-port',
    });
  });

  it('🔴 内容脚本只肯代发【同源 + 平台表内 + 回溯腿那两条路径】的 URL', async () => {
    const { isAllowedBackfillUrl, serveBackfillFetch } = await import('../lib/backfill/tab-port');
    const cases: Array<[string, boolean]> = [
      [`${ORIGIN}/backend-api/conversations?offset=0&limit=100`, true],
      [`${ORIGIN}/backend-api/conversation/abc`, true],
      [`${ORIGIN}/backend-api/accounts/check`, false],        // 平台内但不是回溯腿的路径
      ['https://evil.example.com/steal', false],              // 跨源
      ['https://claude.ai/backend-api/conversations', false], // 别的平台（对本页面是跨源）
      ['not a url', false],
    ];
    for (const [url, allowed] of cases) {
      expect([url, isAllowedBackfillUrl(url, ORIGIN)]).toEqual([url, allowed]);
    }
    const refused = await serveBackfillFetch('https://evil.example.com/steal', ORIGIN, syntheticPageFetch as any);
    console.log('[C19-2] 越权 URL 的回复:', refused);
    expect(refused).toEqual({ ok: false, error: 'refused: url is not a same-origin backfill endpoint' });
    expect(contentFetches).toEqual([]);        // 🔴 连 fetch 都没被调用
  });

  it('🔴 开关是关的时候，有页面开着也一条都不取', async () => {
    liveTabs.set(42, ORIGIN);
    const mod = await bootBackground();
    await dispatch({ type: 'chat-captured', payload: liveCapture() }, 42);
    await mod.backfillTickSettled();
    console.log('[C19-2] 默认（关）+ 有页面开着 ->', mod.lastBackfillTick()?.reason, '代发 URL:', contentFetches);
    expect(mod.lastBackfillTick()?.reason).toBe('disabled');
    expect(contentFetches).toEqual([]);
  });
});

// ===========================================================================
// 任务 4 · Popup 的三种状态必须与实际一致
// ===========================================================================
describe('C19 任务 4 · Popup 说的话与实际状态一致', () => {
  async function viewFor(opts: { enabled: boolean; transportWired: boolean }) {
    const { tickBlockReason, setBackfillEnabled, isBackfillEnabled } =
      await import('../lib/backfill/schedule');
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const { renderPopup, popupText, pickBackfillState, collectFailures } = await import('../lib/popup-view');
    await setBackfillEnabled(browserLocalStore(), opts.enabled);
    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => isBackfillEnabled(browserLocalStore()),
      isDownloadPaused: () => false,
      hasHttp: opts.transportWired,
    });
    const snapshot = await browserLocalSnapshot();
    const state = pickBackfillState(snapshot);
    const view = renderPopup({
      enabled: opts.enabled, block, guard: null, state,
      target: state ? { platform: state.platform, scope: state.scope } : null,
      failures: collectFailures(snapshot),
    });
    return { view, text: popupText(view) };
  }

  it('状态一 · 关：说的是「开关没有打开」，绝不提端口', async () => {
    const { view, text } = await viewFor({ enabled: false, transportWired: false });
    console.log('[C19-4 · 关]\n' + text + '\n');
    expect(view.running).toContain('未在运行');
    expect(view.running).toContain('开关没有打开');
    expect(text).not.toContain('正在归档');
  });

  it('🔴 状态二 · 开但没有可用页面：必须仍然是「未在运行 + 缺什么」', async () => {
    const { view, text } = await viewFor({ enabled: true, transportWired: false });
    console.log('[C19-4 · 开但没端口]\n' + text + '\n');
    expect(view.running).toContain('未在运行');
    expect(view.missing).toContain('取数通道');
    // 🔴 C18 的诚实不许被破坏。
    for (const forbidden of ['正在归档', '正在回溯', '预计剩余']) {
      expect(text).not.toContain(forbidden);
    }
  });

  it('🔴 状态三 · 真的在跑：只有四道闸门全过才允许出现「正在归档」', async () => {
    const { view, text } = await viewFor({ enabled: true, transportWired: true });
    console.log('[C19-4 · 真的在跑]\n' + text + '\n');
    expect(view.running).toContain('正在归档');
    expect(view.missing).toBeNull();
    expect(view.running).toContain('5 分钟');
    expect(view.running).toContain('200');
    expect(text).not.toContain('预计剩余');
  });

  it('🔴 background 报给 Popup 的 transportWired 是【现场 ping 出来的】，不是静态标志', async () => {
    await enableBackfill();
    const mod = await bootBackground();
    await dispatch({ type: 'cs-backfill-tab-hello', origin: ORIGIN }, 5);

    liveTabs.set(5, ORIGIN);
    const open = await mod.backfillRuntimeStatus();
    liveTabs.delete(5);
    const closed = await mod.backfillRuntimeStatus();
    console.log('[C19-4] 页面开着 ->', open, ' 页面关掉 ->', closed);
    expect(open.transportWired).toBe(true);
    expect(closed.transportWired).toBe(false);
  });
});
