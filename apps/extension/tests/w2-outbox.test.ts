/**
 * W2 · 发件箱：write-ahead、只在 ack 时删除、退避、容量、导出。
 *
 * 规范 §10 的扩展侧义务逐条钉在这里：
 *   · 实时捕获先落发件箱再做任何投递尝试（write-ahead），**只在匹配 ack 时删除**；
 *   · 发件箱【绝不】为了腾地方丢条目 —— 满了就拒收新的那一条并说出来；
 *   · 非 retryable 的 nack ⇒ 进 `rejected`（保留、可见、进导出），不再自动重试；
 *   · 排空是串行的，同一时刻只有一个在跑。
 *
 * 「SW 被杀」用 **新建一个模块实例读同一个 IndexedDB** 来模拟：
 * `vi.resetModules()` 之后重新 import，与 MV3 里 worker 被回收再唤醒是同一件事。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import type { DeliverResult } from '../lib/native-host';

const DELIVERED: DeliverResult = {
  delivered: true, status: 'stored', shard: '0001-x.json', requestId: 'r', sha256: 's',
};
const RETRYABLE: DeliverResult = {
  delivered: false, reason: 'send-failed', retryable: true, requestId: 'r', sha256: 's',
};
const REJECTED: DeliverResult = {
  delivered: false, reason: 'nack', kind: 'invalid-bundle', retryable: false,
  detail: 'payload is not an inbox bundle', requestId: 'r', sha256: 's',
};

/** 每个用例都从一个全新的、空的 IndexedDB 开始。 */
function freshIdb(): void {
  (globalThis as any).indexedDB = new IDBFactory();
}

async function outbox() {
  return await import('../lib/outbox');
}

const NAME_A = 'chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json';
const PAYLOAD_A = '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}';
const NAME_B = 'chatgpt-bbbbbbbb-1111-2222-3333-444444444444.json';
const PAYLOAD_B = '{"sessionId":"bbbbbbbb-1111-2222-3333-444444444444"}';

beforeEach(() => {
  vi.resetModules();
  freshIdb();
});

// ===========================================================================
// 1 · write-ahead 与「只在 ack 时删除」
// ===========================================================================
describe('W2-OUTBOX · write-ahead 与只在 ack 时删除', () => {
  it('入队之后条目就在盘上：主键是 payload 字节的 sha256', async () => {
    const ob = await outbox();
    const res = await ob.enqueue(NAME_A, PAYLOAD_A);
    expect(res.accepted).toBe(true);
    expect(res.duplicate).toBeUndefined();

    const entries = await ob.listEntries();
    expect(entries).toHaveLength(1);
    expect(entries![0]).toMatchObject({
      sha256: res.sha256,
      name: NAME_A,
      payload: PAYLOAD_A,
      bytes: PAYLOAD_A.length,
      attempts: 0,
      lastError: null,
      lastAttemptAt: null,
      state: 'pending',
    });
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(PAYLOAD_A));
    const expected = [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
    expect(res.sha256).toBe(expected);
  });

  it('🔴 匹配的 ack ⇒ 条目被删掉，发件箱空了', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A);
    const report = await ob.drainOutbox({ deliver: async () => DELIVERED });

    expect(report).toMatchObject({ attempted: 1, delivered: 1, rejected: 0, stoppedBy: 'drained' });
    expect(await ob.listEntries()).toEqual([]);
    expect(await ob.summary()).toMatchObject({ pending: 0, rejected: 0, bytes: 0 });
  });

  it('🔴 超时 / lastError（retryable）⇒ 条目【还在】，只多了 attempts 与 lastError', async () => {
    const ob = await outbox();
    const { sha256 } = await ob.enqueue(NAME_A, PAYLOAD_A);
    const report = await ob.drainOutbox({ deliver: async () => RETRYABLE, now: () => 1_000 });

    expect(report).toMatchObject({ attempted: 1, delivered: 0, rejected: 0, stoppedBy: 'host-unavailable' });
    const entry = (await ob.getEntry(sha256!)).ok ? (await ob.getEntry(sha256!)) as any : null;
    expect(entry.entry).toMatchObject({
      state: 'pending',
      attempts: 1,
      lastError: 'send-failed',
      lastAttemptAt: 1_000,
    });
    expect(await ob.listEntries()).toHaveLength(1);   // 一个字节都没少
  });

  it('🔴 响应不合规范（malformed-response，retryable）⇒ 同上，条目还在', async () => {
    const ob = await outbox();
    const { sha256 } = await ob.enqueue(NAME_A, PAYLOAD_A);
    await ob.drainOutbox({
      deliver: async () => ({
        delivered: false, reason: 'malformed-response', detail: 'ack sha256 does not match',
        retryable: true, requestId: 'r', sha256: 's',
      }),
      now: () => 5_000,
    });
    const lookup = await ob.getEntry(sha256!);
    expect(lookup).toMatchObject({ ok: true, entry: { state: 'pending', attempts: 1 } });
  });
});

