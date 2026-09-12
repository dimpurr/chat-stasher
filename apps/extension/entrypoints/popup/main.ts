/**
 * C18 · Popup 的接线。
 *
 * W2 之后它多了三件事，每一件都只显示**已经存在的事实**，不猜：
 *  1. 落盘通道：显示 background 最近一次问 `hello` 拿到的答案（含 stage/machine/版本），
 *     或者具名的失败原因 + 修复命令。Popup 自己一行探测代码都没有。
 *  2. 发件箱：直接读同一个扩展源下的 IndexedDB —— 与 background 排空时读的是同一份数据。
 *  3. 「导出未送达的会话」：Blob + `<a download>`，**不用 downloads 权限**。
 *     用户点击那一下就是用户手势，所以浏览器允许这个下载。
 *
 * 🔴 本文件【绝不】发起任何网络请求，也【绝不】触发一次回溯。
 *    打开开关只是写一个布尔值；真正的 tick 仍然只由两条心跳唤醒。
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
import {
  backfillStateEntries,
  collectFailures,
  pickBackfillState,
  renderPopup,
  summarizeOutbox,
  POPUP_START_BACKFILL_MESSAGE,
  POPUP_STATUS_MESSAGE,
  type BackfillRuntimeStatus,
  type PopupModel,
  type PopupView,
} from '../../lib/popup-view';
import { clearFailures } from '../../lib/backfill/failures';
import {
  buildExportFile,
  listEntries,
  loadLastExport,
  recordExport,
  undeliveredEntries,
} from '../../lib/outbox';
import { loadHostPause, loadHostStatus } from '../../lib/host-status';
import { EXPORT_NOTHING_QUEUED, EXPORT_NO_HISTORY } from '../../lib/ui-strings';

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
  return { transportWired: false, lastTickReason: null, liveTarget: null };
}

async function collect(): Promise<PopupModel> {
  const store = browserLocalStore();
  const runtime = await askBackground();

  const enabled = await isBackfillEnabled(store);

  let snapshot: Record<string, unknown> | null = null;
  try {
    snapshot = await browserLocalSnapshot();
  } catch (err) {
    console.warn('[chat-stasher] popup snapshot read failed', (err as Error).message);
  }
  const state = pickBackfillState(snapshot);

  // 🔴 C30 · 回溯目标登记表。这是闹钟那条路【唯一】的目标来源。
  const targets = await loadTargets(store);
  const lastTick = await loadLastTick(store);

  // 🔴 W2 · 发件箱。listEntries() 返回 null = 读不出来 ⇒ 照实说读不出来，
  //    绝不显示成「空的」（那是把未知记成空）。
  let outbox: PopupModel['outbox'];
  try {
    const entries = await listEntries();
    outbox = entries === null ? null : summarizeOutbox(entries);
  } catch (err) {
    console.warn('[chat-stasher] popup outbox read failed', (err as Error).message);
    outbox = null;
  }
  const lastExport = await loadLastExport(store);
  const hostPause = await loadHostPause(store);
  // 通道状态优先用 background 刚问回来的那一次；问不到就退回 storage 里上一次的结论。
  const nativeHost = runtime.nativeHost ?? await loadHostStatus(store);

  // 🔴 与 tickBackfill 共用的那一个判断，顺序天然一致。
  const block = await tickBlockReason({
    hasStore: store !== null,
    isEnabled: () => enabled,
    isHostPaused: async () => hostPause !== null,
    hasHttp: runtime.transportWired,
    hasTargets: targets.length > 0,
  });

  return {
    enabled,
    block,
    state,
    target: state ? { platform: state.platform, scope: state.scope } : null,
    // 🔴 C20：跨所有平台/账号汇总。读不到快照 ⇒ 空清单（那时候我们确实什么都不知道）。
    failures: collectFailures(snapshot),
    lastTick,
    // 🔴 C33 · 「开始回溯这个平台」那个按钮的两个前提，都是【事实】，不是推断。
    liveTarget: runtime.liveTarget ?? null,
    targetCount: targets.length,
    nativeHost,
    outbox,
    hostPause,
    lastExport,
  };
}

/**
 * 🔴 C20 · 「我知道了，清空这份清单」。
 * 遍历快照里每一份欠账集合，把 failures / failuresDropped 清零后写回去。
 * **不触发任何重新抓取** —— 这是产品拍板的「不重试」，按钮只表示「我看到了」。
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

/**
 * 🔴 C33 · 用户按下「开始回溯这个平台」。
 *
 * 登记这件事交给 background 做，不在这里直接写 storage：只有它能现场 ping 出
 * 「此刻活着的那个通道是谁」。Popup 手里那份 liveTarget 是打开时的快照，
 * 用它去登记就等于拿一份可能已经过期的事实当真 —— 那正是「猜」。
 */
