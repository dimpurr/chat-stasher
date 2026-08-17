/**
 * C17 · 回溯腿【端到端】串一次。
 *
 * 与 c11/c12/c13 的分工：
 *  · c11 单独验引擎、c12 单独验守卫、c13 只验"接线到达"（runBackfill 被 vi.mock 掉了）。
 *  · 🔴 本文件【不 mock 任何一环】：真引擎 + 真欠账 + 真定速 + 真 download.ts +
 *    真 download-guard + 真 background.ts 入口，只把 browser.* 换成假实现、
 *    http 端口换成合成服务器。
 *  · 全程零真实网络、零登录态：http 端口是本文件里的一个纯函数，
 *    browser.downloads 也只是把 filename 记进数组。
 *
 * 每个用例都从【真实入口】出发：runtime.onMessage('chat-captured')。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import type { CapturedFetch } from '../lib/contract';

// ---------------------------------------------------------------------------
// 假浏览器。storage 是【跨 resetModules 存活的】—— 这正是"浏览器重启"的模型：
// 内存态（模块变量）清零，storage.local 留着。
// ---------------------------------------------------------------------------
const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const downloadCalls: Array<{ id: number; filename: string }> = [];
const changeListeners: Array<(d: any) => void> = [];
/** 让某次下载不报 complete（用来真制造停滞）。 */
let stallFilenames: string[] = [];

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
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
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
  downloads: {
    async download(opts: any) {
      const id = downloadCalls.length + 1;
      downloadCalls.push({ id, filename: opts.filename });
      if (!stallFilenames.includes(opts.filename)) {
        setTimeout(() => { for (const fn of changeListeners) fn({ id, state: { current: 'complete' } }); }, 0);
      }
      return id;
    },
    onChanged: { addListener(fn: any) { changeListeners.push(fn); } },
    async removeFile() {},
    async erase() {},
  },
};

// ---------------------------------------------------------------------------
// 合成"服务器"。绝不碰网络：就是一个 (url) => {status,text} 的纯函数。
// ---------------------------------------------------------------------------
interface ServerOpts {
  ids: string[];
  /** null ⇒ 列表接口【不给】total，用来验"没有分母就不许出现 %"。 */
  total: number | null;
  pageSize: number;
  /** 额外塞进某一页的重复 id（模拟"同一个会话被枚举两次"）。 */
  dupeOnPage?: { page: number; id: string };
}

function makeServer(opts: ServerOpts) {
  const calls: string[] = [];
  const port = async (url: string) => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/backend-api/conversations') {
      const offset = Number(u.searchParams.get('offset') ?? '0');
      const page = Math.floor(offset / opts.pageSize);
      const slice = opts.ids.slice(offset, offset + opts.pageSize);
      const items = slice.map((id) => ({ id, title: 'synthetic' }));
      if (opts.dupeOnPage && opts.dupeOnPage.page === page) {
        items.push({ id: opts.dupeOnPage.id, title: 'synthetic-dupe' });
      }
      const body: Record<string, unknown> = { items };
      if (opts.total !== null) body.total = opts.total;
      return { status: 200, text: JSON.stringify(body) };
    }
    // 取正文：必须命中 chatgpt 的 responseShape（mapping + current_node）。
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

/** 实时腿的载荷（合成夹具，不是任何真人的对话）。 */
function liveCapture(account = 'acct-fixture-1'): CapturedFetch {
  const sid = 'aaaaaaaa-1111-2222-3333-444444444444';
  return {
    url: `https://chatgpt.com/backend-api/conversation/${sid}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: account }),
    pageUrl: `https://chatgpt.com/c/${sid}`,
    capturedAt: 1_700_000_000_000,
  };
}

/** 走真实入口：加载 background → 执行 defineBackground 回调 → 派发一条消息。 */
async function bootAndDispatch(payload: CapturedFetch): Promise<{ mod: any; responded: any }> {
  const mod: any = await import('../entrypoints/background');
  if (runtimeListeners.length === 0) await mod.default();
  const responded = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    expect(ret).toBe(true);
  });
  await mod.backfillTickSettled();
  return { mod, responded };
}

const STATE_KEY = 'cs_backfill_v1:chatgpt:acct-fixture-1';

function stateOf(key = STATE_KEY): any {
  return store[key] ?? null;
}

function detailCalls(calls: string[]): string[] {
  return calls.filter((u) => u.includes('/backend-api/conversation/'));
}

function finalWrites(): string[] {
  return downloadCalls.filter((d) => !d.filename.endsWith('.part')).map((d) => d.filename);
}