// ===========================================================================
// 2 · 去重（sha256 就是主键）
// ===========================================================================
describe('W2-OUTBOX · 按内容去重', () => {
  it('同一个 payload 入队两次 ⇒ 只有一条，第二次报 duplicate', async () => {
    const ob = await outbox();
    const first = await ob.enqueue(NAME_A, PAYLOAD_A);
    const second = await ob.enqueue(NAME_A, PAYLOAD_A);
    expect(second).toMatchObject({ accepted: true, duplicate: true, sha256: first.sha256 });
    expect(await ob.listEntries()).toHaveLength(1);
    expect((await ob.summary())!.bytes).toBe(PAYLOAD_A.length);
  });

  it('内容不同但名字相同 ⇒ 是两条（内容寻址，不是按名字去重）', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A);
    await ob.enqueue(NAME_A, `${PAYLOAD_A} `);
    expect(await ob.listEntries()).toHaveLength(2);
  });
});

// ===========================================================================
// 3 · 容量
// ===========================================================================
describe('W2-OUTBOX · 容量满了拒收新的，绝不动旧的', () => {
  it('🔴 满 ⇒ 新条目被拒且原因可见；已有条目一条不少、一个字节不改', async () => {
    const ob = await outbox();
    const capacityBytes = PAYLOAD_A.length + PAYLOAD_B.length;
    await ob.enqueue(NAME_A, PAYLOAD_A, { capacityBytes });
    await ob.enqueue(NAME_B, PAYLOAD_B, { capacityBytes });
    const before = await ob.listEntries();
    expect(before).toHaveLength(2);
    expect((await ob.summary({ capacityBytes }))).toMatchObject({
      pending: 2, bytes: capacityBytes, capacityBytes, full: true,
    });

    const third = await ob.enqueue('chatgpt-c.json', '{"sessionId":"cccccccc-1111-2222-3333-444444444444"}', {
      capacityBytes,
    });
    expect(third.accepted).toBe(false);
    expect(third.reason).toBe('outbox-full');
    // 拒绝时也要说得出「现在到底堆了多少」——数字来自真实的库存，不是猜的。
    expect(third.summary).toMatchObject({ pending: 2, bytes: capacityBytes, full: true });

    const after = await ob.listEntries();
    expect(after).toEqual(before);          // 🔴 一条不少、一个字节不改
    expect((await ob.summary({ capacityBytes }))!.full).toBe(true);
  });

  it('未满时仍然收（这条只是证明上面那条不是因为别的原因红的）', async () => {
    const ob = await outbox();
    const res = await ob.enqueue(NAME_A, PAYLOAD_A, { capacityBytes: 10_000 });
    expect(res.accepted).toBe(true);
  });
});

