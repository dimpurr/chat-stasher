/**
 * C13 · 回溯腿的【运行时接线】。
 *
 * C11/C12 之后回溯腿的模块是齐的、测试是绿的，但 entrypoints/background.ts
 * 从头到尾没有 import 过 runBackfill —— 也就是说浏览器里它永远不会执行。
 * 这个文件就是那根缺失的线：把「什么时候跑一次」这件事写成一个纯粹、可测、
 * 可关的函数，由 background.ts 在实时腿的触发点顺带唤起。
 *
 * 心跳（W2 现状）：两条 —— 实时腿被唤醒时顺带踢一脚（C13），以及 chrome.alarms
 * 周期醒来（C19，见 lib/backfill/alarm.ts）。两条走的是同一个 tickBackfill，
 * 闸门顺序完全一致。C13 当时拒绝定时器是因为那会儿 manifest 里没有 'alarms'；
 * C19 把它加进了 permissions，那条理由不再成立。
 *
 * 🔴 为什么每次只清 1 笔账（DEFAULT_TICK_DETAILS）：
 *    MV3 的 service worker 空闲就会被回收，一次 tick 必须是短的。
 *    engine 每清一笔账立刻落盘，所以「短 tick × 很多次」与「长 tick」在进度上等价，
 *    但前者对 SW 生命周期友好，也天然更温和 —— 正好是产品要的「好几天里静静补完」。
 */

import { runBackfill, type BackfillOptions, type HttpPort, type RunReport } from './engine';
import { loadHostPause, resumeBackfill } from '../host-status';
import type { BackfillStore } from './store';

/** 开关的存储键。cs_* 前缀同族，不新增权限。 */
export const BACKFILL_ENABLED_KEY = 'cs_backfill_enabled_v1';

/**
 * 🔴 默认【关】。论证见 C13 报告；一句话版本：
 * 实时腿只归档「用户此刻正打开的这条对话」，默认开是无感的；
 * 回溯腿要拿用户的登录态把【整个账号的历史】翻一遍、并往本机 host 的 stage 里
 * 写成百上千个分片 ——
 * 那是另一种行为，必须先有一次明确的「开」。开过一次就持久化，不会反复打扰。
 */
export const BACKFILL_DEFAULT_ENABLED = false;

export async function isBackfillEnabled(store: BackfillStore | null): Promise<boolean> {
  if (!store) return false;
  const raw = await store.load(BACKFILL_ENABLED_KEY);
  // 只认严格的 true。读不出来 / 结构不对 ⇒ 回落到默认（关），绝不"猜用户同意了"。
  return raw === true ? true : BACKFILL_DEFAULT_ENABLED;
}

/**
 * 显式开关。
 * C13 时这里写的是「函数即接口，本任务不做 UI」—— 结果是**没有任何生产代码调用它**，
 * 用户根本打不开。C18 补上了调用点：entrypoints/popup/main.ts 的那个 checkbox。
 */
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
  /**
   * 🔴 W2 · 上一次投递发现本机 host 够不着，这条腿暂停中，而这一次心跳的
   * `hello` 仍然没有成功。欠账一笔没动；主机一答话就从断点继续。
   * 与 'halted' 的区别见 lib/backfill/types.ts 的 host-unavailable。
   */
  | 'host-paused'
  /**
   * 🔴 C30 · 一个回溯目标都没有 ⇒ 我们连"从哪儿开始补"都不知道。
   * 这【不是】端口问题：通道可能好端端地接着（平台页面开着、ping 得通），
   * 但登记表(cs_backfill_targets_v1)是空的 —— 只有实时腿真的归档过一次对话，
   * 才会写下 platform/origin/账号 scope。C30 之前这一种结局被混报成
   * 'no-http-port'，于是排查的人去查端口，而端口根本没坏。
   * 📌 一个错的原因比没有原因更难查。
   */
  | 'no-targets'
  /** 没有注入 http 端口 ⇒ 绝不会有网络行为（见下方大段说明）。 */
  | 'no-http-port'
  /** 真的调用了 runBackfill。 */
  | 'ran';

export interface TickResult {
  ran: boolean;
  reason: TickReason;
  report: RunReport | null;
}

/**
 * 「这一脚为什么踢不动」的四种闸门。
 * 'already-running' 与 'ran' 不在其中：前者是并发瞬时态，后者不是闸门。
 */
export type TickBlockReason = Extract<
  TickReason,
  'no-store' | 'disabled' | 'host-paused' | 'no-targets' | 'no-http-port'
>;

/**
 * 🔴 闸门顺序的【唯一权威】。
 *
 * 为什么要抽出来：C18 的 Popup 必须告诉用户「现在到底跑不跑得动、卡在哪一道」。
 * 如果 Popup 自己再写一遍这四个 if，两边就会漂 —— 漂的那一天，Popup 会显示
 * 「正在归档」而实际上一条都没在取。所以 tickBackfill 和 Popup 共用这一个函数，
 * 顺序天然一致。
 *
 * 取值用 thunk 而不是 boolean：保持 tickBackfill 原有的**惰性**（开关是关的时候
 * 不去读 download guard 的存储），行为与 C13 逐字一致。
 */
