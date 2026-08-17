/**
 * C12 · 下载停滞守卫。
 *
 * 要解决的产品问题：回溯腿会在好几天里发起成百上千次下载。如果每一次下载都
 * 「迟迟不落地」，用户面对的就是一串停不下来的打扰 ——「弹一次就停」和
 * 「弹五十次」是两个完全不同的产品。这个模块只做一件事：
 *   观测每一次下载是否在超时内到达终态，连续停滞到阈值就把回溯腿停掉。
 *
 * 🔴 三条硬规矩：
 *  1. 【停滞 ≠ 失败】。浏览器明确报 interrupted 的是失败（带 error 原因，可重试）；
 *     既没 complete 也没 interrupted 的是停滞（没有任何原因可报，重试只会再来一次
 *     同样的打扰）。两者分开记账、分开处置。
 *  2. 【绝不替用户猜原因】。停滞可能来自磁盘满、权限被拒、用户手动取消、企业策略
 *     拦截、某个等待确认的系统对话框……我们【观测不到】是哪一个，所以 stalled 的
 *     error 永远是 null，文案也只说「没有在 X 秒内完成」。
 *  3. 【不新增任何权限】。持久化复用 lib/backfill/store.ts 的 BackfillStore 端口
 *     （底下就是 lib/badge.ts 已经在用的 browser.storage.local，cs_* 前缀同族）。
 */

import type { BackfillStore } from './backfill/store';

/** 一次下载观测窗口的默认长度。与 lib/download.ts 原有的 15s 超时保持一致。 */
export const DOWNLOAD_TIMEOUT_MS = 15_000;

/**
 * 连续多少次停滞就熔断。默认 3 的论证：
 *  - 1 太敏感：一次偶发的慢盘 / 大文件 / 瞬时卡顿就会把整条腿停掉，
 *    用户要为一次噪声付出「回溯静止」的代价；
 *  - 3 次 × 15s 观测窗 ≈ 45 秒内做出判断，也就是【最多让用户被打扰 3 次】就收手，
 *    而不是几百次 —— 这正是「弹一次就停 / 弹五十次」那条线该画的位置；
 *  - 再大（比如 10）就失去意义：真出问题时用户已经被打扰十几次了。
 */
export const DEFAULT_STALL_THRESHOLD = 3;

export const GUARD_STATE_VERSION = 1;
/** 存储键。cs_* 前缀同族，不新增权限、不新增依赖。 */
export const GUARD_KEY = `cs_download_guard_v${GUARD_STATE_VERSION}`;

/** 三态：成功 / 明确失败 / 停滞（既没完成也没中断）。 */
export type DownloadOutcome = 'complete' | 'interrupted' | 'stalled';

export interface DownloadResult {
  outcome: DownloadOutcome;
  /** 只有 interrupted 才可能有原因（浏览器给的）。stalled 恒为 null。 */
  error: string | null;
  /** 这次观测实际等了多久（ms）。 */
  waitedMs: number;
}

/** downloads.onChanged 的 delta，只取我们用得到的字段。 */
export interface DownloadDelta {
  id: number;
  state?: { previous?: string; current?: string };
  error?: { previous?: string; current?: string };
}

/**
 * 终态判定。返回 null 表示「这个事件不是终态」—— 注意 null 不等于「停滞」，
 * 停滞是"观测窗口结束了还没等到终态"，由 stalledResult() 表达。
 */
export function classifyDelta(
  delta: DownloadDelta,
): { outcome: 'complete' | 'interrupted'; error: string | null } | null {
  const cur = delta.state?.current;
  if (cur === 'complete') return { outcome: 'complete', error: null };
  if (cur === 'interrupted') return { outcome: 'interrupted', error: delta.error?.current ?? null };
  return null;
}

/** 观测窗口用尽仍未见终态。没有原因可报 ⇒ error 恒为 null。 */
export function stalledResult(waitedMs: number): DownloadResult {
  return { outcome: 'stalled', error: null, waitedMs };
}

export interface GuardState {
  v: typeof GUARD_STATE_VERSION;
  /** 连续停滞次数。一次 complete 清零；interrupted 不动它。 */
  consecutiveStalls: number;
  totalStalls: number;
  /** 明确失败的次数，与停滞【分开】记。 */
  totalInterrupted: number;
  tripped: boolean;
  trippedAt: number | null;
  /** 熔断时用的观测窗口长度，写进文案，避免文案里硬编码一个可能不对的秒数。 */
  timeoutMs: number;
}

export function initialGuardState(timeoutMs: number = DOWNLOAD_TIMEOUT_MS): GuardState {
  return {
    v: GUARD_STATE_VERSION,
    consecutiveStalls: 0,
    totalStalls: 0,
    totalInterrupted: 0,
    tripped: false,
    trippedAt: null,
    timeoutMs,
  };
}

export function isGuardTripped(state: GuardState): boolean {
  return state.tripped;
}

export interface RecordOpts {
  threshold?: number;
  now?: number;
}