// ===========================================================================
// 4 · 退避
// ===========================================================================
describe('W2-OUTBOX · retryable 失败后的指数退避', () => {
  it('🔴 失败后【立刻】不再重发；过了退避窗口才重试', async () => {
    const ob = await outbox();
    const attempts: number[] = [];
    let now = 1_000_000;
    const deliver = async (): Promise<DeliverResult> => {
      attempts.push(now);
      return RETRYABLE;
    };

    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => now });
    await ob.drainOutbox({ deliver, now: () => now });
    expect(attempts).toHaveLength(1);

    // 退避还没到 ⇒ 一次都不许发（这也是"不许打主机"的那条）。
    now += ob.RETRY_BASE_MS - 1;
    const waiting = await ob.drainOutbox({ deliver, now: () => now });
    expect(attempts).toHaveLength(1);
    expect(waiting).toMatchObject({ attempted: 0, waiting: 1 });

    // 窗口到了 ⇒ 重试一次。
    now += 1;
    await ob.drainOutbox({ deliver, now: () => now });
    expect(attempts).toHaveLength(2);
    expect(attempts[1]! - attempts[0]!).toBe(ob.RETRY_BASE_MS);

    // 第二次失败 ⇒ 退避翻倍：再等 60 秒还不够，要等满 120 秒。
    const entries = await ob.listEntries();
    expect(entries![0]!.attempts).toBe(2);
    expect(ob.backoffMs(2)).toBe(2 * ob.RETRY_BASE_MS);
    now += ob.RETRY_BASE_MS;                 // 距上次尝试 60 秒
    expect((await ob.drainOutbox({ deliver, now: () => now })).attempted).toBe(0);
    now += ob.RETRY_BASE_MS;                 // 距上次尝试 120 秒
    await ob.drainOutbox({ deliver, now: () => now });
    expect(attempts).toHaveLength(3);
    expect(attempts[2]! - attempts[1]!).toBe(2 * ob.RETRY_BASE_MS);
  });

  it('退避有上限（1 小时），不会无限翻倍', async () => {
    const ob = await outbox();
    console.log('[W2-OUTBOX] 退避序列(ms):',
      [1, 2, 3, 4, 5, 10, 50].map((n) => ob.backoffMs(n)));
    expect(ob.backoffMs(1)).toBe(60_000);
    expect(ob.backoffMs(2)).toBe(120_000);
    expect(ob.backoffMs(3)).toBe(240_000);
    expect(ob.backoffMs(50)).toBe(3_600_000);
    expect(ob.backoffMs(0)).toBe(0);
  });
});

// ===========================================================================
// 5 · 非 retryable ⇒ rejected，但保留，且出现在导出里
// ===========================================================================
describe('W2-OUTBOX · 非 retryable 的 nack ⇒ rejected', () => {
  it('🔴 条目被保留、标成 rejected、带上 kind，而且【不再】自动重试', async () => {
    const ob = await outbox();
    const { sha256 } = await ob.enqueue(NAME_A, PAYLOAD_A);
    const report = await ob.drainOutbox({ deliver: async () => REJECTED, now: () => 9_000 });

    expect(report).toMatchObject({ attempted: 1, delivered: 0, rejected: 1, stoppedBy: 'drained' });
    const lookup = await ob.getEntry(sha256!);
    expect(lookup).toMatchObject({
      ok: true,
      entry: {
        state: 'rejected',
        rejectKind: 'invalid-bundle',
        lastError: 'nack:invalid-bundle',
        attempts: 1,
      },
    });

    // 再怎么排空也不会去碰它。
    let calls = 0;
    await ob.drainOutbox({ deliver: async () => { calls += 1; return DELIVERED; }, now: () => 10_000_000 });
    expect(calls).toBe(0);

    // 但它仍然在「未送达」里，所以导出里有它。
    const undelivered = await ob.undeliveredEntries();
    expect(undelivered).toHaveLength(1);
    expect(undelivered![0]!.payload).toBe(PAYLOAD_A);
  });

  it('一条被判死不影响下一条：同一个排空里继续送后面的', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    const seen: string[] = [];
    const report = await ob.drainOutbox({
      deliver: async (name) => {
        seen.push(name);
        return name === NAME_A ? REJECTED : DELIVERED;
      },
    });
    expect(seen).toEqual([NAME_A, NAME_B]);
    expect(report).toMatchObject({ attempted: 2, delivered: 1, rejected: 1 });
    const left = await ob.listEntries();
    expect(left!.map((e) => e.name)).toEqual([NAME_A]);
    expect(left![0]!.state).toBe('rejected');
  });
});

