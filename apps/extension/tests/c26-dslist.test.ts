/**
 * C26 · DeepSeek 会话列表的形状，以及**带着「它可能是错的」去写**这件事。
 *
 * 🔴 全部合成夹具：没有任何一行会碰真实平台接口，没有对 deepseek.com 发过请求，
 *    没有登录态，没有真实对话正文与账号。http 端口一律显式注入。
 *
 * DEEPSEEK_PLAN 认的那些字段名是 R25 从四个互不相干的开源实现里**逆向交叉**出来的，
 * 不是官方契约（出处与时效风险写在 DEEPSEEK_PLAN 头上）。逆向来的东西随时可能是错的，
 * 所以这份测试真正盯的不是「对的时候能跑」，而是**错的时候会怎样**：
 *
 *   1. 正常两页 ⇒ 第二页确实带上 before_seq_id=第一页最小 seq_id，has_more:false 后停；
 *   2. 🔴 响应里没有 chat_sessions ⇒ halt('shape-changed')，
 *      **且必须与「这个用户没有会话」是两条能被分辨的路径**（本文件用对照组把这句话钉死）；
 *   3. 记录里没有 seq_id ⇒ 只抓第一页，并具名报告 'cursor-missing'，不许假装抓全了；
 *   4. updated_at 当**数值**处理（不是 ISO 串，也绝不 new Date(string)）；
 *   5. 正文段没有出处 ⇒ 在取第一条正文之前 halt('detail-unsupported')，欠账原封不动。
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  BACKFILL_LIST_ONLY_PLATFORMS,
  BACKFILL_SUPPORTED_PLATFORMS,
  DEEPSEEK_LIST_PATH,
  DEEPSEEK_PLAN,
  backfillPlanFor,
  parseDeepSeekListPage,
} from '../lib/backfill/enumerate';
import { isAllowedBackfillUrl } from '../lib/backfill/tab-port';
import type { Clock } from '../lib/backfill/pace';

const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';
const LIMIT = 100;

function fakeClock(): Clock {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

interface FixtureSession {
  id: string;
  seq_id?: number;
  updated_at?: unknown;
  title?: string;
}

/** 一页合成响应。信封逐字按 R25 的 data.biz_data / chat_sessions / has_more 来。 */
function pageBody(sessions: FixtureSession[], hasMore: boolean, opts: { omitHasMore?: boolean } = {}): string {
  const biz: Record<string, unknown> = { chat_sessions: sessions };
  if (!opts.omitHasMore) biz.has_more = hasMore;
  return JSON.stringify({ code: 0, msg: 'ok', data: { biz_data: biz } });
}

function session(n: number, over: Partial<FixtureSession> = {}): FixtureSession {
  return {
    id: `ds-${String(n).padStart(4, '0')}-aaaaaaaa`,
    seq_id: 1000 - n,
    // 🔴 数值型时间戳（秒），不是 ISO 串。三源一致。
    updated_at: 1_755_000_000 + n,
    title: 'synthetic-fixture',
    ...over,
  };
}

/**
 * 合成后端：按 URL 里的 before_seq_id 决定回哪一页。
 * calls 记下每一次被请求的 URL —— 「第二页真的带了游标」「一条正文都没发」都靠它证明。
 */
function backend(pages: string[]) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname !== DEEPSEEK_LIST_PATH) {
      // 🔴 回溯腿在 DeepSeek 上【只该】打列表这一条路径。打到别处就是测试该红的时候。
      throw new Error(`unexpected path ${u.pathname}`);
    }
    const index = Math.min(calls.length - 1, pages.length - 1);
    return { status: 200, text: pages[index] ?? '' };
  };
  return { http, calls };
}

async function run(store: ReturnType<typeof memoryStore>, http: (url: string) => Promise<HttpResponse>, scope: string) {
  return runBackfill({
    platform: 'deepseek',
    origin: DEEPSEEK_ORIGIN,
    scope,
    store,
    http,
    clock: fakeClock(),
    listLimit: LIMIT,
  });
}

