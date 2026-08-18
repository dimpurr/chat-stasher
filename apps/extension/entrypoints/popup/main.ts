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
import {
  loadLastTick,
  loadTargets,
  syncBackfillAlarm,
  type AlarmsApi,
} from '../../lib/backfill/alarm';
import { isGuardTripped, loadGuardState, type GuardState } from '../../lib/download-guard';
import {
  backfillStateEntries,
  collectFailures,
  pickBackfillState,
  renderPopup,
  POPUP_STATUS_MESSAGE,
  type BackfillRuntimeStatus,
  type PopupModel,
  type PopupView,
} from '../../lib/popup-view';
import { clearFailures } from '../../lib/backfill/failures';

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

  // 🔴 C30 · 回溯目标登记表。这是闹钟那条路【唯一】的目标来源，
  //    所以 Popup 必须读它 —— 只看 transportWired 就等于拿"通道通不通"
  //    去回答"有没有活要干"，而那两件事根本不是一回事。
  //    真机上正是这一处让 Popup 宣称「正在归档」，而闹钟每次都无事可做。
  const targets = await loadTargets(store);
  // 闹钟最近一跳做了什么（存储里读的；SW 被回收也还在）。
  const lastTick = await loadLastTick(store);

  // 🔴 与 tickBackfill 共用的那一个判断，顺序天然一致。
  const block = await tickBlockReason({
    hasStore: store !== null,
    isEnabled: () => enabled,
    isDownloadPaused: () => (guard ? isGuardTripped(guard) : false),
    hasHttp: runtime.transportWired,
    hasTargets: targets.length > 0,
  });

  return {
    enabled,
    block,
    guard,
    state,
    target: state ? { platform: state.platform, scope: state.scope } : null,
    // 🔴 C20：跨所有平台/账号汇总。读不到快照 ⇒ 空清单（那时候我们确实什么都不知道）。
    failures: collectFailures(snapshot),
    lastTick,
  };
}

/**
 * 🔴 C20 · 「我知道了，清空这份清单」。
 * 遍历快照里每一份欠账集合，把 failures / failuresDropped 清零后写回去。
 * **不触发任何重新抓取** —— 这是产品拍板的「不重试」，按钮只表示「我看到了」。
 * 清完之后那几条会话既不在 pending 也不在 archived，所以它们不会再被自动碰到。
 */
async function onClearFailures(): Promise<void> {
  const store = browserLocalStore();
  if (!store) return;
  let snapshot: Record<string, unknown> | null = null;
  try {
    snapshot = await browserLocalSnapshot();
  } catch (err) {
    console.warn('[chat-stasher] popup snapshot read failed', (err as Error).message);
    return;
  }
  for (const { key, state } of backfillStateEntries(snapshot)) {
    if (state.failures === undefined && !state.failuresDropped) continue;
    clearFailures(state);
    await store.save(key, state);
  }
  await refresh();
}

function text(id: string, value: string): void {
  const el = document.getElementById(id);
  if (el) el.textContent = value;
}

function paint(view: PopupView): void {
  text('status', view.status);
  // 🔴 C20：有失败项时这一行必须出现在最显眼的位置；没有时整块隐藏，
  //    绝不留一个空壳让用户以为「这里本来就该是空的」。
  text('failures', view.failures ?? '');
  const failBox = document.getElementById('failures');
  if (failBox) failBox.hidden = view.failures === null;

  const clearBtn = document.getElementById('clear-failures') as HTMLButtonElement | null;
  if (clearBtn) {
    clearBtn.textContent = view.clearFailures.label;
    clearBtn.hidden = !view.clearFailures.visible;
  }

  text('running', view.running);
  text('missing', view.missing ?? '');
  text('progress', view.progress);
  // 🔴 C22：哪些平台补得回历史、哪些暂时补不回。永远显示 —— 它不是错误提示，
  //    而是「你那个平台一动不动是为什么」的答案，必须在用户想问之前就在那儿。
  text('coverage', view.coverage);
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

document.getElementById('clear-failures')?.addEventListener('click', () => {
  void onClearFailures().catch((err) => {
    console.warn('[chat-stasher] popup clear-failures failed', (err as Error).message);
    void refresh();
  });
});

void refresh().catch((err) => {
  console.error('[chat-stasher] popup render failed', (err as Error).message);
});