// ===========================================================================
// 6 · write-ahead 真的扛得住 SW 被杀
// ===========================================================================
describe('W2-OUTBOX · 送达之前 SW 被杀', () => {
  it('🔴 新模块实例（同一个库）里条目还在，下一次排空把它送出去', async () => {
    const first = await outbox();
    const { sha256 } = await first.enqueue(NAME_A, PAYLOAD_A);
    expect(sha256).toBeTruthy();
    // 🔴 这里【没有】排空 —— 模拟「写完了，还没送出去就被回收」。

    // SW 被回收再唤醒：模块内存全没了，IndexedDB 留着。
    vi.resetModules();
    const second = await outbox();
    const entries = await second.listEntries();
    expect(entries).toHaveLength(1);
    expect(entries![0]!.payload).toBe(PAYLOAD_A);

    const sent: string[] = [];
    const report = await second.drainOutbox({
      deliver: async (name, payload) => { sent.push(payload); return DELIVERED; },
    });
    expect(report.delivered).toBe(1);
    expect(sent).toEqual([PAYLOAD_A]);
    expect(await second.listEntries()).toEqual([]);   // ack 之后才删
  });

  it('🔴 排空跑到一半被杀（第一条已 ack）⇒ 只有那一条消失，其余的仍在', async () => {
    const first = await outbox();
    // 入队时刻分开写：FIFO 的顺序因此是确定的（同一毫秒会按 sha 兜底排序）。
    await first.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await first.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    const order: string[] = [];
    await first.drainOutbox({
      deliver: async (name) => {
        order.push(name);
        // A 成功（条目被删），B 遇到主机不在（条目留下、这一轮就此打住）。
        return name === NAME_A ? DELIVERED : RETRYABLE;
      },
      now: () => 3_000,
    });
    expect(order).toEqual([NAME_A, NAME_B]);

    vi.resetModules();
    const second = await outbox();
    const left = await second.listEntries();
    expect(left!.map((e) => e.name)).toEqual([NAME_B]);
    expect(left![0]!.state).toBe('pending');
  });
});

// ===========================================================================
// 7 · 串行与互斥
// ===========================================================================
describe('W2-OUTBOX · 排空是串行且互斥的', () => {
  it('🔴 两条并发触发只有一条排空在跑，不会重复送', async () => {
    const ob = await outbox();
    // 入队时刻分开写：FIFO 顺序因此是确定的（同一毫秒会按 sha 兜底排序）。
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });

    const calls: string[] = [];
    let inFlightNow = 0;
    let maxConcurrent = 0;
    const deliver = async (name: string): Promise<DeliverResult> => {
      calls.push(name);
      inFlightNow += 1;
      maxConcurrent = Math.max(maxConcurrent, inFlightNow);
      await new Promise((resolve) => setTimeout(resolve, 1));
      inFlightNow -= 1;
      return DELIVERED;
    };

    // 同一拍里触发两次（一次捕获 + 一次闹钟的模型）。
    const [a, b] = await Promise.all([
      ob.drainOutbox({ deliver }),
      ob.drainOutbox({ deliver }),
    ]);

    expect(calls).toEqual([NAME_A, NAME_B]);   // 每条只送一次
    expect(maxConcurrent).toBe(1);             // 没有两条同时在飞
    expect(a).toBe(b);                         // 第二次触发共享同一次排空的结果
    expect(a).toMatchObject({ attempted: 2, delivered: 2 });
    expect(await ob.listEntries()).toEqual([]);
  });

  it('maxPerRun 到了就收手，剩下的留到下一次（并且仍然是 pending）', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    const report = await ob.drainOutbox({ deliver: async () => DELIVERED, maxPerRun: 1 });
    expect(report).toMatchObject({ attempted: 1, delivered: 1, stoppedBy: 'batch' });
    expect((await ob.listEntries())!.map((e) => e.name)).toEqual([NAME_B]);
  });

  it('FIFO：先入队的先送', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    const order: string[] = [];
    await ob.drainOutbox({ deliver: async (name) => { order.push(name); return DELIVERED; } });
    expect(order).toEqual([NAME_A, NAME_B]);
  });
});