const UUIDS = [
  'b1111111-0000-4000-8000-000000000001',
  'b2222222-0000-4000-8000-000000000002',
  'b3333333-0000-4000-8000-000000000003',
  'b4444444-0000-4000-8000-000000000004',
];

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  downloadCalls.length = 0;
  changeListeners.length = 0;
  stallFilenames = [];
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

// ===========================================================================
// 任务 1 · 串一条完整的回溯（N = 4 > 1）
// ===========================================================================
describe('C17 任务 1 · 枚举 → 欠账 → 定速逐个取 → 落盘 → 欠账减少 → 进度更新', () => {
  it('N=4 全链路，每一步都有独立证据', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 2 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    // ---- tick 1：枚举跑完 + 清第 1 笔账 ----
    await bootAndDispatch(liveCapture());
    const listUrls = server.calls.filter((u) => u.includes('/backend-api/conversations'));
    console.log('[C17-1] 证据A 枚举实际请求的列表页:', listUrls);
    expect(listUrls.length).toBe(2);            // 2 页 × pageSize 2 = 4 条

    const s1 = stateOf();
    console.log('[C17-1] 证据B 欠账集合(storage 里读回来的):', {
      key: STATE_KEY, pending: s1.pending, archived: s1.archived,
      totalKnown: s1.totalKnown, totalSource: s1.totalSource, enumCursor: s1.enumCursor,
    });
    expect(s1.enumCursor.complete).toBe(true);
    expect(s1.archived.length).toBe(1);
    expect(s1.pending.length).toBe(3);

    console.log('[C17-1] 证据C tick1 实际取正文的 URL:', detailCalls(server.calls));
    expect(detailCalls(server.calls).length).toBe(1);   // 「逐个」：一次 tick 只取一条

    console.log('[C17-1] 证据D tick1 落盘的最终文件:', finalWrites());
    console.log('[C17-1] 证据E tick1 进度文案:', mod.lastBackfillTick()?.report?.progress);
    console.log('[C17-1] 证据F tick1 paceTrace:', mod.lastBackfillTick()?.report?.paceTrace);

    // ---- tick 2..4：把剩下 3 笔清完，逐个观察欠账下降 ----
    const trail: Array<{ tick: number; pending: number; archived: number; progress: string }> = [];
    for (let i = 2; i <= 4; i += 1) {
      await bootAndDispatch(liveCapture());
      const s = stateOf();
      trail.push({
        tick: i, pending: s.pending.length, archived: s.archived.length,
        progress: mod.lastBackfillTick()!.report!.progress,
      });
    }
    console.log('[C17-1] 证据G 欠账逐 tick 下降 + 进度文案:', trail);
    expect(trail.map((t) => t.pending)).toEqual([2, 1, 0]);
    expect(trail.map((t) => t.archived)).toEqual([2, 3, 4]);

    // 逐个取：4 次 detail 请求，顺序 = FIFO
    expect(detailCalls(server.calls).length).toBe(4);
    expect(detailCalls(server.calls).map((u) => u.split('/').pop())).toEqual(UUIDS);

    // 落盘：4 个会话各一个最终文件（外加实时腿自己的那条）
    const backfillFiles = finalWrites().filter((f) => UUIDS.some((id) => f.includes(id)));
    console.log('[C17-1] 证据H 回溯落盘的 4 个最终文件:', backfillFiles);
    expect(backfillFiles.length).toBe(4);

    console.log('[C17-1] 证据I 末态进度文案:', trail[trail.length - 1]!.progress);
    expect(trail[trail.length - 1]!.progress).toContain('100%');
  });
});

