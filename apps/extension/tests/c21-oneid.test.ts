/**
 * C21 · 一个会话只该有一个身份。
 *
 * 根因（不是症状）：同一个会话的身份在链路上被【表达了两次】——
 *   1. 欠账键   = 列表接口的 items[].id（lib/backfill/enumerate.ts:64-68）
 *   2. 落盘文件名 = 从 URL 里【再抠一次】（lib/contract.ts:485 extractSessionId
 *      → entrypoints/background.ts:103 的 `${platform}-${sanitizePathSegment(...)}`）
 * 两次表达之间是两个有损函数（正则截断 / 字符替换），所以
 * **两个不同的欠账键可以塌成同一个文件名**，后写的把先写的覆盖掉
 * （lib/download.ts:120,133 conflictAction:'overwrite'）。
 *
 * 本文件钉住三件事：
 *  1. 🔴 两个不同的会话 id，走完整条链路之后，落盘文件名必须不同（先红）。
 *  2. 🔴 起不出安全文件名的 id 绝不硬塞：进失败清单，绝不与别人塌成同名。
 *  3. 🔴 实时腿的既有行为逐字未变（它没有「枚举给的 id」，只能靠 URL 抠）。
 *
 * 全程零真实网络、零登录态：http 端口是本文件里的一个纯函数，
 * browser.downloads 只是把 filename 记进数组。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import type { CapturedFetch } from '../lib/contract';

// ---------------------------------------------------------------------------
// 假浏览器（与 c17 / c20 同构）
// ---------------------------------------------------------------------------
const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const downloadCalls: Array<{ id: number; filename: string; url: string }> = [];
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
      downloadCalls.push({ id, filename: opts.filename, url: opts.url });
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

/** 实时腿的那一条：从页面响应里来，【没有】枚举给的 id。 */
const LIVE_SID = 'aaaaaaaa-1111-2222-3333-444444444444';
function liveCapture(): CapturedFetch {
  return {
    url: `https://chatgpt.com/backend-api/conversation/${LIVE_SID}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-fixture-1' }),
    pageUrl: `https://chatgpt.com/c/${LIVE_SID}`,
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

/** 最终文件（去掉 .part 那一半）。 */
const finalFiles = (): string[] =>
  downloadCalls.filter((d) => !d.filename.endsWith('.part')).map((d) => d.filename);

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
  const { resetTickLockForTest, setBackfillEnabled } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
});

// ===========================================================================
// 用例 1 · 🔴 两个不同的欠账键 ⇒ 两个不同的文件名（先红）
// ===========================================================================
describe('C21-1 · 两个不同的会话 id，走完整条链路，落盘文件名必须不同', () => {
  it('🔴 正则截断型：id 的尾巴不在 [0-9a-fA-F-] 字符集里，旧链路会把两条塌成同一个名字', async () => {
    // 两个欠账键都是列表接口的合法 id（只要求非空 string），前缀相同、尾巴不同。
    // 旧链路：extractSessionId 用 /backend-api/conversation/([0-9a-fA-F-]{8,}) 再抠一次
    //        ⇒ 'Z' 不在字符集里 ⇒ 两条都被截成 'aaaaaaaa-bbbb' ⇒ 同一个文件名。
    const idA = 'aaaaaaaa-bbbb';
    const idB = 'aaaaaaaa-bbbbZZZZ';
    const server = makeServer([idA, idB]);
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    // 一次 tick 只清 1 笔账（DEFAULT_TICK_DETAILS=1），所以踢两脚。
    await bootAndDispatch(liveCapture());
    await bootAndDispatch(liveCapture());

    const s = stateOf();
    // 只看这两条欠账写出来的文件（实时腿自己那条另算，见用例 3）。
    const debtFiles = finalFiles().filter((f) => f.includes('aaaaaaaa-bbbb'));
    console.log('[C21-1] 两个欠账键:', [idA, idB]);
    console.log('[C21-1] detail 请求:', server.calls.filter((u) => u.includes('/conversation/')));
    console.log('[C21-1] 这两条欠账写出的最终文件:', debtFiles);
    console.log('[C21-1] 账本:', { pending: s.pending, archived: s.archived, failures: s.failures });

    // (a) 两条都真的取过正文
    expect(server.calls.filter((u) => u.includes(encodeURIComponent(idA))).length).toBeGreaterThan(0);
    expect(server.calls.filter((u) => u.includes(encodeURIComponent(idB))).length).toBeGreaterThan(0);

    // (b) 🔴 本任务的判据：两个文件名必须【不同】
    expect(debtFiles).toHaveLength(2);
    expect(new Set(debtFiles).size).toBe(2);

    // (c) 文件名里带的必须是【欠账键本身】，不是被截短的那个
    expect(debtFiles.some((f) => f.endsWith(`chatgpt-${idA}.json`))).toBe(true);
    expect(debtFiles.some((f) => f.endsWith(`chatgpt-${idB}.json`))).toBe(true);

    // (d) 两条都清了账，一条失败都没有（身份没被重新推导过，就不可能对不上）
    expect(s.archived.sort()).toEqual([idA, idB].sort());
    expect(s.failures ?? []).toEqual([]);
  });
});

