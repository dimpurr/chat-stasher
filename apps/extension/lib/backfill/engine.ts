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
import { dropDebt, enqueueDebts, nextDebt, settleDebt } from './debts';
import { recordFailure, type FailureEntry, type FailureReason } from './failures';
import {
  backfillPlanFor,
  detailRequestInit,
  listRequestInit,
  unsupportedBackfillFor,
  DEFAULT_LIST_LIMIT,
  type BackfillRequestInit,
} from './enumerate';
import { formatProgress } from './progress';
import { DEFAULT_PACE, Pacer, systemClock, type BackfillPace, type Clock } from './pace';
import type { BackfillStore } from './store';
import {
  BACKFILL_STATE_VERSION,
  dayKeyOf,
  initialState,
  stateKey,
  type BackfillState,
  type EnumTruncation,
  type HaltReason,
  type HaltRecord,
  type StopReason,
} from './types';

export interface HttpResponse {
  status: number;
  text: string;
}

/**
 * 🔴 C23 · 通道现在能表达 POST。
 *
 * 改动只有一处：多了一个**可选**的第二参数。为什么这样改而不是换个新类型：
 *  · 既有的所有实现（测试夹具、tabHttpPort）都是 `(url) => ...`，
 *    在 TypeScript 里少写参数依然可赋值 ⇒ **一行都不用改就还能用**；
 *  · engine 在 GET 段【逐字仍然调用 `http(url)`】，一个参数都不多传
 *    （见 sendVia），所以 ChatGPT 那条路上连实参个数都没变。
 *
 * init 省略 ⇒ GET 且无 body。方法/Content-Type/body 顶层键三样都是闭集，
 * 声明在 lib/backfill/enumerate.ts，强制在 lib/backfill/tab-port.ts。
 */
export type HttpPort = (url: string, init?: BackfillRequestInit) => Promise<HttpResponse>;

/** 🔴 GET 且无 body ⇒ 逐字退回旧调用 `http(url)`。向后兼容的落点就是这一行。 */
async function sendVia(http: HttpPort, url: string, init: BackfillRequestInit): Promise<HttpResponse> {
  if (init.method === 'GET' && init.body === undefined) return http(url);
  return http(url, init);
}

/** 默认端口：故意会炸。没接线就绝不会有网络行为。 */
export const notWiredHttp: HttpPort = async (url: string) => {
  throw new Error(`[chat-stasher] backfill http port is not wired (refused to fetch ${new URL(url).pathname})`);
};

/**
 * 🔴 C20 · sink 回答的那件事：**到底存下来了没有。**
 * 与 entrypoints/background.ts 的 HandledResult 结构相容（那边就是原样 return 回来）。
 */
export interface SinkOutcome {
  saved: boolean;
  /** 没存下来时的技术理由。只进日志和失败清单，绝不含正文。 */
  reason?: string;
  /**
   * 🔴 落盘时【实际用来命名】的那个身份。根因治理的另一半：
   * 欠账键来自列表接口的 items[].id，文件名来自「从 URL 再抠一次」——
   * 以前中间没有任何一致性校验。把它带回来，engine 就能当场对一次账。
   * 出口不报这个字段（undefined）⇒ 不做这项校验，行为与 C19 逐字一致。
   */
  sessionId?: string;
}

/**
 * 把 sink 的返回值判成「存下来了 / 没存下来」。
 *
 * 🔴 **`undefined` 判成成功**，这是一个必须写清楚的妥协：
 *    老的 `sink: async (c) => { ... }`（返回 void）在类型上仍然合法，
 *    engine 拿不到任何异议 ⇒ 只能当它成功。
 *    换句话说：**一个不报告结果的出口，仍然可以静默清账。**
 *    生产接线（entrypoints/background.ts 两处）已经改成 return handleCaptured 的结果，
 *    所以生产路径上不存在这个洞；但如果以后有人新接一个不 return 的 sink，
 *    这个洞会重新出现。要彻底堵死就得让 sink 的返回类型变成必填 —— 那会一次性
 *    弄红全部既有测试夹具，超出本任务的范围，所以这里选择【写明】而不是【偷偷收紧】。
 */