// ===========================================================================
// 任务 2 · 四个接缝反面
// ===========================================================================
describe('C17 任务 2 · 反面 1：中途"浏览器重启"（清内存态、留 storage）', () => {
  it('从欠账集合接着跑，不重头来', async () => {
    await enableBackfill();
    const server1 = makeServer({ ids: UUIDS, total: 4, pageSize: 2 });
    let mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server1.port);
    await bootAndDispatch(liveCapture());
    await bootAndDispatch(liveCapture());
    const before = stateOf();
    console.log('[C17-2.1] 重启前:', { archived: before.archived, pending: before.pending });
    expect(before.archived.length).toBe(2);

    // ---- "浏览器重启"：所有模块内存态清零，storage 原样留着 ----
    vi.resetModules();
    runtimeListeners.length = 0;
    const { resetTickLockForTest } = await import('../lib/backfill/schedule');
    resetTickLockForTest();
    const server2 = makeServer({ ids: UUIDS, total: 4, pageSize: 2 });
    mod = await import('../entrypoints/background');
    // 重启后 transport 也是 null（生产事实），必须重新注入才会动。
    expect(mod.lastBackfillTick()).toBeNull();       // 内存态确实清零了
    mod.configureBackfillTransport(server2.port);

    await bootAndDispatch(liveCapture());
    const after = stateOf();
    console.log('[C17-2.1] 重启后第一次 tick 取的正文:', detailCalls(server2.calls));
    console.log('[C17-2.1] 重启后:', { archived: after.archived, pending: after.pending });

    // 关键断言：重启后没有重新枚举、没有重抓已归档的，直接接着第 3 条。
    expect(server2.calls.filter((u) => u.includes('/conversations'))).toEqual([]);
    expect(detailCalls(server2.calls).map((u) => u.split('/').pop())).toEqual([UUIDS[2]]);
    expect(after.archived).toEqual([UUIDS[0], UUIDS[1], UUIDS[2]]);
  });
});

describe('C17 任务 2 · 反面 2：中途熔断（download-paused）', () => {
  it('熔断即停，欠账不动；resume 之后从断点继续', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 2 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    await bootAndDispatch(liveCapture());
    const beforeTrip = stateOf();
    expect(beforeTrip.archived.length).toBe(1);

    // 真制造熔断：连续 3 次停滞（下载不报终态）——走真实的 download.ts + guard。
    const { recordDownloadOutcome, stalledResult, loadGuardState } =
      await import('../lib/download-guard');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const st = browserLocalStore()!;
    for (let i = 0; i < 3; i += 1) await recordDownloadOutcome(st, stalledResult(15_000), { now: 1 });
    const guard = await loadGuardState(st);
    console.log('[C17-2.2] 守卫状态:', guard);
    expect(guard.tripped).toBe(true);

    const detailsBefore = detailCalls(server.calls).length;
    await bootAndDispatch(liveCapture());
    console.log('[C17-2.2] 熔断态下 tick reason =', mod.lastBackfillTick()?.reason);
    const paused = stateOf();
    console.log('[C17-2.2] 熔断后欠账:', { archived: paused.archived.length, pending: paused.pending.length });
    expect(mod.lastBackfillTick()?.reason).toBe('download-paused');
    expect(detailCalls(server.calls).length).toBe(detailsBefore);   // 一个请求都没多发
    expect(paused.pending).toEqual(beforeTrip.pending);             // 欠账原封不动

    // 显式恢复 → 从断点继续
    await mod.resumeBackfillAfterDownloadGuard();
    await bootAndDispatch(liveCapture());
    const resumed = stateOf();
    console.log('[C17-2.2] 恢复后:', { reason: mod.lastBackfillTick()?.reason, archived: resumed.archived });
    expect(resumed.archived).toEqual([UUIDS[0], UUIDS[1]]);         // 接着第 2 条，不是重头
  });
});

