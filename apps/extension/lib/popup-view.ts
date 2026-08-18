/**
 * C18 · Popup 的【纯渲染层】。
 *
 * 为什么单独一个文件、且一行 DOM 都不碰：
 *  entrypoints/popup/main.ts 里没法在 node 里跑测试，而本任务最要命的一条判据
 *  ——「开着但没端口时【绝不许】显示成正在归档」—— 必须能被断言。
 *  所以把「显示什么」全部收进这里，main.ts 只负责取数据和塞进 DOM。
 *
 * 🔴 三条硬规矩：
 *  1. 进度文案【一律复用 lib/backfill/progress.ts 的 formatProgress】。
 *     本文件不许自己算百分比、不许自己拼 '%' —— 另写一份就等于把 C11 的
 *     「分母不可信就不许出现百分比」绕过去了。
 *  2. 「开」和「在跑」是两件事，必须分两行说。开关开着不等于它在动。
 *  3. 不出现任何「预计剩余 X 天 / X 小时」。我们没有速率模型，编一个就是骗人。
 */

import { formatProgress, computeProgress } from './backfill/progress';
import {
  describeFailureReason,
  droppedOf,
  failuresOf,
  MAX_FAILURES,
  type FailureEntry,
} from './backfill/failures';
import {
  BACKFILL_ALARM_PERIOD_MINUTES,
  type BackfillTickRecord,
} from './backfill/alarm';
import {
  BACKFILL_PARTIAL,
  BACKFILL_SUPPORTED_PLATFORMS,
  BACKFILL_UNSUPPORTED,
  BACKFILL_UNSUPPORTED_PLATFORMS,
} from './backfill/enumerate';
import { DEFAULT_DETAIL_PACE } from './backfill/pace';
import type { TickBlockReason } from './backfill/schedule';
import { stateKey, BACKFILL_STATE_VERSION, type BackfillState } from './backfill/types';
import { guardAlertDetail, type GuardState } from './download-guard';

/**
 * Popup ↔ background 的消息类型。
 * 常量放在这里而不是 background.ts：popup 只要 import 一个字符串，
 * 不该把整个 background 模块（连带 download / badge / engine）拖进 popup 的包。
 */
export const POPUP_STATUS_MESSAGE = 'cs-backfill-status';

/** background 回给 Popup 的运行时事实。 */
export interface BackfillRuntimeStatus {
  /**
   * 🔴 取数通道到底接上没有。
   * C18 时这里恒为 false（没有任何生产代码注入端口）。C19 之后它是
   * background 【现场 ping 一次】的结果：此刻有没有一个活着的、已登录的
   * 平台标签页可以替我们取数。仍然是事实，不是推测。
   */
  transportWired: boolean;
  /** 最近一次 tick 的结论；SW 被回收后会变回 null —— 那本身也是实话。 */
  lastTickReason: string | null;
}

export interface PopupModel {
  /** 开关的持久化取值。 */
  enabled: boolean;
  /**
   * 🔴 四道闸门里最先卡住的那一道，来自 lib/backfill/schedule.ts 的
   * tickBlockReason —— 与运行时 tickBackfill 用的是**同一个函数**。
   * null 表示四道全过。
   */
  block: TickBlockReason | null;
  /** C12 守卫状态；拿不到就是 null。 */
  guard: GuardState | null;
  /** 欠账集合。null 表示 storage 里还没有这个集合（一次都没跑过）。 */
  state: BackfillState | null;
  /** 这份进度是哪个平台/哪个账号的。认不出来就是 null。 */
  target: { platform: string; scope: string } | null;
  /**
   * 🔴 C20 · 落盘失败清单（**跨所有平台/账号汇总**）。
   * 为什么要汇总而不是只看 model.state：进度那一栏只挑一份集合显示（挑已归档最多的），
   * 失败要是也只看那一份，另一个账号下丢掉的东西就会在 UI 上凭空消失 ——
   * 那正是「显示成一切正常」。
   */
  failures: FailureSummary;
  /**
   * 🔴 C30 · 闹钟最近一跳的留痕（存储里读的，不是内存）。
   * null = 存储里还没有这条记录 —— 那本身也是实话（闹钟还没醒过一次，
   * 或者刚装上还没到点），照实说，绝不编一条。
   */
  lastTick?: BackfillTickRecord | null;
}

/** 汇总后的失败清单。entries 已按时间从新到旧排好。 */
export interface FailureSummary {
  entries: FailureEntry[];
  /** 因超过上限被丢掉的更早的失败条数。🔴 绝不静默截断。 */
  dropped: number;
}

