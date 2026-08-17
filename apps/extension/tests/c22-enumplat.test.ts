/**
 * C22 · 回溯腿的平台枚举表。
 *
 * 🔴 全部合成夹具：没有任何一行会碰真实平台接口，没有登录态，没有真实对话正文。
 *    http 端口一律注入；不注入就是 notWiredHttp（调用即抛错）。
 *
 * 三条判据，对应三种【对用户含义完全不同】的结局：
 *   1. 能回溯的平台 ⇒ 合成响应 ⇒ 枚举出 N(≥2) 条 ⇒ 进欠账集合；
 *   2. 形状不合   ⇒ halt('shape-changed') 留痕，绝不静默；
 *   3. 还没支持的平台 ⇒ halt('unsupported-platform')，**且一个请求都不许发出去** ——
 *      它必须与「枚举出 0 条」（= 你真的没有历史）是两个可区分的结果。
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { PLATFORMS } from '../lib/contract';
import {
  BACKFILL_SUPPORTED_PLATFORMS,
  BACKFILL_UNSUPPORTED,
  BACKFILL_UNSUPPORTED_PLATFORMS,
  backfillPlanFor,
  unsupportedBackfillFor,
} from '../lib/backfill/enumerate';
import { isAllowedBackfillUrl } from '../lib/backfill/tab-port';
import { renderPopup, popupText, coverageLine, NO_FAILURES } from '../lib/popup-view';
import type { Clock } from '../lib/backfill/pace';

const CHATGPT_ORIGIN = 'https://chatgpt.com';
const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';

function fakeClock(): Clock {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

function ids(n: number): string[] {
  return Array.from({ length: n }, (_, i) => `conv-${String(i).padStart(4, '0')}-aaaaaaaa`);
}

function listBody(all: string[]): string {
  return JSON.stringify({
    items: all.map((id) => ({ id, title: 'synthetic-fixture' })),
    total: all.length,
    limit: 100,
    offset: 0,
  });
}

/** 合成后端。记录每一次被请求的 URL —— 「一个请求都没发」要靠它来证明。 */
function backend(all: string[], listText?: string) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/backend-api/conversations') {
      const offset = Number(u.searchParams.get('offset') ?? 0);
      return { status: 200, text: listText ?? listBody(all.slice(offset, offset + 100)) };
    }
    const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
    return {
      status: 200,
      text: JSON.stringify({ current_node: `${id}-n`, mapping: { [`${id}-n`]: {} } }),
    };
  };
  return { http, calls };
}

// ---------------------------------------------------------------------------
// 判据 1 · 能填的平台：合成响应 ⇒ 枚举出 N 条 ⇒ 进欠账集合（N ≥ 2）
// ---------------------------------------------------------------------------
describe('C22-1 · 能回溯的平台真的枚举得出来', () => {
  it('合成列表响应 ⇒ 欠账集合里有 N≥2 条，且没有 halt', async () => {
    const store = memoryStore();
    const all = ids(5);
    const be = backend(all);

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: CHATGPT_ORIGIN,
      scope: 'acct-c22',
      store,
      http: be.http,
      clock: fakeClock(),
      // 只跑枚举段：正文一条都不取（budget=0），这条用例只证明「列表进得了欠账集合」。
      maxDetails: 0,
    });

    expect(report.halted).toBeNull();
    expect(report.enumeratedPages).toBe(1);
    expect(report.newDebts).toBe(5);
    expect(report.newDebts).toBeGreaterThanOrEqual(2);
    expect(report.state.pending).toEqual(all);
    expect(report.state.totalKnown).toBe(5);
    // 🔴 枚举段用的必须是 chatgpt plan 自己的路径。
    expect(be.calls[0]).toBe(`${CHATGPT_ORIGIN}/backend-api/conversations?offset=0&limit=100`);
  });

  it('plan 的七项声明齐了才算「能回溯」', () => {
    const plan = backfillPlanFor('chatgpt');
    expect(plan).not.toBeNull();
    expect(plan!.listPath).toBe('/backend-api/conversations');
    expect(plan!.detailPath).toBe('/backend-api/conversation/');
    expect(plan!.listUrl(CHATGPT_ORIGIN, 20, 50))
      .toBe(`${CHATGPT_ORIGIN}/backend-api/conversations?offset=20&limit=50`);
    expect(plan!.detailUrl(CHATGPT_ORIGIN, 'a b')).toBe(`${CHATGPT_ORIGIN}/backend-api/conversation/a%20b`);
    expect(typeof plan!.parseListPage).toBe('function');
    // 出处这一项也是【必填】——「没有外部出处」也必须写出来，不许留空。
    expect(plan!.provenance.length).toBeGreaterThan(0);
  });
});

