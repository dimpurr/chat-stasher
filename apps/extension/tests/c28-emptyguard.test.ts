/**
 * C28 · 正文接口返回成功但内容为空时，不能把空当成「没有东西」。
 *
 * 全部是合成夹具：只给 engine 注入一个测试 plan 来演示未来 detail parser 的
 * 接缝，不改变生产 plan，不给 DeepSeek 增加正文路径，也不向任何平台发请求。
 */

import { describe, expect, it } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { CHATGPT_LIST_PATH, CHATGPT_PLAN, type BackfillEnumPlan } from '../lib/backfill/enumerate';
import { memoryStore } from '../lib/backfill/store';
import { stateKey, type BackfillState } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const ID = 'conv-c28-empty-aaaaaaaa';

function fakeClock(): Clock {
  let now = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => now, async sleep(ms: number) { now += ms; } };
}

function listBody(): string {
  return JSON.stringify({ items: [{ id: ID }], total: 1 });
}

/** 结构是好的；C28 测的是「结构好但内容空」，不是 shape-changed。 */
function detailBody(): string {
  return JSON.stringify({ mapping: {}, current_node: 'synthetic-node' });
}

function planFor(outcome: 'detail-empty-unverified' | 'detail-empty-confirmed'): BackfillEnumPlan {
  return {
    ...CHATGPT_PLAN,
    parseDetailPage: () => ({ ok: true, outcome }),
  };
}

function backend(): (url: string) => Promise<HttpResponse> {
  return async (url: string) => new URL(url).pathname === CHATGPT_LIST_PATH
    ? { status: 200, text: listBody() }
    : { status: 200, text: detailBody() };
}

async function run(
  outcome: 'detail-empty-unverified' | 'detail-empty-confirmed',
  scope: string,
) {
  const store = memoryStore();
  const report = await runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope,
    store,
    http: backend(),
    clock: fakeClock(),
    pace: {
      enumerate: { minIntervalMs: 0, maxPerDay: null },
      detail: { minIntervalMs: 0, maxPerDay: null },
    },
    plans: (platform) => platform === 'chatgpt' ? planFor(outcome) : null,
    sink: () => ({ saved: true, sessionId: ID }),
  });
  const persisted = await store.load(stateKey('chatgpt', scope)) as BackfillState;
  return { report, persisted };
}

describe('C28 · 正文空护栏', () => {
  it('HTTP 200 + 形状正确 + 内容为空 ⇒ 具名停机，欠账原封不动且正文 complete=false', async () => {
    const { report, persisted } = await run('detail-empty-unverified', 'acct-c28-unverified');

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('detail-empty-unverified');
    expect(report.halted?.reason).not.toBe('shape-changed');
    expect(report.state.pending).toEqual([ID]);
    expect(report.state.archived).toEqual([]);
    expect(report.archivedThisRun).toEqual([]);
    expect(report.detailOutcomes).toEqual([
      { sessionId: ID, outcome: 'detail-empty-unverified', complete: false, at: expect.any(Number) },
    ]);
    expect(report.state.detailOutcomes).toEqual(report.detailOutcomes);
    expect(persisted.pending).toEqual([ID]);
    expect(persisted.archived).toEqual([]);
    expect(persisted.detailOutcomes).toEqual(report.detailOutcomes);
  });

  it('「本来就是合法空会话」是另一个具名值，且与未证实空在报告/落盘状态中可分辨', async () => {
    const { report, persisted } = await run('detail-empty-confirmed', 'acct-c28-confirmed');

    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.detailOutcomes).toEqual([
      { sessionId: ID, outcome: 'detail-empty-confirmed', complete: true, at: expect.any(Number) },
    ]);
    expect(report.detailOutcomes[0]?.outcome).not.toBe('detail-empty-unverified');
    expect(report.state.detailOutcomes).toEqual(report.detailOutcomes);
    expect(persisted.detailOutcomes).toEqual(report.detailOutcomes);
    expect(report.state.pending).toEqual([]);
    expect(report.state.archived).toEqual([ID]);
  });
});
