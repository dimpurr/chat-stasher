/**
 * 诚实的进度。
 *
 * 🔴 硬要求：**分母拿不到时，绝不显示百分比**。
 *
 * 判据（只有全部满足才允许出现百分比）：
 *   1. totalSource === 'response-total' —— 分母来自接口自己给的 total，
 *      不是我们估的、不是「已枚举条数」冒充的；
 *   2. totalKnown 是有限整数且 > 0；
 *   3. 已归档条数 <= totalKnown（否则说明分母已经过期/不可信，退回未知）。
 * 任何一条不满足，就走「总数未知」文案，并且输出里**不含 '%' 字符**。
 *
 * 为什么不用「已枚举条数」当分母：枚举还没跑完时它会一路变大，进度条会先冲到
 * 100% 再掉回去 —— 那是个假的百分比，比没有进度条更坏。
 */

import type { BackfillState } from './types';

export interface ProgressView {
  archived: number;
  pending: number;
  /** 只有可信时才是数字，否则 null。 */
  totalKnown: number | null;
  /** 0-100 的整数；不可信时为 null。null ⇒ 渲染层禁止出现 '%'。 */
  percent: number | null;
  /** 为什么没有百分比（有百分比时为 null）—— 让「不知道」本身是可读的。 */
  unknownReason: string | null;
  halted: BackfillState['halted'];
}

export function computeProgress(state: BackfillState): ProgressView {
  const archived = state.archived.length;
  const pending = state.pending.length;

  let percent: number | null = null;
  let unknownReason: string | null = null;
  let totalKnown: number | null = null;

  if (state.totalSource !== 'response-total') {
    unknownReason = '接口未提供 total';
  } else if (
    state.totalKnown === null ||
    !Number.isFinite(state.totalKnown) ||
    !Number.isInteger(state.totalKnown) ||
    state.totalKnown <= 0
  ) {
    unknownReason = 'total 不是可用的正整数';
  } else if (archived > state.totalKnown) {
    unknownReason = '已归档数超过 total，分母已过期';
  } else {
    totalKnown = state.totalKnown;
    percent = Math.floor((archived / totalKnown) * 100);
  }

  return { archived, pending, totalKnown, percent, unknownReason, halted: state.halted };
}

/**
 * 一行人话进度。
 * 分母未知时的显示形如：`已归档 12 条，欠账 30 条，总数未知（接口未提供 total）`
 * —— 说清楚已经做了多少、还欠多少、以及为什么没有分母，就是不给百分比。
 */
export function formatProgress(state: BackfillState): string {
  const view = computeProgress(state);
  const head = view.halted ? `[已停止：${view.halted.reason}] ` : '';
  const body =
    view.percent === null
      ? `已归档 ${view.archived} 条，欠账 ${view.pending} 条，总数未知（${view.unknownReason}）`
      : `已归档 ${view.archived} / 共 ${view.totalKnown} 条（${view.percent}%），欠账 ${view.pending} 条`;
  return head + body;
}
