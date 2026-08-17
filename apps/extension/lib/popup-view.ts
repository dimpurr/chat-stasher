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
  /** 🔴 取数通道到底接上没有。生产构建里恒为 false。 */
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
}

export interface PopupView {
  /** 第一行：开关本身处在什么状态。 */
  status: string;
  /** 第二行：🔴 到底在不在跑。 */
  running: string;
  /** 第三行：卡在哪一道、缺什么。不缺就是 null。 */
  missing: string | null;
  /** 第四行：进度（复用 C11）。 */
  progress: string;
  /** 补充说明，可为空。 */
  notes: string[];
  toggle: { label: string; checked: boolean; disabled: boolean };
}

/** 开关那一行的固定措辞。开关只表示「用户同意了」，不表示「它在跑」。 */
export const TOGGLE_LABEL = '自动回溯历史对话';

export function renderPopup(model: PopupModel): PopupView {
  const status = statusLine(model);
  const running = runningLine(model);
  const missing = missingLine(model) || null;
  const progress = progressLine(model);

  return {
    status,
    running,
    missing,
    progress,
    notes: notesFor(model),
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
    case 'no-http-port':
      // 🔴 这一条是 C18 的核心：开关打开了，但一条都没有在取。
      return '运行：未在运行 —— 开关已经打开了，但取数通道没有接上，一条也没有在取。';
    case null:
      // 四道闸门全过。仍然不说"正在归档"：这条腿是被实时腿唤醒时才动一下的，
      // 此刻（你正看着 Popup）它多半是静止的。
      return '运行：已就绪 —— 下一次你在受支持的平台上产生一次抓取时，会顺带清 1 笔账。';
  }
}

function missingLine(model: PopupModel): string {
  switch (model.block) {
    case 'no-store':
      return '缺：browser.storage.local。没有持久化就没有可断可续，'
        + '与其每次重启都从头爬一遍，不如不跑。';
    case 'no-http-port':
      return '缺：取历史列表和正文的取数通道（http 端口）。'
        + '当前版本【没有任何生产代码注入它】，所以这条腿走不到发请求那一步。'
        + '把开关打开只完成了「你同意了」这一半，另一半还没接上。';
    case 'download-paused':
      return '缺：需要你先确认下面的写入问题，然后手动恢复。';
    case 'disabled':
    case null:
      return '';
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

function notesFor(model: PopupModel): string[] {
  const notes: string[] = [];
  if (model.target) {
    notes.push(`这份进度对应：平台 ${model.target.platform} · 归档范围 ${model.target.scope}`);
  } else if (model.state === null) {
    notes.push('还没有任何平台的回溯记录，所以这里没有可显示的归档范围。');
  }

  if (!model.enabled) {
    notes.push(
      '打开之后会做什么：在你打开受支持平台的页面、并产生一次抓取时，'
      + '顺带把历史对话一笔一笔地慢慢补下来，每次只补一笔。关掉即停。',
    );
  }

  if (model.block === 'download-paused' && model.guard) {
    const p = computeProgress(model.state ?? emptyStateFor(model));
    // 复用 C12 已经写好的告警正文，不另写一份。
    notes.push(guardAlertDetail(model.guard, { archived: p.archived, pending: p.pending }));
  }

  if (model.state?.halted) {
    notes.push(
      `这条腿已经停下并留痕：${model.state.halted.reason} —— ${model.state.halted.detail}`,
    );
  }
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
  const lines = [view.status, view.running];
  if (view.missing) lines.push(view.missing);
  lines.push(view.progress);
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