// ---------------------------------------------------------------------------
// 判据 2 · 🔴 形状不合 ⇒ halt('shape-changed') 留痕，不静默
// ---------------------------------------------------------------------------
describe('C22-2 · 形状不合必须留痕', () => {
  it('列表响应没有 items ⇒ halt(shape-changed)，欠账集合里落了盘', async () => {
    const store = memoryStore();
    // 平台把 items 改名成了 conversations（合成的「接口改版」）。
    const drifted = JSON.stringify({ conversations: [{ id: 'x-0000-aaaaaaaa' }], total: 1 });
    const be = backend([], drifted);

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: CHATGPT_ORIGIN,
      scope: 'acct-drift',
      store,
      http: be.http,
      clock: fakeClock(),
    });

    // 🔴 三件事一件都不许少：停了、理由对、留了痕。
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('items');
    // 🔴 静默的反面：留痕必须【落盘】，重启之后还在。
    const persisted = await store.load('cs_backfill_v1:chatgpt:acct-drift');
    expect((persisted as { halted?: { reason: string } }).halted?.reason).toBe('shape-changed');
    // 🔴 绝不许被当成「枚举出 0 条」：一条欠账都没入队，但账本上写着为什么。
    expect(report.newDebts).toBe(0);
    expect(report.state.enumCursor.complete).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 判据 3 · 🔴 不支持的平台 = 明确的「不支持」，而不是「枚举出 0 条」
// ---------------------------------------------------------------------------
describe('C22-3 · 「还不会读你的历史」与「你没有历史」必须分开', () => {
  it('还没支持的平台 ⇒ halt(unsupported-platform)，且一个请求都没发', async () => {
    const store = memoryStore();
    const be = backend([]);

    const report = await runBackfill({
      platform: 'deepseek',
      origin: DEEPSEEK_ORIGIN,
      scope: 'acct-ds',
      store,
      http: be.http,
      clock: fakeClock(),
    });

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('unsupported-platform');
    // 🔴 在发出任何请求【之前】就停住 —— 以前这里会拿 ChatGPT 的路径去打 DeepSeek。
    expect(be.calls).toEqual([]);
    // 留痕里必须写清缺哪几项，不许只说一句「不支持」。
    expect(report.halted?.detail).toContain('missing:');
    expect(report.halted?.detail).toContain('parseListPage');
  });

  it('对照组：支持的平台但列表真的是空的 ⇒ 不是 halt，是「跑完了，没有历史」', async () => {
    const store = memoryStore();
    const be = backend([]);

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: CHATGPT_ORIGIN,
      scope: 'acct-empty',
      store,
      http: be.http,
      clock: fakeClock(),
    });

    // 🔴 与上一条对照：两种结局在账本上长得完全不一样。
    expect(report.stopped).toBe('queue-empty');
    expect(report.halted).toBeNull();
    expect(report.newDebts).toBe(0);
    expect(report.state.enumCursor.complete).toBe(true);
    expect(be.calls.length).toBe(1);
  });

  it('内容脚本不替「还不会回溯」的平台代发任何请求', () => {
    // 支持的平台：自己的两条路径放行。
    expect(isAllowedBackfillUrl(`${CHATGPT_ORIGIN}/backend-api/conversations?offset=0&limit=100`, CHATGPT_ORIGIN)).toBe(true);
    expect(isAllowedBackfillUrl(`${CHATGPT_ORIGIN}/backend-api/conversation/abc`, CHATGPT_ORIGIN)).toBe(true);
    // 🔴 还没支持的平台：即使路径长得像，也一律拒发。
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/backend-api/conversations`, DEEPSEEK_ORIGIN)).toBe(false);
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/fetch_page`, DEEPSEEK_ORIGIN)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 4 · 两张表必须把平台表【盖满】—— 新加一个平台却忘了表态，就在这里红
// ---------------------------------------------------------------------------
describe('C22-4 · 每个平台都必须有一个明确结论', () => {
  it('平台表里每一行，恰好落在「能回溯」或「登记为暂时不能」其中一侧', () => {
    for (const row of PLATFORMS) {
      const plan = backfillPlanFor(row.id);
      const gap = unsupportedBackfillFor(row.id);
      expect(
        (plan === null) !== (gap === null),
        `平台 ${row.id} 必须恰好出现在两张表之一`,
      ).toBe(true);
    }
    expect(BACKFILL_SUPPORTED_PLATFORMS.length + BACKFILL_UNSUPPORTED_PLATFORMS.length)
      .toBe(PLATFORMS.length);
    // 今天的真实状况，写死在测试里：5 个平台，只有 1 个补得回历史。
    expect(BACKFILL_SUPPORTED_PLATFORMS).toEqual(['chatgpt']);
    expect(BACKFILL_UNSUPPORTED_PLATFORMS).toEqual(['deepseek', 'gemini', 'claude', 'kimi']);
  });

  it('每条「暂时不能」都必须写明缺哪几项 + 给用户一句人话', () => {
    for (const gap of BACKFILL_UNSUPPORTED) {
      expect(gap.missing.length, `${gap.platform} 必须写明缺什么`).toBeGreaterThan(0);
      expect(gap.userNote.length).toBeGreaterThan(0);
      // 🔴 不许暗示它在补。
      expect(gap.userNote).not.toContain('正在');
    }
  });
});

// ---------------------------------------------------------------------------
// 5 · Popup 必须把这件事说出来
// ---------------------------------------------------------------------------
describe('C22-5 · Popup 的诚实说明', () => {
  it('那一行同时说清「谁能补」「谁不能」「不能≠坏了」', () => {
    const line = coverageLine();
    expect(line).toContain('chatgpt');
    for (const id of BACKFILL_UNSUPPORTED_PLATFORMS) expect(line).toContain(id);
    expect(line).toContain('还不能');
    expect(line).toContain('不是坏了');
    // 沿用 C18 的红线：进度类文案里不许出现百分号和时间预估。
    expect(line).not.toContain('%');
    expect(line).not.toContain('预计');
  });

  it('这一行出现在 popupText 里，且名单不是手写的', () => {
    const out = popupText(renderPopup({
      enabled: true, block: 'no-http-port', guard: null, state: null, target: null,
      failures: NO_FAILURES,
    }));
    expect(out).toContain(coverageLine());
    expect(out).not.toContain('%');
    // 逐平台的卡点也要能看到（notes 里）。
    expect(out).toContain('DeepSeek：还不能回溯历史');
    expect(out).toContain('Gemini：还不能回溯历史');
  });

  it('halted=unsupported-platform 时说的是「还没实现」，不是「平台改版了」', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      guard: null,
      state: {
        v: 1, platform: 'deepseek', scope: 'acct-ds', totalKnown: null, totalSource: 'unknown',
        enumCursor: { offset: 0, complete: false }, pending: [], archived: [],
        detailToday: { day: '', count: 0 },
        halted: { reason: 'unsupported-platform', at: 0, detail: 'missing: listUrl' },
      },
      target: { platform: 'deepseek', scope: 'acct-ds' },
      failures: NO_FAILURES,
    });
    const out = popupText(view);
    expect(out).toContain('还没有实现');
    expect(out).toContain('不是平台改版');
  });
});
