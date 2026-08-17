/**
 * C11 · 回溯腿（backfill leg）的共享类型。
 *
 * 产品要求（12:52 原话）：「非常温和地慢慢地去爬我历史上所有的对话 …… 有一个进度条
 * …… 非常温和的一点一点的在好几天之后」。所以这条腿的三个硬约束是：
 *   1. 可断可续 —— 状态必须落盘，重启从断点继续；
 *   2. 枚举 / 取正文分开定速 —— 枚举很便宜（ChatGPT 约 10 页拿完 1000 条），
 *      真正要温和的是随后逐条取正文的那 1000 次；
 *   3. 进度必须诚实 —— 分母拿不到就绝不显示百分比，被限流/形状变了必须停下留痕。
 *
 * 这里只放类型与常量，没有任何 I/O，方便 MAIN world / 测试双向复用。
 */

export const BACKFILL_STATE_VERSION = 1;

/**
 * 停下来并留痕的原因。出现任何一个都表示「这条腿不再自己往前爬」，
 * 需要人来看一眼 —— 绝不允许静默地爬不动。
 */
export type HaltReason =
  /** HTTP 429 / 403 / 5xx：被平台限流或拒绝 */
  | 'rate-limited'
  /** 响应能拿到，但结构不是我们认识的形状（接口改了） */
  | 'shape-changed'
  /** 网络/传输层直接抛错 */
  | 'transport-error'
  /** 没有可用的持久化存储 ⇒ 无法可断可续 ⇒ 宁可不爬 */
  | 'storage-unavailable';

export interface HaltRecord {
  reason: HaltReason;
  /** 发生时间（clock.now()，毫秒） */
  at: number;
  /** 只放技术细节：URL 路径、状态码、缺哪个字段。绝不放对话正文。 */
  detail: string;
}

/** 一次 run 为什么结束。halted 之外都属于正常的「温和地停一下」。 */
export type StopReason =
  | 'queue-empty'
  | 'budget-exhausted'
  | 'daily-cap'
  | 'aborted'
  | 'halted';

/** total 的来源。只有 'response-total' 才配当进度条的分母。 */
export type TotalSource = 'response-total' | 'unknown';

/** 每天配额的计数窗口，按 UTC 日期切分。 */
export interface DailyCounter {
  /** YYYY-MM-DD（UTC） */
  day: string;
  count: number;
}

/**
 * 「欠账集合」——- 与 CLI 侧 state/debts-v2.json 的 per-destination 欠账集合同构：
 * 已清的不再入队、可断可续、进度 = 已清 / 总数。
 */
export interface BackfillState {
  v: typeof BACKFILL_STATE_VERSION;
  platform: string;
  /** 归档范围键：同一账号一个集合（ADR-002 的账号轴）。 */
  scope: string;
  /** 接口直给的会话总数；拿不到就是 null。 */
  totalKnown: number | null;
  totalSource: TotalSource;
  /** 枚举游标：offset + 是否枚举完。 */
  enumCursor: { offset: number; complete: boolean };
  /** 欠账：已枚举出来、但还没取到正文的会话 id。 */
  pending: string[];
  /** 已清：已经归档过的会话 id，永不再入队。 */
  archived: string[];
  /** 今天已经取了多少条正文（跨重启有效）。 */
  detailToday: DailyCounter;
  /** 非 null 表示这条腿已经停下并留痕。 */
  halted: HaltRecord | null;
}

export function initialState(platform: string, scope: string): BackfillState {
  return {
    v: BACKFILL_STATE_VERSION,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pending: [],
    archived: [],
    detailToday: { day: '', count: 0 },
    halted: null,
  };
}

/** 存储键。与 badge 的 cs_* 前缀同族，不新增任何权限。 */
export function stateKey(platform: string, scope: string): string {
  return `cs_backfill_v${BACKFILL_STATE_VERSION}:${platform}:${scope}`;
}

export function dayKeyOf(nowMs: number): string {
  return new Date(nowMs).toISOString().slice(0, 10);
}