export interface PopupView {
  /** 第一行：开关本身处在什么状态。 */
  status: string;
  /**
   * 🔴 C20 · 第二行：**有东西没存下来的时候，这一行必须出现。**
   * 没有失败项时是 null（那时候「一切正常」才是实话）。
   * 位置刻意排在「在不在跑」之前：一条腿跑得再顺，也不该盖过「有东西丢了」。
   */
  failures: string | null;
  /** 第三行：🔴 到底在不在跑。 */
  running: string;
  /** 第三行：卡在哪一道、缺什么。不缺就是 null。 */
  missing: string | null;
  /** 第四行：进度（复用 C11）。 */
  progress: string;
  /**
   * 🔴 C22 · 第五行：**哪些平台补得回历史、哪些暂时补不回。**
   *
   * 为什么这一行必须存在：用户装了扩展、把开关打开了，结果他常用的那个平台
   * 一条历史都没动 —— 在他眼里这和「坏了」没有任何区别。
   * 实时腿支持 5 个平台，回溯腿只写了 1 个；不说这件事，就是让用户
   * 对着一个永远不动的进度条自己猜。
   *
   * 🔴 它与 lib/backfill/enumerate.ts 的两张表【同源】：不是在这里手写一份平台名单，
   *    所以将来谁填上了一个平台，这行文案自己就变了，不会漂。
   */
  coverage: string;
  /** 补充说明，可为空。 */
  notes: string[];
  toggle: { label: string; checked: boolean; disabled: boolean };
  /** 🔴 C20 · 「我知道了 / 清空失败清单」按钮。没有失败项时不显示。 */
  clearFailures: { label: string; visible: boolean };
}

/** 开关那一行的固定措辞。开关只表示「用户同意了」，不表示「它在跑」。 */
export const TOGGLE_LABEL = '自动回溯历史对话';

/** 清空按钮的固定措辞。「我知道了」而不是「重试」—— 按下去什么都不会被重抓。 */
export const CLEAR_FAILURES_LABEL = '我知道了，清空这份清单';

/** 空清单。给 model 用的常量，省得各处手拼。 */
export const NO_FAILURES: FailureSummary = { entries: [], dropped: 0 };

export function renderPopup(model: PopupModel): PopupView {
  const status = statusLine(model);
  const running = runningLine(model);
  const missing = missingLine(model) || null;
  const progress = progressLine(model);
  const hasFailures = model.failures.entries.length > 0 || model.failures.dropped > 0;

  return {
    status,
    failures: hasFailures ? failuresLine(model.failures) : null,
    running,
    missing,
    progress,
    coverage: coverageLine(),
    notes: notesFor(model),
    clearFailures: { label: CLEAR_FAILURES_LABEL, visible: hasFailures },
    toggle: {
      label: TOGGLE_LABEL,
      checked: model.enabled,
      // 🔴 熔断态下开关【仍然可切】：熔断是落盘出口的问题，不是"用户不许反悔"。
      // 只有存储不可用时切了也存不住，那才禁用 —— 并且 missing 行会说清原因。
      disabled: model.block === 'no-store',
    },
  };
}

function statusLine(model: PopupModel): string {
  if (model.block === 'download-paused') {
    return '状态：开关是开的，但已因归档写入连续停滞而自动暂停';
  }
  if (model.enabled) return '状态：开关是开的';
  return '状态：开关是关的（这是默认值，需要你手动打开）';
}

/**
 * 🔴 本文件最重要的一个函数。
 * 每一个分支都必须让用户看出「在跑 / 没在跑」，**不许有含糊的第三种说法**。
 */