function sinkVerdict(
  outcome: SinkOutcome | void | undefined,
  debtId: string,
): { ok: true } | { ok: false; reason: FailureReason; detail: string } {
  if (!outcome) return { ok: true };
  if (outcome.saved !== true) {
    return { ok: false, reason: 'not-saved', detail: outcome.reason ?? 'sink reported saved:false' };
  }
  if (outcome.sessionId !== undefined && outcome.sessionId !== debtId) {
    return {
      ok: false,
      reason: 'identity-mismatch',
      // 只放长度，不放两个 id 本身：完整会话 id 不进任何日志。
      detail: `debt key (len ${debtId.length}) != file identity (len ${outcome.sessionId.length})`,
    };
  }
  return { ok: true };
}

export interface BackfillOptions {
  platform: string;
  /** 平台源，例如 https://chatgpt.com。必须命中 lib/contract.ts 的平台表。 */
  origin: string;
  /** 归档范围键（账号轴）。 */
  scope: string;
  store: BackfillStore | null;
  http?: HttpPort;
  /**
   * 🔴 测试接缝，与 http/clock/pace 同类：换一张 plan 表。
   * **生产代码从不设置它** —— background.ts 的两处调用（kickBackfill / runAlarmTick）
   * 都没有这个字段，所以线上恒为 backfillPlanFor，允许集一点没变大。
   * 存在的唯一理由：C23 要证明「通道能发 POST」，而生产表里【故意】还没有任何
   * POST 平台（kimi/gemini 的参数仍然没有出处，填了就是编）。
   */
  plans?: (platform: string) => import('./enumerate').BackfillEnumPlan | null;
  clock?: Clock;
  pace?: BackfillPace;
  listLimit?: number;
  /** 本次 run 最多取几条正文；用来切片跑，也用来模拟「跑一半被打断」。 */
  maxDetails?: number;
  /** 每步之前问一次要不要中断（模拟浏览器关掉 / SW 被回收）。 */
  shouldAbort?: () => boolean;
  /**
   * 归档出口：产出与实时腿完全同形的 CapturedFetch，落盘逻辑不分叉。
   *
   * 🔴 C20：返回值现在【说了算】—— 见 SinkOutcome 和 sinkVerdict()。
   *    以前这里是 `=> Promise<void> | void`，接线处 `await handleCaptured(c)` 之后
   *    把 HandledResult 丢在地上，于是「没落盘」和「落盘了」在欠账账本上是同一个结果。
   */
  sink?: (captured: CapturedFetch) => Promise<SinkOutcome | void> | SinkOutcome | void;
  /**
   * C12 下载停滞守卫的闸门：返回 true 表示「已熔断」，这条腿必须暂停。
   * 🔴 只暂停回溯腿（慢爬）。实时腿走 entrypoints/background.ts 的
   *    onMessage → handleCaptured，完全不经过这里，所以不受影响。
   * 不注入就等于没有守卫 —— 与 C11 的行为逐字一致。
   */
  downloadGuard?: () => boolean | Promise<boolean>;
}

export interface RunReport {
  stopped: StopReason;
  enumeratedPages: number;
  newDebts: number;
  archivedThisRun: string[];
  /** 🔴 C20：本次 run 里「取到了正文但没能落盘」的那几条，已进失败清单、不会重试。 */
  failedThisRun: FailureEntry[];
  /** 枚举时被「已归档 ⇒ 不再入队」挡掉的条数。 */
  skippedAlreadyArchived: number;
  /** 枚举时已经在欠账里、无需重复入队的条数。 */
  skippedAlreadyPending: number;
  progress: string;
  halted: HaltRecord | null;
  /**
   * 🔴 C26 · 枚举**没能走完**的具名理由；null = 没有这回事。
   * 非 null 时 state.enumCursor.complete 虽然是 true，但它的意思是「停在这里了」，
   * 不是「已经全部列完」。见 lib/backfill/types.ts 的 EnumTruncation。
   */
  enumTruncated: EnumTruncation | null;
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

