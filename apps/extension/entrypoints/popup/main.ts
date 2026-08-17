/**
 * C18 · Popup 的接线。
 *
 * 这是回溯腿的【第一个用户真的能打开的入口】。C13 把开关写成了函数
 * (setBackfillEnabled)，但全仓没有任何生产调用点 —— entrypoints/ 下只有
 * background 和两个 content script，用户无从打开它。这个文件补上那个调用点。
 *
 * 🔴 生产构建里的注册链（谁把它挂上去的）：
 *   entrypoints/popup/index.html 存在 ⇒ WXT 在 manifest 里生成
 *   action.default_popup = "popup.html"（Firefox MV2 是 browser_action）。
 *   ⇒ 用户点工具栏图标 ⇒ 浏览器打开 popup.html ⇒ 执行本文件。
 *   **不需要任何新权限**：action/browser_action 的 popup 不是一项权限。
 *
 * 🔴 点开之后的调用链：
 *   collect() → browser.runtime.sendMessage({type: POPUP_STATUS_MESSAGE})
 *             （这一脚会把 MV3 的 SW 叫醒，background 同步答 transportWired）
 *             → browserLocalStore() 读开关 / 读 C12 守卫状态
 *             → browserLocalSnapshot() 读欠账集合
 *             → tickBlockReason(...)【与 tickBackfill 同一个函数】
 *             → renderPopup(...) → paint()
 *   切开关 → setBackfillEnabled(store, on) → 再走一遍 collect() + paint()。
 *
 * 🔴 本文件【绝不】发起任何网络请求，也【绝不】触发一次回溯。
 *    打开开关只是写一个布尔值；真正的 tick 仍然只由实时腿唤醒。
 */

import {
  browserLocalStore,
  browserLocalSnapshot,
} from '../../lib/backfill/store';
import {
  isBackfillEnabled,
  setBackfillEnabled,
  tickBlockReason,
} from '../../lib/backfill/schedule';
import { syncBackfillAlarm, type AlarmsApi } from '../../lib/backfill/alarm';
import { isGuardTripped, loadGuardState, type GuardState } from '../../lib/download-guard';
import {
  pickBackfillState,
  renderPopup,
  POPUP_STATUS_MESSAGE,
  type BackfillRuntimeStatus,
  type PopupModel,
  type PopupView,
} from '../../lib/popup-view';

/**
 * 问 background 要运行时事实。问不到（SW 起不来 / 消息没人接）时
 * 【按最保守的方向回答：没接上】—— 宁可显示「未在运行」，也不许显示成在跑。
 */
async function askBackground(): Promise<BackfillRuntimeStatus> {
  try {
    const reply = await browser.runtime.sendMessage({ type: POPUP_STATUS_MESSAGE });
    if (reply && typeof (reply as BackfillRuntimeStatus).transportWired === 'boolean') {
      return reply as BackfillRuntimeStatus;
    }
  } catch (err) {
    console.warn('[chat-stasher] popup status query failed', (err as Error).message);
  }
  return { transportWired: false, lastTickReason: null };
}

async function collect(): Promise<PopupModel> {
  const store = browserLocalStore();
  const runtime = await askBackground();

  const enabled = await isBackfillEnabled(store);

  let guard: GuardState | null = null;
  if (store) {
    try {
      guard = await loadGuardState(store);
    } catch (err) {
      console.warn('[chat-stasher] popup guard read failed', (err as Error).message);
    }
  }

  let snapshot: Record<string, unknown> | null = null;
  try {
    snapshot = await browserLocalSnapshot();
  } catch (err) {
    console.warn('[chat-stasher] popup snapshot read failed', (err as Error).message);
  }
  const state = pickBackfillState(snapshot);

  // 🔴 与 tickBackfill 共用的那一个判断，顺序天然一致。
  const block = await tickBlockReason({
    hasStore: store !== null,
    isEnabled: () => enabled,
    isDownloadPaused: () => (guard ? isGuardTripped(guard) : false),
    hasHttp: runtime.transportWired,
  });

  return {
    enabled,
    block,
    guard,
    state,
    target: state ? { platform: state.platform, scope: state.scope } : null,
  };
}

function text(id: string, value: string): void {
  const el = document.getElementById(id);
  if (el) el.textContent = value;
}

function paint(view: PopupView): void {
  text('status', view.status);
  text('running', view.running);
  text('missing', view.missing ?? '');
  text('progress', view.progress);
  text('toggle-label', view.toggle.label);

  const toggle = document.getElementById('toggle') as HTMLInputElement | null;
  if (toggle) {
    toggle.checked = view.toggle.checked;
    toggle.disabled = view.toggle.disabled;
  }

  const notes = document.getElementById('notes');
  if (notes) {
    notes.textContent = '';
    for (const note of view.notes) {
      const p = document.createElement('p');
      // textContent（不是 innerHTML）：文案里可能带用户账号 scope，绝不当 HTML 解析。
      p.textContent = note;
      notes.appendChild(p);
    }
  }
}

async function refresh(): Promise<void> {
  paint(renderPopup(await collect()));
}

async function onToggle(on: boolean): Promise<void> {
  const store = browserLocalStore();
  // 存不住就别假装切成功了：立刻重画，UI 会退回真实取值。
  await setBackfillEnabled(store, on);
  // 🔴 C19：开关和闹钟必须同时改。以【存下来的真实取值】为准，不是以 `on` 为准 ——
  // 存储写失败时开关会退回原值，闹钟也必须跟着退回，不许出现"开关是关的但闹钟还在响"。
  const persisted = await isBackfillEnabled(store);
  const result = await syncBackfillAlarm(
    (browser as unknown as { alarms?: AlarmsApi }).alarms ?? null,
    persisted,
  );
  console.log('[chat-stasher] backfill alarm ->', result);
  await refresh();
}

document.getElementById('toggle')?.addEventListener('change', (ev) => {
  const on = (ev.target as HTMLInputElement).checked;
  void onToggle(on).catch((err) => {
    console.warn('[chat-stasher] popup toggle failed', (err as Error).message);
    void refresh();
  });
});

void refresh().catch((err) => {
  console.error('[chat-stasher] popup render failed', (err as Error).message);
});