function runningLine(model: PopupModel): string {
  switch (model.block) {
    case 'no-store':
      return '运行：未在运行 —— 浏览器存储不可用。';
    case 'disabled':
      return '运行：未在运行 —— 开关没有打开。';
    case 'download-paused':
      return '运行：未在运行 —— 已经暂停，欠账原封不动地留着。';
    case 'no-targets':
      // 🔴 C30 · 这一条与 'no-http-port' 是【两件事】，必须说成两句话：
      //    通道可能好端端地接着（平台页面就开着），但我们连"从哪个账号、
      //    哪个平台开始补"都还不知道 —— 登记表是空的。
      // 🔴 C32 · 末尾这半句是刻意的：这一行本身只说「没在跑」，
      //    读完它的人下一秒就会问「那我该干什么」—— 答案就在紧接着的下一行，
      //    所以这里明确把他指过去，不让他以为这就是全部。
      return '运行：未在运行 —— 开关已经打开了，但还没有任何回溯目标，'
        + '所以闹钟每次醒来都无事可做，一条也没有在取。'
        + '下面那一行写了你要做的那一件事。';
    case 'no-http-port':
      // 🔴 这一条是 C18 的核心，C19 也没有把它拿掉：开关开着但一条都没在取，
      //    必须照实说。变的只是原因 —— 现在是「没有开着的平台页面」。
      return '运行：未在运行 —— 开关已经打开了，但此刻没有可用的取数通道，一条也没有在取。';
    case null:
      // 🔴 四道闸门全过 = 开关开着 + 存储在 + 没熔断 + 【此刻真的有一个活着的、
      // 已登录的平台标签页可以取数】。闹钟也已经在跑（开关打开时创建）。
      // 只有到这一步才允许说"在归档"。
      return `运行：正在归档 —— 每 ${BACKFILL_ALARM_PERIOD_MINUTES} 分钟自动清 1 笔账`
        + `（每天最多 ${DEFAULT_DETAIL_PACE.maxPerDay} 笔，每笔之间至少隔 `
        + `${Math.round(DEFAULT_DETAIL_PACE.minIntervalMs / 1000)} 秒）。`;
  }
}

function missingLine(model: PopupModel): string {
  switch (model.block) {
    case 'no-store':
      return '缺：browser.storage.local。没有持久化就没有可断可续，'
        + '与其每次重启都从头爬一遍，不如不跑。';
    case 'no-http-port':
      // 🔴 C19 改了这条文案的【原因】，因为原因真的变了：端口现在有生产注入了，
      // 但它必须借用一个开着的、已登录的平台页面。没有页面开着就是没有通道。
      return '缺：一个开着的、已登录的受支持平台页面 —— 取数通道要借它才能建立。'
        + '历史对话只在你自己的浏览器页面里取（同源请求，用的是你本来就有的登录态），'
        + '所以只要没有任何受支持平台的标签页开着，这条腿就取不到数。'
        + '打开其中任意一个平台的页面并保持开着，它就会自己继续。';
    case 'no-targets':
      // 🔴 C32 · 这一条必须把话说【全】。C30 已经把「未在运行」说对了，
      //    但用户读完之后的下一个问题是「那我要做什么才会开始？」——
      //    不回答它，这句诚实的话在用户眼里和「坏了」没有区别。
      //
      //    三件事一件都不许少，测试 tests/c32-coldstart.test.ts 逐条钉着：
      //      1. 【具体动作】——「请稍候」这种废话不算数；
      //      2. 【为什么】—— 这不是系统限制，是一句隐私承诺：我们不猜你的账号；
      //      3. 【不承诺做不到的事】—— 我们没有速率模型，绝不说「几分钟内就会开始」。
      //
      //    🔴 这里【只补文案】。登记目标必须先有一次真实捕获，这是 alarm.ts:80-87
      //    有意的设计（闹钟醒来时 SW 全新、没有 tab 也没有账号，唯一不用编的
      //    信息就是实时腿现成攥着的那一个），本单一个字都没有去改它。
      return '缺：一次真的被归档过的对话 —— 回溯要从它身上才知道该补哪个平台、哪个账号的历史。\n'
        + '你现在要做的一件事：在这个平台上发一条消息，或者打开一条你已有的对话，'
        + '让它被【实时归档】一次（照常存进你的下载目录）。'
        + '捕获到那一次之后，我们才会记下这个平台和这个账号，回溯就从那个账号开始往回补。\n'
        + '🔴 为什么非得先有这一次：我们不猜你的账号，只用你真的用过的那一个。'
        + '这个扩展没有任何 host 权限，历史只在你自己已经打开、已经登录的页面里取；'
        + '在你真的用过一次之前，我们既不会去猜一个账号，也不会把整个平台的会话都翻一遍。\n'
        + '（补完要多久取决于你自己有多少历史、页面开着多久 —— 这里不给时间承诺，'
        + '进度会一笔一笔显示在下面。）';
    case 'download-paused':
      return '缺：需要你先确认下面的写入问题，然后手动恢复。';
    case 'disabled':
    case null:
      return '';
  }
}

