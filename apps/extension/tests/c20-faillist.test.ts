/**
 * C20 · 落盘失败要进失败清单，不许悄悄清账。
 *
 * 产品主人拍板的修法（走 C）：
 *   **落盘失败的【不重试】，但【挪进一个用户看得见的失败清单】。**
 *   理由：「丢了」和「丢了但你知道」是完全不同的两件事，
 *         而这个项目的立身之本就是后者。
 *
 * 三个用例：
 *  1. sink 成功 ⇒ 欠账被清、失败清单为空；
 *  2. 🔴 sink 失败 ⇒ 欠账【不被清】、进失败清单、Popup 文案变化；
 *  3. 失败清单达到上限 ⇒ 丢最旧的，并把丢掉的条数记下来（绝不静默截断）。
 *
 * 用例 1/2 都从【真实生产入口】出发：runtime.onMessage('chat-captured')
 *  → handleCaptured → kickBackfill → tickBackfill → runBackfill → sink。
 * 全程零真实网络、零登录态：http 端口是本文件里的一个纯函数，
 * browser.downloads 只是把 filename 记进数组。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import type { CapturedFetch } from '../lib/contract';

// ---------------------------------------------------------------------------
// 假浏览器（与 c17 同构）。storage 跨 resetModules 存活 = 「浏览器重启」的模型。
// ---------------------------------------------------------------------------
const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const downloadCalls: Array<{ id: number; filename: string }> = [];
const changeListeners: Array<(d: any) => void> = [];

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
  },
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
};

/** 合成「服务器」。绝不碰网络：就是一个 (url) => {status,text} 的纯函数。 */
function makeServer(ids: string[], total: number | null = ids.length) {
  const calls: string[] = [];
  const port = async (url: string) => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/backend-api/conversations') {
      const offset = Number(u.searchParams.get('offset') ?? '0');
      const items = ids.slice(offset).map((id) => ({ id, title: 'synthetic' }));
      const body: Record<string, unknown> = { items };
      if (total !== null) body.total = total;
      return { status: 200, text: JSON.stringify(body) };
    }
    const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
    return {
      status: 200,
      text: JSON.stringify({
        mapping: { n1: { id: 'n1', message: { content: { parts: [`synthetic body for ${id}`] } } } },
        current_node: 'n1',
        account_id: 'acct-fixture-1',
      }),
    };
  };
  return { port, calls };
}

function liveCapture(): CapturedFetch {
  const sid = 'aaaaaaaa-1111-2222-3333-444444444444';
  return {
    url: `https://chatgpt.com/backend-api/conversation/${sid}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-fixture-1' }),
    pageUrl: `https://chatgpt.com/c/${sid}`,
    capturedAt: 1_700_000_000_000,
  };
}

let fakeNow = 1_700_000_000_000;
const fakeClock = { now: () => fakeNow, sleep: async (ms: number) => { fakeNow += ms; } };

async function bootAndDispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: fakeClock });
  if (runtimeListeners.length === 0) await mod.default();
  await new Promise<any>((resolve) => {
    runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
  });
  await mod.backfillTickSettled();
  return mod;
}

const STATE_KEY = 'cs_backfill_v1:chatgpt:acct-fixture-1';
const stateOf = (): any => store[STATE_KEY] ?? null;

/** 用 Popup 真正的那条链渲染一次（不另写一份渲染）。 */
async function popupNow(): Promise<{ view: any; text: string }> {
  const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
  const { isBackfillEnabled, tickBlockReason } = await import('../lib/backfill/schedule');
  const { loadGuardState, isGuardTripped } = await import('../lib/download-guard');
  const { renderPopup, popupText, pickBackfillState, collectFailures } =
    await import('../lib/popup-view');

  const st = browserLocalStore();
  const snapshot = await browserLocalSnapshot();
  const guard = st ? await loadGuardState(st) : null;
  const enabled = await isBackfillEnabled(st);
  const state = pickBackfillState(snapshot);
  const block = await tickBlockReason({
    hasStore: st !== null,
    isEnabled: () => enabled,
    isDownloadPaused: () => (guard ? isGuardTripped(guard) : false),
    // 用例里取数通道是注入的显式 transport ⇒ 这一位在 Popup 眼里是 true。
    hasHttp: true,
  });
  const view = renderPopup({
    enabled, block, guard, state,
    target: state ? { platform: state.platform, scope: state.scope } : null,
    failures: collectFailures(snapshot),
  });
  return { view, text: popupText(view) };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  downloadCalls.length = 0;
  changeListeners.length = 0;
  fakeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
});