// ---------------------------------------------------------------------------
// 1 · 正常两页：游标真的被用上了，has_more:false 之后停
// ---------------------------------------------------------------------------
describe('C26-1 · 游标翻页', () => {
  it('第二页带上 before_seq_id=第一页最小 seq_id，has_more:false 后停', async () => {
    const store = memoryStore();
    const first = [session(1), session(2), session(3)];   // seq_id 999 / 998 / 997
    const second = [session(4), session(5)];              // seq_id 996 / 995
    const be = backend([pageBody(first, true), pageBody(second, false)]);

    const report = await run(store, be.http, 'acct-two-pages');

    // 🔴 第一页不带游标；第二页的游标 = 第一页里【最小】的 seq_id（997），不是第一条、也不是最大的。
    expect(be.calls).toEqual([
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}`,
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}&before_seq_id=997`,
    ]);
    expect(report.enumeratedPages).toBe(2);
    expect(report.newDebts).toBe(5);
    expect(report.state.pending).toEqual([...first, ...second].map((s) => s.id));
    // has_more:false ⇒ 正常走完，且【没有】任何截断标记。
    expect(report.state.enumCursor.complete).toBe(true);
    expect(report.enumTruncated).toBeNull();
    expect(report.state.enumCursor.truncated).toBeUndefined();
    // 🔴 DeepSeek 的列表响应里没有总数字段的出处 ⇒ 分母保持未知，绝不拿已枚举条数冒充。
    expect(report.state.totalKnown).toBeNull();
    expect(report.state.totalSource).toBe('unknown');
    // 🔴 这一页比 count 少得多（3 条 vs 100），但因为 has_more:true 就必须继续翻 ——
    //    「返回条数 < count ⇒ 到底了」是拿未知当已知，本实现不这么推断。
    expect(report.enumeratedPages).toBeGreaterThan(1);
  });

  it('游标续得上：跑到一半停下，下一次 run 从落盘的游标接着翻', async () => {
    const store = memoryStore();
    const first = [session(1), session(2)];
    const be1 = backend([pageBody(first, true)]);
    // 第一次：只有第一页，且 has_more:true ⇒ 会继续要第二页（夹具重复回同一页），
    // 所以这里用 shouldAbort 在第二页之前收手，模拟 SW 被回收。
    let steps = 0;
    const report1 = await runBackfill({
      platform: 'deepseek',
      origin: DEEPSEEK_ORIGIN,
      scope: 'acct-resume',
      store,
      http: be1.http,
      clock: fakeClock(),
      listLimit: LIMIT,
      shouldAbort: () => steps++ >= 1,
    });
    expect(report1.stopped).toBe('aborted');
    expect(report1.state.enumCursor.cursor).toBe(998);

    // 第二次：新的 run 读回落盘的游标，第一条请求就必须带着它。
    const be2 = backend([pageBody([session(3)], false)]);
    const report2 = await run(store, be2.http, 'acct-resume');
    expect(be2.calls[0]).toBe(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}&before_seq_id=998`);
    expect(report2.state.pending).toEqual(['ds-0001-aaaaaaaa', 'ds-0002-aaaaaaaa', 'ds-0003-aaaaaaaa']);
  });
});

// ---------------------------------------------------------------------------
// 2 · 🔴 本单最硬的一条：读不到 chat_sessions ⇒「形状变了」，不是「没有会话」
// ---------------------------------------------------------------------------
describe('C26-2 · 不能把未知当成空', () => {
  it('响应里没有 chat_sessions ⇒ halt(shape-changed) 并落盘，绝不当成空列表', async () => {
    const store = memoryStore();
    // 合成的「接口改版」：数组改名了，信封还在。
    const drifted = JSON.stringify({ data: { biz_data: { sessions: [], has_more: false } } });
    const be = backend([drifted]);

    const report = await run(store, be.http, 'acct-drift');

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('chat_sessions');
    // 🔴 留痕必须落盘，重启之后还在 —— 静默的反面。
    const persisted = await store.load('cs_backfill_v1:deepseek:acct-drift');
    expect((persisted as { halted?: { reason: string } }).halted?.reason).toBe('shape-changed');

    // 🔴🔴 这条断言就是本单的靶心：它【不等于】「空列表」那条路径。
    //     枚举没有被标成走完 —— 回溯腿绝不会以为自己干完了。
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.newDebts).toBe(0);
    expect(report.stopped).not.toBe('queue-empty');
  });

  it('对照组：chat_sessions 真的是空数组 ⇒ 不是 halt，是「列完了，你没有历史」', async () => {
    const store = memoryStore();
    const be = backend([pageBody([], false)]);

    const report = await run(store, be.http, 'acct-really-empty');

    // 🔴 与上一条逐项对照：两种结局在账本上长得完全不一样。
    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.state.enumCursor.complete).toBe(true);
    expect(report.enumTruncated).toBeNull();
    expect(report.newDebts).toBe(0);
  });

  it('信封本身变了（没有 data / biz_data）也一样报形状变了', () => {
    expect(parseDeepSeekListPage(JSON.stringify({ biz_data: { chat_sessions: [] } })).ok).toBe(false);
    expect(parseDeepSeekListPage(JSON.stringify({ data: { chat_sessions: [] } })).ok).toBe(false);
    expect(parseDeepSeekListPage('not json at all').ok).toBe(false);
  });

  it('🔴 顶层业务码（code / biz_code）两源打架 ⇒ 解析器不依赖它', () => {
    const sessions = [session(1)];
    const withCode = JSON.stringify({ code: 0, data: { biz_data: { chat_sessions: sessions, has_more: false } } });
    const withBizCode = JSON.stringify({ biz_code: 500, data: { biz_data: { chat_sessions: sessions, has_more: false } } });
    const withNeither = JSON.stringify({ data: { biz_data: { chat_sessions: sessions, has_more: false } } });
    for (const text of [withCode, withBizCode, withNeither]) {
      const parsed = parseDeepSeekListPage(text);
      expect(parsed.ok).toBe(true);
    }
  });
});

// ---------------------------------------------------------------------------
// 3 · 🔴 读不到 seq_id ⇒ 只抓第一页，并【如实报告】
// ---------------------------------------------------------------------------
describe('C26-3 · 翻不了页就说翻不了页', () => {
  it('记录里没有 seq_id ⇒ 只发一次请求，enumTruncated=cursor-missing', async () => {
    const store = memoryStore();
    const noSeq = [session(1, { seq_id: undefined }), session(2, { seq_id: undefined })];
    // has_more 说还有下一页 —— 但我们翻不过去。
    const be = backend([pageBody(noSeq, true), pageBody([session(9)], false)]);

    const report = await run(store, be.http, 'acct-no-seq');

    // 🔴 只抓了第一页：第二页的请求根本没发出去。
    expect(be.calls).toEqual([`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=${LIMIT}`]);
    expect(report.enumeratedPages).toBe(1);
    expect(report.newDebts).toBe(2);
    // 🔴 如实报告，而且是【具名】的、【落盘】的：
    //    complete=true 在这里的意思是「停在这里了」，truncated 就是用来区分这两者的。
    expect(report.enumTruncated).toBe('cursor-missing');
    expect(report.state.enumCursor.truncated).toBe('cursor-missing');
    const persisted = await store.load('cs_backfill_v1:deepseek:acct-no-seq') as {
      enumCursor: { complete: boolean; truncated?: string };
    };
    expect(persisted.enumCursor.truncated).toBe('cursor-missing');
    // 对照组（C26-1 的正常两页）里 truncated 是 undefined —— 两者可分辨。
    expect(report.state.enumCursor.complete).toBe(true);
  });

  it('一页里只要有一条读不出 seq_id，整页的游标就不用（宁可停，不许翻错页）', () => {
    const parsed = parseDeepSeekListPage(
      pageBody([session(1), session(2, { seq_id: undefined }), session(3)], true),
    );
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.nextCursor).toBeNull();
    expect(parsed.page.hasMore).toBe(true);
  });

  it('响应里没有 has_more ⇒ 不许当成「没有下一页」，报 has-more-missing', async () => {
    const store = memoryStore();
    const be = backend([pageBody([session(1)], false, { omitHasMore: true })]);

    const report = await run(store, be.http, 'acct-no-hasmore');

    expect(report.enumeratedPages).toBe(1);
    expect(report.enumTruncated).toBe('has-more-missing');
    expect(report.state.enumCursor.truncated).toBe('has-more-missing');
    // 🔴 与「has_more:false（真的没有下一页）」可分辨：那一条的 enumTruncated 是 null。
    const clean = await run(memoryStore(), backend([pageBody([session(1)], false)]).http, 'acct-clean');
    expect(clean.enumTruncated).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// 4 · updated_at 是**数值**
// ---------------------------------------------------------------------------
describe('C26-4 · updated_at 当数值处理', () => {
  it('数值时间戳被原样收下，取本页最大值，不做任何换算', () => {
    const parsed = parseDeepSeekListPage(pageBody([
      session(1, { updated_at: 1_700_000_000 }),
      session(2, { updated_at: 1_755_123_456 }),
      session(3, { updated_at: 1_600_000_000 }),
    ], false));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    // 🔴 原样的那个数，不是 Date、不是毫秒换算、不是字符串。
    expect(parsed.page.newestUpdatedAt).toBe(1_755_123_456);
    expect(typeof parsed.page.newestUpdatedAt).toBe('number');
  });

  it('毫秒量级的数值同样原样收下（我们不猜它是秒还是毫秒）', () => {
    const parsed = parseDeepSeekListPage(pageBody([session(1, { updated_at: 1_755_123_456_789 })], false));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.newestUpdatedAt).toBe(1_755_123_456_789);
  });

  it('🔴 变成 ISO 串（或任何非数值）⇒ 判成形状变了，绝不 new Date(string) 宽容过去', () => {
    const iso = parseDeepSeekListPage(pageBody([session(1, { updated_at: '2026-08-17T00:00:00Z' })], false));
    expect(iso.ok).toBe(false);
    if (iso.ok) return;
    expect(iso.detail).toContain('updated_at');

    // 「数字被包成字符串」这种漂移最不容易看出来，所以单独钉一条。
    expect(parseDeepSeekListPage(pageBody([session(1, { updated_at: '1755000000' })], false)).ok).toBe(false);
    expect(parseDeepSeekListPage(pageBody([session(1, { updated_at: { seconds: 1 } })], false)).ok).toBe(false);
  });

  it('整个字段缺席 ⇒ 容忍（枚举不需要它），newestUpdatedAt=null', () => {
    const parsed = parseDeepSeekListPage(pageBody([session(1, { updated_at: undefined })], false));
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.page.newestUpdatedAt).toBeNull();
    expect(parsed.page.ids).toEqual(['ds-0001-aaaaaaaa']);
  });
});

// ---------------------------------------------------------------------------
// 5 · 🔴 半条腿：列表列得出来，正文段没有出处
// ---------------------------------------------------------------------------
describe('C26-5 · 只列得出会话 ≠ 补得回历史', () => {
  it('枚举完之后 halt(detail-unsupported)，一条正文请求都没发，欠账原封不动', async () => {
    const store = memoryStore();
    const be = backend([pageBody([session(1), session(2)], false)]);

    const report = await run(store, be.http, 'acct-half');

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('detail-unsupported');
    // 🔴 与 'unsupported-platform' 的差别就在这一行：列表请求真的发过。
    expect(be.calls.length).toBe(1);
    // 🔴 欠账留着，一条都没被清掉、也没被冒充成已归档。
    expect(report.state.pending.length).toBe(2);
    expect(report.state.archived).toEqual([]);
    expect(report.archivedThisRun).toEqual([]);
    // 留痕里要写清缺什么，不许只说一句「不支持」。
    expect(report.halted?.detail).toContain('missing:');
    expect(report.halted?.detail).toContain('detailPath');
  });

  it('平台名单把这个中间态单列出来，不四舍五入到任何一边', () => {
    expect(BACKFILL_LIST_ONLY_PLATFORMS).toEqual(['deepseek']);
    // 🔴 「能补回历史」仍然只有 chatgpt —— 只能列出会话不算补得回历史。
    expect(BACKFILL_SUPPORTED_PLATFORMS).toEqual(['chatgpt']);
    expect(DEEPSEEK_PLAN.partial?.missing.length ?? 0).toBeGreaterThan(0);
    expect(DEEPSEEK_PLAN.partial?.userNote).not.toContain('正在');
  });

  it('plan 的声明与出处：列表段齐了，正文段是 null（不是随便填一个）', () => {
    const plan = backfillPlanFor('deepseek');
    expect(plan).not.toBeNull();
    expect(plan!.listPath).toBe('/api/v0/chat_session/fetch_page');
    expect(plan!.listCursorUrl).toBeTypeOf('function');
    expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, null, 30))
      .toBe(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=30`);
    expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, 42, 30))
      .toBe(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=30&before_seq_id=42`);
    expect(plan!.detailPath).toBeNull();
    expect(plan!.detailUrl).toBeNull();
    // 出处是【必填】，而且必须把「多源交叉、非官方文档、有时效风险」写进去，不许只留结论。
    expect(plan!.provenance).toContain('before_seq_id');
    expect(plan!.provenance).toContain('2025-12');
    // 🔴 五个来源里一次都没出现过的参数名，一个都不许进 URL。
    for (const banned of ['offset=', 'page=', 'page_size=', 'limit=', 'cursor=']) {
      expect(plan!.listCursorUrl!(DEEPSEEK_ORIGIN, 42, 30)).not.toContain(banned);
    }
  });
});

// ---------------------------------------------------------------------------
// 6 · 白名单：走原有那套机制，放行的只有 plan 自己逐字写下来的那条路径
// ---------------------------------------------------------------------------
describe('C26-6 · 内容脚本的白名单', () => {
  it('列表路径放行；正文路径、相邻路径、跨源一律拒', () => {
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=100`, DEEPSEEK_ORIGIN)).toBe(true);
    expect(isAllowedBackfillUrl(
      `${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}?count=100&before_seq_id=997`, DEEPSEEK_ORIGIN)).toBe(true);
    // 🔴 正文段没有出处 ⇒ 一条正文 URL 都没被放行。
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat/history_messages`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 没有变成前缀通配：像但不逐字相等的路径照样拒。
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/fetch_page2`, DEEPSEEK_ORIGIN)).toBe(false);
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 同源这一条没松：页面是别的源就拒。
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}${DEEPSEEK_LIST_PATH}`, 'https://chatgpt.com')).toBe(false);
    // 🔴 ChatGPT 的路径不会因为 DeepSeek 有了 plan 就在 DeepSeek 上被放行。
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/backend-api/conversations`, DEEPSEEK_ORIGIN)).toBe(false);
  });
});
