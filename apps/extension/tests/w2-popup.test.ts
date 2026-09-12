/**
 * W2 · 弹窗新说的那四件事：通道状态、发件箱、回溯暂停、导出。
 *
 * 🔴 每一条都只陈述**已经发生过的事实**（含时间戳），没有一条是"大概是好的"：
 *    · 通道：上一次 `hello` 问到了什么（stage / machine / 版本），或者具名原因 + 修复命令；
 *    · 发件箱：待送几条、被拒几条（列出 kind 与 detail 摘要）、容量用了多少；
 *    · 暂停：回溯因为主机够不着停在哪里、欠账没丢；
 *    · 导出：按钮的措辞与可见性，以及最近一次导出的时间。
 *
 * 这些行全部是**英文**（lib/ui-strings.ts），且与旧的中文文案共存 —— 旧文案
 * 属于另一个 i18n 任务，这个文件不许去动它们。
 */

import { describe, it, expect } from 'vitest';
import {
  channelLine,
  exportLine,
  outboxLine,
  pauseLine,
  popupText,
  renderPopup,
  summarizeOutbox,
  MAX_REJECTED_SAMPLES,
  NO_FAILURES,
  type PopupModel,
} from '../lib/popup-view';
import { OUTBOX_CAPACITY_BYTES, type OutboxEntry } from '../lib/outbox';
import * as ui from '../lib/ui-strings';

const AT = Date.parse('2026-09-12T21:47:03.000Z');

function model(overrides: Partial<PopupModel> = {}): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    ...overrides,
  };
}

function entry(overrides: Partial<OutboxEntry> = {}): OutboxEntry {
  return {
    sha256: 'a'.repeat(64),
    name: 'chatgpt-a.json',
    payload: '{"x":1}',
    bytes: 7,
    enqueuedAt: 1,
    attempts: 0,
    lastError: null,
    lastAttemptAt: null,
    state: 'pending',
    ...overrides,
  };
}

// ===========================================================================
// 通道状态
// ===========================================================================
describe('W2-POPUP · 落盘通道', () => {
  it('连上了 ⇒ 说清 stage / machine / 主机版本 + 什么时候问的', () => {
    const line = channelLine(model({
      nativeHost: { at: AT, ok: true, stage: '/Users/me/stage', machine: 'mac-1', hostVersion: '0.3.0' },
    }));
    console.log('[W2-POPUP] 已连接:', line);
    expect(line).toContain('connected');
    expect(line).toContain('/Users/me/stage');
    expect(line).toContain('mac-1');
    expect(line).toContain('0.3.0');
    expect(line).toContain(ui.stamp(AT));
    // 健康路径守卫：连上了就不该出现修复命令。
    expect(line).not.toContain('install-native-host');
  });

  it('🔴 没连上 ⇒ 具名原因 + 修复命令（用上一次知道的 stage）', () => {
    const line = channelLine(model({
      nativeHost: {
        at: AT, ok: false, reason: 'send-failed',
        detail: 'Specified native messaging host not found.',
        lastKnownStage: '/Users/me/stage',
      },
    }));
    console.log('[W2-POPUP] 未连接:', line);
    expect(line).toContain('NOT connected');
    expect(line).toContain('send-failed');
    expect(line).toContain('chat-stasher install-native-host --stage /Users/me/stage');
    expect(line).toContain('Last known stage');
  });

  it('从来没问过 ⇒ 照实说没问过（不猜一个"大概是好的"）', () => {
    const line = channelLine(model({ nativeHost: null }));
    console.log('[W2-POPUP] 没问过:', line);
    expect(line).toBe(ui.CHANNEL_NO_CHECK);
    expect(line).toContain('no host check has run yet');
    // 连"未连接"都不许说 —— 我们并不知道它连没连上。
    expect(line).not.toContain('NOT connected');
  });

  it('没连过、也没有 stage 记录 ⇒ 修复命令里放占位符，不编一个路径', () => {
    const line = channelLine(model({ nativeHost: { at: AT, ok: false, reason: 'timeout' } }));
    expect(line).toContain('chat-stasher install-native-host --stage <path-to-your-stage-dir>');
  });
});

