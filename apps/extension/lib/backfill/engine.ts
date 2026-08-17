/**
 * 回溯腿的编排：枚举 → 欠账集合 → 逐条取正文 → 交给实时腿同一个落盘出口。
 *
 * 三条不许破的规矩：
 *  1. 每清一笔账立刻落盘 ⇒ 任何时刻被杀掉，重启都能从断点继续；
 *  2. 枚举和取正文各用各的 Pacer ⇒ 两段分开定速；
 *  3. 任何非 2xx / 形状不认识 / 无法持久化 ⇒ halt 并写进 state.halted，
 *     绝不吞掉继续转圈。
 *
 * 🔴 这里【没有】任何默认会真的发请求的路径：http 端口不注入就是 notWiredHttp，
 *    调用它直接抛错。本任务全部用合成夹具测，没有也不该有登录态。
 */

import { getPlatformByOrigin, matchesResponseShape, type CapturedFetch } from '../contract';
import { enqueueDebts, nextDebt, settleDebt } from './debts';
import { detailUrl, listPageUrl, parseConversationListPage, DEFAULT_LIST_LIMIT } from './enumerate';
import { formatProgress } from './progress';
import { DEFAULT_PACE, Pacer, systemClock, type BackfillPace, type Clock } from './pace';
import type { BackfillStore } from './store';
import {
  BACKFILL_STATE_VERSION,
  dayKeyOf,
  initialState,
  stateKey,
  type BackfillState,
  type HaltReason,
  type HaltRecord,
  type StopReason,
} from './types';

export interface HttpResponse {
  status: number;
  text: string;
}

export type HttpPort = (url: string) => Promise<HttpResponse>;

/** 默认端口：故意会炸。没接线就绝不会有网络行为。 */
export const notWiredHttp: HttpPort = async (url: string) => {
  throw new Error(`[chat-stasher] backfill http port is not wired (refused to fetch ${new URL(url).pathname})`);
};

export interface BackfillOptions {
  platform: string;
  /** 平台源，例如 https://chatgpt.com。必须命中 lib/contract.ts 的平台表。 */
  origin: string;
  /** 归档范围键（账号轴）。 */
  scope: string;
  store: BackfillStore | null;
  http?: HttpPort;
  clock?: Clock;
  pace?: BackfillPace;
  listLimit?: number;
  /** 本次 run 最多取几条正文；用来切片跑，也用来模拟「跑一半被打断」。 */
  maxDetails?: number;
  /** 每步之前问一次要不要中断（模拟浏览器关掉 / SW 被回收）。 */
  shouldAbort?: () => boolean;
  /** 归档出口：产出与实时腿完全同形的 CapturedFetch，落盘逻辑不分叉。 */
  sink?: (captured: CapturedFetch) => Promise<void> | void;
}

export interface RunReport {
  stopped: StopReason;
  enumeratedPages: number;
  newDebts: number;
  archivedThisRun: string[];
  /** 枚举时被「已归档 ⇒ 不再入队」挡掉的条数。 */
  skippedAlreadyArchived: number;
  /** 枚举时已经在欠账里、无需重复入队的条数。 */
  skippedAlreadyPending: number;
  progress: string;
  halted: HaltRecord | null;
  /** 每次 gate() 实际等待的毫秒数，两段分开记。 */
  paceTrace: { enumerate: number[]; detail: number[] };
  state: BackfillState;
}

function isBackfillState(value: unknown): value is BackfillState {
  if (!value || typeof value !== 'object') return false;
  const s = value as Partial<BackfillState>;
  return (
    s.v === BACKFILL_STATE_VERSION &&
    Array.isArray(s.pending) &&
    Array.isArray(s.archived) &&
    typeof s.enumCursor === 'object'
  );
}

