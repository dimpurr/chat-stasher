import { describe, it, expect } from 'vitest';
import { runBackfill, loadState, notWiredHttp, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { formatProgress, computeProgress } from '../lib/backfill/progress';
import { enqueueDebts, settleDebt } from '../lib/backfill/debts';
import { initialState, stateKey } from '../lib/backfill/types';
import { DEFAULT_DETAIL_PACE, DEFAULT_ENUM_PACE, type Clock } from '../lib/backfill/pace';
import { parseConversationListPage } from '../lib/backfill/enumerate';

/**
 * C11 回溯腿骨架测试。
 * 🔴 全部用合成夹具 —— 没有任何一行会真的碰平台接口（http 端口是注入的，
 *    不注入就是 notWiredHttp，调用即抛错）。夹具里没有任何真实对话正文。
 */

const ORIGIN = 'https://chatgpt.com';

/** 假时钟：sleep 不真的等，只把虚拟时间往前推并记账。 */
function fakeClock(): Clock & { sleeps: number[]; nowMs: () => number } {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  const sleeps: number[] = [];
  return {
    now: () => t,
    async sleep(ms: number) {
      sleeps.push(ms);
      t += ms;
    },
    sleeps,
    nowMs: () => t,
  };
}

/** 合成的会话列表页。withTotal=false 用来构造「拿不到分母」的场景。 */
function listBody(ids: string[], total: number | null): string {
  const body: Record<string, unknown> = {
    items: ids.map((id) => ({ id, title: 'synthetic-fixture', create_time: 0 })),
    limit: 100,
    offset: 0,
  };
  if (total !== null) body.total = total;
  return JSON.stringify(body);
}

/** 合成的会话正文。满足 lib/contract.ts 里 chatgpt 的 mapping + current_node。 */
function detailBody(id: string): string {
  return JSON.stringify({
    title: 'synthetic-fixture',
    current_node: `${id}-node`,
    mapping: { [`${id}-node`]: { id: `${id}-node`, parent: null, children: [] } },
  });
}

function ids(n: number, from = 0): string[] {
  return Array.from({ length: n }, (_, i) => `conv-${String(i + from).padStart(4, '0')}-aaaaaaaa`);
}

/** 合成后端：一份 id 列表 + 分页，记录每一次被请求的 URL。 */
function fakeBackend(allIds: string[], opts: { total?: number | null; pageSize?: number } = {}) {
  const pageSize = opts.pageSize ?? 100;
  const total = opts.total === undefined ? allIds.length : opts.total;
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/backend-api/conversations') {
      const offset = Number(u.searchParams.get('offset') ?? 0);
      return { status: 200, text: listBody(allIds.slice(offset, offset + pageSize), total) };
    }
    const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
    return { status: 200, text: detailBody(id) };
  };
  const detailCalls = () => calls.filter((c) => c.includes('/backend-api/conversation/'));
  return { http, calls, detailCalls };
}

describe('C11 判据 1 · 可断可续', () => {
  it('跑一半被打断，重启后从断点继续而不是从头', async () => {
    const store = memoryStore();
    const backend = fakeBackend(ids(30));

    const run1 = await runBackfill({
      platform: 'chatgpt',
      origin: ORIGIN,
      scope: 'acct-fixture',
      store,
      http: backend.http,
      clock: fakeClock(),
      maxDetails: 3, // 模拟「做了 3 条就被打断」
    });
    console.log('[C11-1] run1 stopped =', run1.stopped, '| archived =', run1.archivedThisRun.join(','));
    console.log('[C11-1] run1 progress =', run1.progress);

    // 第二次进来是全新的 run（新 Pacer、新时钟），只共享落盘状态。
    const run2 = await runBackfill({
      platform: 'chatgpt',
      origin: ORIGIN,
      scope: 'acct-fixture',
      store,
      http: backend.http,
      clock: fakeClock(),
      maxDetails: 3,
    });
    console.log('[C11-1] run2 stopped =', run2.stopped, '| archived =', run2.archivedThisRun.join(','));
    console.log('[C11-1] run2 progress =', run2.progress);

    expect(run1.archivedThisRun).toHaveLength(3);
    expect(run2.archivedThisRun).toHaveLength(3);
    // 断点续跑：run2 一条都不能和 run1 重合
    expect(run2.archivedThisRun).not.toEqual(run1.archivedThisRun);
    expect(run2.state.archived).toHaveLength(6);
    expect(run2.state.pending).toHaveLength(24);
    // 枚举不重跑：run2 没有再打列表接口
    expect(run2.enumeratedPages).toBe(0);
  });
});