// ===========================================================================
// 8 · 读不出来 ≠ 空
// ===========================================================================
describe('W2-OUTBOX · IndexedDB 读不出来时必须说"不知道"', () => {
  beforeEach(() => {
    // 这个上下文里没有 IndexedDB。
    delete (globalThis as any).indexedDB;
  });

  it('🔴 summary() 返回 null（不是 0），listEntries() 返回 null（不是 []）', async () => {
    const ob = await outbox();
    expect(await ob.summary()).toBeNull();
    expect(await ob.listEntries()).toBeNull();
    expect(await ob.undeliveredEntries()).toBeNull();
  });

  it('🔴 入队被拒收且具名（绝不静默"存下了"）', async () => {
    const ob = await outbox();
    const res = await ob.enqueue(NAME_A, PAYLOAD_A);
    expect(res).toMatchObject({ accepted: false, reason: 'outbox-unavailable' });
  });

  it('🔴 getEntry 说"读不出来"，绝不冒充"这一条不在"', async () => {
    const ob = await outbox();
    expect(await ob.getEntry('whatever')).toEqual({ ok: false, reason: 'outbox-unavailable' });
  });

  it('排空如实报 outbox-unavailable，且一次都没调用 deliver', async () => {
    const ob = await outbox();
    let calls = 0;
    const report = await ob.drainOutbox({ deliver: async () => { calls += 1; return DELIVERED; } });
    expect(report.stoppedBy).toBe('outbox-unavailable');
    expect(calls).toBe(0);
  });
});

// ===========================================================================
// 9 · 导出（§8）
// ===========================================================================
describe('W2-OUTBOX · 导出文件', () => {
  it('🔴 内容逐行等于各 payload（字节级），每行一个 \\n，顺序 = 入队顺序', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    const entries = (await ob.listEntries())!;

    const at = Date.parse('2026-09-12T21:47:03.123Z');
    const file = ob.buildExportFile(entries, at);

    expect(file.content).toBe(`${PAYLOAD_A}\n${PAYLOAD_B}\n`);
    // 字节级：文件字节 = 各 payload 的 UTF-8 字节 + 每行一个 \n。
    const bytes = new TextEncoder().encode(file.content).byteLength;
    expect(file.bytes).toBe(bytes);
    expect(bytes).toBe(
      new TextEncoder().encode(PAYLOAD_A).byteLength + 1
      + new TextEncoder().encode(PAYLOAD_B).byteLength + 1,
    );
    // 逐行切回来必须与 payload 逐字相同（没有转义、没有加引号、没有截断）。
    const lines = file.content.slice(0, -1).split('\n');
    expect(lines).toEqual([PAYLOAD_A, PAYLOAD_B]);
    expect(file.entries).toBe(2);
  });

  it('🔴 文件名严格是 chat-stasher-export-<UTC yyyymmddThhmmssZ>.jsonl', async () => {
    const ob = await outbox();
    expect(ob.exportFilename(Date.parse('2026-09-12T21:47:03.123Z')))
      .toBe('chat-stasher-export-20260912T214703Z.jsonl');
    // 零点/单位数月日也要补零，且是 UTC 不是本地时间。
    expect(ob.exportFilename(Date.parse('2026-01-02T03:04:05.000Z')))
      .toBe('chat-stasher-export-20260102T030405Z.jsonl');
  });

  it('被拒的条目也在导出里（§10：它被保留并进导出）', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    await ob.drainOutbox({
      deliver: async (name) => (name === NAME_A ? REJECTED : RETRYABLE),
    });

    const file = ob.buildExportFile((await ob.undeliveredEntries())!, Date.now());
    expect(file.content).toBe(`${PAYLOAD_A}\n${PAYLOAD_B}\n`);
    expect(file.entries).toBe(2);
  });

  it('导出【不删除】任何条目（主机恢复后会以 duplicate 确认它们）', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A);
    ob.buildExportFile((await ob.undeliveredEntries())!, Date.now());
    expect(await ob.listEntries()).toHaveLength(1);
  });

  it('空发件箱 ⇒ 空内容、0 条（导出方自己决定要不要生成文件）', async () => {
    const ob = await outbox();
    const file = ob.buildExportFile([], Date.now());
    expect(file).toMatchObject({ content: '', entries: 0, bytes: 0 });
  });

  it('最近一次导出的记录能写能读（弹窗显示的就是它）', async () => {
    const ob = await outbox();
    const { memoryStore } = await import('../lib/backfill/store');
    const store = memoryStore();
    expect(await ob.loadLastExport(store)).toBeNull();      // 没导出过 ⇒ null，不编一条
    await ob.recordExport(store, {
      at: 1_700_000_000_000, entries: 3, bytes: 300, filename: 'chat-stasher-export-x.jsonl',
    });
    expect(await ob.loadLastExport(store)).toMatchObject({ entries: 3, bytes: 300 });
  });
});
