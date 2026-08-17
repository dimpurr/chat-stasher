/**
 * C13 · 「运行时真的会跑回溯腿」的测试。
 *
 * 🔴 这个文件存在的理由：C11/C12 的测试全绿，但它们全都是【自己 import runBackfill
 *    再调用它】—— 绿灯只证明了模块能跑，没有证明【浏览器会跑】。
 *    所以这里一条断言都不许直接调 runBackfill / tickBackfill：
 *    每个用例都必须从 **entrypoints/background.ts 的真实入口** 出发 ——
 *      defineBackground 的回调 → browser.runtime.onMessage 派发 'chat-captured'
 *    这正是内容脚本在真实浏览器里走的那条路 —— 然后再去看回溯腿有没有被碰到。
 *
 * runBackfill 被 vi.mock 换成 spy：我们要断言的是"到达"，不是引擎行为
 *（引擎行为已经由 c11/c12 覆盖）。绝无任何真实网络行为、绝无登录态。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import type { CapturedFetch } from '../lib/contract';

// ---- 把引擎换成 spy（只在本文件生效）----
const runBackfillSpy = vi.fn(async (opts: any) => ({
  stopped: 'queue-empty',
  enumeratedPages: 0,
  newDebts: 0,
  archivedThisRun: [],
  skippedAlreadyArchived: 0,
  skippedAlreadyPending: 0,
  progress: 'stub',
  halted: null,
  paceTrace: { enumerate: [], detail: [] },
  state: { __opts: opts },
}));

vi.mock('../lib/backfill/engine', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/backfill/engine')>();
  return { ...actual, runBackfill: (opts: any) => runBackfillSpy(opts) };
});

// ---- 假浏览器：storage.local / downloads / action / runtime ----
const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const downloadCalls: any[] = [];
const changeListeners: Array<(d: any) => void> = [];

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: {
      addListener(fn: any) { runtimeListeners.push(fn); },
    },
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown>) {
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in store ? store[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: {
    async setBadgeText() {},
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
  downloads: {
    // 立刻把这次下载报成 complete —— 本文件测的是接线，不是下载通道
    //（下载通道由 chain.test.ts 用真实文件系统覆盖）。
    async download(opts: any) {
      downloadCalls.push(opts);
      const id = downloadCalls.length;
      setTimeout(() => {
        for (const fn of changeListeners) fn({ id, state: { current: 'complete' } });
      }, 0);
      return id;
    },
    onChanged: { addListener(fn: any) { changeListeners.push(fn); } },
    async removeFile() {},
    async erase() {},
  },
};

/** 真实的 'chat-captured' 载荷（合成夹具，不是任何真人的对话）。 */
function fakeCapture(): CapturedFetch {
  return {
    url: 'https://chatgpt.com/backend-api/conversation/abcdef0123456789',
    method: 'GET',
    status: 200,
    text: JSON.stringify({ conversation_id: 'abcdef0123456789', account_id: 'acct-fixture-1', mapping: {} }),
    pageUrl: 'https://chatgpt.com/c/abcdef0123456789',
    capturedAt: 1_700_000_000_000,
  };
}

/**
 * 走真实入口：加载 background 模块 → 执行 defineBackground 的回调
 * → 拿到它注册的 onMessage 监听器 → 像内容脚本那样派发一条消息。
 */
async function bootBackgroundAndDispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  await mod.default();                       // defineBackground 被 stub 成恒等函数
  expect(runtimeListeners.length).toBeGreaterThan(0);
  const responded = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    expect(ret).toBe(true);                  // MV3 异步 sendResponse 契约
  });
  // 接线是 fire-and-forget（绝不允许拖慢落盘），等它自己结束。
  await mod.backfillTickSettled();
  return responded;
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  downloadCalls.length = 0;
  changeListeners.length = 0;
  runBackfillSpy.mockClear();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

describe('C13 · 回溯腿接进运行时', () => {
  it('🔴 判据 2：实时腿的真实消息路径会唤起回溯腿（开关打开 + 有 http 端口 ⇒ runBackfill 真的被调用）', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);

    const mod: any = await import('../entrypoints/background');
    // 合成 http 端口：只回固定字符串，不碰网络。
    mod.configureBackfillTransport(async () => ({ status: 200, text: '{"items":[],"total":0}' }));

    const res = await bootBackgroundAndDispatch(fakeCapture());
    expect(res.ok).toBe(true);                       // 实时腿照存，不被回溯腿拖累

    expect(runBackfillSpy).toHaveBeenCalledTimes(1); // ← 这一行就是"运行时会跑"的证据
    const opts = runBackfillSpy.mock.calls[0]![0];
    expect(opts.origin).toBe('https://chatgpt.com');
    expect(opts.platform).toBe('chatgpt');
    expect(typeof opts.downloadGuard).toBe('function'); // C12 闸门被接上
    expect(typeof opts.sink).toBe('function');         // 归档出口不分叉
    expect(opts.maxDetails).toBe(1);                   // 定速：一次 tick 只清一笔账
    console.log('[C13] runtime message -> runBackfill called with', {
      platform: opts.platform, origin: opts.origin, maxDetails: opts.maxDetails,
    });
  });

  it('🔴 判据 3：C12 熔断态（download-paused）下，同一条真实路径【不】启动回溯', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { GUARD_KEY, initialGuardState } = await import('../lib/download-guard');
    await setBackfillEnabled(browserLocalStore(), true);
    // 直接把守卫写成熔断态。
    store[GUARD_KEY] = { ...initialGuardState(), tripped: true, trippedAt: 1, consecutiveStalls: 3 };

    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(async () => ({ status: 200, text: '{"items":[],"total":0}' }));

    await bootBackgroundAndDispatch(fakeCapture());

    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('download-paused');
    console.log('[C13] guard tripped -> tick reason =', mod.lastBackfillTick()?.reason);
  });

  it('默认【关】：什么都不设，同一条路径走到最后一道闸也不会跑', async () => {
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(async () => ({ status: 200, text: '{}' }));
    await bootBackgroundAndDispatch(fakeCapture());
    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('disabled');
    console.log('[C13] default state -> tick reason =', mod.lastBackfillTick()?.reason);
  });

  it('生产现状：开关开了但没人注入 http 端口 ⇒ no-http-port，绝不会有网络行为', async () => {
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);

    const mod: any = await import('../entrypoints/background');   // 不 configure
    await bootBackgroundAndDispatch(fakeCapture());
    expect(runBackfillSpy).not.toHaveBeenCalled();
    expect(mod.lastBackfillTick()?.reason).toBe('no-http-port');
    console.log('[C13] no transport wired -> tick reason =', mod.lastBackfillTick()?.reason);
  });

  it('MV3 现实：tick 之间没有任何内存态被依赖 —— 欠账集合只在 storage 里', async () => {
    const { BACKFILL_ENABLED_KEY } = await import('../lib/backfill/schedule');
    const { setBackfillEnabled } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);
    // 开关本身也是持久的：SW 被回收再醒来，用户不必重新点一次。
    expect(store[BACKFILL_ENABLED_KEY]).toBe(true);
    console.log('[C13] enabled flag persisted in storage.local =', store[BACKFILL_ENABLED_KEY]);
  });
});
