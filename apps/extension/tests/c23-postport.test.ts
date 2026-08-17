/**
 * C23 · 回溯通道从「只能 GET」扩成「能表达 POST，且 method/body 都受白名单约束」。
 *
 * 🔴 全部合成夹具。没有任何一行会碰真实平台接口，没有登录态，没有真实对话正文。
 * 🔴 **本任务不接任何平台**：生产的 plan 表里仍然只有 ChatGPT（GET），
 *    kimi/gemini 的参数仍然没有出处。下面用到的 POST plan 是**测试自己造的**，
 *    通过 engine 的 `plans` 接缝与 tab-port 的 `lookup` 接缝注入，
 *    生产代码两处调用（background.ts kickBackfill / runAlarmTick）都没有这两个字段。
 *
 * 三条判据：
 *   1. GET 路径行为不变（对照测试：实参个数、wire 消息、fetch 调用全部逐字比对）
 *   2. POST 能带 body 发出，且 body/method 都受白名单约束
 *   3. 不在白名单里的 method 或 url ⇒ 拒绝，且留痕
 */

import { describe, it, expect, vi } from 'vitest';
import { runBackfill, type HttpPort, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { stateKey } from '../lib/backfill/types';
import {
  CHATGPT_PLAN,
  backfillPlanFor,
  detailRequestInit,
  listRequestInit,
  ALLOWED_BACKFILL_CONTENT_TYPES,
  ALLOWED_BACKFILL_METHODS,
  MAX_REQUEST_BODY_BYTES,
  type BackfillEnumPlan,
} from '../lib/backfill/enumerate';
import {
  BACKFILL_FETCH_MESSAGE,
  REFUSED_URL_REASON,
  checkBackfillRequest,
  handleBackfillMessage,
  serveBackfillFetch,
  tabHttpPort,
} from '../lib/backfill/tab-port';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const NO_WAIT = { enumerate: { minIntervalMs: 0, maxPerDay: null }, detail: { minIntervalMs: 0, maxPerDay: null } };

function fakeClock(): Clock {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

const IDS = ['conv-0000-aaaaaaaa', 'conv-0001-aaaaaaaa'];
const ID0 = IDS[0]!;

/** ChatGPT 形状的合成列表页。 */
function listText(ids: string[]): string {
  return JSON.stringify({ items: ids.map((id) => ({ id })), total: ids.length });
}
/** ChatGPT 形状的合成正文（requiredPaths: mapping / current_node）。 */
const DETAIL_TEXT = JSON.stringify({ mapping: {}, current_node: 'synthetic' });

// ---------------------------------------------------------------------------
// 合成的 POST plan —— 🔴 只存在于测试里，绝不进 BACKFILL_PLANS。
// 路径故意沿用 ChatGPT 那两条，好让「同源 + 平台表 + 路径」三关都通过，
// 于是这个用例检验的**只有 method/body 这两个新维度**。
// ---------------------------------------------------------------------------
const POST_PLAN: BackfillEnumPlan = {
  ...CHATGPT_PLAN,
  listUrl: (origin) => `${origin}${CHATGPT_PLAN.listPath}`,   // POST 段参数在 body 里，URL 不带 query
  listPost: {
    contentType: 'application/json',
    bodyKeys: ['offset', 'limit'],
    body: (_origin, offset, limit) => JSON.stringify({ offset, limit }),
  },
  detailPost: {
    contentType: 'application/json',
    bodyKeys: ['conversationId'],
    body: (_origin, id) => JSON.stringify({ conversationId: id }),
  },
};
const postLookup = (id: string): BackfillEnumPlan | null => (id === 'chatgpt' ? POST_PLAN : null);

// ===========================================================================
describe('C23-1 · GET 路径行为一个字不变（对照测试）', () => {
  it('engine 在 GET 段【只传一个实参】调用 http，且 CapturedFetch.method 仍是 GET', async () => {
    const seen: Array<{ url: string; argc: number }> = [];
    const http: HttpPort = async function (this: unknown, url: string): Promise<HttpResponse> {
      // eslint-disable-next-line prefer-rest-params
      seen.push({ url, argc: arguments.length });
      return new URL(url).pathname === CHATGPT_PLAN.listPath
        ? { status: 200, text: listText(IDS) }
        : { status: 200, text: DETAIL_TEXT };
    };
    const methods: string[] = [];
    const report = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-1',
      store: memoryStore(), http, clock: fakeClock(), pace: NO_WAIT,
      sink: (c) => { methods.push(c.method); return { saved: true, sessionId: c.sessionId }; },
    });

    console.log('[C23-1] http 每次调用的实参个数:', seen.map((s) => s.argc));
    console.log('[C23-1] CapturedFetch.method:', methods);
    console.log('[C23-1] 归档:', report.archivedThisRun, 'halted:', report.halted);

    expect(seen.every((s) => s.argc === 1)).toBe(true);   // 🔴 一个参数都没多传
    expect(methods).toEqual(['GET', 'GET']);
    expect(report.archivedThisRun).toEqual(IDS);
    expect(report.halted).toBeNull();
    // 生产的 chatgpt plan 到今天仍然是纯 GET —— 没有偷偷给它填 POST。
    expect(listRequestInit(CHATGPT_PLAN, ORIGIN, 0, 100)).toEqual({ method: 'GET' });
    expect(detailRequestInit(CHATGPT_PLAN, ORIGIN, ID0)).toEqual({ method: 'GET' });
    expect(backfillPlanFor('chatgpt')!.listPost).toBeUndefined();
    expect(backfillPlanFor('chatgpt')!.detailPost).toBeUndefined();
  });

  it('tabHttpPort 在 GET 段发出的 wire 消息逐字仍是 {type, url}', async () => {
    const sent: unknown[] = [];
    const port = tabHttpPort(7, async (_id, msg) => { sent.push(msg); return { ok: true, status: 200, text: 'x' }; });
    await port(`${ORIGIN}${CHATGPT_PLAN.listPath}?offset=0&limit=100`);
    await port(`${ORIGIN}${CHATGPT_PLAN.detailPath}abc`, { method: 'GET' });
    console.log('[C23-1] wire 消息:', JSON.stringify(sent));
    expect(sent).toEqual([
      { type: BACKFILL_FETCH_MESSAGE, url: `${ORIGIN}${CHATGPT_PLAN.listPath}?offset=0&limit=100` },
      { type: BACKFILL_FETCH_MESSAGE, url: `${ORIGIN}${CHATGPT_PLAN.detailPath}abc` },
    ]);
  });

  it('内容脚本在 GET 段调用 fetch 时也只传一个实参', async () => {
    const argcs: number[] = [];
    const fetchImpl = async function (url: string) {
      // eslint-disable-next-line prefer-rest-params
      argcs.push(arguments.length);
      return { status: 200, text: async () => DETAIL_TEXT, url };
    };
    const reply = await handleBackfillMessage(
      { type: BACKFILL_FETCH_MESSAGE, url: `${ORIGIN}${CHATGPT_PLAN.detailPath}abc` },
      ORIGIN,
      fetchImpl as never,
    );
    console.log('[C23-1] 内容脚本 fetch 实参个数:', argcs, '回复:', JSON.stringify(await reply));
    expect(argcs).toEqual([1]);
    expect(await reply).toEqual({ ok: true, status: 200, text: DETAIL_TEXT });
  });
});