// ===========================================================================
// 用例 1 · sink 成功
// ===========================================================================
describe('C20-1 · sink 成功 ⇒ 欠账被清、失败清单为空', () => {
  it('走真实入口：落盘成功的那一条进 archived，failures 一条都没有', async () => {
    // 合法 uuid ⇒ extractSessionId 抠得出来 ⇒ handleCaptured 返回 saved:true，
    // 且它用来命名的身份与欠账键逐字相同 ⇒ 一致性校验也过。
    const id = 'b1111111-0000-4000-8000-000000000001';
    const server = makeServer([id]);
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    await bootAndDispatch(liveCapture());

    const s = stateOf();
    const files = downloadCalls.filter((d) => !d.filename.endsWith('.part')).map((d) => d.filename);
    console.log('[C20-1] 落盘的最终文件:', files.filter((f) => f.includes('b1111111')));
    console.log('[C20-1] 欠账账本:', { pending: s.pending, archived: s.archived, failures: s.failures });
    console.log('[C20-1] 进度文案:', mod.lastBackfillTick()!.report!.progress);

    expect(files.some((f) => f.includes(id))).toBe(true);   // 真的写出去了
    expect(s.archived).toEqual([id]);                        // 欠账被清
    expect(s.pending).toEqual([]);
    expect(s.failures).toEqual([]);                          // 🔴 失败清单为空
    expect(s.failuresDropped).toBe(0);
    expect(mod.lastBackfillTick()!.report!.failedThisRun).toEqual([]);

    const { view, text } = await popupNow();
    console.log('[C20-1] Popup 全文:\n' + text);
    expect(view.failures).toBeNull();                        // 没有失败项 ⇒ 不出那一行
    expect(view.clearFailures.visible).toBe(false);
  });
});

// ===========================================================================
// 用例 2 · 🔴 sink 失败（这一条必须先红过）
// ===========================================================================
describe('C20-2 · sink 失败 ⇒ 欠账不被清、进失败清单、Popup 文案变化', () => {
  it('🔴 抠不到 sessionId ⇒ handleCaptured 返回 saved:false ⇒ 一个字节都没写、账也不许划掉', async () => {
    // 'shortid' 是合法的欠账键（列表接口只要求非空 string），但 chatgpt 的
    // sessionIdPatterns 是 /backend-api/conversation/([0-9a-fA-F-]{8,})，
    // 'shortid' 不是 hex ⇒ extractSessionId 返回 null ⇒ handleCaptured 直接
    // 返回 { saved:false, reason:'no-session-id ...' }，一个文件都不写。
    const server = makeServer(['shortid']);
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    downloadCalls.length = 0;

    await bootAndDispatch(liveCapture());

    const s = stateOf();
    const files = downloadCalls.filter((d) => !d.filename.endsWith('.part')).map((d) => d.filename);
    console.log('[C20-2] detail 请求:', server.calls.filter((u) => u.includes('/conversation/')));
    console.log('[C20-2] 与 shortid 有关的落盘文件:', files.filter((f) => f.includes('shortid')));
    console.log('[C20-2] 欠账账本:', { pending: s.pending, archived: s.archived });
    console.log('[C20-2] 🔴 失败清单条目真实样例:', JSON.stringify(s.failures, null, 2));
    console.log('[C20-2] 进度文案:', mod.lastBackfillTick()!.report!.progress);

    // (a) 确实取到了正文，但一个字节都没落盘
    expect(server.calls.filter((u) => u.includes('/conversation/')).length).toBe(1);
    expect(files.filter((f) => f.includes('shortid'))).toEqual([]);

    // (b) 🔴 欠账【不被清】—— 既不进 archived，进度分子也就不会说谎
    expect(s.archived).toEqual([]);
    expect(mod.lastBackfillTick()!.report!.progress).toContain('已归档 0');
    expect(mod.lastBackfillTick()!.report!.progress).not.toContain('100%');

    // (c) 🔴 进失败清单
    expect(s.failures).toHaveLength(1);
    expect(s.failures[0].shortId).toBe('shortid');
    expect(s.failures[0].reason).toBe('not-saved');
    expect(s.failures[0].platform).toBe('chatgpt');
    expect(typeof s.failures[0].at).toBe('number');
    // 🔴 自证不存敏感物：条目里只有这四个字段，没有正文、没有 URL。
    expect(Object.keys(s.failures[0]).sort()).toEqual(['at', 'platform', 'reason', 'shortId']);
    const blob = JSON.stringify(s.failures);
    expect(blob).not.toContain('synthetic body');        // 没有对话正文
    expect(blob).not.toContain('http');                  // 没有任何 URL
    expect(blob).not.toContain('chatgpt.com');

    // (d) 🔴 不重试：它既不在 pending 里排队，下一次 tick 也不会把它捡回来
    expect(s.pending).toEqual([]);
    const detailsBefore = server.calls.filter((u) => u.includes('/conversation/')).length;
    await bootAndDispatch(liveCapture());
    const s2 = stateOf();
    console.log('[C20-2] 再踢一脚之后:', {
      detailCalls: server.calls.filter((u) => u.includes('/conversation/')).length,
      failures: s2.failures.length, archived: s2.archived,
    });
    expect(server.calls.filter((u) => u.includes('/conversation/')).length).toBe(detailsBefore);
    expect(s2.failures).toHaveLength(1);                 // 没有重试，也没有重复记账

    // (e) 🔴 Popup 文案变化 —— 不许显示成一切正常
    const { view, text } = await popupNow();
    console.log('[C20-2] 🔴 Popup 全文（有失败项时）:\n' + text);
    expect(view.failures).not.toBeNull();
    expect(view.failures).toContain('没有存下来');
    expect(view.failures).toContain('不会自动再试');
    expect(view.failures).toContain('1 条');
    expect(view.clearFailures.visible).toBe(true);
    expect(text).toContain('shortid…');                  // 短 ID 出现在详情里
    expect(text).not.toContain('synthetic body');        // 但正文绝不出现
    expect(text).not.toContain('https://');              // 完整 URL 也不出现

    // (f) 「确认/清空」之后清单空了，而且【没有触发任何重新抓取】
    const { backfillStateEntries, collectFailures } = await import('../lib/popup-view');
    const { clearFailures } = await import('../lib/backfill/failures');
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const st = browserLocalStore()!;
    for (const { key, state } of backfillStateEntries(await browserLocalSnapshot())) {
      clearFailures(state);
      await st.save(key, state);
    }
    const after = collectFailures(await browserLocalSnapshot());
    console.log('[C20-2] 清空之后的汇总:', after);
    expect(after).toEqual({ entries: [], dropped: 0 });
    expect(server.calls.filter((u) => u.includes('/conversation/')).length).toBe(detailsBefore);
    expect((await popupNow()).view.failures).toBeNull();
  });
});