export async function loadState(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<BackfillState> {
  const raw = await store.load(stateKey(platform, scope));
  // 版本对不上就重开一个空集合，而不是拿旧结构硬凑。
  return isBackfillState(raw) ? raw : initialState(platform, scope);
}

async function persist(store: BackfillStore, state: BackfillState): Promise<void> {
  await store.save(stateKey(state.platform, state.scope), state);
}

/** 非 2xx 的分类：限流类 vs 其它。两者都停，但留痕的理由不同。 */
function haltReasonForStatus(status: number): HaltReason {
  if (status === 429 || status === 403 || status >= 500) return 'rate-limited';
  return 'shape-changed';
}

export async function runBackfill(opts: BackfillOptions): Promise<RunReport> {
  const clock = opts.clock ?? systemClock;
  const pace = opts.pace ?? DEFAULT_PACE;
  const http = opts.http ?? notWiredHttp;
  const listLimit = opts.listLimit ?? DEFAULT_LIST_LIMIT;
  const enumPacer = new Pacer(pace.enumerate, clock, 'enumerate');
  const detailPacer = new Pacer(pace.detail, clock, 'detail');

  const emptyTrace = { enumerate: enumPacer.waits, detail: detailPacer.waits };

  if (!opts.store) {
    // 没有持久化 ⇒ 没有可断可续 ⇒ 不许开跑。留痕只能进日志（存不下来）。
    const state = initialState(opts.platform, opts.scope);
    state.halted = {
      reason: 'storage-unavailable',
      at: clock.now(),
      detail: 'browser.storage.local unavailable; refusing to run without a resumable debt set',
    };
    console.warn('[chat-stasher] backfill halted: storage-unavailable');
    return {
      stopped: 'halted',
      enumeratedPages: 0,
      newDebts: 0,
      archivedThisRun: [],
      skippedAlreadyArchived: 0,
      skippedAlreadyPending: 0,
      progress: formatProgress(state),
      halted: state.halted,
      paceTrace: emptyTrace,
      state,
    };
  }

  const store = opts.store;
  const state = await loadState(store, opts.platform, opts.scope);
  const archivedThisRun: string[] = [];
  let enumeratedPages = 0;
  let newDebts = 0;
  let skippedAlreadyArchived = 0;
  let skippedAlreadyPending = 0;

  const halt = async (reason: HaltReason, detail: string): Promise<RunReport> => {
    state.halted = { reason, at: clock.now(), detail };
    await persist(store, state);
    // 只打技术细节，绝不打对话正文。
    console.warn(`[chat-stasher] backfill halted: ${reason} — ${detail}`);
    return report('halted');
  };

  const report = (stopped: StopReason): RunReport => ({
    stopped,
    enumeratedPages,
    newDebts,
    archivedThisRun,
    skippedAlreadyArchived,
    skippedAlreadyPending,
    progress: formatProgress(state),
    halted: state.halted,
    paceTrace: { enumerate: enumPacer.waits, detail: detailPacer.waits },
    state,
  });

  // 上一次已经停下留痕了：要人来看过、清掉 halted 才继续，绝不自己重试打平台。
  if (state.halted) return report('halted');

  const platformRow = getPlatformByOrigin(opts.origin);
  if (!platformRow) {
    return halt('shape-changed', `origin ${opts.origin} is not in the platform table`);
  }

  // ---- 第一段：枚举（便宜，一次跑完）----
  while (!state.enumCursor.complete) {
    if (opts.shouldAbort?.()) return report('aborted');
    await enumPacer.gate();
    const url = listPageUrl(opts.origin, state.enumCursor.offset, listLimit);
    let res: HttpResponse;
    try {
      res = await http(url);
    } catch (err) {
      return halt('transport-error', `list offset=${state.enumCursor.offset}: ${(err as Error).message}`);
    }
    if (res.status < 200 || res.status > 299) {
      return halt(
        haltReasonForStatus(res.status),
        `list offset=${state.enumCursor.offset} returned HTTP ${res.status}`,
      );
    }
    const parsed = parseConversationListPage(res.text);
    if (!parsed.ok) {
      return halt('shape-changed', `list offset=${state.enumCursor.offset}: ${parsed.detail}`);
    }
    enumeratedPages += 1;

    // 分母只认接口自己给的 total。拿不到就保持 'unknown'，进度那边会拒绝显示百分比。
    if (parsed.page.total !== null) {
      state.totalKnown = parsed.page.total;
      state.totalSource = 'response-total';
    }

    // 先量一下这一页里有多少是「已经清过账的」—— 这是「不重复抓」的直接证据。
    const archivedSet = new Set(state.archived);
    const pendingSet = new Set(state.pending);
    for (const id of parsed.page.ids) {
      if (archivedSet.has(id)) skippedAlreadyArchived += 1;
      else if (pendingSet.has(id)) skippedAlreadyPending += 1;
    }
    newDebts += enqueueDebts(state, parsed.page.ids).length;

    state.enumCursor.offset += parsed.page.ids.length;
    if (
      parsed.page.ids.length === 0 ||
      (state.totalKnown !== null && state.enumCursor.offset >= state.totalKnown)
    ) {
      state.enumCursor.complete = true;
    }
    await persist(store, state);
  }

  // ---- 第二段：逐条取正文（贵，必须温和）----
  const today = dayKeyOf(clock.now());
  if (state.detailToday.day !== today) {
    state.detailToday = { day: today, count: 0 };
  }
  const dailyCap = pace.detail.maxPerDay;
  const budget = opts.maxDetails ?? Number.POSITIVE_INFINITY;

  while (state.pending.length > 0) {
    if (opts.shouldAbort?.()) return report('aborted');
    if (archivedThisRun.length >= budget) return report('budget-exhausted');
    if (dailyCap !== null && state.detailToday.count >= dailyCap) return report('daily-cap');

    const id = nextDebt(state);
    if (id === null) break;

    await detailPacer.gate();
    const url = detailUrl(opts.origin, id);
    let res: HttpResponse;
    try {
      res = await http(url);
    } catch (err) {
      return halt('transport-error', `detail: ${(err as Error).message}`);
    }
    if (res.status < 200 || res.status > 299) {
      return halt(haltReasonForStatus(res.status), `detail returned HTTP ${res.status}`);
    }
    // 用实时腿同一个形状校验器：接口改了这里第一个知道。
    if (!matchesResponseShape(platformRow, res.text)) {
      return halt('shape-changed', `detail body does not match the ${platformRow.id} response shape`);
    }

    const captured: CapturedFetch = {
      url,
      method: 'GET',
      status: res.status,
      text: res.text,
      pageUrl: `${opts.origin}/c/${id}`,
      capturedAt: clock.now(),
    };
    await opts.sink?.(captured);

    settleDebt(state, id);
    state.detailToday.count += 1;
    archivedThisRun.push(id);
    // 每清一笔账立刻落盘 —— 可断可续的全部秘密就在这一行。
    await persist(store, state);
  }

  return report('queue-empty');
}