// ===========================================================================
describe('C23-2 · POST 能带 body 发出，且 body/method 受白名单约束', () => {
  it('engine → tabHttpPort → 内容脚本 → fetch：POST 的 body 一路到达页面侧', async () => {
    const fetched: Array<{ url: string; init: unknown }> = [];
    const fetchImpl = async (url: string, init?: unknown) => {
      fetched.push({ url, init });
      const body = JSON.parse(String((init as { body?: string }).body));
      return {
        status: 200,
        text: async () => (new URL(url).pathname === CHATGPT_PLAN.listPath
          ? listText(IDS.slice(Number(body.offset), Number(body.offset) + Number(body.limit)))
          : DETAIL_TEXT),
      };
    };
    // 内容脚本这一跳走【真实的】handleBackfillMessage，只把 plan 表换成合成的那张。
    const port = tabHttpPort(9, async (_id, msg) =>
      handleBackfillMessage(msg, ORIGIN, fetchImpl as never, postLookup));

    const report = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-post',
      store: memoryStore(), http: port, clock: fakeClock(), pace: NO_WAIT,
      plans: postLookup,
      sink: (c) => ({ saved: true, sessionId: c.sessionId }),
    });

    console.log('[C23-2] 页面侧真正收到的请求:', JSON.stringify(fetched, null, 1));
    console.log('[C23-2] 归档:', report.archivedThisRun, 'halted:', report.halted);

    expect(report.halted).toBeNull();
    expect(report.archivedThisRun).toEqual(IDS);
    expect(fetched[0]).toEqual({
      url: `${ORIGIN}${CHATGPT_PLAN.listPath}`,
      init: { method: 'POST', body: JSON.stringify({ offset: 0, limit: 100 }), contentType: 'application/json' },
    });
    expect(fetched[1]!.init).toEqual({
      method: 'POST',
      body: JSON.stringify({ conversationId: ID0 }),
      contentType: 'application/json',
    });
  });

  it('method / contentType / bodyKeys 三个闭集都在代码里，且 body 只能是标量的闭集键', () => {
    expect(ALLOWED_BACKFILL_METHODS).toEqual(['GET', 'POST']);
    expect(ALLOWED_BACKFILL_CONTENT_TYPES).toEqual(['application/json']);

    const url = `${ORIGIN}${CHATGPT_PLAN.listPath}`;
    const ct = 'application/json';
    const cases: Array<[string, unknown, boolean]> = [
      ['闭集内的键 + 标量值', { offset: 0, limit: 100 }, true],
      ['多出一个键', { offset: 0, limit: 100, callback: 'https://evil.example.com' }, false],
      ['键在闭集里但值是嵌套对象', { offset: { $gt: 0 }, limit: 100 }, false],
      ['键在闭集里但值是数组', { offset: [1, 2, 3], limit: 100 }, false],
    ];
    for (const [label, body, allowed] of cases) {
      const v = checkBackfillRequest(
        { url, method: 'POST', body: JSON.stringify(body), contentType: ct }, ORIGIN, postLookup);
      console.log(`[C23-2] ${label} ⇒`, v.ok ? 'ALLOW' : `REFUSE(${(v as { detail: string }).detail})`);
      expect([label, v.ok]).toEqual([label, allowed]);
    }

    // body 不是 JSON 对象 / 超尺寸 / Content-Type 不对，一律拒。
    const oversize = JSON.stringify({ offset: 'x'.repeat(MAX_REQUEST_BODY_BYTES), limit: 1 });
    for (const [label, spec] of [
      ['body 不是 JSON', { url, method: 'POST', body: 'not-json', contentType: ct }],
      ['body 是数组', { url, method: 'POST', body: '[1,2]', contentType: ct }],
      ['body 超过 MAX_REQUEST_BODY_BYTES', { url, method: 'POST', body: oversize, contentType: ct }],
      ['Content-Type 不在闭集里', { url, method: 'POST', body: '{"offset":0}', contentType: 'text/plain' }],
      ['POST 但没有 body', { url, method: 'POST', contentType: ct }],
    ] as Array<[string, Parameters<typeof checkBackfillRequest>[0]]>) {
      const v = checkBackfillRequest(spec, ORIGIN, postLookup);
      console.log(`[C23-2] ${label} ⇒`, v.ok ? 'ALLOW' : `REFUSE(${(v as { detail: string }).detail})`);
      expect([label, v.ok]).toEqual([label, false]);
    }
  });
});