describe('C11 判据 2 · 不重复抓', () => {
  it('已归档的绝不再入队，也绝不再取一次正文', async () => {
    const store = memoryStore();
    const backend = fakeBackend(ids(10));

    const run1 = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture', store,
      http: backend.http, clock: fakeClock(), maxDetails: 4,
    });
    const firstBatch = backend.detailCalls().slice();

    // 模拟「过了一天又枚举一遍」：把枚举游标重置，重新枚举同一批 id。
    const st = await loadState(store, 'chatgpt', 'acct-fixture');
    st.enumCursor = { offset: 0, complete: false };
    await store.save(stateKey('chatgpt', 'acct-fixture'), st);

    const run2 = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'acct-fixture', store,
      http: backend.http, clock: fakeClock(), maxDetails: 4,
    });

    console.log('[C11-2] 重新枚举 10 条：新入队 =', run2.newDebts,
      '| 因已归档被挡掉 =', run2.skippedAlreadyArchived,
      '| 因已在欠账里被挡掉 =', run2.skippedAlreadyPending);
    const secondBatch = backend.detailCalls().slice(firstBatch.length);
    console.log('[C11-2] run1 取正文 =', firstBatch.length, '条；run2 取正文 =', secondBatch.length, '条');
    const overlap = secondBatch.filter((u) => firstBatch.includes(u));
    console.log('[C11-2] 两次取正文的 URL 交集 =', overlap.length);

    expect(run1.archivedThisRun).toHaveLength(4);
    expect(run2.skippedAlreadyArchived).toBe(4); // 4 条已清账 => 绝不再入队
    expect(run2.skippedAlreadyPending).toBe(6); // 6 条还欠着 => 也不重复入队
    expect(run2.newDebts).toBe(0);
    expect(overlap).toHaveLength(0);
    // 已归档的 id 一个都没回到欠账里
    for (const done of run1.archivedThisRun) {
      expect(run2.state.pending).not.toContain(done);
    }
  });

  it('enqueueDebts 纯函数层面就挡住重复', () => {
    const st = initialState('chatgpt', 'acct-fixture');
    enqueueDebts(st, ['a', 'b', 'c']);
    settleDebt(st, 'b');
    const added = enqueueDebts(st, ['a', 'b', 'c', 'd']);
    expect(added).toEqual(['d']); // a/c 已在欠账，b 已清账
    expect(st.pending).toEqual(['a', 'c', 'd']);
    expect(st.archived).toEqual(['b']);
  });
});

describe('C11 判据 3 · 分母未知时绝不显示百分比（一票否决）', () => {
  it('列表接口不给 total ⇒ 输出里不含 % 字符', async () => {
    const store = memoryStore();
    // total 缺失；最后补一个空页让枚举知道到头了。
    const backend = fakeBackend(ids(5), { total: null, pageSize: 5 });

    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'no-total', store,
      http: backend.http, clock: fakeClock(), maxDetails: 2,
    });

    console.log('[C11-3] 分母未知时的进度原文 =>', run.progress);
    expect(run.state.totalSource).toBe('unknown');
    expect(run.state.totalKnown).toBeNull();
    expect(computeProgress(run.state).percent).toBeNull();
    expect(run.progress).not.toContain('%');
    expect(run.progress).toContain('总数未知');
  });

  it('枚举整段失败（一页都没拿到）⇒ 仍然不含 %', async () => {
    const store = memoryStore();
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'enum-dead', store,
      http: async () => ({ status: 429, text: '' }),
      clock: fakeClock(),
    });
    console.log('[C11-3] 枚举被限流时的进度原文 =>', run.progress);
    expect(run.stopped).toBe('halted');
    expect(run.progress).not.toContain('%');
  });

  it('假分母的三种来路都被拒：非接口 total / 非正整数 / 已归档超过 total', () => {
    const a = initialState('chatgpt', 'x');
    a.totalKnown = 100; // 有数字，但来源不是接口
    a.totalSource = 'unknown';
    expect(computeProgress(a).percent).toBeNull();
    expect(formatProgress(a)).not.toContain('%');

    const b = initialState('chatgpt', 'x');
    b.totalSource = 'response-total';
    b.totalKnown = 0;
    expect(computeProgress(b).percent).toBeNull();
    expect(formatProgress(b)).not.toContain('%');

    const c = initialState('chatgpt', 'x');
    c.totalSource = 'response-total';
    c.totalKnown = 2;
    c.archived = ['a', 'b', 'c'];
    expect(computeProgress(c).percent).toBeNull();
    expect(formatProgress(c)).not.toContain('%');
  });

  it('分母真的来自接口时，才允许出现百分比', () => {
    const st = initialState('chatgpt', 'x');
    st.totalSource = 'response-total';
    st.totalKnown = 1000;
    st.archived = ids(120);
    st.pending = ids(10, 900);
    console.log('[C11-3] 分母可信时的进度原文 =>', formatProgress(st));
    expect(computeProgress(st).percent).toBe(12);
    expect(formatProgress(st)).toContain('12%');
  });
});