// ===========================================================================
// 用例 2 · 起不出安全文件名的 id：宁可留痕，也绝不与别人塌成同名
// ===========================================================================
describe('C21-2 · 文件名不安全的 id ⇒ 不落盘、进失败清单', () => {
  it('两个只差在被 sanitize 掉的那个字符上的 id，一个文件都不许写', async () => {
    // sanitizePathSegment 把 ' ' 和 '/' 都换成 '_' ⇒ 这两个 id 曾经塌成同一个名字。
    const idA = 'aaaaaaaa bbbbbbbb';
    const idB = 'aaaaaaaa/bbbbbbbb';
    const server = makeServer([idA, idB]);
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    await bootAndDispatch(liveCapture());
    await bootAndDispatch(liveCapture());

    const s = stateOf();
    // 🔴 实时腿自己那条（LIVE_SID）不算 —— 这里只看这两个欠账键写出了什么。
    const debtFiles = finalFiles().filter((f) => !f.includes(LIVE_SID));
    console.log('[C21-2] 两个欠账键:', [idA, idB]);
    console.log('[C21-2] 写出的文件:', debtFiles);
    console.log('[C21-2] 失败清单:', JSON.stringify(s.failures, null, 2));

    expect(debtFiles).toEqual([]);        // 一个字节都没写 ⇒ 不可能塌
    expect(s.archived).toEqual([]);       // 也没有一条冒充成功
    expect(s.failures).toHaveLength(2);
    expect(s.failures.every((f: any) => f.reason === 'not-saved')).toBe(true);
  });
});

// ===========================================================================
// 用例 3 · 🔴 实时腿的既有行为没被改变
// ===========================================================================
describe('C21-3 · 实时腿（没有枚举给的 id）行为逐字未变', () => {
  it('仍然从 URL 抠 id、仍然存成同一个文件名、仍然回 saved:true', async () => {
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(undefined);   // 回溯腿不接线 ⇒ 只剩实时腿这一条路
    const payload = liveCapture();
    const result = await new Promise<any>(async (resolve) => {
      if (runtimeListeners.length === 0) await mod.default();
      runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    });
    await mod.backfillTickSettled();

    console.log('[C21-3] 实时腿的返回:', result);
    console.log('[C21-3] 实时腿写出的文件:', finalFiles());

    expect(result.ok).toBe(true);
    expect(result.saved).toBe(true);
    // 🔴 与 C20 之前逐字相同的文件名：平台前缀 + 从 URL 抠出来的 sessionId
    expect(result.finalName).toBe(`chat-stasher/inbox/chatgpt-${LIVE_SID}.json`);
    expect(result.sessionId).toBe(LIVE_SID);
    expect(finalFiles()).toEqual([`chat-stasher/inbox/chatgpt-${LIVE_SID}.json`]);
  });

  it('🔴 页面【不许】自己指定身份：带 sessionId 的页面载荷一律不认', async () => {
    const { isCapturedFetchShape } = await import('../lib/contract');
    const clean = liveCapture();
    console.log('[C21-3b] 干净载荷通过校验:', isCapturedFetchShape(clean));
    expect(isCapturedFetchShape(clean)).toBe(true);
    // 页面若能塞 sessionId，就等于页面能指定写到哪个文件名 —— 必须挡死。
    const spoofed = { ...clean, sessionId: '../../../etc/passwd' };
    console.log('[C21-3b] 带 sessionId 的载荷通过校验:', isCapturedFetchShape(spoofed));
    expect(isCapturedFetchShape(spoofed)).toBe(false);
  });
});
