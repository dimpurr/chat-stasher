/**
 * Q2 badge（W2 版）· 角标说的必须是发件箱的真实状态。
 *
 * 语义（任务 8）：
 *   · 待送数 > 0 ⇒ 显示数字；
 *   · 有被拒【或】发件箱已满 ⇒ 告警态（不是数字）；
 *   · 都没有 ⇒ 空角标。
 *
 * 🔴 还有第四条：**读不出发件箱时不许显示成"空"或"0"**。
 *    「我不知道」和「没有待送」是两个状态，角标是用户唯一一眼能看到的地方，
 *    它上面不许出现把未知说成已知的东西。
 *
 * 旧版（「最近 5 分钟捕获计数 + 过期清空」）随自动下载通道一起删除了 ——
 * 那条过期规则存在的理由是「计数写在 storage 里、可能比写它的 worker 活得久」，
 * 而角标现在是发件箱的派生值，不一致在结构上不可能出现。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';

/**
 * 🔴 每一处都用动态 import 取模块：`vi.resetModules()` 会作废模块注册表，
 *    而 `lib/badge` 里缓存的 IndexedDB 连接是**模块级**的。静态 import 拿到的是
 *    「上一个用例那一刻」的模块实例，它会继续用上一轮的连接 —— 于是写进去的
 *    条目和读出来的库不是同一个（测试会红，而生产里不存在这个形状：
 *    一个 SW 只有一张模块图，库也只有一份）。
 *    所以在每个用例里重新 import，与 `vi.resetModules()` 配对使用。
 */
async function badgeModule() {
  return await import('../lib/badge');
}

const badgeCalls: Array<{ kind: 'text' | 'bg' | 'title'; text?: string; color?: string; title?: string }> = [];

const fakeBrowser: any = {
  action: {
    async setBadgeText(o: { text: string }) { badgeCalls.push({ kind: 'text', text: o.text }); },
    async setBadgeBackgroundColor(o: { color: string }) { badgeCalls.push({ kind: 'bg', color: o.color }); },
    async setTitle(o: { title: string }) { badgeCalls.push({ kind: 'title', title: o.title }); },
  },
  runtime: { id: 'badge-test' },
};

function last(kind: 'text' | 'bg' | 'title') {
  return [...badgeCalls].reverse().find((c) => c.kind === kind);
}

beforeEach(() => {
  badgeCalls.length = 0;
  vi.resetModules();
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
});

async function enqueueOne(payload = '{"a":1}', name = 'chatgpt-a.json') {
  const { enqueue } = await import('../lib/outbox');
  return await enqueue(name, payload);
}

describe('Q2 · badgeFor（纯函数）', () => {
  it('空发件箱 ⇒ 没有角标（不是 0，也不是灰色）', async () => {
    const { badgeFor } = await badgeModule();
    expect(badgeFor({ pending: 0, rejected: 0, bytes: 0, capacityBytes: 100, full: false })).toBeNull();
  });

  it('待送 N ⇒ 数字 N + 待送色的标题', async () => {
    const { badgeFor, BADGE_COLOR_WAITING } = await badgeModule();
    const plan = badgeFor({ pending: 3, rejected: 0, bytes: 10, capacityBytes: 100, full: false })!;
    expect(plan.text).toBe('3');
    expect(plan.color).toBe(BADGE_COLOR_WAITING);
    expect(plan.title).toContain('3 capture(s) waiting');
    console.log('[Q2] 待送 3 的角标:', plan);
  });

  it('🔴 有被拒 ⇒ 告警态压过数字（数字会读成"在推进"）', async () => {
    const { badgeFor, BADGE_ALERT_TEXT, BADGE_COLOR_ALERT } = await badgeModule();
    const plan = badgeFor({ pending: 5, rejected: 2, bytes: 10, capacityBytes: 100, full: false })!;
    expect(plan.text).toBe(BADGE_ALERT_TEXT);
    expect(plan.text).not.toBe('5');
    expect(plan.color).toBe(BADGE_COLOR_ALERT);
    expect(plan.title).toContain('2 rejected');
    expect(plan.title).toContain('5 capture(s) waiting');
  });

  it('🔴 发件箱已满 ⇒ 告警态（待送数是 0 也要告警：新的捕获正在被拒收）', async () => {
    const { badgeFor, BADGE_ALERT_TEXT } = await badgeModule();
    const plan = badgeFor({ pending: 0, rejected: 0, bytes: 100, capacityBytes: 100, full: true })!;
    expect(plan.text).toBe(BADGE_ALERT_TEXT);
    expect(plan.title).toContain('full');
  });

  it('🔴 发件箱读不出来 ⇒ 告警态，且标题明说"说不出来"，绝不当成空', async () => {
    const { badgeFor, BADGE_ALERT_TEXT } = await badgeModule();
    const plan = badgeFor(null)!;
    expect(plan).not.toBeNull();
    expect(plan.text).toBe(BADGE_ALERT_TEXT);
    expect(plan.title).toContain('cannot be read');
    expect(plan.title).toContain('cannot say what is queued');
  });
});

describe('Q2 · refreshBadge（走真实的发件箱）', () => {
  it('空发件箱 ⇒ 角标被清空', async () => {
    const { refreshBadge } = await badgeModule();
    await refreshBadge();
    expect(last('text')!.text).toBe('');
  });

  it('入队两条 ⇒ 角标显示 2；两条都被 ack 之后 ⇒ 清空', async () => {
    const { refreshBadge, BADGE_COLOR_WAITING } = await badgeModule();
    await enqueueOne('{"a":1}', 'chatgpt-a.json');
    await enqueueOne('{"a":2}', 'chatgpt-b.json');
    const plan = await refreshBadge();
    expect(plan!.text).toBe('2');
    expect(last('text')!.text).toBe('2');
    expect(last('bg')!.color).toBe(BADGE_COLOR_WAITING);

    const { drainOutbox } = await import('../lib/outbox');
    await drainOutbox({
      deliver: async () => ({
        delivered: true, status: 'stored', shard: 's', requestId: 'r', sha256: 'x',
      }),
    });
    await refreshBadge();
    expect(last('text')!.text).toBe('');
  });

  it('🔴 有一条被拒 ⇒ 角标转告警态，标题里说得出被拒几条', async () => {
    const { refreshBadge, BADGE_ALERT_TEXT } = await badgeModule();
    await enqueueOne('{"a":1}', 'chatgpt-a.json');
    const { drainOutbox } = await import('../lib/outbox');
    await drainOutbox({
      deliver: async () => ({
        delivered: false, reason: 'nack', kind: 'invalid-bundle', retryable: false,
        detail: 'not a bundle', requestId: 'r', sha256: 'x',
      }),
    });
    await refreshBadge();
    expect(last('text')!.text).toBe(BADGE_ALERT_TEXT);
    expect(last('title')!.title).toContain('1 rejected');
  });

  it('🔴 没有 action API 的浏览器构建里静默 no-op（角标绝不许把捕获带下水）', async () => {
    const { refreshBadge } = await badgeModule();
    vi.stubGlobal('browser', { runtime: { id: 'no-action' } });
    await enqueueOne();
    // 不抛错、也一个角标 API 都没调到 —— 角标是装饰性的，它自己出问题不许冒泡。
    await expect(refreshBadge()).resolves.toBeDefined();
    expect(badgeCalls).toEqual([]);
  });
});
