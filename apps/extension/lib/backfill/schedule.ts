/**
 * C13 · 回溯腿的【运行时接线】。
 *
 * C11/C12 之后回溯腿的模块是齐的、测试是绿的，但 entrypoints/background.ts
 * 从头到尾没有 import 过 runBackfill —— 也就是说浏览器里它永远不会执行。
 * 这个文件就是那根缺失的线：把「什么时候跑一次」这件事写成一个纯粹、可测、
 * 可关的函数，由 background.ts 在实时腿的触发点顺带唤起。
 *
 * 🔴 为什么不用 chrome.alarms 做定时：
 *    wxt.config.ts:9 的 manifest permissions 只有 ['downloads']，**没有 'alarms'**。
 *    本任务的硬约束是不新增任何权限，所以这里【不】引入任何定时器 API，
 *    只借用「实时腿被唤醒」这个已经存在的事件当心跳。
 *
 * 🔴 为什么每次只清 1 笔账（DEFAULT_TICK_DETAILS）：
 *    MV3 的 service worker 空闲就会被回收，一次 tick 必须是短的。
 *    engine 每清一笔账立刻落盘，所以「短 tick × 很多次」与「长 tick」在进度上等价，
 *    但前者对 SW 生命周期友好，也天然更温和 —— 正好是产品要的「好几天里静静补完」。
 */

import { runBackfill, type BackfillOptions, type HttpPort, type RunReport } from './engine';
import type { BackfillStore } from './store';

/** 开关的存储键。cs_* 前缀同族，不新增权限。 */
export const BACKFILL_ENABLED_KEY = 'cs_backfill_enabled_v1';

/**
 * 🔴 默认【关】。论证见 C13 报告；一句话版本：
 * 实时腿只归档「用户此刻正打开的这条对话」，默认开是无感的；
 * 回溯腿要拿用户的登录态把【整个账号的历史】翻一遍、并往下载目录写成百上千个文件 ——
 * 那是另一种行为，必须先有一次明确的「开」。开过一次就持久化，不会反复打扰。
 */
export const BACKFILL_DEFAULT_ENABLED = false;

export async function isBackfillEnabled(store: BackfillStore | null): Promise<boolean> {
  if (!store) return false;
  const raw = await store.load(BACKFILL_ENABLED_KEY);
  // 只认严格的 true。读不出来 / 结构不对 ⇒ 回落到默认（关），绝不"猜用户同意了"。
  return raw === true ? true : BACKFILL_DEFAULT_ENABLED;
}

/** 显式开关（函数即接口，本任务不做 UI）。 */
export async function setBackfillEnabled(store: BackfillStore | null, on: boolean): Promise<boolean> {
  if (!store) return false;
  await store.save(BACKFILL_ENABLED_KEY, on === true);
  return on === true;
}

/** 一次 tick 最多清几笔账。 */
export const DEFAULT_TICK_DETAILS = 1;

/**
 * 一次 tick 为什么没跑 / 跑了。
 * 每一种都是【明确的、可断言的】结果 —— 不许出现"静默什么都没发生"。
 */
export type TickReason =
  /** 上一次 tick 还没结束（单飞锁）。 */
  | 'already-running'
  /** 没有 storage.local ⇒ 没有可断可续 ⇒ 不开跑。 */
  | 'no-store'
  /** 开关是关的（默认态）。 */
  | 'disabled'
  /** C12 熔断态：落盘出口有问题，这条腿必须暂停。 */
  | 'download-paused'
  /** 没有注入 http 端口 ⇒ 绝不会有网络行为（见下方大段说明）。 */
  | 'no-http-port'
  /** 真的调用了 runBackfill。 */
  | 'ran';

export interface TickResult {
  ran: boolean;
  reason: TickReason;
  report: RunReport | null;
}

export interface TickDeps {
  store: BackfillStore | null;
  platform: string;
  origin: string;
  scope: string;
  /**
   * 🔴 取正文/枚举的网络端口。**故意没有默认值。**
   * 不注入就直接 'no-http-port' 返回，engine 连碰都不碰 ——
   * 这样「回溯腿会不会发请求」是由接线方一处显式决定的，
   * 而不是藏在某个默认参数里。C13 的产物里【没有】任何地方注入真实 fetch。
   */
  http?: HttpPort;
  /** C12 闸门。 */
  downloadGuard?: () => boolean | Promise<boolean>;
  /** 归档出口：与实时腿同一个落盘函数，落盘逻辑不分叉。 */
  sink?: BackfillOptions['sink'];
  maxDetails?: number;
  pace?: BackfillOptions['pace'];
  clock?: BackfillOptions['clock'];
  shouldAbort?: () => boolean;
}

/**
 * 单飞锁。MV3 的 SW 是单线程事件循环，模块级变量在 SW 存活期间有效；
 * SW 被回收后这个锁自然消失 —— 这没问题，因为进度全在 storage 里，
 * 锁只是防止同一个存活期内并发跑两份而互相抢欠账。
 */
let inFlight = false;

/** 只给测试用：SW 被回收 ⇒ 模块状态清零。 */
export function resetTickLockForTest(): void {
  inFlight = false;
}

/**
 * 跑一次回溯。**这是运行时唯一的入口**，检查顺序是有意的：
 *   单飞 → 存储 → 开关 → 熔断 → http 端口 → 真跑。
 * 熔断检查【在】http 端口之前：熔断态下我们连"要不要发请求"都不该问。
 */
export async function tickBackfill(deps: TickDeps): Promise<TickResult> {
  if (inFlight) return { ran: false, reason: 'already-running', report: null };
  inFlight = true;
  try {
    if (!deps.store) return { ran: false, reason: 'no-store', report: null };
    if (!(await isBackfillEnabled(deps.store))) {
      return { ran: false, reason: 'disabled', report: null };
    }
    if (deps.downloadGuard && (await deps.downloadGuard())) {
      // 欠账原封不动；清掉熔断态之后下一次 tick 就从断点继续。
      return { ran: false, reason: 'download-paused', report: null };
    }
    if (!deps.http) return { ran: false, reason: 'no-http-port', report: null };

    const report = await runBackfill({
      platform: deps.platform,
      origin: deps.origin,
      scope: deps.scope,
      store: deps.store,
      http: deps.http,
      clock: deps.clock,
      pace: deps.pace,
      maxDetails: deps.maxDetails ?? DEFAULT_TICK_DETAILS,
      shouldAbort: deps.shouldAbort,
      sink: deps.sink,
      downloadGuard: deps.downloadGuard,
    });
    return { ran: true, reason: 'ran', report };
  } finally {
    inFlight = false;
  }
}
