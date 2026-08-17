/**
 * C18 · Popup —— 回溯腿第一个用户真的能打开的入口。
 *
 * 这里验三件事，一件都不许含糊：
 *  1. 打开开关会【持久化】，浏览器重启（模块状态清零、storage 留着）之后仍然是开的；
 *  2. 🔴 开着但取数通道没接上时，文案必须说【未在运行】并说清缺什么，
 *     且【整段文本不含 '%' 字符】；绝不许出现「正在归档」这类说法；
 *  3. 熔断态下要出告警，并且开关【仍然可切】。
 *
 * 全程零网络、零登录态：只有假的 browser.* 和纯函数。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import type { BackfillState } from '../lib/backfill/types';
import { stateKey } from '../lib/backfill/types';

// ---------------------------------------------------------------------------
// 假浏览器。storage 跨 vi.resetModules() 存活 —— 这就是「浏览器重启」的模型。
// ---------------------------------------------------------------------------
const store: Record<string, unknown> = {};

const fakeBrowser: any = {
  runtime: { id: 'mock-extension-id' },
  storage: {
    local: {
      async get(query: Record<string, unknown> | null) {
        if (query === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(query)) out[k] = k in store ? store[k] : query[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
};

beforeEach(() => {
  for (const k of Object.keys(store)) delete store[k];
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
});

/** 一份「跑过一点、但接口没给 total」的欠账集合 —— 正是不许出现百分比的那种。 */
function seedState(platform = 'chatgpt', scope = 'acct-1'): BackfillState {
  const state: BackfillState = {
    v: 1,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 40, complete: false },
    pending: ['d1', 'd2', 'd3'],
    archived: ['a1', 'a2'],
    detailToday: { day: '2026-08-17', count: 2 },
    halted: null,
  };
  store[stateKey(platform, scope)] = state;
  return state;
}

// ---------------------------------------------------------------------------
// 1 · 开关持久化
// ---------------------------------------------------------------------------
describe('C18-1 · 打开开关会持久化，重启后仍是开的', () => {
  it('默认是关的；setBackfillEnabled(true) 之后，重新加载模块仍然读到开', async () => {
    const first = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');

    // 🔴 默认值本身没有被改动。
    expect(first.BACKFILL_DEFAULT_ENABLED).toBe(false);
    expect(await first.isBackfillEnabled(browserLocalStore())).toBe(false);

    // Popup 上那一次点击做的就是这一件事。
    await first.setBackfillEnabled(browserLocalStore(), true);
    expect(store[first.BACKFILL_ENABLED_KEY]).toBe(true);

    // 「浏览器重启」：模块状态清零，storage.local 留着。
    vi.resetModules();
    const second = await import('../lib/backfill/schedule');
    const { browserLocalStore: store2 } = await import('../lib/backfill/store');
    expect(await second.isBackfillEnabled(store2())).toBe(true);

    // 关掉也要立刻持久化。
    await second.setBackfillEnabled(store2(), false);
    vi.resetModules();
    const third = await import('../lib/backfill/schedule');
    const { browserLocalStore: store3 } = await import('../lib/backfill/store');
    expect(await third.isBackfillEnabled(store3())).toBe(false);
  });

  it('🔴 Chrome 形状（只有 globalThis.chrome，没有 browser）下也能存住', async () => {
    vi.unstubAllGlobals();
    vi.stubGlobal('chrome', fakeBrowser); // 不 stub browser —— 这就是 Chrome MV3
    vi.resetModules();
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { setBackfillEnabled, isBackfillEnabled } = await import('../lib/backfill/schedule');
    expect(browserLocalStore()).not.toBeNull();
    await setBackfillEnabled(browserLocalStore(), true);
    expect(await isBackfillEnabled(browserLocalStore())).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 2 · 🔴 开着但没端口
// ---------------------------------------------------------------------------
describe('C18-2 · 开着但取数通道没接上', () => {
  it('文案必须说「未在运行」并说清缺什么，且整段不含百分号', async () => {
    seedState();
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const { setBackfillEnabled, isBackfillEnabled, tickBlockReason } =
      await import('../lib/backfill/schedule');
    const { loadGuardState, isGuardTripped } = await import('../lib/download-guard');
    const { renderPopup, popupText, pickBackfillState, collectFailures } = await import('../lib/popup-view');

    // 用户在 Popup 上把开关打开了。
    await setBackfillEnabled(browserLocalStore(), true);

    const guard = await loadGuardState(browserLocalStore()!);
    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => isBackfillEnabled(browserLocalStore()),
      isDownloadPaused: () => isGuardTripped(guard),
      // 🔴 生产构建里就是这个值：没有任何代码注入 http 端口。
      hasHttp: false,
    });
    expect(block).toBe('no-http-port');

    const snapshot = await browserLocalSnapshot();
    const state = pickBackfillState(snapshot);
    const view = renderPopup({
      enabled: true,
      block,
      guard,
      state,
      target: state ? { platform: state.platform, scope: state.scope } : null,
      // C20：走真实的汇总函数，不写死 —— 本用例的夹具里没有失败项，所以它是空的。
      failures: collectFailures(snapshot),
    });
    const out = popupText(view);

    // 🔴 三条判据。
    expect(view.running).toContain('未在运行');
    expect(view.missing).not.toBeNull();
    expect(view.missing!).toContain('取数通道');
    expect(out).not.toContain('%');

    // 🔴 绝不许出现的说法。
    for (const forbidden of ['正在归档', '正在回溯', '正在运行', '预计剩余']) {
      expect(out).not.toContain(forbidden);
    }

    // 开关本身要如实显示成「开」，不能因为跑不动就假装是关的。
    expect(view.toggle.checked).toBe(true);
    expect(view.status).toContain('开关是开的');
  });

  it('闸门判断与 tickBackfill 是同一个函数 —— 真跑一次 tick 得到同样的结论', async () => {
    seedState();
    const { browserLocalStore } = await import('../lib/backfill/store');
    const schedule = await import('../lib/backfill/schedule');
    await schedule.setBackfillEnabled(browserLocalStore(), true);
    schedule.resetTickLockForTest();

    const tick = await schedule.tickBackfill({
      store: browserLocalStore(),
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope: 'acct-1',
      // http 不注入 —— 与生产构建一致。
    });
    expect(tick).toEqual({ ran: false, reason: 'no-http-port', report: null });
  });

  it('开关是关的时候，文案说的是「开关没有打开」而不是缺端口', async () => {
    const { tickBlockReason } = await import('../lib/backfill/schedule');
    const { renderPopup, popupText, NO_FAILURES } = await import('../lib/popup-view');
    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => false,
      isDownloadPaused: () => false,
      hasHttp: false,
    });
    expect(block).toBe('disabled');
    const out = popupText(renderPopup({
      enabled: false, block, guard: null, state: null, target: null, failures: NO_FAILURES,
    }));
    expect(out).toContain('未在运行');
    expect(out).toContain('开关没有打开');
    expect(out).not.toContain('%');
  });
});