  // 🔴 C19：Pacer 的构造挪到 loadState 之后 —— 它现在需要 state.lastFetchAt 当种子。
  // 没有 store 的那一支根本没跑过任何请求，等待序列必然是空的。
  const emptyTrace = { enumerate: [] as number[], detail: [] as number[] };

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
      failedThisRun: [],
      skippedAlreadyArchived: 0,
      skippedAlreadyPending: 0,
      progress: formatProgress(state),
      halted: state.halted,
      enumTruncated: null,
      paceTrace: emptyTrace,
      state,
    };
  }

  const store = opts.store;
  const state = await loadState(store, opts.platform, opts.scope);

  // 🔴 C19 · BUG-3：用落盘的「上一次取数时刻」给两段定速器播种，间隔于是跨 tick 生效。
  // 旧集合没有这个字段 ⇒ null ⇒ 与 C11 行为逐字一致。
  const anchors = state.lastFetchAt ?? { enumerate: null, detail: null };
  const enumPacer = new Pacer(pace.enumerate, clock, 'enumerate', anchors.enumerate);
  const detailPacer = new Pacer(pace.detail, clock, 'detail', anchors.detail);
  /** 把某一段刚刚放行的时刻写回 state（落盘由各自的 persist 负责）。 */
  const anchor = (segment: 'enumerate' | 'detail', at: number | null): void => {
    state.lastFetchAt = { ...(state.lastFetchAt ?? { enumerate: null, detail: null }), [segment]: at };
  };

  const archivedThisRun: string[] = [];
  const failedThisRun: FailureEntry[] = [];
  let enumeratedPages = 0;
  let newDebts = 0;
  let skippedAlreadyArchived = 0;
  let skippedAlreadyPending = 0;
  let enumTruncated: EnumTruncation | null = state.enumCursor.truncated ?? null;

  /**
   * 🔴 C26 · 「枚举读不下去了」的唯一出口。
   *
   * 它做三件事，一件都不能少：把游标停下（complete=true，别再空转打平台）、
   * 在**落盘的账本上**留一个具名标记（enumCursor.truncated）、并在本次 RunReport 里报出来。
   * complete=true 在这里【不是】「全部列完了」的意思 —— truncated 就是用来区分这两者的。
   * 少了它，「只回溯到第一页」和「一共就这一页」在账本上会长得一模一样。
   */
  const stopEnumerating = (why: EnumTruncation): void => {
    state.enumCursor.complete = true;
    state.enumCursor.truncated = why;
    enumTruncated = why;
    console.warn(
      `[chat-stasher] backfill enumeration stopped early: ${why} —`
      + ` enumerated ${state.enumCursor.offset} conversation(s) so far; this is NOT "no more history"`,
    );
  };

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
    failedThisRun,
    skippedAlreadyArchived,
    skippedAlreadyPending,
    progress: formatProgress(state),
    halted: state.halted,
    enumTruncated,
    paceTrace: { enumerate: enumPacer.waits, detail: detailPacer.waits },
    state,
  });

  // 上一次已经停下留痕了：要人来看过、清掉 halted 才继续，绝不自己重试打平台。
  if (state.halted) return report('halted');

  /** 熔断态 ⇒ 立刻收手。欠账不动、不落盘任何新状态：暂停不是失败。 */
  const guardTripped = async (): Promise<boolean> =>
    opts.downloadGuard ? await opts.downloadGuard() : false;
  if (await guardTripped()) return report('download-paused');

  const platformRow = getPlatformByOrigin(opts.origin);
  if (!platformRow) {
    return halt('shape-changed', `origin ${opts.origin} is not in the platform table`);
  }

  // 🔴 C22 · 在【发出任何请求之前】问一句：这个平台的枚举我们到底写没写过。
  //
  // 以前这里没有这一问，engine 直接用 ChatGPT 的 listPageUrl(origin) 拼 URL ——
  // 于是 DeepSeek 用户会被拿 `https://chat.deepseek.com/backend-api/conversations`
  // 去打自己的账号，拿回 404，然后账本上留下一条 'shape-changed'。
  // 那条留痕是**准确的谎话**：接口没变，是我们从来没写过它。
  const plan = (opts.plans ?? backfillPlanFor)(platformRow.id);
  if (!plan) {
    const gap = unsupportedBackfillFor(platformRow.id);
    // 平台表里有、但两张表都没登记 ⇒ 这是接线漏了，同样必须说得出口。
    const detail = gap
      ? `platform ${platformRow.id} has no backfill enumeration yet; missing: ${gap.missing.join(' | ')}`
      : `platform ${platformRow.id} is in the platform table but is registered in neither`
        + ' BACKFILL_PLANS nor BACKFILL_UNSUPPORTED (lib/backfill/enumerate.ts)';
    return halt('unsupported-platform', detail);
  }

  // ---- 第一段：枚举（便宜，一次跑完）----
  //
  // 🔴 C26：这里有两种翻页方式，**由 plan 说了算**，不是由响应内容临时决定的：
  //   · 声明了 listCursorUrl ⇒ 游标翻页（DeepSeek：count + before_seq_id）；
  //   · 没声明             ⇒ offset 翻页（ChatGPT，行为与 C22 逐字一致）。
  const cursorMode = plan.listCursorUrl !== undefined;
  // 留痕里说清「停在哪一页」。游标模式下 offset 是【已枚举条数】而不是请求参数，
  // 所以两种模式的说法必须不一样 —— 一句读起来对、其实指错东西的留痕，比没有更糟。
  const listWhere = (): string =>
    cursorMode
      ? `list cursor=${state.enumCursor.cursor ?? 'first-page'} (enumerated ${state.enumCursor.offset})`
      : `list offset=${state.enumCursor.offset}`;
  while (!state.enumCursor.complete) {
    if (opts.shouldAbort?.()) return report('aborted');
    if (await guardTripped()) return report('download-paused');
    await enumPacer.gate();
    anchor('enumerate', enumPacer.lastAt);
    const url = cursorMode
      ? plan.listCursorUrl!(opts.origin, state.enumCursor.cursor ?? null, listLimit)
      : plan.listUrl(opts.origin, state.enumCursor.offset, listLimit);
    // 🔴 C23：body 只可能出自 plan 自己的构造器（listRequestInit → spec.body）。
    //    没有任何一条路径能让页面侧或消息侧决定这里发什么。
    const init = listRequestInit(plan, opts.origin, state.enumCursor.offset, listLimit);
    let res: HttpResponse;
    try {
      res = await sendVia(http, url, init);
    } catch (err) {
      return halt('transport-error', `${listWhere()}: ${(err as Error).message}`);
    }
    if (res.status < 200 || res.status > 299) {
      return halt(
        haltReasonForStatus(res.status),
        `${listWhere()} returned HTTP ${res.status}`,
      );
    }
    const parsed = plan.parseListPage(res.text);
    if (!parsed.ok) {
      return halt('shape-changed', `${listWhere()}: ${parsed.detail}`);
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

    // offset 在两种模式下都往前走：offset 模式它【就是】下一页的参数，
    // 游标模式它只是「已经枚举过多少条」的计数（进度那边在用）。
    state.enumCursor.offset += parsed.page.ids.length;
    if (parsed.page.ids.length === 0) {
      state.enumCursor.complete = true;
    } else if (cursorMode) {
      // 🔴 游标翻页的终止判断【只认 has_more】。
      //    绝不用「这一页比 count 少 ⇒ 到底了」去推断 —— 那是拿未知当已知。
      if (parsed.page.hasMore === undefined) {
        // 响应里没有这个布尔信号 ⇒ 我们【不知道】后面还有没有。停，并具名留痕。
        stopEnumerating('has-more-missing');
      } else if (parsed.page.hasMore === false) {
        state.enumCursor.complete = true;
      } else if (parsed.page.nextCursor === null || parsed.page.nextCursor === undefined) {
        // 接口说还有下一页，但这一页没能给出游标 ⇒ 翻不过去。
        // 🔴 只回溯到了这一页，必须说出来，不许假装抓全了。
        stopEnumerating('cursor-missing');
      } else {
        state.enumCursor.cursor = parsed.page.nextCursor;
      }
    } else if (state.totalKnown !== null && state.enumCursor.offset >= state.totalKnown) {
      state.enumCursor.complete = true;
    }
    await persist(store, state);
  }

  // 🔴 C26 · 在发出【任何一条正文请求之前】问一句：这个平台的正文段我们写没写过。
  //
  // DeepSeek 就是这个中间态：列表段有四源交叉的出处（会话已经列出来、欠账已经落盘），
  // 正文段没有出处。这时候：
  //  · 绝不能继续往下跑 —— plan.detailUrl 是 null，硬跑就得现编一个正文路由；
  //  · 也绝不能悄悄 return 'queue-empty' —— 那等于说「补完了」，而一条正文都没取。
  // 所以它是一条**独立的、会落盘的** halt：欠账原封不动地留着，
  // 等正文段有了出处，接着这批欠账往下清就行。
  //
  // 🔴 只在【真的有欠账等着】时才 halt。一条欠账都没有（用户真的没有历史）时
  //    没有任何东西被这半条腿挡住，那就该走正常的 'queue-empty' ——
  //    否则「你没有历史」又会被写成一条停机记录，正是本仓库最想避免的那种混淆。
  if (!plan.detailUrl || !plan.detailPath) {
    if (state.pending.length === 0) return report('queue-empty');
    const gap = plan.partial?.missing.join(' | ') ?? 'detailPath / detailUrl';
    return halt(
      'detail-unsupported',
      `platform ${platformRow.id} can enumerate its conversation list`
      + ` (${state.pending.length} pending, ${state.archived.length} archived)`
      + ` but has no backfill detail route yet; missing: ${gap}`,
    );
  }
  const detailUrlOf = plan.detailUrl;

  // ---- 第二段：逐条取正文（贵，必须温和）----
  const today = dayKeyOf(clock.now());
  if (state.detailToday.day !== today) {
    state.detailToday = { day: today, count: 0 };
  }
  const dailyCap = pace.detail.maxPerDay;
  const budget = opts.maxDetails ?? Number.POSITIVE_INFINITY;

  while (state.pending.length > 0) {
    if (opts.shouldAbort?.()) return report('aborted');
    // 跑到一半才熔断也要立刻收手：每清一笔账都已落盘，停在这里不丢任何进度。
    if (await guardTripped()) return report('download-paused');
    if (archivedThisRun.length >= budget) return report('budget-exhausted');
    if (dailyCap !== null && state.detailToday.count >= dailyCap) return report('daily-cap');

    const id = nextDebt(state);
    if (id === null) break;

    await detailPacer.gate();
    // 🔴 在【发请求之前】就把时刻落盘。取正文这一段是真正要温和的那一段：
    // 万一 SW 在 fetch 中途被回收、或用户关掉浏览器，下一次 tick 也必须知道
    // 「刚刚已经取过一次了」，否则重启就成了绕过间隔的后门。
    // 代价是每笔账多一次 storage.local 写（约每 20 秒一次），可以忽略。
    anchor('detail', detailPacer.lastAt);
    await persist(store, state);
    const url = detailUrlOf(opts.origin, id);
    const init = detailRequestInit(plan, opts.origin, id);
    let res: HttpResponse;
    try {
      res = await sendVia(http, url, init);
    } catch (err) {
      // 🔴 POST 的失败与 GET 的失败走【同一行】：halt('transport-error') + 落盘留痕。
      //    没有为 POST 新开任何一条静默路径。
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
      // 🔴 C23：如实写实际发出的方法。GET 段仍然逐字是 'GET'（init.method 默认就是它）。
      method: init.method,
      status: res.status,
      text: res.text,
      pageUrl: `${opts.origin}/c/${id}`,
      capturedAt: clock.now(),
      // 🔴 C21 · 根因治理的落点：**身份只表达一次。**
      //    这条欠账的 id 就是列表接口给的 items[].id（enumerate.ts:64-68），
      //    这里原样带下去，落盘那边不再从 URL 里抠第二遍。
      //    ⇒「两个不同的欠账键塌成同一个文件名」在结构上不再可能：
      //      文件名片段 = 欠账键本身（恒等映射，见 contract.ts 的 pathSafeSessionId）。
      sessionId: id,
    };
    // 🔴 C20 · 本次修法的落点：**sink 的结果说了算。**
    //
    // 产品主人的原话：「丢了」和「丢了但你知道」是完全不同的两件事，
    // 而这个项目的立身之本就是后者。
    //
    // 以前这里是 `await opts.sink?.(captured); settleDebt(state, id);` ——
    // 不看出口有没有真的把文件存下来，欠账照样被划掉，那条对话从此永不再试、
    // 也永远不会有人知道它丢了。现在分两条路：
    //   存下来了 ⇒ settleDebt（清账，进 archived）
    //   没存下来 ⇒ recordFailure（进失败清单，🔴 不清账、不进 archived、不重试）
    const verdict = sinkVerdict(await opts.sink?.(captured), id);

    if (verdict.ok) {
      settleDebt(state, id);
      archivedThisRun.push(id);
    } else {
      // 🔴 从 pending 里拿掉但【不】进 archived：不重试是产品决定，
      //    冒充已归档则会让进度分子说谎 —— 两件事都不许发生。
      dropDebt(state, id);
      failedThisRun.push(recordFailure(state, { id, reason: verdict.reason, at: clock.now() }));
      // 只打技术细节，绝不打对话正文，也绝不打完整 URL / 完整会话 id。
      console.warn(`[chat-stasher] backfill sink did not save: ${verdict.reason} — ${verdict.detail}`);
    }

    // 无论成败都算一次「今天取过的正文」：请求已经真的发出去了，
    // 不计数就等于给失败开了一条绕过每日配额的后门。
    state.detailToday.count += 1;
    // 每清一笔账（或记一条失败）立刻落盘 —— 可断可续的全部秘密就在这一行。
    await persist(store, state);
  }

  return report('queue-empty');
}