/**
 * 🔴 C20 · 失败清单那一行。三件事必须说全，一件都不许少：
 *   1. **几条没存下来**（数字来自真实条目，不估）；
 *   2. **不会自动再试**（这是产品拍板的行为，用户必须知道，否则他会以为等一等就好了）；
 *   3. 被上限丢掉的更早的条数（有就说，🔴 绝不静默截断）。
 * 🔴 这一行【不猜原因】（C12 那条规矩）—— 具体理由码在下面的 notes 里逐条列，
 *    每一条也只陈述我们自己观测到的事实。
 */
function failuresLine(summary: FailureSummary): string {
  const n = summary.entries.length;
  const head = `🔴 失败：有 ${n} 条历史对话取到了正文、但【没有存下来】，而且【不会自动再试】。`;
  if (summary.dropped > 0) {
    return head
      + `（清单最多留 ${MAX_FAILURES} 条，另有更早的 ${summary.dropped} 条已经不在清单里。）`;
  }
  return head;
}

/** 时间戳 → 本地时间字符串。拿到的不是有限数就照实说「时间不详」，绝不编一个。 */
function stampOf(at: number): string {
  if (!Number.isFinite(at)) return '时间不详';
  try {
    return new Date(at).toISOString().replace('T', ' ').slice(0, 19) + ' UTC';
  } catch {
    return '时间不详';
  }
}

function failureNote(summary: FailureSummary): string {
  const lines = [
    '这几条没有存下来（按时间从新到旧）：',
    ...summary.entries.map(
      (e) => `· ${e.platform} · 会话 ${e.shortId}… · ${describeFailureReason(e.reason)} · ${stampOf(e.at)}`,
    ),
    '',
    '🔴 它们不会被自动重试 —— 这是刻意的：同一个出口刚刚已经失败过一次，'
    + '闷头再试一遍只会把同一个失败重复一遍，还会让你以为它已经好了。',
    '欠账账本上它们既不算「已归档」也不再排队，所以进度里的数字没有把它们冒充成成功。',
    '清空这份清单只是「我知道了」，不会触发任何重新抓取。',
  ];
  return lines.join('\n');
}

/**
 * 🔴 C30 · 「闹钟到底醒过没有、那一跳做了什么」。
 *
 * 为什么这一段必须存在：状态行说的是【此刻】的闸门，而用户真正的疑问是
 * 「这几个小时里它一直在做什么」。C30 之前那个答案不存在于任何地方 ——
 * 闹钟每 5 分钟醒一次、每次静默跳过，既不报错也不留痕。
 * 🔴 这里只复述存储里那条记录，一个字都不推断。
 */
function lastTickNote(rec: BackfillTickRecord | null): string | null {
  if (!rec) {
    return '闹钟：存储里还没有任何一次闹钟跳动的记录 —— 可能它还没醒过第一次。'
      + '（这一条是照实说"我不知道"，不是"它没在跑"。）';
  }
  const when = stampOf(rec.at);
  if (rec.ran) {
    return `闹钟：最近一次醒来是 ${when}，那一跳真的跑了（登记表里有 ${rec.targets} 个回溯目标）。`;
  }
  return `闹钟：最近一次醒来是 ${when}，那一跳【什么都没做】——`
    + `${describeTickReason(rec.reason)}（当时登记表里有 ${rec.targets} 个回溯目标）。`;
}

/** 具名结局 → 一句人话。🔴 每一种都必须说得不一样，否则具名就白具了。 */
export function describeTickReason(reason: string): string {
  switch (reason) {
    case 'no-targets':
      return '还没有任何回溯目标，不知道该补哪个平台、哪个账号的历史';
    case 'no-http-port':
      return '没有可用的取数通道（当时没有一个开着的受支持平台页面）';
    case 'disabled':
      return '开关当时是关的';
    case 'no-store':
      return '浏览器存储当时不可用';
    case 'download-paused':
      return '归档写入连续停滞，这条腿已被自动暂停';
    case 'already-running':
      return '上一跳还没结束，这一跳直接让路了';
    case 'ran':
      return '它跑了';
    default:
      // 🔴 认不出来就照原样报出去，绝不吞掉一个我们没见过的结局。
      return `结局码：${reason}`;
  }
}

function progressLine(model: PopupModel): string {
  // 🔴 唯一的进度文案来源。本文件不参与任何百分比计算。
  if (!model.state) {
    return '进度：还没有开始 —— 存储里还没有这个平台的欠账集合，'
      + '也就是说一条都还没有枚举过。';
  }
  return `进度：${formatProgress(model.state)}`;
}