export async function tickBlockReason(gate: {
  hasStore: boolean;
  isEnabled: () => boolean | Promise<boolean>;
  /**
   * 🔴 W2 · 「落盘出口够不着」的暂停态。取代了 C12 的下载停滞守卫：
   * 那时候暂停的原因是「下载写不完」，现在唯一的原因是「本机 host 不在」。
   * 检查仍然排在 http 端口之前：暂停态下我们连"要不要发请求"都不该问。
   */
  isHostPaused: () => boolean | Promise<boolean>;
  hasHttp: boolean;
  /**
   * 🔴 C30 · 有没有【回溯目标】。
   * 省略 ⇒ 视为「有」，这是实时腿那条路的真实情况：它手里现成攥着一个目标，
   * 根本不查登记表。所以老调用点一个字都不用改，行为逐字不变。
   * 闹钟那条路与 Popup 必须显式传 —— 它们的目标只能来自登记表。
   */
  hasTargets?: boolean;
}): Promise<TickBlockReason | null> {
  if (!gate.hasStore) return 'no-store';
  if (!(await gate.isEnabled())) return 'disabled';
  if (await gate.isHostPaused()) return 'host-paused';
  // 🔴 顺序与 runAlarmTick 真实执行的顺序一致：先读登记表拿目标，再去建通道。
  //    没有目标时连"该向哪个源建通道"都答不上来，所以这一道排在端口之前。
  if (gate.hasTargets === false) return 'no-targets';
  if (!gate.hasHttp) return 'no-http-port';
  return null;
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
  /**
   * W2 闸门。省略 ⇒ 用生产实现（lib/host-status.ts 的暂停记录 + `hello`）。
   * 测试可以注入一个假的，好让「主机不在 ⇒ 暂停 / hello 成功 ⇒ 恢复」
   * 这两种结局都能被断言，而不必真的去起一个本机进程。
   */
  host?: HostGate;
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
 * W2 · 主机闸门的形状。
 *  · `paused()` —— 现在是不是「出口够不着」的暂停态（读 storage 里那条记录）；
 *  · `resume()` —— 试一次 `hello`；成功 ⇒ 清掉暂停并返回 true。
 */
export interface HostGate {
  paused(): Promise<boolean>;
  resume(): Promise<boolean>;
}

/** 生产实现：暂停记录 + 一次真的 `hello`（§10 的恢复条件）。 */
export function productionHostGate(store: BackfillStore | null): HostGate {
  return {
    paused: async () => (await loadHostPause(store)) !== null,
    resume: async () => (await resumeBackfill(store)).resumed,
  };
}

/**
 * 跑一次回溯。**这是运行时唯一的入口**，检查顺序是有意的：
 *   单飞 → 存储 → 开关 → 主机暂停（先试着叫醒） → 其余闸门 → 真跑。
 *
 * 🔴 W2 · 「先 hello 再决定」这一脚是 §10 要求的那条恢复路径：
 *    暂停态下不是直接放弃，而是先问一句主机在不在；答话了就把暂停清掉、
 *    接着从**同一笔欠账**往下做（欠账从头到尾没被动过）。
 */
export async function tickBackfill(deps: TickDeps): Promise<TickResult> {
  if (inFlight) return { ran: false, reason: 'already-running', report: null };
  inFlight = true;
  try {
    const gate = deps.host ?? productionHostGate(deps.store);
    // 开关关着的时候连 hello 都不发：没同意就没有任何周期性行为。
    if (deps.store && await isBackfillEnabled(deps.store) && await gate.paused()) {
      const resumed = await gate.resume();
      if (!resumed) return { ran: false, reason: 'host-paused', report: null };
    }

    // 🔴 四道闸门走 tickBlockReason ——【与 Popup 同一份判断】。
    const blocked = await tickBlockReason({
      hasStore: deps.store !== null,
      isEnabled: () => isBackfillEnabled(deps.store),
      isHostPaused: () => gate.paused(),
      hasHttp: deps.http !== undefined,
    });
    if (blocked) return { ran: false, reason: blocked, report: null };
    const store = deps.store!;
    const http = deps.http!;

    const report = await runBackfill({
      platform: deps.platform,
      origin: deps.origin,
      scope: deps.scope,
      store,
      http,
      clock: deps.clock,
      pace: deps.pace,
      maxDetails: deps.maxDetails ?? DEFAULT_TICK_DETAILS,
      shouldAbort: deps.shouldAbort,
      sink: deps.sink,
    });
    return { ran: true, reason: 'ran', report };
  } finally {
    inFlight = false;
  }
}