describe('C11 判据 4 · 节流生效（枚举与取正文分开定速）', () => {
  it('取正文按 20s/条 走，枚举按 2s/页 走，两段互不干扰', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const backend = fakeBackend(ids(6), { pageSize: 2 }); // 6 条 => 3 页

    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'pace', store,
      http: backend.http, clock, maxDetails: 4,
    });

    console.log('[C11-4] 默认值：枚举', DEFAULT_ENUM_PACE.minIntervalMs, 'ms/页；取正文',
      DEFAULT_DETAIL_PACE.minIntervalMs, 'ms/条，每天上限', DEFAULT_DETAIL_PACE.maxPerDay, '条');
    console.log('[C11-4] 枚举实际等待序列(ms) =', JSON.stringify(run.paceTrace.enumerate));
    console.log('[C11-4] 取正文实际等待序列(ms) =', JSON.stringify(run.paceTrace.detail));
    console.log('[C11-4] 虚拟时钟共前进 =', clock.nowMs() - Date.parse('2026-08-17T00:00:00.000Z'), 'ms');

    // 3 页：第一次不等，之后每次补足 2000ms
    expect(run.paceTrace.enumerate).toEqual([0, 2000, 2000]);
    // 4 条正文：第一次不等，之后每次补足 20000ms
    expect(run.paceTrace.detail).toEqual([0, 20000, 20000, 20000]);
    // 全程没有真的等待：虚拟时间前进 = 所有 sleep 之和
    expect(clock.sleeps.reduce((a, b) => a + b, 0)).toBe(4000 + 60000);
  });

  it('每天上限到了就温和停下（不是 halt，是 daily-cap）', async () => {
    const store = memoryStore();
    const backend = fakeBackend(ids(10));
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'cap', store,
      http: backend.http, clock: fakeClock(),
      pace: { enumerate: DEFAULT_ENUM_PACE, detail: { minIntervalMs: 20_000, maxPerDay: 3 } },
    });
    console.log('[C11-4] 每天上限 3 条 => stopped =', run.stopped, '| 今日已取 =', run.state.detailToday.count);
    expect(run.stopped).toBe('daily-cap');
    expect(run.archivedThisRun).toHaveLength(3);
    expect(run.halted).toBeNull(); // 正常的温和停顿，不该留 halt 痕迹
  });
});

