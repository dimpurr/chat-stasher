import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

/**
 * C12 · 下载停滞守卫。
 *
 * 🔴 全部合成夹具：没有一行会真的下载、真的联网、或碰任何登录态。
 *    browser.downloads 是本文件里的假实现，可以被指令为「永远不发终态事件」，
 *    那就是「停滞」——【注意】测试只断言"没有在超时内到达终态"这个观测事实，
 *    绝不断言它是被什么原因造成的。
 */

import {
  classifyDelta,
  stalledResult,
  initialGuardState,
  recordOutcome,
  loadGuardState,
  recordDownloadOutcome,
  resumeAfterGuard,
  guardBadge,
  guardAlertDetail,
  isGuardTripped,
  DEFAULT_STALL_THRESHOLD,
  GUARD_KEY,
  type DownloadResult,
} from '../lib/download-guard';
import { memoryStore } from '../lib/backfill/store';
import { runBackfill } from '../lib/backfill/engine';
import { initialState, stateKey } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

// ---------------------------------------------------------------------------
// 假的 downloads 通道：三种可指令的行为 —— complete / interrupted / 永不终态
// ---------------------------------------------------------------------------

type FakeBehaviour = 'complete' | 'interrupted' | 'silent';

let behaviour: FakeBehaviour = 'complete';
let nextId = 1;
const onChangedListeners: Array<(d: any) => void> = [];

const fakeBrowser: any = {
  downloads: {
    onChanged: { addListener(fn: any) { onChangedListeners.push(fn); } },
    async download(): Promise<number> {
      const id = nextId++;
      if (behaviour === 'complete') {
        setTimeout(() => {
          for (const l of onChangedListeners) l({ id, state: { previous: 'in_progress', current: 'complete' } });
        }, 0);
      } else if (behaviour === 'interrupted') {
        setTimeout(() => {
          for (const l of onChangedListeners) {
            l({ id, state: { previous: 'in_progress', current: 'interrupted' }, error: { current: 'FILE_ACCESS_DENIED' } });
          }
        }, 0);
      }
      // 'silent': 故意什么事件都不发 —— 既没完成也没中断。
      return id;
    },
    async removeFile(): Promise<void> { /* no-op */ },
    async erase(): Promise<void> { /* no-op */ },
  },
};