// ===========================================================================
// 用例 3 · 上限
// ===========================================================================
describe('C20-3 · 失败清单达到上限 ⇒ 丢最旧的，并把丢掉的条数记下来', () => {
  it('55 条全失败：清单只留最新的 50 条，dropped = 5，且文案照实说', async () => {
    const { runBackfill } = await import('../lib/backfill/engine');
    const { memoryStore } = await import('../lib/backfill/store');
    const { MAX_FAILURES } = await import('../lib/backfill/failures');
    const { renderPopup, popupText, NO_FAILURES } = await import('../lib/popup-view');

    const n = MAX_FAILURES + 5;
    const ids = Array.from({ length: n }, (_, i) => `fail-${String(i).padStart(3, '0')}`);
    const server = makeServer(ids);
    const st = memoryStore();
    let now = 1_700_000_000_000;

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope: 'cap-fixture',
      store: st,
      http: server.port,
      clock: { now: () => (now += 1_000), sleep: async () => {} },
      pace: {
        enumerate: { minIntervalMs: 0, maxPerDay: null },
        detail: { minIntervalMs: 0, maxPerDay: null },
      },
      maxDetails: n,
      // 每一条都明确报「没存下来」。
      sink: async () => ({ saved: false, reason: 'synthetic failure' }),
    });

    const state: any = st.data['cs_backfill_v1:chatgpt:cap-fixture'];
    console.log('[C20-3] 上限:', MAX_FAILURES, '· 投喂:', n);
    console.log('[C20-3] 清单长度:', state.failures.length, '· dropped:', state.failuresDropped);
    console.log('[C20-3] 清单头两条:', state.failures.slice(0, 2));
    console.log('[C20-3] 清单末两条:', state.failures.slice(-2));

    expect(report.stopped).toBe('queue-empty');
    expect(state.archived).toEqual([]);                        // 一条都没冒充成功
    expect(state.pending).toEqual([]);                         // 也没有一条留下来重试
    expect(state.failures).toHaveLength(MAX_FAILURES);
    expect(state.failuresDropped).toBe(5);
    // 🔴 丢最旧的：留下的是 fail-005 .. fail-054（最新的 50 条）。
    expect(state.failures[0].shortId).toBe('fail-005');
    expect(state.failures[MAX_FAILURES - 1].shortId).toBe('fail-054');

    // 🔴 绝不静默截断：文案必须把「另有 5 条更早的不在清单里」说出来。
    const view = renderPopup({
      enabled: true, block: null, guard: null, state,
      target: { platform: 'chatgpt', scope: 'cap-fixture' },
      failures: { entries: state.failures, dropped: state.failuresDropped },
    });
    console.log('[C20-3] Popup 失败行:', view.failures);
    expect(view.failures).toContain(`最多留 ${MAX_FAILURES} 条`);
    expect(view.failures).toContain('另有更早的 5 条');
    expect(popupText(view)).toContain('不会自动再试');

    // 仪器自证：同一个渲染器在空清单下【看不见】这一行。
    expect(renderPopup({
      enabled: true, block: null, guard: null, state,
      target: { platform: 'chatgpt', scope: 'cap-fixture' }, failures: NO_FAILURES,
    }).failures).toBeNull();
  });
});