/**
 * 纯函数状态机。
 *  complete    ⇒ 连击清零（但【不】自动解除已有的熔断：自愈会让弹窗轰炸原地复活，
 *                必须有人显式恢复，见 resumeAfterGuard）
 *  interrupted ⇒ 只加失败计数。失败可以重试，不该把腿停掉。
 *  stalled     ⇒ 连击 +1，达到阈值就熔断。
 */
export function recordOutcome(state: GuardState, result: DownloadResult, opts: RecordOpts = {}): GuardState {
  const threshold = opts.threshold ?? DEFAULT_STALL_THRESHOLD;
  const now = opts.now ?? 0;
  const next: GuardState = { ...state };

  if (result.outcome === 'complete') {
    next.consecutiveStalls = 0;
    return next;
  }
  if (result.outcome === 'interrupted') {
    next.totalInterrupted += 1;
    return next;
  }

  next.totalStalls += 1;
  next.consecutiveStalls += 1;
  next.timeoutMs = result.waitedMs;
  if (!next.tripped && next.consecutiveStalls >= threshold) {
    next.tripped = true;
    next.trippedAt = now;
  }
  return next;
}

function isGuardState(value: unknown): value is GuardState {
  if (!value || typeof value !== 'object') return false;
  const s = value as Partial<GuardState>;
  return s.v === GUARD_STATE_VERSION && typeof s.consecutiveStalls === 'number' && typeof s.tripped === 'boolean';
}

/** 读不出来 / 版本对不上 ⇒ 当作全新状态，而不是拿旧结构硬凑。 */
export async function loadGuardState(store: BackfillStore): Promise<GuardState> {
  const raw = await store.load(GUARD_KEY);
  return isGuardState(raw) ? raw : initialGuardState();
}

export async function saveGuardState(store: BackfillStore, state: GuardState): Promise<void> {
  await store.save(GUARD_KEY, state);
}

/** 读-改-写一次观测结果。store 为 null（存储不可用）时静默跳过：守卫是保护层，
 *  不该反过来把落盘路径搞挂；存储不可用本身已经由 engine 的 storage-unavailable 覆盖。 */
export async function recordDownloadOutcome(
  store: BackfillStore | null,
  result: DownloadResult,
  opts: RecordOpts = {},
): Promise<GuardState | null> {
  if (!store) return null;
  const next = recordOutcome(await loadGuardState(store), result, opts);
  await saveGuardState(store, next);
  return next;
}

/**
 * 显式恢复入口。清掉熔断与连击，【不】动欠账集合 —— 欠账在 BackfillState 里，
 * 从头到尾没被碰过，所以恢复后是从断点继续，不是重头来。
 */
export async function resumeAfterGuard(store: BackfillStore): Promise<GuardState> {
  const cur = await loadGuardState(store);
  const next: GuardState = { ...cur, tripped: false, trippedAt: null, consecutiveStalls: 0 };
  await saveGuardState(store, next);
  return next;
}

// ---------------------------------------------------------------------------
// 文案。🔴 只陈述观测到的事实；方向清单一律「可能 / 请检查」；不出现百分比。
// ---------------------------------------------------------------------------

function seconds(ms: number): string {
  const s = ms / 1000;
  return Number.isInteger(s) ? `${s} 秒` : `${s.toFixed(1)} 秒`;
}

/** 未熔断返回 null（角标上不该有任何东西）。角标文字必须极短。 */
export function guardBadge(state: GuardState): { text: string; title: string } | null {
  if (!state.tripped) return null;
  return {
    text: '停',
    title: `chat-stasher：最近连续 ${state.consecutiveStalls} 次归档没有在 ${seconds(state.timeoutMs)}内完成，已暂停自动回溯`,
  };
}

/**
 * 详情文本。数字全部来自真实状态，没有一个是编的；
 * 不显示百分比（分母的可信度由 lib/backfill/progress.ts 判定，这里根本不涉及）。
 */
export function guardAlertDetail(
  state: GuardState,
  progress: { archived: number; pending: number },
): string {
  const lines = [
    '已暂停自动回溯。',
    '',
    `观测到的事实：最近连续 ${state.consecutiveStalls} 次归档写入没有在 ${seconds(state.timeoutMs)}内完成 —— 既没有成功，也没有报错。`,
    `（另有 ${state.totalInterrupted} 次写入是明确失败的，与停滞分开计数。）`,
    '',
    `数据没有丢：已归档 ${progress.archived} 条仍在，欠账 ${progress.pending} 条也还在清单里；恢复后会从这里继续，不会重头再来。`,
    '',
    '我们无法判断是哪一种情况造成的，以下是可能的方向，请逐项检查：',
    '· 请检查是否有等待确认的系统对话框被挡住或最小化了',
    '· 请检查磁盘剩余空间，以及下载目录是否仍然可写',
    '· 请检查浏览器的下载相关设置最近是否有变化',
    '· 请检查是否有安全软件或企业策略在拦截写入',
    '',
    '确认之后可以手动恢复自动回溯。',
  ];
  return lines.join('\n');
}
