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
  /**
   * C12：下载停滞守卫处于熔断态 ⇒ 这条腿暂停。
   * 与 'halted' 的区别：halted 是这条腿【自己】出了问题需要人看一眼；
   * download-paused 是落盘出口出了问题，腿本身是健康的，欠账原封不动，
   * 清掉熔断态（resumeAfterGuard）就能从断点继续。
   */
  | 'download-paused'
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
  /**
   * 🔴 C19 · 跨 tick 的定速锚点：每一段【上一次真实取数】的时刻（clock.now()，毫秒）。
   *
   * 为什么必须落盘：Pacer 是 per-run 的，而运行时一次 tick 只清 1 笔账 ⇒
   * 每个 run 的第一次 gate() 恒等待 0 ⇒ 「每条 20 秒」在浏览器里一次都没生效过
   * （C17-3.B2 实测四条正文零间隔）。把时刻存进欠账集合，间隔就能跨 tick、跨 SW
   * 回收、跨浏览器重启地续上 —— 与 detailToday 的日上限是同一种做法。
   *
   * optional：v1 的旧集合里没有这个字段，读回来是 undefined ⇒ 当作「没有上一次」，
   * 行为与 C11 逐字一致，不需要提版本号，也不会把用户已有的进度作废。
   */
  lastFetchAt?: { enumerate: number | null; detail: number | null };
  /**
   * 🔴 C20 · 落盘失败清单。**同一本账上的第三栏**（另外两栏是 pending / archived）。
   *
   * 为什么在这里而不是另开一个存储键：本次缺陷的根因是「同一个身份被表达了两次」，
   * 再造第二份「这条会话怎么样了」的账本就是同一个错误再犯一遍。
   * 结构、上限、为什么不重试，全在 lib/backfill/failures.ts。
   *
   * optional：C19 及更早的旧集合里没有这两个字段，读回来 undefined ⇒ 当作空清单，
   * 行为与 C19 逐字一致，不需要提版本号，也不会把用户已有的进度作废。
   */
  failures?: import('./failures').FailureEntry[];
  /** 因为超过上限而被丢掉的历史失败条数。🔴 绝不静默截断。 */
  failuresDropped?: number;
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
    lastFetchAt: { enumerate: null, detail: null },
    failures: [],
    failuresDropped: 0,
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