/**
 * 🔴 C22 · 回溯覆盖那一行。三件事必须说全：
 *   1. **哪些平台补得回历史**（名单来自 BACKFILL_PLANS，不是手写的）；
 *   2. **哪些暂时补不回**（名单来自 BACKFILL_UNSUPPORTED）；
 *   3. **补不回 ≠ 坏了 ≠ 没有历史** —— 这句必须写死在文案里，
 *      因为「一动不动」在用户眼里默认就是「坏了」。
 * 🔴 这一行不提任何进度、不提任何时间预估，也不出现百分号。
 */
export function coverageLine(): string {
  const yes = BACKFILL_SUPPORTED_PLATFORMS.join('、');
  const no = BACKFILL_UNSUPPORTED_PLATFORMS.join('、');
  if (no.length === 0) {
    return `回溯覆盖：目前支持的平台（${yes}）都能补历史对话。`;
  }
  return `回溯覆盖：现在只有 ${yes} 能补回历史对话；${no} 【还不能】。`
    + '它们的实时归档照常工作（你正在看的那条对话仍然会被存下来），'
    + '只是过去的历史暂时补不回来 —— 这不是坏了，也不是你没有历史，是我们还没学会读它们的历史列表。';
}

/** 逐平台缺什么。放进 notes，给愿意多看一眼的用户 —— 主行只说结论。 */
function coverageNote(): string {
  const lines = ['这些平台暂时补不回历史，各自卡在哪一步：'];
  // 🔴 C26 · 「只列得出会话、还取不到正文」的平台也要出现在这里。
  //    它离「能补历史」更近，但对用户的结果仍然是【一条都没补回来】——
  //    所以它属于这一段，不许被写成一句听起来像已经支持了的话。
  for (const half of BACKFILL_PARTIAL) {
    lines.push(`· ${half.userNote}`);
  }
  for (const gap of BACKFILL_UNSUPPORTED) {
    lines.push(`· ${gap.userNote}`);
  }
  lines.push('');
  lines.push(
    '🔴 我们不会为了让它「看起来在动」而去猜一个接口地址：猜错的结果不是报错，'
    + '而是你以为历史在补、实际上一条都没补。宁可在这里明说还不支持。',
  );
  return lines.join('\n');
}

function notesFor(model: PopupModel): string[] {
  const notes: string[] = [];
  // 🔴 失败详情排在所有 note 的最前面。有东西丢了就先说这件事。
  if (model.failures.entries.length > 0) notes.push(failureNote(model.failures));

  const tickNote = lastTickNote(model.lastTick ?? null);
  if (tickNote) notes.push(tickNote);

  if (model.target) {
    notes.push(`这份进度对应：平台 ${model.target.platform} · 归档范围 ${model.target.scope}`);
  } else if (model.state === null) {
    notes.push('还没有任何平台的回溯记录，所以这里没有可显示的归档范围。');
  }

  if (!model.enabled) {
    notes.push(
      `打开之后会做什么：只要有一个受支持平台的页面开着，就每 ${BACKFILL_ALARM_PERIOD_MINUTES} 分钟`
      + '在后台悄悄补一笔历史对话，用的是你自己那个页面的登录态；'
      + `每天最多 ${DEFAULT_DETAIL_PACE.maxPerDay} 笔，好几天里慢慢补完。关掉即停（定时器也会一并清掉）。`,
    );
  }

  if (model.block === 'download-paused' && model.guard) {
    const p = computeProgress(model.state ?? emptyStateFor(model));
    // 复用 C12 已经写好的告警正文，不另写一份。
    notes.push(guardAlertDetail(model.guard, { archived: p.archived, pending: p.pending }));
  }

  if (model.state?.halted) {
    // 🔴 C22 · 'unsupported-platform' 不是「出故障停下了」，是「这个平台我们还没写」。
    //    两者都要留痕，但绝不能说成同一句话。
    if (model.state.halted.reason === 'unsupported-platform') {
      notes.push(
        `这个平台（${model.state.platform}）的历史回溯还没有实现，所以这条腿在发出任何请求之前就停住了。`
        + '这不是平台改版，也不是被限流。技术细节：'
        + model.state.halted.detail,
      );
    } else if (model.state.halted.reason === 'detail-unsupported') {
      // 🔴 C26 · 这一条【不能】说成「在发出任何请求之前就停住了」—— 列表请求真的发过，
      //    会话也真的列出来了。半路停下和一步没走，对用户是两件事。
      notes.push(
        `这个平台（${model.state.platform}）的历史会话【已经列出来了】（${model.state.pending.length} 条在等着），`
        + '但取每条对话正文的那一步还没有实现，所以这条腿在取第一条正文之前就停住了，'
        + '目前一条也没有存下来。这不是平台改版，也不是被限流；列出来的这些会一直留着，'
        + '等那一步补上就接着往下清。技术细节：'
        + model.state.halted.detail,
      );
    } else {
      notes.push(
        `这条腿已经停下并留痕：${model.state.halted.reason} —— ${model.state.halted.detail}`,
      );
    }
  }

  // 🔴 覆盖说明排在最后：它是长期事实，不是此刻的状态。
  if (BACKFILL_UNSUPPORTED.length > 0 || BACKFILL_PARTIAL.length > 0) notes.push(coverageNote());
  return notes;
}

