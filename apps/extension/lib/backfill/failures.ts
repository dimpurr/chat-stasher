/**
 * C20 · 落盘失败清单。
 *
 * 产品主人拍板的原话（写在这里，别丢）：
 *   **「丢了」和「丢了但你知道」是完全不同的两件事，而这个项目的立身之本就是后者。**
 *
 * 所以这条清单的定位不是「重试队列」，而是**一张给人看的收据**：
 * 哪几条我没能存下来、大概是哪一类问题、什么时候发生的。
 *
 * 🔴 四条硬规矩：
 *  1. **不重试。** 这是产品决定。本文件里【没有】任何把条目送回欠账的函数，
 *     engine 也不会读这张清单来决定抓什么。唯一的出口是给人看 + 清空。
 *  2. **不许无限增长。** 见 MAX_FAILURES 的上限与论证；溢出走「丢最旧的」，
 *     但丢掉多少必须记进 `dropped` —— 🔴 静默截断会让清单本身变成一句谎话。
 *  3. **只存能定位问题的东西。** 会话短 ID（前 8 个字符）/ 理由码 / 时间戳 /
 *     平台 id。🔴 绝不存对话正文、绝不存完整 URL（URL 里可能带 token）、
 *     也绝不存完整会话 ID。
 *  4. **理由码不许猜原因**（沿用 C12 那条规矩）。每个码都只陈述我们**自己观测到的
 *     事实**（「我抠不出会话 ID」是事实；「平台改接口了」是猜测），
 *     不认识的码一律原样显示，绝不编一句人话糊上去。
 *
 * 存在哪里：**就存在 BackfillState 里**（`state.failures` / `state.failuresDropped`）。
 * 🔴 特意不另开一个存储键 —— 本次缺陷的根因就是「同一个身份被表达了两次」，
 *    再造第二份「这条会话怎么样了」的账本就是同一个错误再犯一遍。
 *    欠账、已归档、失败，是同一本账上的三栏。
 */

import type { BackfillState } from './types';

/** 会话短 ID 保留几个字符。8 个足够在一屏日志里认出是哪一条，又还原不出完整 id。 */
export const SHORT_ID_LEN = 8;

/**
 * 🔴 清单上限 = 50。为什么是 50：
 *
 *  · 它是**给人读的**，不是数据库。50 条已经远超一个人愿意在 popup 里扫一遍的量；
 *    再多不会增加任何可行动的信息。
 *  · 失败超过几条基本就不是「某条对话特殊」而是**系统性问题**（接口变了 / 目录不可写）。
 *    这种时候第 51 个 id 相对第 50 个的信息量是零 —— 该看的是理由码，不是名单长度。
 *  · 拿运行时的量纲对一下：取正文的每日上限是 200 条（`lib/backfill/pace.ts`
 *    的 DEFAULT_DETAIL_PACE.maxPerDay = 200）。50 = 一整天理论产能的 1/4。
 *    一天里四分之一的活儿都失败了，结论早就出来了。
 *  · 存储代价顺带被封死：50 × 约 80 字节 ≈ 4KB，和欠账集合本身比可以忽略，
 *    不会有「失败清单把 storage 撑爆」这条新的失败模式。
 *
 * 溢出规则：**丢最旧的**（不是「停止记录」）。理由：最新的失败才反映当前是什么问题；
 * 停止记录会让清单停在一个过期的快照上，比丢旧的更容易误导。丢掉的条数记进
 * `failuresDropped`，文案里照实说「另有 N 条更早的已经不在清单里」。
 */
export const MAX_FAILURES = 50;

/**
 * 理由码。**闭集**，每一个都对应 engine 里一处确定的分支。
 * 🔴 新增码必须同时在 describeFailureReason 里给一句「只陈述观测事实」的人话。
 */
export type FailureReason =
  /** sink 明确回答 saved:false —— 一个字节都没写出去。 */
  | 'not-saved'
  /**
   * sink 说写成功了，但它用来命名的身份和欠账键对不上。
   * 🔴 这一条直指根因：欠账键来自列表接口的 items[].id，文件名来自「从 URL 再抠一次」，
   *    以前中间没有任何一致性校验 —— 于是两个不同的欠账可以塌成同一个文件名，
   *    后写的把先写的覆盖掉（C17 BUG-2）。现在对不上就不清账。
   */
  | 'identity-mismatch';

export interface FailureEntry {
  /** 会话 ID 的前 8 个字符。🔴 不是完整 ID。 */
  shortId: string;
  /** 平台 id（chatgpt / claude / ...）。不是敏感物，且没有它就不知道去哪儿找。 */
  platform: string;
  reason: FailureReason | string;
  /** clock.now() 毫秒。 */
  at: number;
}

/** 只留前 8 个字符。空 id ⇒ '(空)'，绝不假装有一个。 */
export function shortIdOf(id: string): string {
  if (!id) return '(空)';
  return id.slice(0, SHORT_ID_LEN);
}

/** 读回来的清单。旧集合里没有这两个字段 ⇒ 空清单，行为与 C19 逐字一致。 */
export function failuresOf(state: BackfillState): FailureEntry[] {
  return Array.isArray(state.failures) ? state.failures : [];
}

export function droppedOf(state: BackfillState): number {
  return typeof state.failuresDropped === 'number' && state.failuresDropped > 0
    ? state.failuresDropped
    : 0;
}

/**
 * 记一条失败。**就地改 state**（和 settleDebt 一样的写法，落盘由调用方的 persist 负责）。
 *
 * 🔴 这个函数【不】把 id 放回 pending，也【不】把它写进 archived：
 *    不重试（产品决定），也绝不冒充「已归档」。它从此只活在这张清单上。
 *
 * 🔴 「不会被重新入队」是**结构上成立**的，不是靠一张黑名单挡的：
 *    engine 的枚举段是 `while (!state.enumCursor.complete)`，而取正文段只在枚举
 *    跑完之后才开始 ⇒ 任何失败发生时 enumCursor.complete 早就是 true ⇒
 *    后续每一次 tick 都直接跳过枚举，不存在「再枚举一遍又把它捡回来」的路径。
 *    （正因为如此，这里也就**不需要**为了去重而保存完整会话 ID —— 规矩 3 得以成立。）
 */
export function recordFailure(
  state: BackfillState,
  entry: { id: string; reason: FailureReason | string; at: number },
): FailureEntry {
  const row: FailureEntry = {
    shortId: shortIdOf(entry.id),
    platform: state.platform,
    reason: entry.reason,
    at: entry.at,
  };
  const list = failuresOf(state).slice();
  list.push(row);
  let dropped = droppedOf(state);
  while (list.length > MAX_FAILURES) {
    list.shift();
    dropped += 1;
  }
  state.failures = list;
  state.failuresDropped = dropped;
  return row;
}

/** 用户「确认 / 清空」。连 dropped 一起归零 —— 确认过就是确认过了。 */
export function clearFailures(state: BackfillState): void {
  state.failures = [];
  state.failuresDropped = 0;
}

/**
 * 理由码 → 一句只陈述观测事实的人话。
 * 🔴 不认识的码**原样吐回去**，绝不编。
 */
export function describeFailureReason(reason: string): string {
  switch (reason) {
    case 'not-saved':
      return '取到了正文，但没有写出任何文件';
    case 'identity-mismatch':
      return '写出去了，但用的文件名身份和这条欠账对不上';
    default:
      return `理由码 ${reason}`;
  }
}