describe('C17 任务 2 · 反面 3：枚举给不出 total', () => {
  it('进度文案里绝不出现 % —— 且先证明仪器能看出反面', async () => {
    await enableBackfill();
    // (a) 仪器自证：有 total 时，同一个检查【看得见】百分比
    const withTotal = makeServer({ ids: UUIDS, total: 4, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(withTotal.port);
    await bootAndDispatch(liveCapture());
    const yes = mod.lastBackfillTick()!.report!.progress;
    console.log('[C17-2.3] 仪器自证（有 total）:', yes);
    expect(yes).toContain('%');

    // (b) 真反面：另一个 scope，列表不给 total
    const noTotal = makeServer({ ids: UUIDS, total: null, pageSize: 4 });
    mod.configureBackfillTransport(noTotal.port);
    await bootAndDispatch(liveCapture('acct-no-total'));
    const no = mod.lastBackfillTick()!.report!.progress;
    console.log('[C17-2.3] 真反面（无 total）:', no);
    expect(no).not.toContain('%');
    expect(no).toContain('总数未知');

    const s = stateOf('cs_backfill_v1:chatgpt:acct-no-total');
    console.log('[C17-2.3] 无 total 时的 state:', { totalKnown: s.totalKnown, totalSource: s.totalSource });
    expect(s.totalSource).toBe('unknown');
  });
});

describe('C17 任务 2 · 反面 4：同一个会话被枚举两次', () => {
  it('不重复落盘 —— 且先证明仪器能看出重复', async () => {
    await enableBackfill();
    // (a) 仪器自证：真的写两次同一个会话时，finalWrites() 【看得见】两条同名记录
    const mod: any = await import('../entrypoints/background');
    const dup = liveCapture();
    await bootAndDispatch(dup);
    await mod.handleCaptured(dup);
    const liveName = finalWrites().filter((f) => f.includes('aaaaaaaa-1111'));
    console.log('[C17-2.4] 仪器自证（强行写两次同一 id）:', liveName);
    expect(liveName.length).toBe(2);

    // (b) 真反面：第 0 页把 UUIDS[0] 再塞一遍
    downloadCalls.length = 0;
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 2, dupeOnPage: { page: 0, id: UUIDS[0]! } });
    mod.configureBackfillTransport(server.port);
    for (let i = 0; i < 4; i += 1) await bootAndDispatch(liveCapture());

    const s = stateOf();
    console.log('[C17-2.4] 枚举带重复时的 state:', { pending: s.pending, archived: s.archived });
    const dupFiles = finalWrites().filter((f) => f.includes(UUIDS[0]!));
    console.log('[C17-2.4] UUIDS[0] 的最终文件:', dupFiles);
    console.log('[C17-2.4] UUIDS[0] 的 detail 请求次数:',
      detailCalls(server.calls).filter((u) => u.endsWith(UUIDS[0]!)).length);

    expect(new Set(s.archived).size).toBe(s.archived.length);   // archived 无重复
    expect(dupFiles.length).toBe(1);                            // 只落盘一次
    expect(detailCalls(server.calls).filter((u) => u.endsWith(UUIDS[0]!)).length).toBe(1);
  });
});