describe('C11 · 被限流 / 形状变了 必须停下留痕', () => {
  it('取正文时 429 => halt(rate-limited) 并落盘，下一次 run 拒绝继续', async () => {
    const store = memoryStore();
    const all = ids(5);
    let detailHits = 0;
    const http = async (url: string): Promise<HttpResponse> => {
      if (url.includes('/backend-api/conversations')) {
        return { status: 200, text: listBody(all, all.length) };
      }
      detailHits += 1;
      return detailHits >= 3 ? { status: 429, text: '' } : { status: 200, text: detailBody('x') };
    };

    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'limited', store, http, clock: fakeClock(),
    });
    console.log('[C11-halt] halt 记录 =', JSON.stringify(run.halted));
    console.log('[C11-halt] 停机后的进度原文 =>', run.progress);
    expect(run.stopped).toBe('halted');
    expect(run.halted?.reason).toBe('rate-limited');
    expect(run.progress).toContain('已停止');

    // 留痕必须是持久的：重启后不自己重试打平台
    const again = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'limited', store,
      http: async () => { throw new Error('MUST NOT be called after halt'); },
      clock: fakeClock(),
    });
    expect(again.stopped).toBe('halted');
    expect(again.halted?.reason).toBe('rate-limited');
    console.log('[C11-halt] 重启后 =', again.stopped, '（没有再打任何接口）');
  });

  it('列表形状变了 => halt(shape-changed)，不猜、不静默', async () => {
    const store = memoryStore();
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'shape', store,
      http: async () => ({ status: 200, text: JSON.stringify({ conversations: [] }) }),
      clock: fakeClock(),
    });
    console.log('[C11-halt] 形状不认识 =>', JSON.stringify(run.halted));
    expect(run.halted?.reason).toBe('shape-changed');
    expect(run.progress).not.toContain('%');
  });

  it('正文形状变了 => 也 halt（复用实时腿的 matchesResponseShape）', async () => {
    const store = memoryStore();
    const all = ids(2);
    const http = async (url: string): Promise<HttpResponse> =>
      url.includes('/backend-api/conversations')
        ? { status: 200, text: listBody(all, all.length) }
        : { status: 200, text: JSON.stringify({ nodes: [], head: 'x' }) }; // 缺 mapping/current_node
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'shape2', store, http, clock: fakeClock(),
    });
    console.log('[C11-halt] 正文形状不匹配 =>', JSON.stringify(run.halted));
    expect(run.halted?.reason).toBe('shape-changed');
  });

  it('没有可持久化的存储 => 直接 halt，不假装在跑', async () => {
    const run = await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'nostore', store: null,
      http: async () => { throw new Error('MUST NOT be called without storage'); },
      clock: fakeClock(),
    });
    console.log('[C11-halt] 无存储 =>', JSON.stringify(run.halted));
    console.log('[C11-halt] 无存储时的进度原文 =>', run.progress);
    expect(run.stopped).toBe('halted');
    expect(run.halted?.reason).toBe('storage-unavailable');
    expect(run.progress).not.toContain('%');
  });
});

describe('C11 · 不接线就绝不发请求 + 落盘出口与实时腿同形', () => {
  it('默认 http 端口调用即抛错（结构性保证：没有登录态也不会去试）', async () => {
    await expect(notWiredHttp(`${ORIGIN}/backend-api/conversations?offset=0&limit=100`)).rejects.toThrow(
      /not wired/,
    );
  });

  it('sink 收到的是 CapturedFetch 同形对象，可以直接走实时腿的落盘链路', async () => {
    const store = memoryStore();
    const backend = fakeBackend(ids(1));
    const seen: Array<{ url: string; method: string; status: number; pageUrl?: string }> = [];
    await runBackfill({
      platform: 'chatgpt', origin: ORIGIN, scope: 'sink', store,
      http: backend.http, clock: fakeClock(),
      sink: (c) => { seen.push({ url: c.url, method: c.method, status: c.status, pageUrl: c.pageUrl }); },
    });
    console.log('[C11-sink] sink 收到 =', JSON.stringify(seen[0]));
    expect(seen).toHaveLength(1);
    expect(seen[0]!.method).toBe('GET');
    expect(seen[0]!.url).toContain('/backend-api/conversation/');
  });

  it('parseConversationListPage 对畸形输入一律 ok:false', () => {
    expect(parseConversationListPage('not json').ok).toBe(false);
    expect(parseConversationListPage('[]').ok).toBe(false);
    expect(parseConversationListPage('{"items":{}}').ok).toBe(false);
    expect(parseConversationListPage('{"items":[{"no_id":1}]}').ok).toBe(false);
    const good = parseConversationListPage('{"items":[{"id":"a"}],"total":7}');
    expect(good.ok && good.page.total).toBe(7);
  });
});