// ---------------------------------------------------------------------------
// 3 · 熔断态
// ---------------------------------------------------------------------------
describe('C18-3 · 熔断态', () => {
  it('显示告警，并且开关仍然可切', async () => {
    seedState();
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const { setBackfillEnabled, isBackfillEnabled, tickBlockReason } =
      await import('../lib/backfill/schedule');
    const { recordDownloadOutcome, stalledResult, loadGuardState, isGuardTripped } =
      await import('../lib/download-guard');
    const { renderPopup, popupText, pickBackfillState, collectFailures } = await import('../lib/popup-view');

    await setBackfillEnabled(browserLocalStore(), true);
    // 真把守卫打到熔断：连续 3 次停滞。
    for (let i = 0; i < 3; i += 1) {
      await recordDownloadOutcome(browserLocalStore(), stalledResult(15_000), { now: 1_000 + i });
    }
    const guard = await loadGuardState(browserLocalStore()!);
    expect(isGuardTripped(guard)).toBe(true);

    const block = await tickBlockReason({
      hasStore: true,
      isEnabled: () => isBackfillEnabled(browserLocalStore()),
      isDownloadPaused: () => isGuardTripped(guard),
      hasHttp: false,
    });
    expect(block).toBe('download-paused');

    const snapshot = await browserLocalSnapshot();
    const state = pickBackfillState(snapshot);
    const view = renderPopup({
      enabled: true, block, guard, state,
      target: state ? { platform: state.platform, scope: state.scope } : null,
      failures: collectFailures(snapshot),
    });
    const out = popupText(view);

    expect(view.status).toContain('停滞');
    expect(view.running).toContain('未在运行');
    expect(out).toContain('已暂停自动回溯');
    expect(out).toContain('数据没有丢');
    // 🔴 熔断不剥夺用户切开关的权利。
    expect(view.toggle.disabled).toBe(false);
    expect(view.toggle.checked).toBe(true);
    expect(out).not.toContain('%');
  });
});

// ---------------------------------------------------------------------------
// 4 · 进度文案必须来自 C11，不许另写一份
// ---------------------------------------------------------------------------
describe('C18-4 · 进度文案守 C11 的规矩', () => {
  it('分母可信时才出现百分比，且与 formatProgress 逐字一致', async () => {
    const { renderPopup, NO_FAILURES } = await import('../lib/popup-view');
    const { formatProgress } = await import('../lib/backfill/progress');
    const trusted: BackfillState = {
      v: 1, platform: 'chatgpt', scope: 'acct-1',
      totalKnown: 10, totalSource: 'response-total',
      enumCursor: { offset: 10, complete: true },
      pending: ['d1'], archived: ['a1', 'a2', 'a3'],
      detailToday: { day: '2026-08-17', count: 3 }, halted: null,
    };
    const view = renderPopup({
      enabled: true, block: 'no-http-port', guard: null, state: trusted,
      target: { platform: 'chatgpt', scope: 'acct-1' }, failures: NO_FAILURES,
    });
    expect(view.progress).toBe(`进度：${formatProgress(trusted)}`);
    expect(view.progress).toContain('30%');
  });

  it('分母不可信时 progress 行不含百分号', async () => {
    const { renderPopup, NO_FAILURES } = await import('../lib/popup-view');
    const untrusted: BackfillState = {
      v: 1, platform: 'chatgpt', scope: 'acct-1',
      totalKnown: null, totalSource: 'unknown',
      enumCursor: { offset: 0, complete: false },
      pending: ['d1'], archived: [],
      detailToday: { day: '', count: 0 }, halted: null,
    };
    const view = renderPopup({
      enabled: true, block: 'no-http-port', guard: null, state: untrusted,
      target: { platform: 'chatgpt', scope: 'acct-1' }, failures: NO_FAILURES,
    });
    expect(view.progress).not.toContain('%');
    expect(view.progress).toContain('总数未知');
  });

  it('pickBackfillState 只认键与值对得上的集合', async () => {
    const { pickBackfillState } = await import('../lib/popup-view');
    const good = seedState('chatgpt', 'acct-1');
    expect(pickBackfillState({ ...store, 'cs_backfill_v1:bogus:x': { v: 1 } })).toEqual(good);
    expect(pickBackfillState({ cs_count: 3 })).toBeNull();
    expect(pickBackfillState(null)).toBeNull();
  });
});
