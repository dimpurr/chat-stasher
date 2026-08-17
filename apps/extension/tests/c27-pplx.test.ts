/**
 * C27 · Perplexity 会话列表：没有 has_more，只能把空页与短页作为两种
 * 不可靠但可留痕的客户端推断。
 *
 * 全部 HTTP 都是合成夹具；本文件不会登录或向 perplexity.ai 发请求。
 */

import { describe, expect, it } from 'vitest';
import { runBackfill, type HttpPort, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { checkBackfillRequest } from '../lib/backfill/tab-port';
import {
  BACKFILL_LIST_ONLY_PLATFORMS,
  BACKFILL_SUPPORTED_PLATFORMS,
  PERPLEXITY_LIST_PATH,
  PERPLEXITY_PLAN,
  backfillPlanFor,
  parsePerplexityListPage,
} from '../lib/backfill/enumerate';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://www.perplexity.ai';
const LIMIT = 2;

function fakeClock(): Clock {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

function thread(n: number): { thread_id: string } {
  return { thread_id: `pplx-${String(n).padStart(4, '0')}-aaaaaaaa` };
}

function pageBody(items: Array<{ thread_id: string }>): string {
  // R26 的列表结局只依赖返回数组长度；没有 total / has_more / count。
  return JSON.stringify(items);
}

interface Call {
  url: string;
  init?: { method?: string; body?: string; contentType?: string };
}

function backend(pages: string[]): { http: HttpPort; calls: Call[] } {
  const calls: Call[] = [];
  const http: HttpPort = async (url, init) => {
    calls.push({ url, init });
    expect(new URL(url).pathname).toBe(PERPLEXITY_LIST_PATH);
    return { status: 200, text: pages[calls.length - 1] ?? '[]' };
  };
  return { http, calls };
}

async function run(store: ReturnType<typeof memoryStore>, pages: string[], scope: string) {
  const be = backend(pages);
  const report = await runBackfill({
    platform: 'perplexity',
    origin: ORIGIN,
    scope,
    store,
    http: be.http,
    clock: fakeClock(),
    listLimit: LIMIT,
  });
  return { report, calls: be.calls };
}

function requestBody(offset: number): string {
  return JSON.stringify({ limit: LIMIT, offset, ascending: false, search_term: '' });
}

describe('C27-1 · POST 分页参数', () => {
  it('两页请求：第二页 offset=limit，固定参数原样保留', async () => {
    const { report, calls } = await run(
      memoryStore(),
      [pageBody([thread(1), thread(2)]), pageBody([thread(3)])],
      'acct-two-pages',
    );

    expect(calls).toHaveLength(2);
    expect(calls.map((call) => call.url)).toEqual([
      `${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
      `${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
    ]);
    expect(calls.map((call) => JSON.parse(call.init?.body ?? 'null'))).toEqual([
      { limit: LIMIT, offset: 0, ascending: false, search_term: '' },
      { limit: LIMIT, offset: LIMIT, ascending: false, search_term: '' },
    ]);
    expect(calls.every((call) => call.init?.method === 'POST')).toBe(true);
    expect(calls.every((call) => call.init?.contentType === 'application/json')).toBe(true);
    expect(report.enumeratedPages).toBe(2);
    expect(report.newDebts).toBe(3);
  });

  it('plan 的 URL 不偷塞分页 query，body 键闭集与固定值可逐项核对', () => {
    expect(backfillPlanFor('perplexity')).toBe(PERPLEXITY_PLAN);
    expect(PERPLEXITY_PLAN.listUrl(ORIGIN, 999, LIMIT))
      .toBe(`${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`);
    expect(PERPLEXITY_PLAN.listPost?.bodyKeys).toEqual([
      'limit', 'offset', 'ascending', 'search_term',
    ]);
    expect(JSON.parse(PERPLEXITY_PLAN.listPost!.body(ORIGIN, LIMIT, LIMIT)))
      .toEqual({ limit: LIMIT, offset: LIMIT, ascending: false, search_term: '' });
    expect(PERPLEXITY_PLAN.detailPath).toBeNull();
    expect(PERPLEXITY_PLAN.detailUrl).toBeNull();
  });
});

describe('C27-2 · 没有终止字段时，空页与短页必须分开留痕', () => {
  it('空页停：记录 empty-page-inferred，且 complete 不为真', async () => {
    const store = memoryStore();
    const { report, calls } = await run(
      store,
      [pageBody([thread(1), thread(2)]), pageBody([])],
      'acct-empty-page',
    );

    expect(calls).toHaveLength(2);
    expect(report.enumTruncated).toBe('empty-page-inferred');
    expect(report.state.enumCursor.truncated).toBe('empty-page-inferred');
    expect(report.state.enumCursor.complete).toBe(false);
    const persisted = await store.load('cs_backfill_v1:perplexity:acct-empty-page') as {
      enumCursor: { complete: boolean; truncated?: string };
    };
    expect(persisted.enumCursor).toEqual({
      offset: LIMIT * 2,
      complete: false,
      truncated: 'empty-page-inferred',
    });
  });

  it('短页停：记录 short-page-inferred；它与空页不是同一个结局', async () => {
    const { report } = await run(
      memoryStore(),
      [pageBody([thread(1), thread(2)]), pageBody([thread(3)])],
      'acct-short-page',
    );

    expect(report.enumTruncated).toBe('short-page-inferred');
    expect(report.state.enumCursor.truncated).toBe('short-page-inferred');
    expect(report.enumTruncated).not.toBe('empty-page-inferred');
    expect(report.state.enumCursor.complete).toBe(false);
    // 列表段确实读到了，但正文段没有出处，所以不是“用户没有会话”。
    expect(report.halted?.reason).toBe('detail-unsupported');
  });
});

describe('C27-3 · 形状漂移与没有会话必须可区分', () => {
  it('拿不到顶层数组 ⇒ shape-changed，不能当成用户没有会话', async () => {
    const store = memoryStore();
    const { report, calls } = await run(store, [JSON.stringify({ threads: [] })], 'acct-shape');

    expect(calls).toHaveLength(1);
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('top-level array');
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.state.pending).toEqual([]);
    expect(report.stopped).not.toBe('queue-empty');
    expect((await store.load('cs_backfill_v1:perplexity:acct-shape') as any).halted.reason)
      .toBe('shape-changed');
  });

  it('真正的空数组是推断停点，不是 shape-changed', async () => {
    const { report } = await run(memoryStore(), [pageBody([])], 'acct-no-history');
    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.enumTruncated).toBe('empty-page-inferred');
    expect(report.state.enumCursor.complete).toBe(false);
  });

  it('解析器只认列表数组与 thread_id，不读取未经证实的时间字段', () => {
    expect(parsePerplexityListPage(pageBody([thread(1)]) )).toEqual({
      ok: true,
      page: { ids: ['pplx-0001-aaaaaaaa'], total: null },
    });
    expect(parsePerplexityListPage(JSON.stringify({ threads: [] })).ok).toBe(false);
    expect(parsePerplexityListPage(JSON.stringify([{ id: 'wrong-field' }])).ok).toBe(false);
  });
});

describe('C27-4 · Perplexity 回溯白名单', () => {
  it('精确列表路径 + 正确 POST 放行；相似前缀、正文路径、跨源拒发', () => {
    const listUrl = `${ORIGIN}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`;
    const valid = {
      url: listUrl,
      method: 'POST',
      body: requestBody(0),
      contentType: 'application/json',
    } as const;
    expect(checkBackfillRequest(valid, ORIGIN).ok).toBe(true);
    expect(checkBackfillRequest({
      ...valid,
      url: `${ORIGIN}${PERPLEXITY_LIST_PATH}2?version=2.18&source=default`,
    }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({
      ...valid,
      url: `${ORIGIN}/rest/thread/get_thread/pplx-0001`,
    }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({
      ...valid,
      url: `https://evil.example${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
    }, ORIGIN).ok).toBe(false);
  });

  it('平台名单：Perplexity 只列得出会话，supported 仍只有 ChatGPT', () => {
    expect(BACKFILL_LIST_ONLY_PLATFORMS).toEqual(['deepseek', 'perplexity']);
    expect(BACKFILL_SUPPORTED_PLATFORMS).toEqual(['chatgpt']);
  });
});