// ===========================================================================
describe('C23-3 · 不在白名单里的 method 或 url ⇒ 拒绝，且留痕', () => {
  it('GET 段被要求发 POST ⇒ 拒发、不调用 fetch、留痕', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const calls: string[] = [];
    const fetchImpl = async (url: string) => { calls.push(url); return { status: 200, text: async () => '{}' }; };
    // 生产 plan（chatgpt）的两段都是 GET —— 所以下面这条 POST 必须被拒。
    const reply = await serveBackfillFetch(
      { url: `${ORIGIN}${CHATGPT_PLAN.listPath}`, method: 'POST', body: '{"offset":0}', contentType: 'application/json' },
      ORIGIN,
      fetchImpl as never,
    );
    console.log('[C23-3] GET 段发 POST 的回复:', JSON.stringify(reply));
    console.log('[C23-3] 留痕:', warn.mock.calls.map((c) => String(c[0])));
    expect(reply).toEqual({ ok: false, error: 'refused: chatgpt list segment must be GET, got POST' });
    expect(calls).toEqual([]);                                    // 🔴 fetch 根本没被调用
    expect(warn.mock.calls.map((c) => String(c[0]))).toEqual([
      '[chat-stasher] backfill fetch refused: refused: chatgpt list segment must be GET, got POST',
    ]);
    warn.mockRestore();
  });

  it('闭集之外的 method（DELETE/PUT/…）⇒ 拒发、留痕', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const calls: string[] = [];
    const fetchImpl = async (url: string) => { calls.push(url); return { status: 200, text: async () => '{}' }; };
    for (const method of ['DELETE', 'PUT', 'PATCH', 'OPTIONS']) {
      const reply = await serveBackfillFetch(
        { url: `${ORIGIN}${CHATGPT_PLAN.detailPath}abc`, method }, ORIGIN, fetchImpl as never);
      console.log(`[C23-3] method=${method} ⇒`, JSON.stringify(reply));
      expect(reply).toEqual({ ok: false, error: `refused: method ${method} is not in the allowed set` });
    }
    expect(calls).toEqual([]);
    expect(warn.mock.calls.length).toBe(4);                       // 🔴 四次拒发，四条留痕
    warn.mockRestore();
  });

  it('白名单外的 url（跨源 / 非回溯路径）⇒ 拒发、留痕，wire 理由与 C22 逐字相同', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const calls: string[] = [];
    const fetchImpl = async (url: string) => { calls.push(url); return { status: 200, text: async () => '{}' }; };
    for (const url of [
      'https://evil.example.com/steal',                 // 跨源
      `${ORIGIN}/backend-api/accounts/deactivate`,      // 同源但不是回溯路径
      `${ORIGIN}/backend-api/conversations/../../admin`,
    ]) {
      const reply = await serveBackfillFetch(url, ORIGIN, fetchImpl as never);
      console.log('[C23-3] url 拒发:', url, '⇒', JSON.stringify(reply));
      expect(reply).toEqual({ ok: false, error: REFUSED_URL_REASON });
    }
    expect(calls).toEqual([]);
    console.log('[C23-3] url 维度留痕:', warn.mock.calls.map((c) => String(c[0])));
    expect(warn.mock.calls.length).toBe(3);
    warn.mockRestore();
  });

  it('🔴 拒发与取数失败走同一条路：{ok:false} → tabHttpPort throw → engine halt 并落盘', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    // 一个「plan 说 GET、engine 却按 POST 发」的畸形组合：内容脚本必须挡下来，
    // 而 engine 必须走它原本那条 transport-error 的 halt 路径，不新增静默分支。
    const store = memoryStore();
    const port = tabHttpPort(3, async (_id, msg) =>
      handleBackfillMessage(msg, ORIGIN, (async () => { throw new Error('should not be reached'); }) as never));
    const report = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-halt',
      store, http: (url) => port(url, { method: 'POST', body: '{"offset":0}', contentType: 'application/json' }),
      clock: fakeClock(), pace: NO_WAIT,
      sink: () => ({ saved: true }),
    });
    console.log('[C23-3] halt 记录:', JSON.stringify(report.halted));
    console.log('[C23-3] stopped:', report.stopped);
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('transport-error');
    expect(report.halted?.detail).toContain('must be GET, got POST');
    // 🔴 留痕落盘了 —— 重启之后仍然看得见，不是只在内存里。
    const persisted = await store.load(stateKey('chatgpt', 'acct-halt')) as { halted?: unknown } | null;
    console.log('[C23-3] 落盘的 halted:', JSON.stringify(persisted?.halted));
    expect(persisted?.halted).toBeTruthy();
    warn.mockRestore();
  });
});