async function onStartBackfill(): Promise<void> {
  let reply: unknown = null;
  try {
    reply = await browser.runtime.sendMessage({ type: POPUP_START_BACKFILL_MESSAGE });
  } catch (err) {
    console.warn('[chat-stasher] popup start-backfill failed', (err as Error).message);
  }
  const ok = !!reply && (reply as { ok?: boolean }).ok === true;
  if (!ok) {
    console.warn('[chat-stasher] backfill target not registered:',
      (reply as { reason?: string } | null)?.reason ?? 'no reply');
  }
  await refresh();
}

/**
 * 🔴 W2 · 「导出未送达的会话」。
 *
 * 规范 §8：文件名 `chat-stasher-export-<UTC yyyymmddThhmmssZ>.jsonl`，
 * 每行一个 payload（原样）加一个 `\n`。
 *
 * 🔴 用户点击 = 用户手势，所以 `<a download>` 是被允许的 —— 这里【不使用】
 *    `downloads` 权限（它已经从 manifest 里删掉了，见 wxt.config.ts）。
 *
 * 🔴 导出【不删除】任何条目：它们仍然排在发件箱里，主机恢复之后会以 duplicate
 *    被确认并清掉（§7 —— 内容寻址，重发是安全的）。
 */
async function onExportUndelivered(): Promise<void> {
  let entries;
  try {
    entries = await undeliveredEntries();
  } catch (err) {
    console.warn('[chat-stasher] popup outbox read failed', (err as Error).message);
    entries = null;
  }
  if (entries === null) {
    // 读不出来就【不生成文件】：凭空生成一个空文件，等于告诉用户「没有未送达的」。
    setExportNote('Outbox: unreadable — this browser context has no IndexedDB. '
      + 'No export file was written.');
    return;
  }
  if (entries.length === 0) {
    // 空发件箱：一个文件都不生成，照实说一句。
    setExportNote(EXPORT_NOTHING_QUEUED);
    return;
  }

  const at = Date.now();
  const file = buildExportFile(entries, at);
  const blob = new Blob([file.content], { type: 'application/x-ndjson' });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = file.filename;
  anchor.style.display = 'none';
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  // 撤销得晚一点：撤销太早会让下载拿不到数据（Firefox 尤其）。
  setTimeout(() => URL.revokeObjectURL(url), 30_000);

  await recordExport(browserLocalStore(), {
    at,
    entries: file.entries,
    bytes: file.bytes,
    filename: file.filename,
  });
  await refresh();
}

function setExportNote(text: string): void {
  const el = document.getElementById('last-export');
  if (el) el.textContent = text;
}

function text(id: string, value: string): void {
  const el = document.getElementById(id);
  if (el) el.textContent = value;
}

function paint(view: PopupView): void {
  text('status', view.status);
  text('channel', view.channel);
  // 🔴 W2：暂停行、发件箱行与「导出」那一行都只在有内容时出现，
  //    绝不留一个空壳让用户以为「这里本来就该是空的」。
  text('pause', view.pause ?? '');
  const pauseBox = document.getElementById('pause');
  if (pauseBox) pauseBox.hidden = view.pause === null;

  text('outbox', view.outbox ?? '');
  const outboxBox = document.getElementById('outbox');
  if (outboxBox) outboxBox.hidden = view.outbox === null;

  text('last-export', view.lastExport || EXPORT_NO_HISTORY);
  const exportBtn = document.getElementById('export-file') as HTMLButtonElement | null;
  if (exportBtn) {
    exportBtn.textContent = view.exportFile.label;
    exportBtn.hidden = !view.exportFile.visible;
  }

  // 🔴 C20：有失败项时这一行必须出现在最显眼的位置；没有时整块隐藏。
  text('failures', view.failures ?? '');
  const failBox = document.getElementById('failures');
  if (failBox) failBox.hidden = view.failures === null;

  const clearBtn = document.getElementById('clear-failures') as HTMLButtonElement | null;
  if (clearBtn) {
    clearBtn.textContent = view.clearFailures.label;
    clearBtn.hidden = !view.clearFailures.visible;
  }

  const startBtn = document.getElementById('start-backfill') as HTMLButtonElement | null;
  if (startBtn) {
    startBtn.textContent = view.startBackfill.label;
    startBtn.hidden = !view.startBackfill.visible;
  }

  text('running', view.running);
  text('missing', view.missing ?? '');
  text('progress', view.progress);
  // 🔴 C22：哪些平台补得回历史、哪些暂时补不回。永远显示。
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

document.getElementById('start-backfill')?.addEventListener('click', () => {
  void onStartBackfill().catch((err) => {
    console.warn('[chat-stasher] popup start-backfill failed', (err as Error).message);
    void refresh();
  });
});

document.getElementById('export-file')?.addEventListener('click', () => {
  void onExportUndelivered().catch((err) => {
    console.warn('[chat-stasher] popup export failed', (err as Error).message);
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