// ===========================================================================
// 任务 3 · 接缝 bug（这些用例【钉住当前的真实行为】，包括错误的行为）
// ===========================================================================
describe('C17 任务 3 · 接缝 A：欠账键 vs 落盘文件名是不是同一个身份', () => {
  it('🔴 BUG-1：落盘被跳过（拿不到 sessionId），欠账照样被清 ⇒ 静默丢数据', async () => {
    await enableBackfill();
    // 'shortid' 是合法的欠账键（enumerate 只要求非空 string），
    // 但 chatgpt 的 sessionIdPatterns 是 /backend-api/conversation/([0-9a-fA-F-]{8,})，
    // 'shortid' 不是 hex ⇒ extractSessionId 返回 null ⇒ handleCaptured 直接跳过不落盘。
    const server = makeServer({ ids: ['shortid'], total: 1, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    downloadCalls.length = 0;
    await bootAndDispatch(liveCapture());

    const s = stateOf();
    const files = finalWrites().filter((f) => f.includes('shortid'));
    console.log('[C17-3.A] detail 请求:', detailCalls(server.calls));
    console.log('[C17-3.A] 落盘的文件里跟 shortid 有关的:', files);
    console.log('[C17-3.A] 欠账状态:', { pending: s.pending, archived: s.archived });
    console.log('[C17-3.A] 进度文案:', mod.lastBackfillTick()!.report!.progress);

    expect(detailCalls(server.calls).length).toBe(1);   // 确实取了正文
    expect(files.length).toBe(0);                       // 🔴 但一个字节都没落盘
    expect(s.archived).toEqual(['shortid']);            // 🔴 欠账却被清了（永不再入队）
    // 进度还会说"已归档 1 条 / 共 1 条（100%）" —— 分子是假的。
    expect(mod.lastBackfillTick()!.report!.progress).toContain('100%');
  });

  it('🔴 BUG-2：两个不同的欠账键塌成同一个文件名 ⇒ 后写的覆盖先写的', async () => {
    await enableBackfill();
    // 这两个 id 都以同一段 hex-dash 开头，sessionIdPatterns 的贪婪匹配到此为止：
    //   'deadbeef01-zzz' 和 'deadbeef01-yyy' 都 ⇒ sessionId 'deadbeef01-'
    const ids = ['deadbeef01-zzz', 'deadbeef01-yyy'];
    const server = makeServer({ ids, total: 2, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    downloadCalls.length = 0;
    await bootAndDispatch(liveCapture());
    await bootAndDispatch(liveCapture());

    const s = stateOf();
    const files = finalWrites().filter((f) => f.includes('deadbeef01'));
    console.log('[C17-3.A2] 欠账键:', s.archived);
    console.log('[C17-3.A2] 落盘文件名:', files);
    expect(s.archived).toEqual(ids);                      // 两个不同的欠账都清了
    expect(files.length).toBe(2);                         // 写了两次
    expect(new Set(files).size).toBe(1);                  // 🔴 但是同一个文件名
    // 先写的那条对话在磁盘上被后写的覆盖 ⇒ 2 个会话只剩 1 个文件。
  });
});

describe('C17 任务 3 · 接缝 B：定速器与熔断谁优先 / 熔断时定速的计时', () => {
  it('熔断优先于定速：熔断态下 gate() 一次都没被调用', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    await bootAndDispatch(liveCapture());

    const { recordDownloadOutcome, stalledResult } = await import('../lib/download-guard');
    const { browserLocalStore } = await import('../lib/backfill/store');
    for (let i = 0; i < 3; i += 1) {
      await recordDownloadOutcome(browserLocalStore()!, stalledResult(15_000), { now: 1 });
    }
    await bootAndDispatch(liveCapture());
    // schedule.ts 在 runBackfill 之前就挡住了 ⇒ 连 Pacer 都没被 new 出来。
    console.log('[C17-3.B] 熔断态 tick:', mod.lastBackfillTick());
    expect(mod.lastBackfillTick()?.reason).toBe('download-paused');
    expect(mod.lastBackfillTick()?.report).toBeNull();
  });

  it('🔴 BUG-3：Pacer 是 per-run 的，tick 之间不续 ⇒ 取正文的 20s 最小间隔在运行时从未生效', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    const traces: Array<{ enumerate: number[]; detail: number[] }> = [];
    for (let i = 0; i < 4; i += 1) {
      await bootAndDispatch(liveCapture());
      traces.push(mod.lastBackfillTick()!.report!.paceTrace);
    }
    console.log('[C17-3.B2] 每次 tick 的 paceTrace:', traces);
    // 每次 tick 的 detail 段都只有一次 gate()，而且【等了 0 毫秒】——
    // 因为 engine 每次 runBackfill 都 new 一个 Pacer，lastAt 从 null 开始。
    for (const t of traces) expect(t.detail).toEqual([0]);
    // 也就是说：4 条正文被连续取完，中间没有任何强制间隔。
    expect(detailCalls(server.calls).length).toBe(4);
  });
});

describe('C17 任务 3 · 接缝 C：进度分母来自枚举，分子来自另一处', () => {
  it('🔴 BUG-4：sink 抛错（下载真失败）时，整个 tick 抛出去被吞掉，既不 halt 也不留痕', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    // 先让枚举跑完并清 1 笔（正常）
    await bootAndDispatch(liveCapture());
    const ok = stateOf();
    expect(ok.archived.length).toBe(1);

    // 现在让下一条的 .part 下载"永远不完成" ⇒ download.ts 抛 DownloadFailure
    stallFilenames = [`chat-stasher/inbox/chatgpt-${UUIDS[1]}.json.part`];
    vi.useFakeTimers();
    const dispatched = bootAndDispatch(liveCapture());
    await vi.advanceTimersByTimeAsync(20_000);
    vi.useRealTimers();
    await dispatched;

    const after = stateOf();
    console.log('[C17-3.C] 下载停滞后:', {
      archived: after.archived.length, pending: after.pending.length,
      halted: after.halted, lastTickReason: mod.lastBackfillTick()?.reason,
    });
    // 好的一面：欠账没被清（没丢数据）
    expect(after.archived.length).toBe(1);
    expect(after.pending[0]).toBe(UUIDS[1]);
    // 🔴 坏的一面：state.halted 仍然是 null，lastTick 停留在上一次成功的结果 ——
    // 运行时看不到"这次 tick 炸了"，只有一行 console.warn。
    expect(after.halted).toBeNull();
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
  });
});

// ===========================================================================
// 任务 4 · 运行时真的会跑吗
// ===========================================================================
describe('C17 任务 4 · 运行时', () => {
  it('🔴 生产事实：没有任何生产代码注入 http 端口 ⇒ 这条链在浏览器里永远走不到 runBackfill', async () => {
    await enableBackfill();                       // 用户甚至已经显式打开了开关
    const mod: any = await import('../entrypoints/background');
    // 【不】调用 configureBackfillTransport —— 这就是真实的生产状态
    await bootAndDispatch(liveCapture());
    console.log('[C17-4] 未注入 transport 时的 tick:', mod.lastBackfillTick());
    expect(mod.lastBackfillTick()?.reason).toBe('no-http-port');
    expect(stateOf()).toBeNull();                 // 欠账集合根本没被创建过
  });
});