function emptyStateFor(model: PopupModel): BackfillState {
  return {
    v: BACKFILL_STATE_VERSION,
    platform: model.target?.platform ?? 'unknown',
    scope: model.target?.scope ?? 'default',
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pending: [],
    archived: [],
    detailToday: { day: '', count: 0 },
    halted: null,
  };
}

/** 把一份 view 拍平成纯文本 —— 测试断言和「贴出完整文案」都用它。 */
export function popupText(view: PopupView): string {
  const lines = [view.status];
  if (view.failures) lines.push(view.failures);
  lines.push(view.running);
  if (view.missing) lines.push(view.missing);
  lines.push(view.progress);
  lines.push(view.coverage);
  for (const n of view.notes) lines.push('', n);
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// 从 storage.local 的全量快照里挑一份欠账集合。
// Popup 不知道用户当前是哪个账号（那信息只在实时腿的消息里现成带着），
// 所以只能把已经存在的集合列出来。挑「已归档最多」的那一份显示 —— 稳定、可解释，
// 不依赖任何插入顺序。
// ---------------------------------------------------------------------------

const STATE_KEY_PREFIX = `cs_backfill_v${BACKFILL_STATE_VERSION}:`;

function looksLikeState(value: unknown): value is BackfillState {
  if (!value || typeof value !== 'object') return false;
  const s = value as Partial<BackfillState>;
  return s.v === BACKFILL_STATE_VERSION
    && typeof s.platform === 'string'
    && typeof s.scope === 'string'
    && Array.isArray(s.pending)
    && Array.isArray(s.archived);
}

export function pickBackfillState(snapshot: Record<string, unknown> | null): BackfillState | null {
  if (!snapshot) return null;
  let best: BackfillState | null = null;
  for (const [key, value] of Object.entries(snapshot)) {
    if (!key.startsWith(STATE_KEY_PREFIX)) continue;
    if (!looksLikeState(value)) continue;
    // 键里带着 platform/scope，值里也带着；只认两边对得上的，避免显示一份错位的进度。
    if (stateKey(value.platform, value.scope) !== key) continue;
    if (!best || value.archived.length > best.archived.length) best = value;
  }
  return best;
}

/** 快照里所有合法的欠账集合（键值一致的那些）。失败汇总和清空都要遍历它。 */
export function backfillStateEntries(
  snapshot: Record<string, unknown> | null,
): Array<{ key: string; state: BackfillState }> {
  if (!snapshot) return [];
  const out: Array<{ key: string; state: BackfillState }> = [];
  for (const [key, value] of Object.entries(snapshot)) {
    if (!key.startsWith(STATE_KEY_PREFIX)) continue;
    if (!looksLikeState(value)) continue;
    if (stateKey(value.platform, value.scope) !== key) continue;
    out.push({ key, state: value });
  }
  return out;
}

/**
 * 🔴 C20 · 把【所有】平台/账号的失败清单汇总成一份。
 * 进度那一栏只挑一份集合显示；失败要是也只看那一份，另一个账号下丢掉的东西
 * 就会在 UI 上凭空消失 —— 那正是「把有失败项显示成一切正常」。
 * 排序：时间从新到旧（最近发生的最有诊断价值）。
 */
export function collectFailures(snapshot: Record<string, unknown> | null): FailureSummary {
  const entries: FailureEntry[] = [];
  let dropped = 0;
  for (const { state } of backfillStateEntries(snapshot)) {
    entries.push(...failuresOf(state));
    dropped += droppedOf(state);
  }
  entries.sort((a, b) => b.at - a.at);
  return { entries, dropped };
}