beforeEach(() => {
  behaviour = 'complete';
  nextId = 1;
  vi.stubGlobal('browser', fakeBrowser);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

// ---------------------------------------------------------------------------
// 任务 1 · 三态各一个测试
// ---------------------------------------------------------------------------

describe('C12 任务1 · 下载三态观测', () => {
  it('成功：complete 事件 ⇒ outcome=complete，无 error', async () => {
    const { writeCommitted } = await import('../lib/download');
    behaviour = 'complete';
    const seen: DownloadResult[] = [];
    await writeCommitted('c12-ok', '{"synthetic":true}', { timeoutMs: 200, onOutcome: (r) => { seen.push(r); } });
    expect(seen.length).toBeGreaterThanOrEqual(1);
    expect(seen.every((r) => r.outcome === 'complete')).toBe(true);
    expect(seen.every((r) => r.error === null)).toBe(true);
  });

  it('失败：interrupted 事件 ⇒ outcome=interrupted 且带上浏览器给的 error 原因', async () => {
    const { writeCommitted } = await import('../lib/download');
    behaviour = 'interrupted';
    const seen: DownloadResult[] = [];
    await expect(
      writeCommitted('c12-fail', '{"synthetic":true}', { timeoutMs: 200, onOutcome: (r) => { seen.push(r); } }),
    ).rejects.toThrow();
    expect(seen.map((r) => r.outcome)).toContain('interrupted');
    expect(seen.find((r) => r.outcome === 'interrupted')?.error).toBe('FILE_ACCESS_DENIED');
  });

  it('停滞：超时内既没 complete 也没 interrupted ⇒ outcome=stalled，且 error 必须是 null（不猜原因）', async () => {
    const { writeCommitted } = await import('../lib/download');
    behaviour = 'silent';
    const seen: DownloadResult[] = [];
    await expect(
      writeCommitted('c12-stall', '{"synthetic":true}', { timeoutMs: 60, onOutcome: (r) => { seen.push(r); } }),
    ).rejects.toThrow();
    expect(seen.map((r) => r.outcome)).toEqual(['stalled']);
    // 🔴 停滞 ≠ 失败：没有 error 原因可报，就必须是 null，不许编一个。
    expect(seen[0]!.error).toBeNull();
    expect(seen[0]!.waitedMs).toBe(60);
  });

  it('classifyDelta：只有真终态才判定，其余一律 null（= 还没到终态）', () => {
    expect(classifyDelta({ id: 1, state: { current: 'complete' } })).toEqual({ outcome: 'complete', error: null });
    expect(classifyDelta({ id: 1, state: { current: 'interrupted' }, error: { current: 'DISK_FULL' } }))
      .toEqual({ outcome: 'interrupted', error: 'DISK_FULL' });
    expect(classifyDelta({ id: 1, state: { current: 'in_progress' } })).toBeNull();
    expect(classifyDelta({ id: 1, filename: { current: 'x' } } as any)).toBeNull();
    expect(stalledResult(15_000)).toEqual({ outcome: 'stalled', error: null, waitedMs: 15_000 });
  });

  it('停滞与失败分开记账：interrupted 不计入停滞连击，也永远不熔断', () => {
    let s = initialGuardState();
    for (let i = 0; i < 10; i += 1) {
      s = recordOutcome(s, { outcome: 'interrupted', error: 'NETWORK_FAILED', waitedMs: 10 }, { now: 1000 + i });
    }
    expect(s.totalInterrupted).toBe(10);
    expect(s.consecutiveStalls).toBe(0);
    expect(s.totalStalls).toBe(0);
    expect(isGuardTripped(s)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 任务 2 · 熔断
// ---------------------------------------------------------------------------

describe('C12 任务2 · 熔断', () => {
  it('连续 N 次停滞 ⇒ 熔断；成功一次即清零连击', () => {
    let s = initialGuardState();
    for (let i = 1; i < DEFAULT_STALL_THRESHOLD; i += 1) {
      s = recordOutcome(s, stalledResult(15_000), { now: 100 + i });
      expect(isGuardTripped(s)).toBe(false);
    }
    s = recordOutcome(s, stalledResult(15_000), { now: 999 });
    expect(isGuardTripped(s)).toBe(true);
    expect(s.consecutiveStalls).toBe(DEFAULT_STALL_THRESHOLD);
    expect(s.trippedAt).toBe(999);

    // 未熔断时，一次成功把连击清零
    let t = initialGuardState();
    t = recordOutcome(t, stalledResult(15_000), { now: 1 });
    t = recordOutcome(t, { outcome: 'complete', error: null, waitedMs: 5 }, { now: 2 });
    expect(t.consecutiveStalls).toBe(0);
  });

  it('熔断不自愈：已熔断后即便再来一次 complete，也必须等显式恢复', () => {
    let s = initialGuardState();
    for (let i = 0; i < DEFAULT_STALL_THRESHOLD; i += 1) s = recordOutcome(s, stalledResult(15_000), { now: i });
    expect(isGuardTripped(s)).toBe(true);
    s = recordOutcome(s, { outcome: 'complete', error: null, waitedMs: 5 }, { now: 50 });
    expect(isGuardTripped(s)).toBe(true);
  });

  it('熔断态跨重启持久化，并暂停回溯腿；恢复后从欠账继续、不重头来', async () => {
    const store = memoryStore();
    for (let i = 0; i < DEFAULT_STALL_THRESHOLD; i += 1) {
      await recordDownloadOutcome(store, stalledResult(15_000), { now: 1000 + i });
    }
    expect((store.data as any)[GUARD_KEY]).toBeTruthy();

    // 「浏览器重启」= 拿一个只共享底层数据的新 store 重新读
    const rebooted = memoryStore(store.data);
    const loaded = await loadGuardState(rebooted);
    expect(isGuardTripped(loaded)).toBe(true);

    // 欠账集合先播种：3 条待办，1 条已归档
    const seeded = initialState('chatgpt', 'acct-synthetic');
    seeded.pending = ['conv-0001-aaaaaaaa', 'conv-0002-aaaaaaaa', 'conv-0003-aaaaaaaa'];
    seeded.archived = ['conv-0000-aaaaaaaa'];
    seeded.enumCursor = { offset: 4, complete: true };
    await rebooted.save(stateKey('chatgpt', 'acct-synthetic'), seeded);

    const clock: Clock = { now: () => 1_700_000_000_000, async sleep() { /* 不真的等 */ } };
    const guardedRun = await runBackfill({
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope: 'acct-synthetic',
      store: rebooted,
      clock,
      downloadGuard: async () => isGuardTripped(await loadGuardState(rebooted)),
      // http 端口故意不注入 ⇒ 一旦真的往前跑就会抛错，跑不到这里才算暂停成功
    });
    expect(guardedRun.stopped).toBe('download-paused');
    expect(guardedRun.archivedThisRun).toEqual([]);
    // 🔴 数据没丢：欠账原封不动
    expect(guardedRun.state.pending).toHaveLength(3);

    // 恢复 ⇒ 从欠账继续，不重头来
    const resumed = await resumeAfterGuard(rebooted);
    expect(isGuardTripped(resumed)).toBe(false);
    expect(resumed.consecutiveStalls).toBe(0);

    const fetched: string[] = [];
    const after = await runBackfill({
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope: 'acct-synthetic',
      store: rebooted,
      clock,
      downloadGuard: async () => isGuardTripped(await loadGuardState(rebooted)),
      http: async (url: string) => {
        const id = url.split('/').pop()!;
        fetched.push(id);
        return {
          status: 200,
          text: JSON.stringify({
            title: 'synthetic-fixture',
            current_node: `${id}-node`,
            mapping: { [`${id}-node`]: { id: `${id}-node`, parent: null, children: [] } },
          }),
        };
      },
    });
    expect(after.stopped).toBe('queue-empty');
    // 只取了 3 条欠账，那条已归档的没有被重抓 —— 「不重头来」的直接证据
    expect(fetched).toEqual(['conv-0001-aaaaaaaa', 'conv-0002-aaaaaaaa', 'conv-0003-aaaaaaaa']);
    expect(after.state.archived).toHaveLength(4);
  });
});

// ---------------------------------------------------------------------------
// 任务 3 · 文案
// ---------------------------------------------------------------------------

describe('C12 任务3 · 告警文案', () => {
  function trippedState() {
    let s = initialGuardState();
    for (let i = 0; i < DEFAULT_STALL_THRESHOLD; i += 1) s = recordOutcome(s, stalledResult(15_000), { now: i });
    return recordOutcome(s, { outcome: 'interrupted', error: 'X', waitedMs: 1 }, { now: 99 });
  }

  it('角标只在熔断时出现，且文案不猜原因', () => {
    expect(guardBadge(initialGuardState())).toBeNull();
    const badge = guardBadge(trippedState())!;
    expect(badge.text.length).toBeLessThanOrEqual(4);
    expect(badge.title).toContain('没有在');
    expect(badge.title).toContain('已暂停自动回溯');
  });

  it('详情文案：只陈述观测事实，无假百分比、无假成功数、不指认原因', () => {
    const text = guardAlertDetail(trippedState(), { archived: 7, pending: 12 });
    // 事实
    expect(text).toContain(`最近连续 ${DEFAULT_STALL_THRESHOLD} 次`);
    expect(text).toContain('15 秒');
    expect(text).toContain('欠账 12 条');
    expect(text).toContain('已归档 7 条');
    // 🔴 不许出现百分比（沿用 progress.ts 的诚实原则）
    expect(text).not.toContain('%');
    // 🔴 不许替用户断言原因
    for (const forbidden of ['因为', '你开了', '由于你', '一定是', '原因是']) {
      expect(text).not.toContain(forbidden);
    }
    // 可能的方向必须是「可能 / 请检查」的措辞
    expect(text).toContain('请检查');
    expect(text).toContain('无法判断');
    // 让用户知道数据没丢
    expect(text).toContain('没有丢');
    // 停滞与失败分开陈述
    expect(text).toContain('另有 1 次写入是明确失败的');
  });
});