// ===========================================================================
// 发件箱
// ===========================================================================
describe('W2-POPUP · 发件箱', () => {
  it('空且未满 ⇒ 不出这一行（那时候"没有待送"本身就是全部事实）', () => {
    expect(outboxLine(model({ outbox: { pending: 0, rejected: 0, bytes: 0, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] } }))).toBeNull();
    expect(outboxLine(model())).toBeNull();
  });

  it('待送 N ⇒ 说清待送数、被拒数与容量占用', () => {
    const line = outboxLine(model({
      outbox: {
        pending: 3, rejected: 0, bytes: 1024 * 1024, capacityBytes: OUTBOX_CAPACITY_BYTES,
        full: false, rejectedKinds: [], rejectedSamples: [],
      },
    }))!;
    console.log('[W2-POPUP] 待送 3:', line);
    expect(line).toContain('3 waiting');
    expect(line).toContain('0 rejected');
    expect(line).toContain('1.0 MiB');
    expect(line).toContain('256.0 MiB');
  });

  it('🔴 有被拒 ⇒ 逐 kind 计数 + 逐条 detail 摘要', () => {
    const line = outboxLine(model({
      outbox: {
        pending: 1, rejected: 2, bytes: 10, capacityBytes: 100, full: false,
        rejectedKinds: [{ kind: 'invalid-bundle', count: 2 }],
        rejectedSamples: [
          { kind: 'invalid-bundle', detail: 'nack:invalid-bundle' },
          { kind: 'invalid-bundle', detail: 'nack:invalid-bundle' },
        ],
      },
    }))!;
    console.log('[W2-POPUP] 有被拒:', line);
    expect(line).toContain('2 rejected');
    expect(line).toContain('invalid-bundle × 2');
    expect(line).toContain('never retried');
    expect(line).toContain('nack:invalid-bundle');
  });

  it('🔴 满了 ⇒ 明说在拒收新的，并明说已排队的没被删', () => {
    const line = outboxLine(model({
      outbox: {
        pending: 5, rejected: 0, bytes: 100, capacityBytes: 100, full: true,
        rejectedKinds: [], rejectedSamples: [],
      },
    }))!;
    expect(line).toContain('FULL');
    expect(line).toContain('Nothing queued was deleted');
  });

  it('🔴 读不出来 ⇒ 说读不出来，绝不当成空', () => {
    const line = outboxLine(model({ outbox: null }))!;
    console.log('[W2-POPUP] 读不出来:', line);
    expect(line).toContain('unreadable');
    expect(line).toContain('cannot say what is queued');
  });

  it('summarizeOutbox 数得对，并且样本有条数上限', () => {
    const entries = [
      entry({ sha256: '1'.repeat(64), state: 'pending', bytes: 10 }),
      entry({ sha256: '2'.repeat(64), state: 'rejected', bytes: 20, rejectKind: 'config', lastError: 'nack:config' }),
      entry({ sha256: '3'.repeat(64), state: 'rejected', bytes: 30, rejectKind: 'config', lastError: 'nack:config' }),
      ...Array.from({ length: 10 }, (_, i) => entry({
        sha256: String(i).padStart(64, '7'), state: 'rejected' as const, bytes: 5,
        rejectKind: 'invalid-bundle', lastError: 'nack:invalid-bundle',
      })),
    ];
    const out = summarizeOutbox(entries, 1000);
    console.log('[W2-POPUP] 汇总:', { ...out, rejectedSamples: out.rejectedSamples.length });
    expect(out.pending).toBe(1);
    expect(out.rejected).toBe(12);
    expect(out.bytes).toBe(10 + 20 + 30 + 50);
    expect(out.rejectedKinds).toEqual([
      { kind: 'invalid-bundle', count: 10 },
      { kind: 'config', count: 2 },
    ]);
    expect(out.rejectedSamples).toHaveLength(MAX_REJECTED_SAMPLES);
    expect(out.full).toBe(false);
  });

  it('🔴 摘要里不放 payload / URL / 正文', () => {
    const out = summarizeOutbox([
      entry({
        state: 'rejected', rejectKind: 'invalid-bundle', lastError: 'nack:invalid-bundle',
        payload: '{"secret":"synthetic conversation body"}',
      }),
    ]);
    const blob = JSON.stringify(out);
    expect(blob).not.toContain('synthetic conversation body');
    expect(blob).not.toContain('secret');
  });
});

// ===========================================================================
// 回溯暂停
// ===========================================================================
describe('W2-POPUP · 回溯暂停', () => {
  it('暂停中 ⇒ 说清原因、什么时候发现的、欠账没动', () => {
    const line = pauseLine(model({
      block: 'host-paused',
      hostPause: { reason: 'host-unavailable', at: AT, detail: 'timeout' },
    }))!;
    console.log('[W2-POPUP] 暂停:', line);
    expect(line).toContain('PAUSED');
    expect(line).toContain('host-unavailable');
    expect(line).toContain(ui.stamp(AT));
    expect(line).toContain('untouched');
  });

  it('没暂停 ⇒ 这一行不出现', () => {
    expect(pauseLine(model({ hostPause: null }))).toBeNull();
    expect(pauseLine(model())).toBeNull();
  });
});

