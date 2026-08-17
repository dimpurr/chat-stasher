/**
 * 欠账集合的纯函数部分：入队 / 清账 / 计数。
 * 没有 I/O，没有时钟 —— 所有「不重复抓」的规则都在这里，可以被单测直接钉住。
 */

import type { BackfillState } from './types';

/**
 * 把枚举到的 id 并入欠账集合。
 * 硬规则：**已归档的绝不再入队**，已经在欠账里的也不重复入队。
 * 返回真正新增的 id（用来在报告里说清楚「这一页有几条是新的」）。
 */
export function enqueueDebts(state: BackfillState, ids: readonly string[]): string[] {
  const archived = new Set(state.archived);
  const pending = new Set(state.pending);
  const added: string[] = [];
  for (const id of ids) {
    if (!id || archived.has(id) || pending.has(id)) continue;
    pending.add(id);
    state.pending.push(id);
    added.push(id);
  }
  return added;
}

/** 清一笔账：从欠账里移出，记进已归档。重复清账是幂等的。 */
export function settleDebt(state: BackfillState, id: string): void {
  const at = state.pending.indexOf(id);
  if (at >= 0) state.pending.splice(at, 1);
  if (!state.archived.includes(id)) state.archived.push(id);
}

/**
 * 🔴 C20 · 把一笔账从欠账里**拿掉但不清账**：不进 archived。
 *
 * 只有一个调用方：engine 在 sink 明确报「没存下来」时用它。
 * 与 settleDebt 的区别就是那一行 `state.archived.push` —— 而那一行正是
 * 「文件没落盘、欠账照样被划掉」的全部内容。
 * 为什么还是要从 pending 里拿掉：**不重试是产品拍板的决定**；留在 FIFO 队首
 * 会让下一次 tick 原地打转，把「不重试」变成「无限重试」。
 * 它去哪儿了：lib/backfill/failures.ts 的失败清单（同一本账的第三栏）。
 */
export function dropDebt(state: BackfillState, id: string): void {
  const at = state.pending.indexOf(id);
  if (at >= 0) state.pending.splice(at, 1);
}

/** 下一笔要清的账（FIFO：先枚举到的先归档，行为可预测）。 */
export function nextDebt(state: BackfillState): string | null {
  return state.pending[0] ?? null;
}

export function isArchived(state: BackfillState, id: string): boolean {
  return state.archived.includes(id);
}