// ===========================================================================
// 导出
// ===========================================================================
describe('W2-POPUP · 导出', () => {
  it('有未送达的 ⇒ 按钮出现；全都送出去了 ⇒ 按钮不出现', () => {
    const withPending = renderPopup(model({
      outbox: { pending: 1, rejected: 0, bytes: 1, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] },
    }));
    expect(withPending.exportFile.visible).toBe(true);
    expect(withPending.exportFile.label).toBe(ui.EXPORT_BUTTON_LABEL);

    const withRejected = renderPopup(model({
      outbox: { pending: 0, rejected: 1, bytes: 1, capacityBytes: 100, full: false, rejectedKinds: [{ kind: 'config', count: 1 }], rejectedSamples: [] },
    }));
    expect(withRejected.exportFile.visible).toBe(true);

    const empty = renderPopup(model({
      outbox: { pending: 0, rejected: 0, bytes: 0, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] },
    }));
    expect(empty.exportFile.visible).toBe(false);
  });

  it('🔴 最近一次导出的时间与文件名要看得见；没导出过就照实说没有', () => {
    expect(exportLine(model())).toBe(ui.EXPORT_NO_HISTORY);
    const line = exportLine(model({
      lastExport: { at: AT, entries: 4, bytes: 2048, filename: 'chat-stasher-export-20260912T214703Z.jsonl' },
    }));
    console.log('[W2-POPUP] 最近一次导出:', line);
    expect(line).toContain(ui.stamp(AT));
    expect(line).toContain('4 capture(s)');
    expect(line).toContain('chat-stasher-export-20260912T214703Z.jsonl');
    // 导出【不】删除条目 —— 这句话必须写在用户看得到的地方。
    expect(line).toContain('stay in the outbox');
  });

  it('按钮在拍平的文本里看得见（否则"它出没出现"断言不了）', () => {
    const out = popupText(renderPopup(model({
      outbox: { pending: 2, rejected: 0, bytes: 1, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] },
    })));
    expect(out).toContain(`[按钮] ${ui.EXPORT_BUTTON_LABEL}`);
    expect(out).toContain('2 waiting');
  });
});

// ===========================================================================
// 与 C18 的旧红线共存
// ===========================================================================
describe('W2-POPUP · 新文案不许踩 C18 的旧红线', () => {
  it('🔴 W2 新增的每一行都不含百分号（进度那行的规矩是全页的）', () => {
    const m = model({
      block: 'host-paused',
      hostPause: { reason: 'host-unavailable', at: AT },
      nativeHost: { at: AT, ok: false, reason: 'timeout', lastKnownStage: '/s' },
      outbox: {
        pending: 9, rejected: 3, bytes: 12345678, capacityBytes: OUTBOX_CAPACITY_BYTES, full: true,
        rejectedKinds: [{ kind: 'config', count: 3 }],
        rejectedSamples: [{ kind: 'config', detail: 'nack:config' }],
      },
      lastExport: { at: AT, entries: 12, bytes: 999, filename: 'f.jsonl' },
    });
    const lines = [channelLine(m), outboxLine(m), pauseLine(m), exportLine(m)].filter(Boolean) as string[];
    expect(lines).toHaveLength(4);
    for (const line of lines) expect([line, line.includes('%')]).toEqual([line, false]);
    // 顺带：整套 popupText 在"什么都有"的状态下也不含百分号。
    expect(popupText(renderPopup(m))).not.toContain('%');
  });

  it('🔴 新文案里不出现任何时间承诺（我们没有速率模型）', () => {
    const m = model({
      nativeHost: { at: AT, ok: false, reason: 'timeout' },
      outbox: { pending: 1, rejected: 0, bytes: 1, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] },
    });
    const lines = [channelLine(m), outboxLine(m), exportLine(m)].filter((l): l is string => l !== null);
    expect(lines).toHaveLength(3);
    for (const line of lines) {
      for (const promise of ['预计', '几分钟', '很快', 'in a few minutes', 'soon', 'ETA']) {
        expect([line, line.includes(promise)]).toEqual([line, false]);
      }
    }
  });
});
